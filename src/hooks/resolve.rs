use super::constants::REWRITE_HOOK_FILE;

const RTK_BINARY_NAME: &str = "rtk";

/// Resolve the absolute path to the rtk binary for use in hook commands.
/// ISSUE #1820: subagent shells may not source the user's profile, so bare
/// `rtk` fails with exit 127. Falls back to `"rtk hook {subcommand}"`.
pub fn resolve_absolute_hook_command(subcommand: &str) -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(canonical) = exe.canonicalize() {
            // `cargo test` binary is "rtk-<hash>", skip it.
            let is_rtk_binary = canonical
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|name| name == RTK_BINARY_NAME);

            if is_rtk_binary {
                let path_str = canonical.to_string_lossy();
                if path_str.contains(' ') {
                    let escaped = path_str.replace('"', r#"\""#);
                    return format!("\"{}\" hook {}", escaped, subcommand);
                }
                return format!("{} hook {}", path_str, subcommand);
            }
        }
    }
    format!("rtk hook {}", subcommand)
}

/// Detect whether a command string is any RTK hook variant (bare, absolute path, or legacy script).
pub fn is_rtk_hook_command(cmd: &str) -> bool {
    if cmd.contains(REWRITE_HOOK_FILE) {
        return true;
    }
    // Strip all quotes so "/path/with spaces/rtk" hook claude → /path/with spaces/rtk hook claude
    let normalized: String = cmd.chars().filter(|&c| c != '"').collect();
    if let Some(pos) = normalized.rfind("rtk hook ") {
        if pos == 0
            || matches!(
                normalized.as_bytes().get(pos - 1),
                Some(b'/' | b'\\' | b' ')
            )
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::constants::CLAUDE_HOOK_COMMAND;

    #[test]
    fn test_bare_claude_hook() {
        assert!(is_rtk_hook_command("rtk hook claude"));
    }

    #[test]
    fn test_bare_cursor_hook() {
        assert!(is_rtk_hook_command("rtk hook cursor"));
    }

    #[test]
    fn test_absolute_unix_path() {
        assert!(is_rtk_hook_command("/home/user/.cargo/bin/rtk hook claude"));
    }

    #[test]
    fn test_absolute_windows_path() {
        assert!(is_rtk_hook_command(
            "C:\\Users\\user\\.cargo\\bin\\rtk hook claude"
        ));
    }

    #[test]
    fn test_quoted_path_with_spaces() {
        assert!(is_rtk_hook_command(
            "\"/home/user/my tools/rtk\" hook claude"
        ));
    }

    #[test]
    fn test_legacy_script() {
        assert!(is_rtk_hook_command("~/.claude/hooks/rtk-rewrite.sh"));
    }

    #[test]
    fn test_legacy_script_with_path() {
        assert!(is_rtk_hook_command(
            "/home/user/.claude/hooks/rtk-rewrite.sh --some-flag"
        ));
    }

    #[test]
    fn test_unrelated_command_rejected() {
        assert!(!is_rtk_hook_command("echo hello"));
    }

    #[test]
    fn test_partial_match_rejected() {
        // "not-rtk hook claude" — hyphen is not a valid separator
        assert!(!is_rtk_hook_command("not-rtk hook claude"));
    }

    #[test]
    fn test_empty_string_rejected() {
        assert!(!is_rtk_hook_command(""));
    }

    #[test]
    fn test_fallback_in_test_binary() {
        let result = resolve_absolute_hook_command("claude");
        assert_eq!(result, CLAUDE_HOOK_COMMAND);
    }

    #[test]
    fn test_fallback_returns_bare_with_subcommand() {
        let result = resolve_absolute_hook_command("cursor");
        assert_eq!(result, "rtk hook cursor");
    }

    #[test]
    fn test_output_is_detectable() {
        let resolved = resolve_absolute_hook_command("claude");
        assert!(
            is_rtk_hook_command(&resolved),
            "resolved command '{}' not detected by is_rtk_hook_command",
            resolved
        );
    }
}
