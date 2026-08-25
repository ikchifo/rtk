//! Filters `uv` package manager output to strip resolution/download noise.

use crate::core::runner;
use crate::core::stream::{self, exec_capture, FilterMode, StdinMode};
use crate::core::tracking;
use crate::core::truncate::{CAP_INVENTORY, CAP_WARNINGS};
use crate::core::utils::{resolved_command, strip_ansi, truncate};
use anyhow::{Context, Result};
use regex::Regex;
use std::sync::LazyLock;

static PYTHON_FRAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*File ".*", line \d+.*$"#).unwrap());
static PYTHON_EXCEPTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[A-Za-z_][A-Za-z0-9_.]*(?:Error|Exception):").unwrap());
static JS_FRAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*at .+:\d+:\d+.*$").unwrap());
static ERROR_START_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)\berror\b").unwrap(),
        Regex::new(r"(?i)\bfailed\b").unwrap(),
        Regex::new(r"(?i)\bfailure\b").unwrap(),
        Regex::new(r"(?i)\bexception\b").unwrap(),
        Regex::new(r"(?i)\bpanic\b").unwrap(),
        Regex::new(r"(?i)\bwarn(?:ing)?\b").unwrap(),
        Regex::new(r"(?i)\bassert(?:ion)?\b").unwrap(),
        Regex::new(r"^\s*FAILED\b").unwrap(),
        Regex::new(r"^\s*ERROR\b").unwrap(),
        Regex::new(r"^\s*E\s+").unwrap(),
        Regex::new(r"^\s*Caused by:").unwrap(),
        Regex::new(r"^\s*note:").unwrap(),
        Regex::new(r"^\s*help:").unwrap(),
    ]
});

const MAX_TRACEBACK_FRAMES: usize = CAP_WARNINGS;
const MAX_ERROR_CONTINUATION_LINES: usize = CAP_WARNINGS;
const MAX_FALLBACK_TAIL_LINES: usize = CAP_WARNINGS;
const MAX_PROGRAM_LINE_CHARS: usize = 500;
const TEE_SLUG_STDOUT: &str = "uv-run-stdout";
const TEE_SLUG_STDERR: &str = "uv-run-stderr";

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

    let result = stream::run_streaming(&mut cmd, StdinMode::Inherit, FilterMode::CaptureOnly)
        .context("failed to run uv run")?;
    let filtered = filter_uv_run_output(
        &result.raw,
        &result.raw_stdout,
        &result.raw_stderr,
        result.exit_code,
    );
    let shown = runner::print_with_hint(
        &filtered,
        &result.raw,
        &result.raw,
        "uv",
        result.exit_code,
    );

    Ok((result.raw, shown, result.exit_code))
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
    #[allow(dead_code)]
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

fn filter_uv_run_output(output: &str, stdout: &str, stderr: &str, exit_code: i32) -> String {
    if exit_code == 0 {
        return filter_successful_run(stdout, stderr);
    }

    // On failure the streams are scanned merged: a Python traceback interleaves
    // stdout and stderr, and splitting it would break frame ordering.
    let clean = strip_ansi(output);
    let extracted = extract_diagnostics(&clean);
    if !extracted.is_empty() {
        return extracted;
    }

    let tail: Vec<String> = clean
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| truncate(line, 200))
        .collect();

    // The exit code already carries the failure; restating it would only add
    // tokens, so the command's own message is returned untouched.
    let skip = tail.len().saturating_sub(MAX_FALLBACK_TAIL_LINES);
    tail[skip..].join("\n")
}

/// Expects ANSI-stripped input.
fn extract_diagnostics(clean: &str) -> String {
    let lines: Vec<&str> = clean.lines().collect();
    let mut selected: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        if is_traceback_start(trimmed) {
            let (block, next_idx) = collect_traceback_block(&lines, i);
            selected.extend(block);
            selected.push(String::new());
            i = next_idx;
            continue;
        }

        if is_error_start(trimmed) {
            let (block, next_idx) = collect_error_block(&lines, i);
            selected.extend(block);
            selected.push(String::new());
            i = next_idx;
            continue;
        }

        i += 1;
    }

    selected.join("\n").trim().to_string()
}

