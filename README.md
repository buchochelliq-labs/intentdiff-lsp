# intentdiff-lsp

[![CI](https://github.com/buchochelliq-labs/intentdiff-lsp/actions/workflows/ci.yml/badge.svg)](https://github.com/buchochelliq-labs/intentdiff-lsp/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust 1.93](https://img.shields.io/badge/rust-1.93-orange.svg)](https://www.rust-lang.org/)

The **IntentDiff LSP layer** — two standalone crates (side-by-side, mirroring their
monorepo build shape; each owns its `[patch]` table):

- `crates/lsp-client` — the generic (intentdiff-agnostic) LSP client codec.
- `crates/lsp-server` — the native `intentdiff-lsp-server` binary: links the engine
  ([intentdiff-core](https://github.com/buchochelliq-labs/intentdiff-core)) in-process
  plus the client codec.

## Build

```bash
cargo build --release --manifest-path crates/lsp-server/Cargo.toml
cargo test  --manifest-path crates/lsp-client/Cargo.toml
cargo test  --manifest-path crates/lsp-server/Cargo.toml
```

Toolchain: Rust 1.93.0 (pinned in CI).

## Provenance

Migrated files-only (no history) from the IntentDiff monorepo
(`buchochelliq-labs/intentdiff`), which remains the archive of record. License: MIT.
