# Codex CLI Hooks

> Part of [`hooks/`](../README.md) -- see also
> [`src/hooks/`](../../src/hooks/README.md) for installation code

## Specifics

- Prompt-level guidance via awareness document -- no programmatic hook
- `rtk-awareness.md` is injected directly into `AGENTS.md` as a marker block
- `RTK.md` remains a generated reference artifact for people and tooling
- Existing relative and absolute `@RTK.md` references are migrated
  automatically
- Installed to `$CODEX_HOME` when set, otherwise `~/.codex/`, by
  `rtk init --codex`
