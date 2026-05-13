//! Filters `uv` package manager output to strip resolution/download noise.

use crate::core::stream::exec_capture;
use crate::core::tracking;
use crate::core::utils::resolved_command;
use anyhow::{Context, Result};

/// Maximum lines before truncation for generic/unknown subcommands.
const GENERIC_TRUNCATE_LINES: usize = 60;

/// Maximum lines for `uv tree` output.
const TREE_TRUNCATE_LINES: usize = 40;

/// Maximum packages shown in `uv pip list` before collapsing.
const PIP_LIST_MAX: usize = 30;

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    let subcommand = args.first().map(|s| s.as_str()).unwrap_or("");
    let sub_args = if args.is_empty() { &[] } else { &args[1..] };

    let (raw_output, filtered, exit_code) = match subcommand {
        "run" => run_uv_run(sub_args, verbose)?,
        "sync" => run_uv_sync(sub_args, verbose)?,
        "lock" => run_uv_lock(sub_args, verbose)?,
        "add" | "remove" => run_uv_add_remove(subcommand, sub_args, verbose)?,
        "tree" => run_uv_tree(sub_args, verbose)?,
        "init" | "venv" => run_passthrough(args, verbose)?,
        "pip" => run_uv_pip(sub_args, verbose)?,
        _ => run_generic(args, verbose)?,
    };

    timer.track(
        &format!("uv {}", args.join(" ")),
        &format!("rtk uv {}", args.join(" ")),
        &raw_output,
        &filtered,
    );

    Ok(exit_code)
}

fn run_uv_run(args: &[String], verbose: u8) -> Result<(String, String, i32)> {
    let mut cmd = resolved_command("uv");
    cmd.arg("run");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: uv run {}", args.join(" "));
    }

    let result = exec_capture(&mut cmd).context("failed to run uv run")?;
    let raw = format!("{}\n{}", result.stdout, result.stderr);

    let summary = parse_resolution_summary(&result.stderr);
    let filtered = filter_uv_run(&result.stdout, &summary);
    print!("{}", filtered);

    Ok((raw, filtered, result.exit_code))
}

fn run_uv_sync(args: &[String], verbose: u8) -> Result<(String, String, i32)> {
    let mut cmd = resolved_command("uv");
    cmd.arg("sync");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: uv sync {}", args.join(" "));
    }

    let result = exec_capture(&mut cmd).context("failed to run uv sync")?;
    let raw = format!("{}\n{}", result.stdout, result.stderr);

    let filtered = filter_uv_sync(&result.stderr);
    println!("{}", filtered);

    Ok((raw, filtered, result.exit_code))
}

fn run_uv_lock(args: &[String], verbose: u8) -> Result<(String, String, i32)> {
    let mut cmd = resolved_command("uv");
    cmd.arg("lock");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: uv lock {}", args.join(" "));
    }

    let result = exec_capture(&mut cmd).context("failed to run uv lock")?;
    let raw = format!("{}\n{}", result.stdout, result.stderr);

    let filtered = filter_uv_lock(&result.stderr);
    println!("{}", filtered);

    Ok((raw, filtered, result.exit_code))
}

fn run_uv_add_remove(
    subcommand: &str,
    args: &[String],
    verbose: u8,
) -> Result<(String, String, i32)> {
    let mut cmd = resolved_command("uv");
    cmd.arg(subcommand);
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: uv {} {}", subcommand, args.join(" "));
    }

    let result = exec_capture(&mut cmd)
        .with_context(|| format!("failed to run uv {}", subcommand))?;
    let raw = format!("{}\n{}", result.stdout, result.stderr);

    let filtered = filter_uv_add_remove(subcommand, &result.stderr);
    println!("{}", filtered);

    Ok((raw, filtered, result.exit_code))
}

fn run_uv_tree(args: &[String], verbose: u8) -> Result<(String, String, i32)> {
    let mut cmd = resolved_command("uv");
    cmd.arg("tree");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: uv tree {}", args.join(" "));
    }

    let result = exec_capture(&mut cmd).context("failed to run uv tree")?;
    let raw = format!("{}\n{}", result.stdout, result.stderr);

    let filtered = truncate_output(&result.stdout, TREE_TRUNCATE_LINES);
    print!("{}", filtered);

    Ok((raw, filtered, result.exit_code))
}

