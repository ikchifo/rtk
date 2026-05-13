use crate::core::stream::exec_capture;
use crate::core::tracking;
use crate::core::utils::{human_bytes, join_with_overflow, resolved_command};
use anyhow::{Context, Result};

/// Max items before truncation in list/du output.
const MAX_ITEMS: usize = 20;
/// Max lines for cat output before truncation.
const MAX_CAT_LINES: usize = 20;
/// Max per-directory lines in du output.
const MAX_DU_LINES: usize = 15;
/// Max lines for unknown subcommand fallback.
const MAX_FALLBACK_LINES: usize = 60;

/// Run a gsutil command with token-optimized output.
pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let subcmd = args.first().map(String::as_str).unwrap_or("");
    let rest = if args.len() > 1 { &args[1..] } else { &[] };

    match subcmd {
        "ls" => run_ls(rest, verbose),
        "cp" | "mv" | "rsync" => run_transfer(subcmd, rest, verbose),
        "cat" => run_cat(rest, verbose),
        "stat" => run_stat(rest, verbose),
        "du" => run_du(rest, verbose),
        _ => run_fallback(args, verbose),
    }
}

fn run_ls(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let raw_cmd = format!("gsutil ls {}", args.join(" "));

    if verbose > 0 {
        eprintln!("gsutil ls {}", args.join(" "));
    }

    let mut cmd = resolved_command("gsutil");
    cmd.arg("ls");
    cmd.args(args);
    let result = exec_capture(&mut cmd).context("failed to run gsutil ls")?;
    let raw_output = result.combined();

    if !result.success() {
        let msg = compact_error(&result.stderr);
        println!("{}", msg);
        timer.track(&raw_cmd, "rtk gsutil ls", &raw_output, &msg);
        return Ok(result.exit_code);
    }

    let is_long = args.iter().any(|a| a == "-l" || a == "-la" || a == "-al");
    let filtered = if is_long {
        filter_ls_long(&result.stdout)
    } else {
        filter_ls_short(&result.stdout)
    };

    println!("{}", filtered);
    timer.track(&raw_cmd, "rtk gsutil ls", &raw_output, &filtered);
    Ok(0)
}

fn run_transfer(subcmd: &str, args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let raw_cmd = format!("gsutil {} {}", subcmd, args.join(" "));

    if verbose > 0 {
        eprintln!("gsutil {} {}", subcmd, args.join(" "));
    }

    let mut cmd = resolved_command("gsutil");
    cmd.arg(subcmd);
    cmd.args(args);
    let result = exec_capture(&mut cmd).context("failed to run gsutil transfer")?;
    let raw_output = result.combined();

    if !result.success() {
        let msg = compact_error(&result.stderr);
        println!("{}", msg);
        timer.track(
            &raw_cmd,
            &format!("rtk gsutil {}", subcmd),
            &raw_output,
            &msg,
        );
        return Ok(result.exit_code);
    }

    let filtered = filter_transfer(&result.stdout, &result.stderr, subcmd);
    println!("{}", filtered);
    timer.track(
        &raw_cmd,
        &format!("rtk gsutil {}", subcmd),
        &raw_output,
        &filtered,
    );
    Ok(0)
}

fn run_cat(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let raw_cmd = format!("gsutil cat {}", args.join(" "));

    if verbose > 0 {
        eprintln!("gsutil cat {}", args.join(" "));
    }

    let mut cmd = resolved_command("gsutil");
    cmd.arg("cat");
    cmd.args(args);
    let result = exec_capture(&mut cmd).context("failed to run gsutil cat")?;
    let raw_output = result.combined();

    if !result.success() {
        let msg = compact_error(&result.stderr);
        println!("{}", msg);
        timer.track(&raw_cmd, "rtk gsutil cat", &raw_output, &msg);
        return Ok(result.exit_code);
    }

    let filtered = filter_cat(&result.stdout);
    println!("{}", filtered);
    timer.track(&raw_cmd, "rtk gsutil cat", &raw_output, &filtered);
    Ok(0)
}

