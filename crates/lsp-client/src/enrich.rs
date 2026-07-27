//! Generic batch-hover enrichment (#100 S2 slice 3): the transport half of
//! `TypeEnricher.enrich` (`src/intentdiff/lsp/enricher.py`), with the tree-aware target
//! collection left to the caller (in intentdiff, the core's `SemanticNode` walker — this
//! crate stays agnostic of any AST shape).
//!
//! Mirrors the Python semantics: `didOpen` → all hover queries concurrently under a
//! [`MAX_CONCURRENT_HOVERS`] cap → `didClose` (always, even when querying fails), with
//! per-target failures (timeouts, server errors) silently skipped so enrichment degrades
//! gracefully instead of failing the caller's pipeline.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::client::{LspClient, LspError};

/// Max concurrent hover requests per file (`_MAX_CONCURRENT`).
pub const MAX_CONCURRENT_HOVERS: usize = 50;

/// One hover query: the id the result is stored under, and the 0-based position to hover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverTarget {
    pub id: String,
    pub line: u32,
    pub col: u32,
}

/// `TypeEnricher.enrich`, minus the tree walk: open the document, hover every target
/// concurrently (capped), close the document, and return `{id: type_string}` for the
/// targets the server had information for.
///
/// A `didOpen` failure returns an empty map (the Python enricher logs and returns `{}`);
/// per-target failures are skipped; `didClose` is always attempted.
pub async fn enrich_targets(
    client: &LspClient,
    uri: &str,
    language_id: &str,
    text: &str,
    targets: &[HoverTarget],
) -> Result<HashMap<String, String>, LspError> {
    if client.did_open(uri, language_id, text).await.is_err() {
        return Ok(HashMap::new());
    }
    let result = hover_map(client, uri, targets).await;
    let _ = client.did_close(uri).await;
    Ok(result)
}

/// Fire all hover queries concurrently under the cap; collect non-empty results.
async fn hover_map(
    client: &LspClient,
    uri: &str,
    targets: &[HoverTarget],
) -> HashMap<String, String> {
    if targets.is_empty() {
        return HashMap::new();
    }
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_HOVERS));
    let futures = targets.iter().map(|target| {
        let sem = Arc::clone(&sem);
        async move {
            let _permit = sem.acquire().await.expect("semaphore never closed");
            match client.hover(uri, target.line, target.col).await {
                Ok(Some(type_str)) => Some((target.id.clone(), type_str)),
                Ok(None) | Err(_) => None, // no info / timeout / server error → skip
            }
        }
    });
    let results = futures_join_all(futures).await;
    results.into_iter().flatten().collect()
}

/// A tiny join_all so the crate needs no `futures` dependency: polls every future to
/// completion concurrently on the current task.
async fn futures_join_all<F, T>(futures: impl Iterator<Item = F>) -> Vec<T>
where
    F: std::future::Future<Output = T>,
{
    let mut set = Vec::new();
    for fut in futures {
        set.push(Box::pin(fut));
    }
    let mut results: Vec<Option<T>> = std::iter::repeat_with(|| None).take(set.len()).collect();
    let total = set.len();
    let mut done = 0usize;
    std::future::poll_fn(|cx| {
        for (idx, fut) in set.iter_mut().enumerate() {
            if results[idx].is_none() {
                if let std::task::Poll::Ready(value) = fut.as_mut().poll(cx) {
                    results[idx] = Some(value);
                    done += 1;
                }
            }
        }
        if done == total {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    results.into_iter().map(|r| r.expect("all futures completed")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{encode_message, DecodeEvent, FrameDecoder};
    use serde_json::{json, Value};
    use std::time::Duration;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

    /// A peer that answers initialize/shutdown, records didOpen/didClose, and answers each
    /// hover by position: line 0 → "int", line 1 → null result, line 2 → an error.
    async fn positional_peer(io: tokio::io::DuplexStream) -> tokio::task::JoinHandle<Vec<String>> {
        tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(io);
            let mut decoder = FrameDecoder::new();
            let mut chunk = [0u8; 4096];
            let mut lifecycle: Vec<String> = Vec::new();
            loop {
                let n = match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                decoder.feed(&chunk[..n]);
                while let Ok(Some(DecodeEvent::Frame(msg))) = decoder.next_event() {
                    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                    if method.starts_with("textDocument/did") {
                        lifecycle.push(method.to_owned());
                        continue;
                    }
                    let Some(id) = msg.get("id").cloned() else { continue };
                    let body = match method {
                        "initialize" | "shutdown" => {
                            json!({"jsonrpc": "2.0", "id": id, "result": {}})
                        }
                        "textDocument/hover" => {
                            let line = msg["params"]["position"]["line"].as_u64().unwrap_or(9);
                            match line {
                                0 => json!({"jsonrpc": "2.0", "id": id,
                                            "result": {"contents": "int"}}),
                                1 => json!({"jsonrpc": "2.0", "id": id, "result": Value::Null}),
                                _ => json!({"jsonrpc": "2.0", "id": id,
                                            "error": {"code": 1, "message": "boom"}}),
                            }
                        }
                        _ => continue,
                    };
                    let _ = writer.write_all(&encode_message(&body)).await;
                    let _ = writer.flush().await;
                }
            }
            lifecycle
        })
    }

    #[tokio::test]
    async fn enrich_collects_hits_and_skips_missing_and_failed_targets() {
        let (client_io, peer_io) = duplex(1 << 16);
        let peer = positional_peer(peer_io).await;
        let (r, w) = tokio::io::split(client_io);
        let client = LspClient::connect_io(
            Box::new(r),
            Box::new(w),
            "file:///ws",
            Duration::from_secs(2),
        )
        .await
        .unwrap();

        let targets = vec![
            HoverTarget { id: "n-hit".into(), line: 0, col: 4 },
            HoverTarget { id: "n-none".into(), line: 1, col: 0 },
            HoverTarget { id: "n-err".into(), line: 2, col: 0 },
        ];
        let map = enrich_targets(&client, "file:///a.py", "python", "x = 1\n", &targets)
            .await
            .unwrap();
        assert_eq!(map, HashMap::from([("n-hit".to_owned(), "int".to_owned())]));

        client.shutdown().await.unwrap();
        // didOpen before the queries, didClose after — always both.
        assert_eq!(
            peer.await.unwrap(),
            vec!["textDocument/didOpen".to_owned(), "textDocument/didClose".to_owned()]
        );
    }

    #[tokio::test]
    async fn empty_targets_short_circuit_but_still_open_and_close() {
        let (client_io, peer_io) = duplex(1 << 16);
        let peer = positional_peer(peer_io).await;
        let (r, w) = tokio::io::split(client_io);
        let client = LspClient::connect_io(
            Box::new(r),
            Box::new(w),
            "file:///ws",
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let map = enrich_targets(&client, "file:///a.py", "python", "", &[]).await.unwrap();
        assert!(map.is_empty());
        client.shutdown().await.unwrap();
        assert_eq!(
            peer.await.unwrap(),
            vec!["textDocument/didOpen".to_owned(), "textDocument/didClose".to_owned()]
        );
    }
}
