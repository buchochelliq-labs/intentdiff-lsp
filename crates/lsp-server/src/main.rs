//! Native IntentDiff LSP server (#100, A2.5 S3 slice 2 — first cut): stdio transport.
//!
//! Replaces the pygls `intentdiff lsp-server` for the editor flow: live semantic-diff
//! diagnostics (push) + refactoring code lenses (pull) + the custom
//! `intentdiff/semanticDiff` request, with ZERO Python in the process. The LSP framing is
//! the `intentdiff-lsp-client` sans-IO codec (transport-symmetric); the compute is the
//! core's `live_handle_diff_impl` (config-aware, all-language, guardrails included); the
//! response shapes are the core's `lsp_server_shapes` mappings (parity-locked against
//! lsprotocol's encoder).
//!
//! First-cut scope (deliberate):
//! - **stdio only** (matching the live-server transport decision; `--tcp` is Python-only).
//! - **Debounce by coalescing, not timers**: the reader thread queues messages; before
//!   computing, the main loop drains everything already buffered and keeps only the LATEST
//!   content per URI — a burst of `didChange` costs one diff. No timers, no async runtime,
//!   one thread, clean #73 exit on EOF.
//! - Engine fallbacks (`{"fallback": …}`) publish no diagnostics for that revision — the
//!   pygls server's `diff is None` behaviour.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use intentdiff_lsp_client::{encode_message, DecodeEvent, FrameDecoder};
use intentdiff_rust_core::live_server as proto;
use intentdiff_rust_core::lsp_server_shapes as shapes;
use serde_json::{json, Value};

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn dir_has_manifest(dir: &Path) -> bool {
    dir.join("parser_manifest.json").is_file()
}

/// Bundled-parser dir, zero-setup (the live-server chain): `--wasm-dir` > env >
/// exe-adjacent `wasm/` > dev-layout ancestor walk — every candidate manifest-verified.
fn resolve_wasm_dir(args: &[String]) -> String {
    if let Some(dir) = arg_value(args, "--wasm-dir") {
        return dir;
    }
    if let Ok(dir) = std::env::var("INTENTDIFF_WASM_DIR") {
        if !dir.trim().is_empty() {
            return dir;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let shipped = exe_dir.join("wasm");
            if dir_has_manifest(&shipped) {
                return shipped.to_string_lossy().into_owned();
            }
            for ancestor in exe_dir.ancestors() {
                let dev = ancestor.join("src").join("intentdiff").join("wasm");
                if dir_has_manifest(&dev) {
                    return dev.to_string_lossy().into_owned();
                }
            }
        }
    }
    String::new()
}

/// `uri_to_path` (#88 guards ported from `_handlers.py`): file scheme only, percent-decode,
/// explicit `..` traversal rejection, the Windows leading-slash drive fix, and optional
/// workspace containment.
fn uri_to_path(uri: &str, workspace_root: Option<&Path>) -> Result<PathBuf, String> {
    let rest = if let Some(rest) = uri.strip_prefix("file://") {
        rest
    } else if uri.contains("://") {
        return Err(format!("Only file:// URIs are supported, got: {uri:?}"));
    } else {
        uri
    };
    // Strip an authority component (file://host/path is not supported; file:///path is).
    let raw_path = rest.split(['?', '#']).next().unwrap_or(rest);
    let decoded = percent_decode(raw_path);
    if decoded.split(['/', '\\']).any(|part| part == "..") {
        return Err(format!("Path traversal rejected in URI: {uri:?}"));
    }
    // Windows: "/C:/Users/…" → "C:/Users/…".
    let mut text = decoded;
    if text.len() >= 3 {
        let bytes = text.as_bytes();
        if bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
            text = text[1..].to_owned();
        }
    }
    let path = PathBuf::from(&text);
    let resolved = normalize(&path);
    if let Some(root) = workspace_root {
        let root = normalize(root);
        if !resolved.starts_with(&root) {
            return Err(format!(
                "URI {uri:?} resolves to {resolved:?} which is outside the workspace root {root:?}."
            ));
        }
    }
    Ok(resolved)
}