fn run_stat(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let raw_cmd = format!("gsutil stat {}", args.join(" "));

    if verbose > 0 {
        eprintln!("gsutil stat {}", args.join(" "));
    }

    let mut cmd = resolved_command("gsutil");
    cmd.arg("stat");
    cmd.args(args);
    let result = exec_capture(&mut cmd).context("failed to run gsutil stat")?;
    let raw_output = result.combined();

    if !result.success() {
        let msg = compact_error(&result.stderr);
        println!("{}", msg);
        timer.track(&raw_cmd, "rtk gsutil stat", &raw_output, &msg);
        return Ok(result.exit_code);
    }

    let filtered = filter_stat(&result.stdout);
    println!("{}", filtered);
    timer.track(&raw_cmd, "rtk gsutil stat", &raw_output, &filtered);
    Ok(0)
}

fn run_du(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let raw_cmd = format!("gsutil du {}", args.join(" "));

    if verbose > 0 {
        eprintln!("gsutil du {}", args.join(" "));
    }

    let mut cmd = resolved_command("gsutil");
    cmd.arg("du");
    cmd.args(args);
    let result = exec_capture(&mut cmd).context("failed to run gsutil du")?;
    let raw_output = result.combined();

    if !result.success() {
        let msg = compact_error(&result.stderr);
        println!("{}", msg);
        timer.track(&raw_cmd, "rtk gsutil du", &raw_output, &msg);
        return Ok(result.exit_code);
    }

    let filtered = filter_du(&result.stdout);
    println!("{}", filtered);
    timer.track(&raw_cmd, "rtk gsutil du", &raw_output, &filtered);
    Ok(0)
}

fn run_fallback(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let raw_cmd = format!("gsutil {}", args.join(" "));

    if verbose > 0 {
        eprintln!("gsutil {}", args.join(" "));
    }

    let mut cmd = resolved_command("gsutil");
    cmd.args(args);
    let result = exec_capture(&mut cmd).context("failed to run gsutil")?;
    let raw_output = result.combined();

    if !result.success() {
        let msg = compact_error(&result.stderr);
        println!("{}", msg);
        timer.track(&raw_cmd, "rtk gsutil", &raw_output, &msg);
        return Ok(result.exit_code);
    }

    let lines: Vec<&str> = result.stdout.lines().collect();
    let total = lines.len();
    let filtered = if total > MAX_FALLBACK_LINES {
        let kept: Vec<String> = lines
            .iter()
            .take(MAX_FALLBACK_LINES)
            .map(|l| (*l).to_string())
            .collect();
        join_with_overflow(&kept, total, MAX_FALLBACK_LINES, "lines")
    } else {
        result.stdout.clone()
    };

    println!("{}", filtered);
    timer.track(&raw_cmd, "rtk gsutil", &raw_output, &filtered);
    Ok(result.exit_code)
}

// -- filter helpers ----------------------------------------------------------

fn filter_ls_short(stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    let total = lines.len();
    if total <= MAX_ITEMS {
        return stdout.trim_end().to_string();
    }
    let kept: Vec<String> = lines
        .iter()
        .take(MAX_ITEMS)
        .map(|l| (*l).to_string())
        .collect();
    join_with_overflow(&kept, total, MAX_ITEMS, "objects")
}

/// Parse `gsutil ls -l` output. Each line: `SIZE  DATETIME  NAME`, with a
/// `TOTAL:` summary line at the end. Keep at most MAX_ITEMS entries, always
/// keep the TOTAL line, and produce a compact summary header.
fn filter_ls_long(stdout: &str) -> String {
    let mut entries: Vec<String> = Vec::new();
    let mut total_line: Option<&str> = None;
    let mut total_bytes: u64 = 0;
    let mut count: usize = 0;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("TOTAL:") {
            total_line = Some(trimmed);
            continue;
        }
        count += 1;
        // Parse size from first whitespace-delimited field.
        if let Some(size_str) = trimmed.split_whitespace().next() {
            if let Ok(size) = size_str.parse::<u64>() {
                total_bytes += size;
            }
        }
        if entries.len() < MAX_ITEMS {
            entries.push(compact_ls_long_line(trimmed));
        }
    }

    let summary = format!(
        "{} objects | {}",
        count,
        total_line
            .map(|l| l.to_string())
            .unwrap_or_else(|| human_bytes(total_bytes))
    );

    let mut out = String::with_capacity(summary.len() + entries.len() * 60);
    out.push_str(&summary);
    out.push('\n');
    for entry in &entries {
        out.push_str(entry);
        out.push('\n');
    }
    if count > MAX_ITEMS {
        out.push_str(&format!("... +{} more objects", count - MAX_ITEMS));
    }
    out.trim_end().to_string()
}