fn run_uv_pip(args: &[String], verbose: u8) -> Result<(String, String, i32)> {
    let pip_sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let pip_args = if args.is_empty() { &[] } else { &args[1..] };

    match pip_sub {
        "install" => run_uv_pip_install(pip_args, verbose),
        "list" => run_uv_pip_list(pip_args, verbose),
        "show" => run_passthrough_with_prefix("pip", args, verbose),
        _ => run_passthrough_with_prefix("pip", args, verbose),
    }
}

fn run_uv_pip_install(args: &[String], verbose: u8) -> Result<(String, String, i32)> {
    let mut cmd = resolved_command("uv");
    cmd.arg("pip").arg("install");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: uv pip install {}", args.join(" "));
    }

    let result = exec_capture(&mut cmd).context("failed to run uv pip install")?;
    let raw = format!("{}\n{}", result.stdout, result.stderr);

    let filtered = filter_uv_pip_install(&result.stderr);
    println!("{}", filtered);

    Ok((raw, filtered, result.exit_code))
}

fn run_uv_pip_list(args: &[String], verbose: u8) -> Result<(String, String, i32)> {
    let mut cmd = resolved_command("uv");
    cmd.arg("pip").arg("list");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: uv pip list {}", args.join(" "));
    }

    let result = exec_capture(&mut cmd).context("failed to run uv pip list")?;
    let raw = format!("{}\n{}", result.stdout, result.stderr);

    let filtered = filter_uv_pip_list(&result.stdout);
    println!("{}", filtered);

    Ok((raw, filtered, result.exit_code))
}

fn run_passthrough(args: &[String], verbose: u8) -> Result<(String, String, i32)> {
    let mut cmd = resolved_command("uv");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: uv {}", args.join(" "));
    }

    let result = exec_capture(&mut cmd)
        .with_context(|| format!("failed to run uv {}", args.join(" ")))?;
    let raw = format!("{}\n{}", result.stdout, result.stderr);

    print!("{}", result.stdout);
    eprint!("{}", result.stderr);

    Ok((raw.clone(), raw, result.exit_code))
}

fn run_passthrough_with_prefix(
    prefix: &str,
    args: &[String],
    verbose: u8,
) -> Result<(String, String, i32)> {
    let mut cmd = resolved_command("uv");
    cmd.arg(prefix);
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: uv {} {}", prefix, args.join(" "));
    }

    let result = exec_capture(&mut cmd)
        .with_context(|| format!("failed to run uv {} {}", prefix, args.join(" ")))?;
    let raw = format!("{}\n{}", result.stdout, result.stderr);

    print!("{}", result.stdout);
    eprint!("{}", result.stderr);

    Ok((raw.clone(), raw, result.exit_code))
}

fn run_generic(args: &[String], verbose: u8) -> Result<(String, String, i32)> {
    let mut cmd = resolved_command("uv");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: uv {}", args.join(" "));
    }

    let result = exec_capture(&mut cmd)
        .with_context(|| format!("failed to run uv {}", args.join(" ")))?;
    let raw = format!("{}\n{}", result.stdout, result.stderr);

    let combined = result.combined();
    let filtered = truncate_output(&combined, GENERIC_TRUNCATE_LINES);
    print!("{}", filtered);

    Ok((raw, filtered, result.exit_code))
}

// --- Filter / parse helpers ---

#[derive(Debug, Default)]
struct ResolutionSummary {
    resolved: Option<String>,
    prepared: Option<String>,
    installed: Option<String>,
    package_count: usize,
}

impl ResolutionSummary {
    fn one_line(&self) -> Option<String> {
        // Build a compact "uv: resolved N pkgs in Xms" style line
        if let Some(ref resolved) = self.resolved {
            return Some(format!("uv: {}", resolved.to_lowercase()));
        }
        if self.package_count > 0 {
            return Some(format!("uv: installed {} packages", self.package_count));
        }
        None
    }
}

fn parse_resolution_summary(stderr: &str) -> ResolutionSummary {
    let mut summary = ResolutionSummary::default();
    let mut pkg_count = 0usize;

    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Resolved") {
            summary.resolved = Some(trimmed.to_string());
        } else if trimmed.starts_with("Prepared") {
            summary.prepared = Some(trimmed.to_string());
        } else if trimmed.starts_with("Installed") {
            summary.installed = Some(trimmed.to_string());
        } else if trimmed.starts_with("+ ") || trimmed.starts_with("  + ") {
            pkg_count += 1;
        }
    }
    summary.package_count = pkg_count;
    summary
}

fn filter_uv_run(stdout: &str, summary: &ResolutionSummary) -> String {
    let mut result = String::new();
    if let Some(line) = summary.one_line() {
        result.push_str(&line);
        result.push('\n');
    }
    result.push_str(stdout);
    result
}

