# intentdiff-lsp architecture

Two **standalone** crates, side by side:

- **`crates/lsp-client`** — a generic (IntentDiff-agnostic) LSP client codec: message framing,
  request/response correlation, capability negotiation. No engine dependency.
- **`crates/lsp-server`** — the native `intentdiff-lsp-server` binary: links
  [intentdiff-core](https://github.com/buchochelliq-labs/intentdiff-core) in-process (the same
  engine handlers behind the
  [C ABI](https://github.com/buchochelliq-labs/intentdiff-core/blob/main/docs/C_ABI.md))
  plus the client codec, serving semantic-diff-aware diagnostics and code lenses over LSP.

## Why no workspace

Deliberate: cargo only honors `[patch.crates-io]` tables in a build's **top-level manifest**.
A workspace root would silently disable each crate's vendored patch table, so the crates stay
standalone (each is its own build root) — mirroring how they build in the archive monorepo.
