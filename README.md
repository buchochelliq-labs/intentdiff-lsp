# intentumdiff-lsp

[![CI](https://github.com/buchochelliq-labs/intentumdiff-lsp/actions/workflows/ci.yml/badge.svg)](https://github.com/buchochelliq-labs/intentumdiff-lsp/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust 1.95](https://img.shields.io/badge/rust-1.95-orange.svg)](https://www.rust-lang.org/)

The **IntentumDiff LSP layer** — two standalone crates (side-by-side, mirroring their
monorepo build shape; each owns its `[patch]` table):

- `crates/lsp-client` — the generic (intentumdiff-agnostic) LSP client codec.
- `crates/lsp-server` — the native `intentumdiff-lsp-server` binary: links the engine
  ([intentumdiff-core](https://github.com/buchochelliq-labs/intentumdiff-core)) in-process
  plus the client codec.

## Build

```bash
cargo build --release --manifest-path crates/lsp-server/Cargo.toml
cargo test  --manifest-path crates/lsp-client/Cargo.toml
cargo test  --manifest-path crates/lsp-server/Cargo.toml
```

Toolchain: Rust 1.93.0 (pinned in CI).

## Provenance

Migrated files-only (no history) from the IntentumDiff monorepo
(`buchochelliq-labs/intentumdiff`), which remains the archive of record. License: MIT.
