//! Reads source files with optional language-aware filtering to strip boilerplate.

use crate::core::filter::{self, FilterLevel, Language};
use crate::core::tracking;
use anyhow::{bail, Context, Result};
use std::borrow::Cow;
use std::fs;
use std::path::Path;
use std::str::FromStr;

/// An inclusive, one-based range of source lines.
///
/// `LineRange` is parsed from the CLI as `START:END`. Both bounds must be
/// positive, and `start` must not be greater than `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    /// The first included source line.
    pub start: usize,
    /// The last included source line.
    pub end: usize,
}

impl FromStr for LineRange {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let Some((start, end)) = value.split_once(':') else {
            return Err(format!(
                "invalid line range `{value}`: expected START:END with positive line numbers"
            ));
        };

        if end.contains(':') {
            return Err(format!(
                "invalid line range `{value}`: expected exactly one colon in START:END"
            ));
        }

        let start = parse_line_bound(start, "start")?;
        let end = parse_line_bound(end, "end")?;
        if start > end {
            return Err(format!(
                "invalid line range `{value}`: start ({start}) must not exceed end ({end})"
            ));
        }

        Ok(Self { start, end })
    }
}

fn parse_line_bound(value: &str, name: &str) -> std::result::Result<usize, String> {
    if value.is_empty() {
        return Err(format!(
            "line range {name} is missing; expected START:END with positive line numbers"
        ));
    }

    let value = value.parse::<usize>().map_err(|_| {
        format!("line range {name} `{value}` is not a positive integer; expected START:END")
    })?;
    if value == 0 {
        return Err(format!(
            "line range {name} must be greater than zero; expected START:END"
        ));
    }

    Ok(value)
}

pub fn run(
    file: &Path,
    level: FilterLevel,
    line_range: Option<LineRange>,
    max_lines: Option<usize>,
    tail_lines: Option<usize>,
    line_numbers: bool,
    verbose: u8,
) -> Result<()> {
    validate_source_numbering(line_range, level, max_lines, line_numbers)?;

    let timer = tracking::TimedExecution::start();

    if verbose > 0 {
        eprintln!("Reading: {} (filter: {})", file.display(), level);
    }

    // Read file content
    let content = fs::read_to_string(file)
        .with_context(|| format!("Failed to read file: {}", file.display()))?;

    // Detect language from extension
    let lang = file
        .extension()
        .and_then(|e| e.to_str())
        .map(Language::from_extension)
        .unwrap_or(Language::Unknown);

    if verbose > 1 {
        eprintln!("Detected language: {:?}", lang);
    }

    // Select source lines before filtering so a range always addresses the
    // original file, not the filtered representation.
    let selected = select_line_range(&content, line_range);

    // Apply filter
    let filter = filter::get_filter(level);
    let mut filtered = filter.filter(&selected, &lang);

    // Safety: if filtering empties a non-empty selection, fall back to the
    // selected raw source instead of widening the requested line range.
    if filtered.trim().is_empty() && !selected.trim().is_empty() {
        eprintln!(
            "rtk: warning: filter produced empty output for {} ({} bytes), showing raw content",
            file.display(),
            selected.len()
        );
        filtered = selected.to_string();
    }

    if verbose > 0 {
        let original_lines = selected.lines().count();
        let filtered_lines = filtered.lines().count();
        let reduction = if original_lines > 0 {
            ((original_lines - filtered_lines) as f64 / original_lines as f64) * 100.0
        } else {
            0.0
        };
        eprintln!(
            "Lines: {} -> {} ({:.1}% reduction)",
            original_lines, filtered_lines, reduction
        );
    }

    let line_number_start = line_number_start(&filtered, line_range, tail_lines);
    filtered = apply_line_window(&filtered, max_lines, tail_lines, &lang);

    let rtk_output = if line_numbers {
        format_with_line_numbers(&filtered, line_number_start)
    } else {
        filtered.clone()
    };
    print!("{}", rtk_output);
    timer.track(
        &format!("cat {}", file.display()),
        "rtk read",
        &content,
        &rtk_output,
    );
    Ok(())
}