/// Lexical normalization (Python `Path.resolve()` without symlink IO): absolute via cwd,
/// `.` dropped. `..` never appears here — `uri_to_path` rejects it earlier.
fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for comp in absolute.components() {
        match comp {
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct ServerState {
    root: Option<PathBuf>,
    git_ref: String,
    config_json: String,
    wasm_dir: String,
    supports_codelens_refresh: bool,
    /// uri → last served SemanticDiff (the codeLens pull cache).
    diff_cache: HashMap<String, Value>,
    /// uri → latest known buffer content (didOpen/didChange).
    content_cache: HashMap<String, String>,
    /// id source for server→client requests (codeLens refresh).
    next_server_id: i64,
}

fn write_msg(out: &mut impl Write, msg: &Value) {
    let _ = out.write_all(&encode_message(msg));
    let _ = out.flush();
}

fn respond(out: &mut impl Write, id: &Value, result: Value) {
    write_msg(out, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn respond_error(out: &mut impl Write, id: &Value, code: i64, message: &str) {
    write_msg(
        out,
        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let git_ref = arg_value(&args, "--ref").unwrap_or_else(|| "HEAD".to_owned());
    let wasm_dir = resolve_wasm_dir(&args);

    let mut state = ServerState {
        root: None,
        git_ref,
        config_json: "{}".to_owned(),
        wasm_dir,
        supports_codelens_refresh: false,
        diff_cache: HashMap::new(),
        content_cache: HashMap::new(),
        next_server_id: 1,
    };

    // Reader thread: bytes → frames → channel. Exits on EOF/fatal framing; dropping the
    // sender ends the main loop (#73: one thread, joined, nothing orphaned).
    let (tx, rx) = mpsc::channel::<Value>();
    let reader = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut decoder = FrameDecoder::new();
        let mut chunk = [0u8; 8_192];
        loop {
            let n = match stdin.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            decoder.feed(&chunk[..n]);
            loop {
                match decoder.next_event() {
                    Ok(Some(DecodeEvent::Frame(msg))) => {
                        if tx.send(msg).is_err() {
                            return;
                        }
                    }
                    Ok(Some(DecodeEvent::MalformedFrame)) => {}
                    Ok(None) => break,
                    Err(_) => return, // fatal framing violation → close
                }
            }
        }
    });

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    'main: while let Ok(first) = rx.recv() {
        // Debounce by coalescing: drain whatever is already buffered, then keep only the
        // LATEST content per URI. Everything else is handled in arrival order.
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        let mut dirty: Vec<String> = Vec::new();
        for msg in &batch {
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            if method.is_empty() {
                continue; // a response to one of OUR requests (codeLens refresh) — ignore
            }
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let id = msg.get("id");
            match method {
                "initialize" => {
                    let root_uri = params
                        .get("rootUri")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| {
                            params
                                .get("workspaceFolders")
                                .and_then(Value::as_array)
                                .and_then(|f| f.first())
                                .and_then(|f| f.get("uri"))
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        });
                    if let Some(uri) = root_uri {
                        if let Ok(path) = uri_to_path(&uri, None) {
                            // Repo config, same precedence as the live-server binary.
                            if let Ok(loaded) =
                                proto::live_load_project_config_impl(&path.to_string_lossy())
                            {
                                if loaded.trim() != "{}" {
                                    state.config_json = loaded;
                                }
                            }
                            state.root = Some(path);
                        }
                    }
                    state.supports_codelens_refresh = params
                        .pointer("/capabilities/workspace/codeLens/refreshSupport")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if let Some(id) = id {
                        respond(
                            &mut out,
                            id,
                            json!({
                                "capabilities": {
                                    "textDocumentSync": 1,
                                    "codeLensProvider": {"resolveProvider": false},
                                },
                                "serverInfo": {"name": "intentdiff-lsp", "version": "1.0.0"},
                            }),
                        );
                    }
                }
                "initialized" => {}
                "textDocument/didOpen" => {
                    let uri = params.pointer("/textDocument/uri").and_then(Value::as_str);
                    let text = params.pointer("/textDocument/text").and_then(Value::as_str);
                    if let (Some(uri), Some(text)) = (uri, text) {
                        state.content_cache.insert(uri.to_owned(), text.to_owned());
                        if !dirty.iter().any(|u| u == uri) {
                            dirty.push(uri.to_owned());
                        }
                    }
                }
                "textDocument/didChange" => {
                    let uri = params.pointer("/textDocument/uri").and_then(Value::as_str);
                    // Full sync — the last (and only) item is the complete document.
                    let text = params
                        .get("contentChanges")
                        .and_then(Value::as_array)
                        .and_then(|c| c.last())
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str);
                    if let (Some(uri), Some(text)) = (uri, text) {
                        state.content_cache.insert(uri.to_owned(), text.to_owned());
                        if !dirty.iter().any(|u| u == uri) {
                            dirty.push(uri.to_owned());
                        }
                    }
                }
                "textDocument/didClose" => {
                    if let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) {
                        state.diff_cache.remove(uri);
                        state.content_cache.remove(uri);
                        dirty.retain(|u| u != uri);
                    }
                }
                "textDocument/codeLens" => {
                    let uri = params
                        .pointer("/textDocument/uri")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let lenses = state
                        .diff_cache
                        .get(uri)
                        .map(|diff| shapes::semantic_diff_to_codelens_value(diff))
                        .unwrap_or_default();
                    if let Some(id) = id {
                        respond(&mut out, id, Value::Array(lenses));
                    }
                }
                "intentdiff/semanticDiff" => {
                    if let Some(id) = id {
                        let result = semantic_diff_request(&mut state, &params);
                        respond(&mut out, id, result);
                    }
                }
                "shutdown" => {
                    if let Some(id) = id {
                        respond(&mut out, id, Value::Null);
                    }
                }
                "exit" => break 'main,
                _ => {
                    if let Some(id) = id {
                        respond_error(&mut out, id, -32601, &format!("method not found: {method}"));
                    }
                }
            }
        }

        // Compute once per dirty URI with the final coalesced content.
        for uri in dirty {
            compute_and_push(&mut state, &mut out, &uri);
        }
    }

    drop(rx);
    let _ = reader.join();
}