fn filter_uv_sync(stderr: &str) -> String {
    let summary = parse_resolution_summary(stderr);
    let mut new_count = 0usize;
    let mut updated_count = 0usize;

    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("+ ") || trimmed.starts_with("  + ") {
            new_count += 1;
        } else if trimmed.starts_with("~ ") || trimmed.starts_with("  ~ ") {
            updated_count += 1;
        }
    }

    let total = new_count + updated_count;
    let time = extract_time(&summary.resolved);

    let mut result = format!("synced {} packages", total);
    if new_count > 0 || updated_count > 0 {
        result.push_str(&format!(" ({} new, {} updated)", new_count, updated_count));
    }
    if let Some(t) = time {
        result.push_str(&format!(" in {}", t));
    }

    // Include warnings
    append_warnings(stderr, &mut result);

    result
}

fn filter_uv_lock(stderr: &str) -> String {
    let summary = parse_resolution_summary(stderr);
    let mut result = String::new();

    if let Some(ref resolved) = summary.resolved {
        result.push_str(&format!("locked: {}", resolved.to_lowercase()));
    } else {
        result.push_str("uv lock: done");
    }

    append_warnings(stderr, &mut result);
    result
}

fn filter_uv_add_remove(subcommand: &str, stderr: &str) -> String {
    let mut packages: Vec<String> = Vec::new();
    let mut result = String::new();

    for line in stderr.lines() {
        let trimmed = line.trim();
        // "+ package==version" or "- package==version"
        if trimmed.starts_with("+ ") || trimmed.starts_with("- ") {
            packages.push(trimmed.to_string());
        }
    }

    if packages.is_empty() {
        result.push_str(&format!("uv {}: done", subcommand));
    } else {
        result.push_str(&format!("uv {}: ", subcommand));
        // Show the actual packages changed (these are usually few)
        for pkg in &packages {
            result.push_str(&format!("\n  {}", pkg));
        }
    }

    let summary = parse_resolution_summary(stderr);
    if let Some(ref resolved) = summary.resolved {
        result.push_str(&format!("\nlockfile: {}", resolved.to_lowercase()));
    }

    append_warnings(stderr, &mut result);
    result
}

fn filter_uv_pip_install(stderr: &str) -> String {
    let summary = parse_resolution_summary(stderr);
    let mut top_level: Vec<String> = Vec::new();

    for line in stderr.lines() {
        let trimmed = line.trim();
        // Top-level installs don't have leading whitespace before "+"
        if let Some(pkg) = trimmed.strip_prefix("+ ") {
            top_level.push(pkg.to_string());
        }
    }

    let time = extract_time(&summary.installed)
        .or_else(|| extract_time(&summary.resolved));

    let mut result = format!("installed {} packages", top_level.len());
    if let Some(t) = time {
        result.push_str(&format!(" in {}", t));
    }
    if !top_level.is_empty() {
        result.push('\n');
        for pkg in &top_level {
            result.push_str(&format!("  + {}\n", pkg));
        }
    }

    append_warnings(stderr, &mut result);
    result.trim_end().to_string()
}

fn filter_uv_pip_list(stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    if lines.is_empty() {
        return "uv pip list: no packages".to_string();
    }

    // uv pip list outputs a table with header + separator + rows.
    // Strip the separator lines (all dashes/spaces).
    let mut packages: Vec<&str> = Vec::new();
    let mut header: Option<&str> = None;

    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Separator lines are just dashes and spaces
        if is_table_separator(trimmed) {
            continue;
        }
        if header.is_none() {
            header = Some(line);
            continue;
        }
        packages.push(line);
    }

    let total = packages.len();
    let mut result = format!("uv pip list: {} packages\n", total);

    if let Some(h) = header {
        result.push_str(h);
        result.push('\n');
    }

    for pkg in packages.iter().take(PIP_LIST_MAX) {
        result.push_str(pkg);
        result.push('\n');
    }

    if total > PIP_LIST_MAX {
        result.push_str(&format!("+ {} more\n", total - PIP_LIST_MAX));
    }

    result.trim_end().to_string()
}

fn truncate_output(output: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= max_lines {
        return output.to_string();
    }

    let mut result = String::with_capacity(output.len() / 2);
    for line in lines.iter().take(max_lines) {
        result.push_str(line);
        result.push('\n');
    }
    result.push_str(&format!("... +{} more lines\n", lines.len() - max_lines));
    result
}