/// Compact a single `gsutil ls -l` line: `SIZE  DATE  NAME` -> `SIZE NAME`.
fn compact_ls_long_line(line: &str) -> String {
    let parts: Vec<&str> = line.split_whitespace().collect();
    // Typical format: SIZE DATE TIME NAME (or just SIZE NAME for dirs)
    if parts.len() >= 3 {
        let size = parts[0];
        let name = parts[parts.len() - 1];
        format!("{:>10}  {}", size, name)
    } else {
        line.to_string()
    }
}

fn filter_transfer(stdout: &str, stderr: &str, subcmd: &str) -> String {
    let mut file_count: usize = 0;
    let mut total_bytes: u64 = 0;

    // Count "Copying ..." or "Removing ..." lines from stderr (gsutil progress).
    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Copying ")
            || trimmed.starts_with("Removing ")
            || trimmed.starts_with("Moving ")
        {
            file_count += 1;
        }
    }

    // Parse "Operation completed over N objects/B" summary from stderr.
    for line in stderr.lines() {
        if let Some(summary) = parse_operation_summary(line.trim()) {
            if summary.0 > 0 {
                file_count = summary.0;
            }
            if summary.1 > 0 {
                total_bytes = summary.1;
            }
        }
    }

    // Also scan stdout for any summary info.
    if file_count == 0 {
        file_count = stdout.lines().count().max(1);
    }

    let size_part = if total_bytes > 0 {
        format!(" | {}", human_bytes(total_bytes))
    } else {
        String::new()
    };

    format!("{} ok | {} files{}", subcmd, file_count, size_part)
}

/// Parse "Operation completed over N objects/X.Y MiB." from gsutil stderr.
fn parse_operation_summary(line: &str) -> Option<(usize, u64)> {
    if !line.contains("Operation completed") {
        return None;
    }
    let count = extract_number_before(line, "object");
    let bytes = extract_byte_value(line);
    Some((count.unwrap_or(0), bytes.unwrap_or(0)))
}

fn extract_number_before(s: &str, keyword: &str) -> Option<usize> {
    let idx = s.find(keyword)?;
    let before = s[..idx].trim();
    let num_str = before.rsplit_once(' ').map(|(_, n)| n).unwrap_or(before);
    num_str.parse().ok()
}

fn extract_byte_value(s: &str) -> Option<u64> {
    // Look for patterns like "12.3 MiB" or "1.5 GiB" or "512 B"
    let slash_idx = s.find('/')?;
    let after = &s[slash_idx + 1..];
    let after = after.trim().trim_end_matches('.');

    let parts: Vec<&str> = after.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let num: f64 = parts[0].parse().ok()?;
    let unit = parts[1].trim_end_matches('.');
    let multiplier = match unit {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((num * multiplier) as u64)
}

fn filter_cat(stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    let total = lines.len();
    if total <= MAX_CAT_LINES {
        return stdout.to_string();
    }
    let mut out = String::with_capacity(MAX_CAT_LINES * 80 + 40);
    for line in lines.iter().take(MAX_CAT_LINES) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("... +{} more lines", total - MAX_CAT_LINES));
    out
}

/// Keep only key metadata fields from `gsutil stat` output.
const STAT_KEEP_FIELDS: &[&str] = &[
    "Content-Length",
    "Content-Type",
    "Update time",
    "Storage class",
    "Hash (md5)",
    "Hash (crc32c)",
];

fn filter_stat(stdout: &str) -> String {
    let mut out = String::with_capacity(256);
    let mut current_url: Option<&str> = None;

    for line in stdout.lines() {
        let trimmed = line.trim();
        // Object URL lines start with "gs://"
        if trimmed.starts_with("gs://") {
            if current_url.is_some() {
                out.push('\n');
            }
            current_url = Some(trimmed);
            out.push_str(trimmed);
            out.push('\n');
            continue;
        }
        if current_url.is_none() {
            continue;
        }
        // Keep only lines whose key is in our allowlist.
        if let Some((key, _)) = trimmed.split_once(':') {
            let key = key.trim();
            if STAT_KEEP_FIELDS.iter().any(|&f| key.contains(f)) {
                out.push_str("  ");
                out.push_str(trimmed);
                out.push('\n');
            }
        }
    }
    out.trim_end().to_string()
}

