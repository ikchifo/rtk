# RTK - Rust Token Killer (Codex CLI)

**Usage**: Token-optimized CLI proxy for shell commands.

## Route commands explicitly

Use a named RTK route instead of a generic `rtk <cmd>` prefix. An unknown
`rtk <cmd>` invocation uses the generic fallback and may run without filtering.

Use these canonical mappings:

| Shell command | RTK route |
| --- | --- |
| `rg ...`, `grep ...` | `rtk grep ...` |
| `cat <file>` | `rtk read <file>` |
| `head -N <file>` | `rtk read --line-range 1:N <file>` |
| `tail -N <file>` | `rtk read --tail-lines N <file>` |

Keep named RTK tool routes direct:

```bash
rtk git status
rtk cargo test
rtk pytest -q
```

## Codex profile

When your installed RTK version supports it, opt in explicitly:

```bash
rtk --profile codex grep -n pattern src
rtk --profile codex read src/main.rs
```

## Meta Commands

```bash
rtk gain            # Token savings analytics
rtk gain --history  # Recent command savings history
rtk proxy <cmd>     # Run raw command without filtering
```

## Verification

```bash
rtk --version
rtk gain
which rtk
```
