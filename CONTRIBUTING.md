# Contributing to intentdiff-lsp

- Keep `lsp-client` generic — no IntentDiff types leak into the codec.
- LSP payload shapes (diagnostics, code lenses) come from engine handlers; change them in
  intentdiff-core, not here.
- Build + test per [docs/BUILDING.md](docs/BUILDING.md); do NOT introduce a workspace (it
  would disable the vendored `[patch]` tables — see docs/ARCHITECTURE.md).