pub fn run_stdin(
    level: FilterLevel,
    line_range: Option<LineRange>,
    max_lines: Option<usize>,
    tail_lines: Option<usize>,
    line_numbers: bool,
    verbose: u8,
) -> Result<()> {
    use std::io::{self, Read as IoRead};

    validate_source_numbering(line_range, level, max_lines, line_numbers)?;

    let timer = tracking::TimedExecution::start();

    if verbose > 0 {
        eprintln!("Reading from stdin (filter: {})", level);
    }

    // Read from stdin
    let mut content = String::new();
    io::stdin()
        .lock()
        .read_to_string(&mut content)
        .context("Failed to read from stdin")?;

    // No file extension, so use Unknown language
    let lang = Language::Unknown;

    if verbose > 1 {
        eprintln!("Language: {:?} (stdin has no extension)", lang);
    }

    // Select source lines before filtering so a range always addresses the
    // original stdin stream, not the filtered representation.
    let selected = select_line_range(&content, line_range);

    // Apply filter
    let filter = filter::get_filter(level);
    let mut filtered = filter.filter(&selected, &lang);

    if line_range.is_some() && filtered.trim().is_empty() && !selected.trim().is_empty() {
        eprintln!(
            "rtk: warning: filter produced empty output for stdin ({} bytes), showing raw content",
            selected.len()
        );
        filtered = selected.to_string();
    }

    if verbose > 0 {
        let original_lines = selected.lines().count();
        let filtered_lines = filtered.lines().count();
        let reduction = if original_lines > 0 {
            ((original_lines - filtered_lines) as f64 / original_lines as f64) * 100.0
        } else {
            0.0
        };
        eprintln!(
            "Lines: {} -> {} ({:.1}% reduction)",
            original_lines, filtered_lines, reduction
        );
    }

    let line_number_start = line_number_start(&filtered, line_range, tail_lines);
    filtered = apply_line_window(&filtered, max_lines, tail_lines, &lang);

    let rtk_output = if line_numbers {
        format_with_line_numbers(&filtered, line_number_start)
    } else {
        filtered.clone()
    };
    print!("{}", rtk_output);

    timer.track("cat - (stdin)", "rtk read -", &content, &rtk_output);
    Ok(())
}

fn validate_source_numbering(
    line_range: Option<LineRange>,
    level: FilterLevel,
    max_lines: Option<usize>,
    line_numbers: bool,
) -> Result<()> {
    if line_range.is_none() || !line_numbers {
        return Ok(());
    }

    if level != FilterLevel::None {
        bail!(
            "--line-range with --line-numbers requires --level none to preserve source line numbers"
        );
    }

    if max_lines.is_some() {
        bail!(
            "--line-range with --line-numbers cannot be combined with --max-lines because smart truncation does not preserve source line numbers"
        );
    }

    Ok(())
}

fn select_line_range<'a>(content: &'a str, line_range: Option<LineRange>) -> Cow<'a, str> {
    let Some(range) = line_range else {
        return Cow::Borrowed(content);
    };

    let mut selected = String::new();
    for (index, line) in content.split_inclusive('\n').enumerate() {
        let source_line = index.saturating_add(1);
        if source_line > range.end {
            break;
        }
        if source_line >= range.start {
            selected.push_str(line);
        }
    }

    Cow::Owned(selected)
}

fn line_number_start(
    content: &str,
    line_range: Option<LineRange>,
    tail_lines: Option<usize>,
) -> usize {
    let Some(range) = line_range else {
        return 1;
    };

    let skipped = tail_lines.map_or(0, |tail| content.lines().count().saturating_sub(tail));
    range.start.saturating_add(skipped)
}

fn format_with_line_numbers(content: &str, first_line_number: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let last_line_number = first_line_number.saturating_add(lines.len().saturating_sub(1));
    let width = last_line_number.to_string().len();
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let line_number = first_line_number.saturating_add(i);
        out.push_str(&format!(
            "{:>width$} │ {}\n",
            line_number,
            line,
            width = width
        ));
    }
    out
}