fn filter_successful_run(stdout: &str, stderr: &str) -> String {
    // Distinct slugs: both streams can need a tee in the same run, and tee
    // filenames are second-resolution, so a shared slug would make the second
    // write clobber the first and leave one hint pointing at the other's bytes.
    let payload = program_output(stdout, TEE_SLUG_STDOUT);
    let diagnostics = program_output(stderr, TEE_SLUG_STDERR);

    match (payload.is_empty(), diagnostics.is_empty()) {
        (true, true) => "ok".to_string(),
        (false, true) => payload,
        (true, false) => diagnostics,
        (false, false) => format!("{payload}\n{diagnostics}"),
    }
}

fn program_output(text: &str, tee_slug: &str) -> String {
    let clean = strip_ansi(text);
    let lines: Vec<&str> = clean.lines().collect();
    let last_content = lines.iter().rposition(|line| !line.trim().is_empty());

    let Some(last_content) = last_content else {
        return String::new();
    };
    let lines = &lines[..=last_content];
    let capped: Vec<String> = lines
        .iter()
        .map(|line| truncate(line, MAX_PROGRAM_LINE_CHARS))
        .collect();
    let line_was_cut = capped.iter().zip(lines).any(|(cut, full)| cut.len() != full.len());

    if capped.len() <= CAP_INVENTORY {
        let out = capped.join("\n");
        if line_was_cut {
            if let Some(hint) = crate::core::tee::force_tee_hint(&clean, tee_slug) {
                return format!("{out}\n{hint}");
            }
        }
        return out;
    }

    // A program's result is usually its last line, so keep both ends.
    let head = CAP_INVENTORY / 2;
    let tail = CAP_INVENTORY - head;
    let omitted = capped.len() - CAP_INVENTORY;

    let mut out = capped[..head].join("\n");
    out.push_str(&format!("\n... ({omitted} lines omitted)\n"));
    out.push_str(&capped[capped.len() - tail..].join("\n"));

    // A cut in the head region sits before the tail offset, so `tail -n +N` skips it.
    let head_line_was_cut = capped[..head]
        .iter()
        .zip(&lines[..head])
        .any(|(cut, full)| cut != full);

    let hint = if head_line_was_cut {
        crate::core::tee::force_tee_hint(&clean, tee_slug)
    } else {
        crate::core::tee::force_tee_tail_hint(&clean, tee_slug, head + 1)
    };

    if let Some(hint) = hint {
        out.push_str(&format!("\n{hint}"));
    }

    out
}

fn collect_traceback_block(lines: &[&str], start_idx: usize) -> (Vec<String>, usize) {
    let mut block = vec![lines[start_idx].trim().to_string()];
    let mut frames = Vec::new();
    let mut tail = Vec::new();
    let mut idx = start_idx + 1;

    while idx < lines.len() {
        let trimmed = lines[idx].trim();
        if trimmed.is_empty() {
            break;
        }

        if PYTHON_FRAME_RE.is_match(trimmed) {
            frames.push(truncate(trimmed, 160));
        } else {
            tail.push(truncate(trimmed, 200));
        }

        idx += 1;
    }

    block.extend(frames.iter().take(MAX_TRACEBACK_FRAMES).cloned());
    if frames.len() > MAX_TRACEBACK_FRAMES {
        block.push(format!(
            "... +{} more frames",
            frames.len() - MAX_TRACEBACK_FRAMES
        ));
        let full_traceback = lines[start_idx..idx].join("\n");
        if let Some(hint) = crate::core::tee::force_tee_hint(&full_traceback, "uv-traceback") {
            block.push(format!("  {hint}"));
        }
    }

    let tail_lines = tail
        .into_iter()
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    block.extend(tail_lines);

    (dedupe_preserving_order(block), idx)
}

fn collect_error_block(lines: &[&str], start_idx: usize) -> (Vec<String>, usize) {
    let mut block = vec![truncate(lines[start_idx].trim(), 200)];
    let mut continuation_count = 0;
    let mut idx = start_idx + 1;

    while idx < lines.len() {
        let line = lines[idx];
        let trimmed = line.trim();

        if trimmed.is_empty() || !is_error_continuation(line) {
            break;
        }

        continuation_count += 1;
        if continuation_count <= MAX_ERROR_CONTINUATION_LINES {
            block.push(truncate(trimmed, 200));
        }

        idx += 1;
    }

    if continuation_count > MAX_ERROR_CONTINUATION_LINES {
        block.push(format!(
            "... +{} more lines",
            continuation_count - MAX_ERROR_CONTINUATION_LINES
        ));
        let full_block = lines[start_idx..idx].join("\n");
        if let Some(hint) = crate::core::tee::force_tee_hint(&full_block, "uv-error-block") {
            block.push(format!("  {hint}"));
        }
    }

    (dedupe_preserving_order(block), idx)
}