fn filter_du(stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();

    // gsutil du -s gives a single total line; pass through.
    if lines.len() <= 2 {
        return stdout.trim_end().to_string();
    }

    // Last line is typically the total. Keep it always.
    let total_line = lines.last().copied().unwrap_or("");
    let detail_lines = &lines[..lines.len().saturating_sub(1)];
    let detail_count = detail_lines.len();

    if detail_count <= MAX_DU_LINES {
        return stdout.trim_end().to_string();
    }

    let kept: Vec<String> = detail_lines
        .iter()
        .take(MAX_DU_LINES)
        .map(|l| (*l).to_string())
        .collect();
    let mut out = join_with_overflow(&kept, detail_count, MAX_DU_LINES, "directories");
    out.push('\n');
    out.push_str(total_line);
    out
}

fn compact_error(stderr: &str) -> String {
    // Return the first non-empty, non-progress line from stderr.
    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip progress indicators.
        if trimmed.starts_with("Copying ")
            || trimmed.starts_with("/ [")
            || trimmed.starts_with("\\ [")
            || trimmed.starts_with("- [")
            || trimmed.starts_with("| [")
        {
            continue;
        }
        let chars: Vec<char> = trimmed.chars().collect();
        if chars.len() > 120 {
            let truncated: String = chars[..120].iter().collect();
            return format!("{}...", truncated);
        }
        return trimmed.to_string();
    }
    "gsutil: unknown error".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_ls_short_under_limit() {
        let input = "gs://bucket/a\ngs://bucket/b\ngs://bucket/c\n";
        let result = filter_ls_short(input);
        assert_eq!(result, "gs://bucket/a\ngs://bucket/b\ngs://bucket/c");
    }

    #[test]
    fn test_filter_ls_short_over_limit() {
        let lines: Vec<String> = (0..30).map(|i| format!("gs://bucket/obj{}", i)).collect();
        let input = lines.join("\n");
        let result = filter_ls_short(&input);
        assert!(result.contains("... +10 more objects"));
        // First 20 should be present.
        assert!(result.contains("gs://bucket/obj0"));
        assert!(result.contains("gs://bucket/obj19"));
        // 21st should not.
        assert!(!result.contains("gs://bucket/obj20"));
    }

    #[test]
    fn test_filter_ls_long_basic() {
        let input = "\
         100  2024-01-15T10:00:00Z  gs://bucket/file1\n\
         200  2024-01-15T11:00:00Z  gs://bucket/file2\n\
TOTAL: 2 objects, 300 bytes";
        let result = filter_ls_long(input);
        assert!(result.contains("2 objects"));
        assert!(result.contains("TOTAL:"));
        assert!(result.contains("gs://bucket/file1"));
    }

    #[test]
    fn test_filter_ls_long_truncates() {
        let mut input = String::new();
        for i in 0..30 {
            input.push_str(&format!(
                "{}  2024-01-15T10:00:00Z  gs://bucket/obj{}\n",
                i * 100,
                i
            ));
        }
        input.push_str("TOTAL: 30 objects, 43500 bytes\n");
        let result = filter_ls_long(&input);
        assert!(result.contains("30 objects"));
        assert!(result.contains("... +10 more objects"));
    }

    #[test]
    fn test_compact_ls_long_line() {
        let line = "12345  2024-01-15T10:00:00Z  gs://bucket/file.txt";
        let result = compact_ls_long_line(line);
        assert!(result.contains("12345"));
        assert!(result.contains("gs://bucket/file.txt"));
        // Date should be stripped.
        assert!(!result.contains("2024-01-15"));
    }

    #[test]
    fn test_filter_transfer_with_summary() {
        let stderr = "\
Copying gs://src/a to gs://dst/a\n\
Copying gs://src/b to gs://dst/b\n\
Operation completed over 2 objects/1.5 MiB.";
        let result = filter_transfer("", stderr, "cp");
        assert!(result.starts_with("cp ok"));
        assert!(result.contains("2 files"));
        assert!(result.contains("1.5 MB"));
    }

    #[test]
    fn test_filter_transfer_no_summary() {
        let stderr = "Copying gs://src/a to gs://dst/a\n";
        let result = filter_transfer("", stderr, "rsync");
        assert!(result.starts_with("rsync ok"));
        assert!(result.contains("1 files"));
    }

    #[test]
    fn test_parse_operation_summary_found() {
        let line = "Operation completed over 42 objects/128.5 MiB.";
        let (count, bytes) = parse_operation_summary(line).unwrap();
        assert_eq!(count, 42);
        // 128.5 MiB ~ 134,742,016
        assert!(bytes > 134_000_000 && bytes < 135_000_000);
    }

    #[test]
    fn test_parse_operation_summary_none() {
        assert!(parse_operation_summary("some random line").is_none());
    }

    #[test]
    fn test_extract_number_before() {
        assert_eq!(extract_number_before("over 5 objects", "object"), Some(5));
        assert_eq!(extract_number_before("no match here", "object"), None);
    }

    #[test]
    fn test_extract_byte_value_mib() {
        let s = "Operation completed over 3 objects/2.5 MiB.";
        let bytes = extract_byte_value(s).unwrap();
        assert_eq!(bytes, (2.5 * 1024.0 * 1024.0) as u64);
    }

    #[test]
    fn test_extract_byte_value_gib() {
        let s = "completed/1.0 GiB.";
        let bytes = extract_byte_value(s).unwrap();
        assert_eq!(bytes, 1024 * 1024 * 1024);
    }

    #[test]
    fn test_filter_cat_short() {
        let input = "line1\nline2\nline3\n";
        assert_eq!(filter_cat(input), input);
    }

    #[test]
    fn test_filter_cat_truncates() {
        let lines: Vec<String> = (0..50).map(|i| format!("line {}", i)).collect();
        let input = lines.join("\n");
        let result = filter_cat(&input);
        assert!(result.contains("line 0"));
        assert!(result.contains("line 19"));
        assert!(result.contains("... +30 more lines"));
        assert!(!result.contains("line 20\n"));
    }

    #[test]
    fn test_filter_stat_keeps_key_fields() {
        let input = "\
gs://bucket/file.txt:
    Creation time:          Mon, 15 Jan 2024 10:00:00 GMT
    Update time:            Mon, 15 Jan 2024 10:00:00 GMT
    Storage class:          STANDARD
    Content-Length:          12345
    Content-Type:           text/plain
    Hash (crc32c):          abc123==
    Hash (md5):             def456==
    ETag:                   some-etag
    Generation:             1234567890
    Metageneration:         1
    ACL:                    []";
        let result = filter_stat(input);
        assert!(result.contains("gs://bucket/file.txt"));
        assert!(result.contains("Content-Length"));
        assert!(result.contains("Content-Type"));
        assert!(result.contains("Update time"));
        assert!(result.contains("Storage class"));
        assert!(result.contains("Hash (md5)"));
        // Stripped fields.
        assert!(!result.contains("ETag"));
        assert!(!result.contains("Generation:"));
        assert!(!result.contains("Metageneration"));
        assert!(!result.contains("ACL"));
        assert!(!result.contains("Creation time"));
    }

    #[test]
    fn test_filter_du_short() {
        let input = "1024       gs://bucket/dir1\n2048       gs://bucket\n";
        let result = filter_du(input);
        assert_eq!(result, input.trim_end());
    }

    #[test]
    fn test_filter_du_truncates() {
        let mut lines = Vec::with_capacity(25);
        for i in 0..20 {
            lines.push(format!("{}       gs://bucket/dir{}", i * 1024, i));
        }
        lines.push("204800       gs://bucket".to_string());
        let input = lines.join("\n");
        let result = filter_du(&input);
        assert!(result.contains("... +5 more directories"));
        assert!(result.contains("gs://bucket/dir0"));
        assert!(result.contains("204800       gs://bucket"));
    }

    #[test]
    fn test_compact_error_returns_first_meaningful_line() {
        let stderr =
            "\nCopying gs://x to gs://y\nAccessDeniedError: 403 caller does not have permission\n";
        let result = compact_error(stderr);
        assert_eq!(
            result,
            "AccessDeniedError: 403 caller does not have permission"
        );
    }

    #[test]
    fn test_compact_error_truncates_long_lines() {
        let long_line = "E".repeat(200);
        let result = compact_error(&long_line);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 124); // 120 chars + "..."
    }

    #[test]
    fn test_compact_error_empty() {
        assert_eq!(compact_error(""), "gsutil: unknown error");
    }
}