fn apply_line_window(
    content: &str,
    max_lines: Option<usize>,
    tail_lines: Option<usize>,
    lang: &Language,
) -> String {
    if let Some(tail) = tail_lines {
        if tail == 0 {
            return String::new();
        }
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(tail);
        let mut result = lines[start..].join("\n");
        if content.ends_with('\n') {
            result.push('\n');
        }
        return result;
    }

    if let Some(max) = max_lines {
        if max == 0 {
            return String::new();
        }
        return filter::smart_truncate(content, max, lang);
    }

    content.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_rust_file() -> Result<()> {
        let mut file = NamedTempFile::with_suffix(".rs")?;
        writeln!(
            file,
            r#"// Comment
fn main() {{
    println!("Hello");
}}"#
        )?;

        // Just verify it doesn't panic
        run(
            file.path(),
            FilterLevel::Minimal,
            None,
            None,
            None,
            false,
            0,
        )?;
        Ok(())
    }

    #[test]
    fn test_line_range_parses_inclusive_bounds() -> Result<()> {
        let range = "2:5".parse::<LineRange>().map_err(anyhow::Error::msg)?;
        assert_eq!(range, LineRange { start: 2, end: 5 });
        Ok(())
    }

    #[test]
    fn test_line_range_rejects_invalid_bounds() {
        for value in [
            "", "1", "1:", ":1", "0:1", "1:0", "3:2", "a:2", "1:b", "1:2:3",
        ] {
            assert!(
                value.parse::<LineRange>().is_err(),
                "expected `{value}` to be rejected"
            );
        }
    }

    #[test]
    fn test_line_range_parse_errors_identify_the_invalid_bound() {
        let zero_error = "0:2".parse::<LineRange>().unwrap_err();
        assert!(zero_error.contains("greater than zero"));

        let reversed_error = "3:2".parse::<LineRange>().unwrap_err();
        assert!(reversed_error.contains("must not exceed"));
    }

    #[test]
    fn test_line_range_selects_exact_source_lines() {
        let input = "one\ntwo\nthree\nfour\n";
        let range = LineRange { start: 2, end: 3 };
        assert_eq!(select_line_range(input, Some(range)), "two\nthree\n");
    }

    #[test]
    fn test_line_range_handles_source_boundaries() {
        let input = "one\ntwo\nthree\n";
        assert_eq!(
            select_line_range(input, Some(LineRange { start: 1, end: 1 })),
            "one\n"
        );
        assert_eq!(
            select_line_range(input, Some(LineRange { start: 3, end: 5 })),
            "three\n"
        );
        assert_eq!(
            select_line_range(input, Some(LineRange { start: 4, end: 5 })),
            ""
        );
    }

    #[test]
    fn test_line_range_preserves_trailing_newline() {
        assert_eq!(
            select_line_range("one\ntwo\nthree\n", Some(LineRange { start: 2, end: 2 })),
            "two\n"
        );
        assert_eq!(
            select_line_range("one\ntwo\nthree", Some(LineRange { start: 3, end: 3 })),
            "three"
        );
    }

    #[test]
    fn test_ranged_line_numbers_use_source_line_numbers() {
        let range = LineRange { start: 42, end: 43 };
        let selected = "first selected\nsecond selected\n";
        assert_eq!(
            format_with_line_numbers(selected, range.start),
            "42 │ first selected\n43 │ second selected\n"
        );
    }

    #[test]
    fn test_range_is_selected_before_filtering() {
        let input = "outside\n// comment inside range\nkept\n";
        let selected = select_line_range(input, Some(LineRange { start: 2, end: 3 }));
        let filtered = filter::get_filter(FilterLevel::Minimal).filter(&selected, &Language::Rust);
        assert_eq!(filtered, "kept");
    }

    #[test]
    fn test_range_and_tail_apply_in_source_order() {
        let range = LineRange { start: 2, end: 5 };
        let selected = select_line_range("one\ntwo\nthree\nfour\nfive\nsix\n", Some(range));
        let output = apply_line_window(&selected, None, Some(2), &Language::Unknown);
        assert_eq!(output, "four\nfive\n");
        assert_eq!(line_number_start(&selected, Some(range), Some(2)), 4);
        assert_eq!(format_with_line_numbers(&output, 4), "4 │ four\n5 │ five\n");
    }

    #[test]
    fn test_ranged_line_numbers_reject_non_source_preserving_output() {
        let range = Some(LineRange { start: 2, end: 3 });
        let filter_error =
            validate_source_numbering(range, FilterLevel::Minimal, None, true).unwrap_err();
        assert!(filter_error.to_string().contains("--level none"));

        let truncate_error =
            validate_source_numbering(range, FilterLevel::None, Some(2), true).unwrap_err();
        assert!(truncate_error.to_string().contains("--max-lines"));
    }

    #[test]
    fn test_no_range_preserves_legacy_numbering_and_content() {
        let input = "one\ntwo\n";
        assert_eq!(select_line_range(input, None), input);
        assert_eq!(format_with_line_numbers(input, 1), "1 │ one\n2 │ two\n");
    }

    #[test]
    fn test_stdin_support_signature() {
        let _ = run_stdin
            as fn(
                FilterLevel,
                Option<LineRange>,
                Option<usize>,
                Option<usize>,
                bool,
                u8,
            ) -> Result<()>;
    }

    #[test]
    fn test_apply_line_window_tail_lines() {
        let input = "a\nb\nc\nd\n";
        let output = apply_line_window(input, None, Some(2), &Language::Unknown);
        assert_eq!(output, "c\nd\n");
    }

    #[test]
    fn test_apply_line_window_tail_lines_no_trailing_newline() {
        let input = "a\nb\nc\nd";
        let output = apply_line_window(input, None, Some(2), &Language::Unknown);
        assert_eq!(output, "c\nd");
    }

    #[test]
    fn test_apply_line_window_max_lines_still_works() {
        let input = "a\nb\nc\nd\n";
        let output = apply_line_window(input, Some(2), None, &Language::Unknown);
        assert!(output.starts_with("a\n"));
        assert!(output.contains("more lines"));
    }

    #[test]
    fn test_apply_line_window_zero_max_lines_is_empty() {
        let output = apply_line_window("a\nb\n", Some(0), None, &Language::Unknown);
        assert!(output.is_empty());
    }

    fn rtk_bin() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("debug")
            .join("rtk")
    }

    #[test]
    #[ignore]
    fn test_read_two_valid_files_concatenated() {
        let bin = rtk_bin();
        assert!(bin.exists(), "Run `cargo build` first");

        let mut f1 = NamedTempFile::with_suffix(".txt").unwrap();
        let mut f2 = NamedTempFile::with_suffix(".txt").unwrap();
        writeln!(f1, "alpha\nbravo").unwrap();
        writeln!(f2, "charlie\ndelta").unwrap();

        let output = std::process::Command::new(&bin)
            .args([
                "read",
                &f1.path().to_string_lossy(),
                &f2.path().to_string_lossy(),
            ])
            .output()
            .expect("failed to run rtk read");

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("alpha"), "first file content missing");
        assert!(stdout.contains("charlie"), "second file content missing");
    }

    #[test]
    #[ignore]
    fn test_read_valid_and_nonexistent() {
        let bin = rtk_bin();
        assert!(bin.exists(), "Run `cargo build` first");

        let mut f1 = NamedTempFile::with_suffix(".txt").unwrap();
        writeln!(f1, "valid content").unwrap();

        let output = std::process::Command::new(&bin)
            .args([
                "read",
                &f1.path().to_string_lossy(),
                "/tmp/rtk_nonexistent_file.txt",
            ])
            .output()
            .expect("failed to run rtk read");

        assert!(
            !output.status.success(),
            "should exit non-zero on missing file"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("valid content"),
            "valid file should still be printed"
        );
        assert!(
            stderr.contains("rtk_nonexistent_file"),
            "should report missing file on stderr"
        );
    }

    #[test]
    fn test_tail_takes_precedence_over_max_after_range_selection() {
        let selected = select_line_range(
            "one\ntwo\nthree\nfour\nfive\nsix\n",
            Some(LineRange { start: 2, end: 5 }),
        );
        let output = apply_line_window(&selected, Some(1), Some(2), &Language::Unknown);
        assert_eq!(output, "four\nfive\n");
    }

    #[test]
    #[ignore]
    fn test_read_stdin_dedup_warning() {
        let bin = rtk_bin();
        assert!(bin.exists(), "Run `cargo build` first");

        let output = std::process::Command::new(&bin)
            .args(["read", "-", "-"])
            .stdin(std::process::Stdio::piped())
            .output()
            .expect("failed to run rtk read");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("stdin specified more than once"),
            "should warn about duplicate stdin, got stderr: {}",
            stderr
        );
    }
}
