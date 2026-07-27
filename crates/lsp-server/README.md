# intentdiff-lsp-server (native)

The native IntentDiff LSP server (#100, A2.5 Surface S3): a standalone stdio binary that
serves live semantic-diff **diagnostics**, refactoring **code lenses**, and the custom
`intentdiff/semanticDiff` request — the native successor to the pygls
`intentdiff lsp-server`, with **no Python in the process**. It links the pure-Rust engine
(`intentdiff-rust-core` with `default-features = false`) and the sans-IO framing codec from
`intentdiff-lsp-client` (the same codec both directions of the wire share). Extracted to its
own repo at the split (#82).

## Build

```bash
cd crates/lsp-server
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build --release
# -> target/release/intentdiff-lsp-server(.exe)
```

(The codegen-units override sidesteps the engine's known rustc release-OOM at
`codegen-units = 1` on Windows; see `docs/BUILDING.md`.)

## What it speaks

Standard LSP 3.17 over stdio, plus one custom method:

| Message | Behaviour |
|---|---|
| `initialize` | Advertises `textDocumentSync: Full` + a `codeLensProvider`; loads the repo's `intentdiff.yaml` config from `rootUri`; reads the client's `codeLens/refreshSupport`. |
| `textDocument/didOpen` / `didChange` | Updates the buffer and computes a diff against the ref, then **pushes** `publishDiagnostics` and (when supported) requests `workspace/codeLens/refresh`. |
| `textDocument/didClose` | Drops the cached diff and buffer. |
| `textDocument/codeLens` | Returns refactoring-family lenses from the last computed diff (pull, from cache). |
| `intentdiff/semanticDiff` | `oldUri == newUri`: the live buffer (or on-disk file) vs the ref. Distinct URIs: a workspace-contained two-file compare. Errors are in-band `{"error": …}`. |
| `shutdown` / `exit` | Clean exit, no orphaned threads (#73). |

**Compute** is the engine's `live_handle_diff_impl` (config-aware, all-language, guardrails
included) — identical to what the live-server binary serves. **Response shapes** are the
core's `lsp_server_shapes` mappings, parity-locked against `lsprotocol`'s encoder.

## First-cut scope (deliberate)

- **stdio only.** (`--tcp` remains a Python-only convenience; the socket/named-pipe
  transports with the #88 controls are their own careful slice.)
- **Debounce by coalescing, not timers.** One reader thread queues frames; before computing,
  the loop drains what is already buffered and keeps only the *latest* content per URI — a
  keystroke burst costs one diff. No async runtime, one joined thread.
- **`uri_to_path` ports the #88 guards:** file scheme only, percent-decode, explicit `..`
  rejection, the Windows drive-letter fix, and workspace containment.
- Engine fallbacks publish nothing for that revision (the pygls `diff is None` behaviour).

**Parser wasm dir resolution:** `--wasm-dir` flag → `INTENTDIFF_WASM_DIR` env →
`<exe dir>/wasm` → dev-layout ancestor walk for `src/intentdiff/wasm`; every candidate is
verified by `parser_manifest.json`.

## Testing

`tests/lsp_session.rs` spawns the built binary and drives a full editor session over stdio
(handshake, an edit → diagnostics + lens refresh, codeLens pull, both `semanticDiff` modes,
the containment guard, clean exit). It is self-contained — reuses the `intentdiff-lsp-client`
codec, shells out to `git` for the fixture, needs no Python — and skips gracefully when the
bundled wasm parsers or `git` are absent.

```bash
cargo test          # unit (uri_to_path guards) + the integration session
```

## Cutover

The CLI/pyproject switch (making `intentdiff lsp-server` spawn this binary) and deleting
`src/intentdiff/lsp_server/` + the `pygls` / `lsprotocol` extras ride the Phase-B clap CLI —
the porting work itself is complete.
