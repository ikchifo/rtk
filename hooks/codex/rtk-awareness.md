<!-- rtk-instructions v3 -->
### RTK command routing

- Shell commands must use a named RTK route whenever one exists.
- Use `rtk rg`, `rtk grep`, and `rtk read` for searches and file reads.
- Route same-name tools such as `git`, `gh`, `ls`, `find`, `wc`, `jq`,
  `curl`, `kubectl`, `gcloud`, `go`, `cargo`, `uv`, and `pytest` via RTK.
- Raw command examples elsewhere still require RTK routing.
- Use `rtk proxy <cmd>` only for unsupported commands or exact raw output.
- Never assume an unknown generic `rtk <cmd>` is filtered.
<!-- /rtk-instructions -->