/// `_compute_and_push`: diff the live buffer against the ref, cache for codeLens, publish
/// diagnostics, and nudge the client's lens refresh when supported. Engine fallbacks and
/// out-of-root files publish nothing (the pygls `diff is None` path).
fn compute_and_push(state: &mut ServerState, out: &mut impl Write, uri: &str) {
    let Some(root) = state.root.clone() else { return };
    let Some(content) = state.content_cache.get(uri).cloned() else { return };
    let Ok(file_path) = uri_to_path(uri, Some(&root)) else { return };
    let Ok(rel) = file_path.strip_prefix(&root) else { return };
    let rel = rel.to_string_lossy().replace('\\', "/");
    let served = proto::live_handle_diff_impl(
        &root.to_string_lossy(),
        &rel,
        &state.git_ref,
        &content,
        &state.config_json,
        &state.wasm_dir,
    );
    let Ok(payload) = served else { return };
    let Some(parsed) = serde_json::from_str::<Value>(&payload).ok() else { return };
    let Some(diff) = parsed.get("diff").cloned() else { return };

    let diagnostics = shapes::semantic_diff_to_diagnostics_value(&diff);
    state.diff_cache.insert(uri.to_owned(), diff);
    write_msg(
        out,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "diagnostics": diagnostics},
        }),
    );
    if state.supports_codelens_refresh {
        let id = state.next_server_id;
        state.next_server_id += 1;
        write_msg(
            out,
            &json!({"jsonrpc": "2.0", "id": id, "method": "workspace/codeLens/refresh",
                    "params": Value::Null}),
        );
    }
}