fn is_table_separator(line: &str) -> bool {
    !line.is_empty() && line.chars().all(|c| c == '-' || c == ' ')
}

/// Extract timing info from a resolution line like "Resolved 42 packages in 156ms".
fn extract_time(line: &Option<String>) -> Option<String> {
    let line = line.as_ref()?;
    let idx = line.find(" in ")?;
    Some(line[idx + 4..].trim().to_string())
}

fn append_warnings(stderr: &str, result: &mut String) {
    let warnings: Vec<&str> = stderr
        .lines()
        .filter(|l| l.trim_start().starts_with("warning:"))
        .collect();

    if !warnings.is_empty() {
        result.push('\n');
        for w in warnings.iter().take(3) {
            result.push_str(w.trim());
            result.push('\n');
        }
        if warnings.len() > 3 {
            result.push_str(&format!("+ {} more warnings\n", warnings.len() - 3));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_resolution_summary_full() {
        let stderr = "\
Resolved 42 packages in 156ms
Prepared 3 packages in 89ms
Installed 5 packages in 12ms
  + requests==2.31.0
  + urllib3==2.0.0
  + certifi==2023.7.22
  + idna==3.4
  + charset-normalizer==3.2.0";

        let summary = parse_resolution_summary(stderr);
        assert_eq!(
            summary.resolved.as_deref(),
            Some("Resolved 42 packages in 156ms")
        );
        assert_eq!(
            summary.prepared.as_deref(),
            Some("Prepared 3 packages in 89ms")
        );
        assert_eq!(
            summary.installed.as_deref(),
            Some("Installed 5 packages in 12ms")
        );
        assert_eq!(summary.package_count, 5);
    }

    #[test]
    fn test_parse_resolution_summary_empty() {
        let summary = parse_resolution_summary("");
        assert!(summary.resolved.is_none());
        assert!(summary.prepared.is_none());
        assert!(summary.installed.is_none());
        assert_eq!(summary.package_count, 0);
    }

    #[test]
    fn test_resolution_summary_one_line() {
        let summary = ResolutionSummary {
            resolved: Some("Resolved 42 packages in 156ms".to_string()),
            ..Default::default()
        };
        assert_eq!(
            summary.one_line().as_deref(),
            Some("uv: resolved 42 packages in 156ms")
        );
    }

    #[test]
    fn test_resolution_summary_one_line_no_resolve() {
        let summary = ResolutionSummary {
            package_count: 3,
            ..Default::default()
        };
        assert_eq!(
            summary.one_line().as_deref(),
            Some("uv: installed 3 packages")
        );
    }

    #[test]
    fn test_resolution_summary_one_line_empty() {
        let summary = ResolutionSummary::default();
        assert!(summary.one_line().is_none());
    }

    #[test]
    fn test_filter_uv_run_with_summary() {
        let stdout = "Hello from my script!\nResult: 42\n";
        let summary = ResolutionSummary {
            resolved: Some("Resolved 10 packages in 50ms".to_string()),
            ..Default::default()
        };
        let result = filter_uv_run(stdout, &summary);
        assert!(result.starts_with("uv: resolved 10 packages in 50ms"));
        assert!(result.contains("Hello from my script!"));
        assert!(result.contains("Result: 42"));
    }

    #[test]
    fn test_filter_uv_run_no_resolution() {
        let stdout = "Hello from cached run\n";
        let summary = ResolutionSummary::default();
        let result = filter_uv_run(stdout, &summary);
        assert_eq!(result, "Hello from cached run\n");
    }

    #[test]
    fn test_filter_uv_sync() {
        let stderr = "\
Resolved 20 packages in 200ms
Installed 5 packages in 30ms
+ requests==2.31.0
+ flask==3.0.0
~ urllib3==1.26.0";

        let result = filter_uv_sync(stderr);
        assert!(result.contains("synced 3 packages"));
        assert!(result.contains("2 new"));
        assert!(result.contains("1 updated"));
        assert!(result.contains("in 200ms"));
    }

    #[test]
    fn test_filter_uv_sync_empty() {
        let result = filter_uv_sync("");
        assert!(result.contains("synced 0 packages"));
    }

    #[test]
    fn test_filter_uv_lock() {
        let stderr = "Resolved 50 packages in 300ms\n";
        let result = filter_uv_lock(stderr);
        assert!(result.contains("locked: resolved 50 packages in 300ms"));
    }

    #[test]
    fn test_filter_uv_lock_empty() {
        let result = filter_uv_lock("");
        assert_eq!(result, "uv lock: done");
    }

    #[test]
    fn test_filter_uv_add_remove_add() {
        let stderr = "\
Resolved 25 packages in 100ms
+ httpx==0.25.0";

        let result = filter_uv_add_remove("add", stderr);
        assert!(result.contains("uv add:"));
        assert!(result.contains("+ httpx==0.25.0"));
        assert!(result.contains("lockfile:"));
    }

    #[test]
    fn test_filter_uv_add_remove_remove() {
        let stderr = "\
Resolved 20 packages in 80ms
- flask==3.0.0";

        let result = filter_uv_add_remove("remove", stderr);
        assert!(result.contains("uv remove:"));
        assert!(result.contains("- flask==3.0.0"));
    }

    #[test]
    fn test_filter_uv_pip_install() {
        let stderr = "\
Resolved 10 packages in 50ms
Installed 3 packages in 15ms
+ requests==2.31.0
+ urllib3==2.0.0
+ certifi==2023.7.22";

        let result = filter_uv_pip_install(stderr);
        assert!(result.contains("installed 3 packages"));
        assert!(result.contains("in 15ms"));
        assert!(result.contains("+ requests==2.31.0"));
    }

    #[test]
    fn test_filter_uv_pip_install_empty() {
        let result = filter_uv_pip_install("");
        assert!(result.contains("installed 0 packages"));
    }

    #[test]
    fn test_filter_uv_pip_list_small() {
        let stdout = "\
Package    Version
---------- -------
requests   2.31.0
flask      3.0.0
click      8.1.0";

        let result = filter_uv_pip_list(stdout);
        assert!(result.contains("3 packages"));
        assert!(result.contains("requests"));
        assert!(result.contains("flask"));
        assert!(result.contains("click"));
        assert!(!result.contains("-----"));
    }

    #[test]
    fn test_filter_uv_pip_list_truncation() {
        let mut stdout = String::from("Package    Version\n---------- -------\n");
        for i in 0..50 {
            stdout.push_str(&format!("pkg{}    1.0.{}\n", i, i));
        }

        let result = filter_uv_pip_list(&stdout);
        assert!(result.contains("50 packages"));
        assert!(result.contains("+ 20 more"));
    }

    #[test]
    fn test_filter_uv_pip_list_empty() {
        let result = filter_uv_pip_list("");
        assert!(result.contains("no packages"));
    }

    #[test]
    fn test_truncate_output_short() {
        let input = "line1\nline2\nline3\n";
        let result = truncate_output(input, 10);
        assert_eq!(result, input);
    }

    #[test]
    fn test_truncate_output_long() {
        let mut input = String::new();
        for i in 0..100 {
            input.push_str(&format!("line {}\n", i));
        }
        let result = truncate_output(&input, 5);
        assert!(result.contains("line 0"));
        assert!(result.contains("line 4"));
        assert!(!result.contains("line 5"));
        assert!(result.contains("+95 more lines"));
    }

    #[test]
    fn test_is_table_separator() {
        assert!(is_table_separator("---------- -------"));
        assert!(is_table_separator("---"));
        assert!(!is_table_separator("requests   2.31.0"));
        assert!(!is_table_separator(""));
    }

    #[test]
    fn test_extract_time() {
        let line = Some("Resolved 42 packages in 156ms".to_string());
        assert_eq!(extract_time(&line).as_deref(), Some("156ms"));
    }

    #[test]
    fn test_extract_time_none() {
        assert!(extract_time(&None).is_none());
        let no_time = Some("Resolved 42 packages".to_string());
        assert!(extract_time(&no_time).is_none());
    }

    #[test]
    fn test_append_warnings() {
        let stderr = "\
Resolved 10 packages in 50ms
warning: package foo is deprecated
warning: python 3.8 reaches EOL soon";

        let mut result = String::from("base");
        append_warnings(stderr, &mut result);
        assert!(result.contains("warning: package foo is deprecated"));
        assert!(result.contains("warning: python 3.8 reaches EOL soon"));
    }

    #[test]
    fn test_append_warnings_none() {
        let mut result = String::from("base");
        append_warnings("Resolved 10 packages in 50ms", &mut result);
        assert_eq!(result, "base");
    }

    #[test]
    fn test_append_warnings_truncation() {
        let mut stderr = String::new();
        for i in 0..10 {
            stderr.push_str(&format!("warning: issue {}\n", i));
        }

        let mut result = String::from("base");
        append_warnings(&stderr, &mut result);
        assert!(result.contains("+ 7 more warnings"));
    }
}
