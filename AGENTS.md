# Agent instructions — intentdiff-lsp

Two STANDALONE crates: generic lsp-client codec + the intentdiff lsp-server binary.

## Hard invariants
- Do NOT introduce a workspace: cargo only honors [patch] tables in a build's top-level
  manifest — a workspace would silently disable the vendored patches.
- lsp-client stays IntentDiff-agnostic; LSP payload shapes come from engine handlers.

Build: per-crate --manifest-path (see docs/BUILDING.md; Rust 1.93.0).