fn dedupe_preserving_order(lines: Vec<String>) -> Vec<String> {
    let mut deduped = Vec::new();
    for line in lines {
        if deduped.last() != Some(&line) {
            deduped.push(line);
        }
    }
    deduped
}

fn is_traceback_start(line: &str) -> bool {
    line.starts_with("Traceback ")
}

fn is_error_start(line: &str) -> bool {
    if is_traceback_start(line)
        || PYTHON_FRAME_RE.is_match(line)
        || PYTHON_EXCEPTION_RE.is_match(line)
        || JS_FRAME_RE.is_match(line)
    {
        return true;
    }

    if line.contains("No module named ") {
        return true;
    }

    ERROR_START_PATTERNS.iter().any(|pattern| pattern.is_match(line))
}

fn is_error_continuation(line: &str) -> bool {
    let trimmed = line.trim();
    line.starts_with(' ')
        || line.starts_with('\t')
        || trimmed.starts_with('>')
        || trimmed.starts_with('|')
        || trimmed.starts_with("During handling of the above exception")
        || trimmed.starts_with("The above exception")
        || trimmed.starts_with("Caused by:")
        || trimmed.starts_with("note:")
        || trimmed.starts_with("help:")
        || PYTHON_FRAME_RE.is_match(trimmed)
        || PYTHON_EXCEPTION_RE.is_match(trimmed)
        || JS_FRAME_RE.is_match(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::utils::count_tokens;

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

    #[test]
    fn test_filter_uv_run_keeps_program_output_on_success() {
        let stdout = "hello from script\n";

        assert_eq!(
            filter_uv_run_output(stdout, stdout, "", 0),
            "hello from script"
        );
    }

    #[test]
    fn test_filter_uv_run_keeps_data_producing_stdout() {
        let stdout = "{\n  \"users\": 42,\n  \"active\": 37\n}\n";
        let raw = stdout.to_string();

        let result = filter_uv_run_output(&raw, stdout, "", 0);

        assert!(result.contains("\"users\": 42"));
        assert!(result.contains("\"active\": 37"));
    }

    #[test]
    fn test_filter_uv_run_keeps_non_error_stderr_on_success() {
        let stderr = "INFO:root:connected to db\nINFO:root:migrated 3 tables\n";
        let stdout = "done\n";
        let raw = format!("{stderr}{stdout}");

        let result = filter_uv_run_output(&raw, stdout, stderr, 0);

        assert!(result.contains("done"));
        assert!(result.contains("INFO:root:connected to db"));
        assert!(result.contains("INFO:root:migrated 3 tables"));
    }

    #[test]
    fn test_stdout_and_stderr_tee_slugs_are_distinct() {
        assert_ne!(TEE_SLUG_STDOUT, TEE_SLUG_STDERR);
    }

    #[test]
    fn test_program_output_truncates_over_line_cap_keeping_both_ends() {
        let stdout: String = (0..120).map(|i| format!("line{i}\n")).collect();

        let result = program_output(&stdout, TEE_SLUG_STDOUT);

        assert!(result.contains("line0"), "head must survive");
        assert!(result.contains("line119"), "tail must survive");
        assert!(result.contains("lines omitted"));
        assert!(result.lines().count() < 120);
    }

    #[test]
    fn test_program_output_head_cut_switches_away_from_the_tail_hint() {
        let stdout: String = (0..60)
            .map(|i| {
                if i == 3 {
                    format!("{}\n", "x".repeat(900))
                } else {
                    format!("line{i}\n")
                }
            })
            .collect();

        let result = program_output(&stdout, TEE_SLUG_STDOUT);

        assert!(result.contains("lines omitted"));
        assert!(
            !result.contains("see remaining"),
            "a head-region cut must not be reported with a tail offset that skips it, got: {result}"
        );
    }

    #[test]
    fn test_program_output_caps_a_single_huge_line() {
        let stdout = format!("{}\n", "x".repeat(50_000));

        let result = program_output(&stdout, TEE_SLUG_STDOUT);

        assert!(
            result.len() < 2_000,
            "one huge line must be capped, got {} bytes",
            result.len()
        );
        assert!(result.contains("..."));
    }

    #[test]
    fn test_program_output_exact_cap_is_untouched() {
        let stdout: String = (0..CAP_INVENTORY).map(|i| format!("line{i}\n")).collect();

        let result = program_output(&stdout, TEE_SLUG_STDOUT);

        assert!(!result.contains("lines omitted"));
        assert_eq!(result.lines().count(), CAP_INVENTORY);
    }

    #[test]
    fn test_program_output_handles_multibyte_without_panic() {
        let stdout: String = (0..80).map(|i| format!("日本語 🎉 line{i}\n")).collect();

        let result = program_output(&stdout, TEE_SLUG_STDOUT);

        assert!(result.contains("日本語"));
        assert!(result.contains("lines omitted"));
    }

    #[test]
    fn test_filter_uv_run_silent_success_is_ok() {
        assert_eq!(filter_uv_run_output("", "", "", 0), "ok");
    }

    #[test]
    fn test_filter_uv_run_success_keeps_stderr_warnings_with_payload() {
        let stdout = "result: 7\n";
        let stderr = "WARNING: deprecated api\n";
        let raw = format!("{stderr}{stdout}");

        let result = filter_uv_run_output(&raw, stdout, stderr, 0);

        assert!(result.contains("result: 7"));
        assert!(result.contains("WARNING: deprecated api"));
    }

    #[test]
    fn test_filter_uv_run_truncates_python_tracebacks() {
        let output = r#"
Traceback (most recent call last):
  File "/tmp/project/main.py", line 10, in <module>
    run()
  File "/tmp/project/app.py", line 22, in run
    inner()
  File "/tmp/project/lib.py", line 33, in inner
    boom()
  File "/tmp/project/helpers.py", line 44, in boom
    raise RuntimeError("kaboom")
RuntimeError: kaboom
"#;

        let result = filter_uv_run_output(output, "", "", 1);
        assert!(result.contains("Traceback (most recent call last):"));
        assert!(result.contains(r#"File "/tmp/project/main.py", line 10, in <module>"#));
        assert!(result.contains("RuntimeError: kaboom"));
        assert!(!result.contains("run()"));
    }

    #[test]
    fn test_filter_uv_run_truncates_many_python_frames() {
        let mut output = String::from("Traceback (most recent call last):\n");
        for i in 0..(MAX_TRACEBACK_FRAMES + 2) {
            output.push_str(&format!(
                "  File \"/tmp/project/module_{i}.py\", line {i}, in call_{i}\n"
            ));
            output.push_str("    call_next()\n");
        }
        output.push_str("RuntimeError: kaboom\n");

        let result = filter_uv_run_output(&output, "", "", 1);
        assert!(result.contains("Traceback (most recent call last):"));
        assert!(result.contains("... +2 more frames"));
    }

    #[test]
    fn test_filter_uv_run_keeps_failure_summary_lines() {
        let output = r#"
Resolved 8 packages in 30ms
============================= test session starts =============================
FAILED tests/test_api.py::test_healthcheck - AssertionError: expected 200
1 failed, 12 passed in 0.31s
"#;

        let result = filter_uv_run_output(output, "", "", 1);
        assert!(result.contains("FAILED tests/test_api.py::test_healthcheck"));
        assert!(result.contains("1 failed, 12 passed in 0.31s"));
        assert!(!result.contains("Resolved 8 packages"));
    }

    #[test]
    fn test_filter_uv_run_failure_returns_message_without_added_marker() {
        let output = "sync aborted by signal";
        let result = filter_uv_run_output(output, "", "", 2);

        assert_eq!(result, "sync aborted by signal");
    }

    #[test]
    fn test_filter_uv_run_silent_failure_emits_nothing() {
        assert_eq!(filter_uv_run_output("", "", "", 2), "");
    }

    #[test]
    fn test_filter_uv_run_pytest_fixture_token_savings() {
        let input = include_str!("../../../tests/fixtures/uv_run_pytest_failure.txt");
        let output = filter_uv_run_output(input, "", "", 1);
        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 70.0,
            "uv run pytest: expected >=70% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens
        );
        assert!(output.contains("FAILED tests/test_users.py::test_normalize_user_rejects_empty"));
        assert!(output.contains("1 failed, 1 passed"));
        assert!(!output.contains("Downloading cpython"));
    }
}
