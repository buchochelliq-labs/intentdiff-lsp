# Contributing to intentumdiff-lsp

- Keep `lsp-client` generic — no IntentumDiff types leak into the codec.
- LSP payload shapes (diagnostics, code lenses) come from engine handlers; change them in
  intentumdiff-core, not here.
- Build + test per [docs/BUILDING.md](docs/BUILDING.md); do NOT introduce a workspace (it
  would disable the vendored `[patch]` tables — see docs/ARCHITECTURE.md).