/// The custom `intentdiff/semanticDiff` request: same-URI = live buffer (or on-disk file)
/// vs the ref; different URIs = workspace-contained two-file compare. Errors are in-band
/// `{"error": …}` objects, exactly like the pygls handler.
fn semantic_diff_request(state: &mut ServerState, params: &Value) -> Value {
    let old_uri = params.get("oldUri").and_then(Value::as_str).unwrap_or("");
    let new_uri = params.get("newUri").and_then(Value::as_str).unwrap_or("");
    if old_uri.is_empty() || new_uri.is_empty() {
        return json!({"error": "oldUri and newUri are required"});
    }
    let Some(root) = state.root.clone() else {
        return json!({"error": "intentdiff/semanticDiff requires a valid workspace root"});
    };
    let compute = |state: &ServerState| -> Result<Value, String> {
        if old_uri == new_uri {
            let file_path = uri_to_path(new_uri, Some(&root))?;
            let content = match state.content_cache.get(new_uri) {
                Some(text) => text.clone(),
                None => std::fs::read_to_string(&file_path).map_err(|e| e.to_string())?,
            };
            let rel = file_path
                .strip_prefix(&root)
                .map_err(|_| "file is outside the workspace root".to_owned())?
                .to_string_lossy()
                .replace('\\', "/");
            let payload = proto::live_handle_diff_impl(
                &root.to_string_lossy(),
                &rel,
                &state.git_ref,
                &content,
                &state.config_json,
                &state.wasm_dir,
            )?;
            Ok(serde_json::from_str(&payload).unwrap_or(Value::Null))
        } else {
            let old_path = uri_to_path(old_uri, Some(&root))?;
            let new_path = uri_to_path(new_uri, Some(&root))?;
            let old_content = std::fs::read_to_string(&old_path).map_err(|e| e.to_string())?;
            let new_content = std::fs::read_to_string(&new_path).map_err(|e| e.to_string())?;
            let filename = new_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let payload = proto::live_diff_contents_impl(
                &root.to_string_lossy(),
                &filename,
                &old_content,
                &new_content,
                &state.config_json,
                &state.wasm_dir,
            )?;
            Ok(serde_json::from_str(&payload).unwrap_or(Value::Null))
        }
    };
    match compute(state) {
        Ok(parsed) => {
            if let Some(diff) = parsed.get("diff") {
                diff.clone()
            } else if let Some(reason) = parsed.get("fallback").and_then(Value::as_str) {
                json!({"error": format!("native_fallback: {reason}")})
            } else {
                json!({"error": "Diff computation failed"})
            }
        }
        Err(e) => json!({"error": e}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_to_path_ports_the_python_guards() {
        // Traversal rejected.
        assert!(uri_to_path("file:///a/../etc/passwd", None).is_err());
        // Non-file schemes rejected.
        assert!(uri_to_path("https://example.com/x", None).is_err());
        // Percent-decoding.
        let p = uri_to_path("file:///tmp/a%20b.py", None).unwrap();
        assert!(p.to_string_lossy().contains("a b.py"));
        // Windows drive fix: "/C:/x" loses the leading slash.
        if cfg!(windows) {
            let p = uri_to_path("file:///C:/Users/x.py", None).unwrap();
            assert!(p.to_string_lossy().starts_with("C:"));
        }
    }

    #[test]
    fn containment_guard_rejects_out_of_root_paths() {
        let root = std::env::temp_dir();
        let inside = format!("file:///{}", root.join("f.py").to_string_lossy().replace('\\', "/"));
        assert!(uri_to_path(&inside, Some(&root)).is_ok());
        let outside = if cfg!(windows) { "file:///C:/other-root/f.py" } else { "file:///other-root/f.py" };
        assert!(uri_to_path(outside, Some(&root)).is_err());
    }
}
