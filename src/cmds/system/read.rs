//! Reads source files with optional language-aware filtering to strip boilerplate.

use crate::core::filter::{self, FilterLevel, Language};
use crate::core::guard::never_worse;
use crate::core::tracking;
use anyhow::{Context, Result, bail};
use std::borrow::Cow;
use std::fs;
use std::io::{self, Read as IoRead, Write};
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

#[allow(clippy::too_many_arguments)]
pub fn run(
    file: &Path,
    level: FilterLevel,
    line_range: Option<LineRange>,
    max_lines: Option<usize>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    line_numbers: bool,
    verbose: u8,
) -> Result<()> {
    validate_source_numbering(line_range, level, max_lines, line_numbers)?;

    let timer = tracking::TimedExecution::start();

    if verbose > 0 {
        eprintln!("Reading: {} (filter: {})", file.display(), level);
    }

    // `head -n N` stops as soon as it has N lines. Reading the file whole first gives the same
    // answer on a regular file and no answer at all on a device node or a FIFO nobody closes,
    // which is reachable now that `head -n N` rewrites to this.
    if level == FilterLevel::None
        && !line_numbers
        && let Some(head) = head_lines
    {
        let window = read_head_lines(file, head)?;
        io::stdout()
            .lock()
            .write_all(&window)
            .context("Failed to write line window")?;
        timer.track_bytes(
            &format!("cat {}", file.display()),
            "rtk read",
            // The bytes `cat` would have written. Unknowable without reading the file, which is
            // the whole point of not doing that, so it is taken from the size on disk -- and
            // for the unbounded sources above there is no size, only a 0 that would book the
            // window as pure cost. Claim nothing there.
            regular_file_len(file).unwrap_or(window.len()),
            &String::from_utf8_lossy(&window),
        );
        return Ok(());
    }

    // Read file content
    let bytes =
        fs::read(file).with_context(|| format!("Failed to read file: {}", file.display()))?;
    if level == FilterLevel::None
        && !line_numbers
        && let Some(window) = byte_line_window(&bytes, head_lines, tail_lines)
    {
        io::stdout()
            .lock()
            .write_all(window)
            .context("Failed to write line window")?;
        timer.track(
            &format!("cat {}", file.display()),
            "rtk read",
            &String::from_utf8_lossy(&bytes),
            &String::from_utf8_lossy(window),
        );
        return Ok(());
    }
    let content = String::from_utf8(bytes)
        .with_context(|| format!("Failed to decode file: {}", file.display()))?;

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

    // Raw baseline: the unfiltered selection, numbered like the source. The line
    // window applies to the filtered side only, so filtering cannot be undone by
    // a smaller unfiltered window (see tests/read_window_bytes_test.rs).
    let raw = selected.to_string();
    let raw_line_number_start = line_number_start(&selected, line_range, None);
    let line_number_start = line_number_start(&filtered, line_range, tail_lines);
    filtered = apply_line_window(&filtered, max_lines, head_lines, tail_lines, &lang);

    let (raw, rtk_output) = if line_numbers {
        (
            format_with_line_numbers(&raw, raw_line_number_start),
            format_with_line_numbers(&filtered, line_number_start),
        )
    } else {
        (raw, filtered.clone())
    };
    let shown = never_worse(&raw, &rtk_output);
    print!("{}", shown);
    timer.track(&format!("cat {}", file.display()), "rtk read", &raw, shown);
    Ok(())
}

pub fn run_stdin(
    level: FilterLevel,
    line_range: Option<LineRange>,
    max_lines: Option<usize>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    line_numbers: bool,
    verbose: u8,
) -> Result<()> {
    validate_source_numbering(line_range, level, max_lines, line_numbers)?;

    let timer = tracking::TimedExecution::start();

    if verbose > 0 {
        eprintln!("Reading from stdin (filter: {})", level);
    }

    // Read from stdin
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut bytes)
        .context("Failed to read from stdin")?;
    if level == FilterLevel::None
        && !line_numbers
        && let Some(window) = byte_line_window(&bytes, head_lines, tail_lines)
    {
        io::stdout()
            .lock()
            .write_all(window)
            .context("Failed to write line window")?;
        timer.track(
            "cat - (stdin)",
            "rtk read -",
            &String::from_utf8_lossy(&bytes),
            &String::from_utf8_lossy(window),
        );
        return Ok(());
    }
    let content = String::from_utf8(bytes).context("Failed to decode stdin")?;

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

    // Raw baseline mirrors `run`: unfiltered selection, window on the filtered side.
    let raw = selected.to_string();
    let raw_line_number_start = line_number_start(&selected, line_range, None);
    let line_number_start = line_number_start(&filtered, line_range, tail_lines);
    filtered = apply_line_window(&filtered, max_lines, head_lines, tail_lines, &lang);

    let (raw, rtk_output) = if line_numbers {
        (
            format_with_line_numbers(&raw, raw_line_number_start),
            format_with_line_numbers(&filtered, line_number_start),
        )
    } else {
        (raw, filtered.clone())
    };
    let shown = never_worse(&raw, &rtk_output);
    print!("{}", shown);

    timer.track("cat - (stdin)", "rtk read -", &raw, shown);
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
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    lang: &Language,
) -> String {
    if let Some(window) = byte_line_window(content.as_bytes(), head_lines, tail_lines) {
        return String::from_utf8_lossy(window).into_owned();
    }

    if let Some(max) = max_lines {
        if max == 0 {
            return String::new();
        }
        return filter::smart_truncate(content, max, lang);
    }

    content.to_string()
}

/// How much is pulled from the file at a time. Only the lines asked for are ever read, so the
/// chunk bounds how far past the `n`th newline that read can reach.
const READ_CHUNK: usize = 8192;

/// The first `n` newline-terminated lines of `file`, read in chunks and stopped at the `n`th
/// newline so an endless source is never read past what was asked for. Short input, or input
/// whose last line is unterminated, comes back whole, matching [`head_window`].
///
/// Only the unfiltered head window is served this way. A filter level or `--line-numbers`
/// still needs the file whole -- `--tail-lines` inherently so -- and none of those is reachable
/// from a `head` rewrite, which is what made this path the one that had to stop early.
fn read_head_lines(file: &Path, n: usize) -> Result<Vec<u8>> {
    let mut handle =
        fs::File::open(file).with_context(|| format!("Failed to read file: {}", file.display()))?;
    let mut window = Vec::new();
    let mut chunk = [0u8; READ_CHUNK];
    let mut seen = 0;
    while seen < n {
        let read = match handle.read(&mut chunk) {
            Ok(read) => read,
            // `fs::read`, which this replaces, retries this itself; a bare `read` does not,
            // and turning a signal into a failed read would lose the window entirely.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to read file: {}", file.display()));
            }
        };
        if read == 0 {
            break;
        }
        for &byte in &chunk[..read] {
            window.push(byte);
            if byte == b'\n' {
                seen += 1;
                if seen == n {
                    break;
                }
            }
        }
    }
    Ok(window)
}

/// `file`'s size on disk, and `None` for anything whose size says nothing about how much it
/// will produce -- a device node, a FIFO, a socket.
fn regular_file_len(file: &Path) -> Option<usize> {
    let meta = fs::metadata(file).ok()?;
    meta.is_file().then_some(meta.len() as usize)
}

/// First `n` lines, sliced on byte offsets rather than round-tripped through
/// `lines()`, so CRLF endings and an unterminated final line survive verbatim.
/// `\n` is ASCII, so valid UTF-8 input also stays valid after slicing.
fn head_window(content: &[u8], n: usize) -> &[u8] {
    if n == 0 {
        return &[];
    }
    let mut seen = 0;
    for (idx, &byte) in content.iter().enumerate() {
        if byte == b'\n' {
            seen += 1;
            if seen == n {
                return &content[..=idx];
            }
        }
    }
    content
}

/// Last `n` lines, byte-sliced for the same fidelity reasons as `head_window`.
/// A trailing newline terminates the final line instead of starting a new one,
/// so it is excluded before counting separators backwards — otherwise `n` would
/// select one line too few for newline-terminated input.
fn tail_window(content: &[u8], n: usize) -> &[u8] {
    if n == 0 {
        return &[];
    }
    let search_end = match content.last() {
        Some(b'\n') => content.len() - 1,
        _ => content.len(),
    };
    let mut seen = 0;
    for idx in (0..search_end).rev() {
        if content[idx] == b'\n' {
            seen += 1;
            if seen == n {
                return &content[idx + 1..];
            }
        }
    }
    content
}

fn byte_line_window(
    content: &[u8],
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
) -> Option<&[u8]> {
    if let Some(head) = head_lines {
        Some(head_window(content, head))
    } else {
        tail_lines.map(|tail| tail_window(content, tail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// `read_head_lines` must agree with `head_window` byte-for-byte on every shape, since it
    /// replaces it on the unfiltered path -- CRLF endings and an unterminated last line
    /// included.
    ///
    /// The inputs have to span more than one `READ_CHUNK`, because reading across chunks is
    /// the only thing the rewrite added: a set that all fits in the first chunk passes just as
    /// happily with the loop stopped after that chunk.
    #[test]
    fn test_read_head_lines_matches_head_window() -> Result<()> {
        let long_line = "x".repeat(READ_CHUNK * 2);
        // A newline sitting exactly on a chunk boundary, and on either side of it.
        let boundary = |at: usize| format!("{}\n{}\n", "y".repeat(at - 1), "z".repeat(100));

        let mut contents: Vec<String> = [
            "",
            "a",
            "a\n",
            "a\nb\nc\n",
            "a\nb\nc",
            "a\r\nb\r\nc\r\n",
            "\n\n\n",
        ]
        .iter()
        .map(|c| (*c).to_string())
        .collect();
        contents.push(long_line.clone());
        contents.push(format!("{long_line}\n"));
        contents.push(boundary(READ_CHUNK));
        contents.push(boundary(READ_CHUNK + 1));
        contents.push(boundary(READ_CHUNK - 1));
        // Many short lines over several chunks, so the Nth newline lands deep in.
        contents.push((0..4000).map(|i| format!("line {i}\n")).collect());
        // A CRLF straddling a chunk boundary: the `\r` and its `\n` must not come apart.
        contents.push(format!(
            "{}\r\n{}\r\n",
            "w".repeat(READ_CHUNK - 1),
            "v".repeat(50)
        ));

        for content in &contents {
            let mut file = NamedTempFile::new()?;
            file.write_all(content.as_bytes())?;
            file.flush()?;
            for n in [0, 1, 2, 3, 10, 1000, 4000] {
                assert_eq!(
                    read_head_lines(file.path(), n)?,
                    head_window(content.as_bytes(), n),
                    "content of {} bytes, n {n}",
                    content.len()
                );
            }
        }
        Ok(())
    }

    /// The point of reading in chunks: a source with no end still returns. A FIFO nobody ever
    /// closes stands in for the `/dev/urandom` case, which `head -n N` now rewrites to.
    #[cfg(unix)]
    #[test]
    fn test_read_head_lines_returns_from_an_endless_source() -> Result<()> {
        use std::io::Write as _;
        let dir = tempfile::tempdir()?;
        let fifo = dir.path().join("endless");
        // Shelled out rather than called through libc: `unsafe` is not allowed outside proxy
        // mode's signal handling.
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()?
                .success(),
            "mkfifo failed"
        );

        let writer_path = fifo.clone();
        let writer = std::thread::spawn(move || {
            let Ok(mut handle) = fs::OpenOptions::new().write(true).open(&writer_path) else {
                return;
            };
            // Never closes on its own: the read side has to stop itself.
            while handle.write_all(b"line\n").is_ok() {}
        });

        assert_eq!(read_head_lines(&fifo, 3)?, b"line\nline\nline\n");
        drop(writer);
        Ok(())
    }

    /// A device node reports a size of 0, which would book the window as pure cost.
    #[test]
    fn test_regular_file_len_only_answers_for_a_regular_file() -> Result<()> {
        let mut file = NamedTempFile::new()?;
        file.write_all(b"hello\n")?;
        file.flush()?;
        assert_eq!(regular_file_len(file.path()), Some(6));
        assert_eq!(regular_file_len(Path::new("/nonexistent-rtk-test")), None);
        #[cfg(unix)]
        assert_eq!(regular_file_len(Path::new("/dev/null")), None);
        Ok(())
    }

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
        let output = apply_line_window(&selected, None, None, Some(2), &Language::Unknown);
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
                Option<usize>,
                bool,
                u8,
            ) -> Result<()>;
    }

    #[test]
    fn test_apply_line_window_tail_lines() {
        let input = "a\nb\nc\nd\n";
        let output = apply_line_window(input, None, None, Some(2), &Language::Unknown);
        assert_eq!(output, "c\nd\n");
    }

    #[test]
    fn test_apply_line_window_tail_lines_no_trailing_newline() {
        let input = "a\nb\nc\nd";
        let output = apply_line_window(input, None, None, Some(2), &Language::Unknown);
        assert_eq!(output, "c\nd");
    }

    #[test]
    fn test_head_window_matches_native_head() {
        let input = "1\n2\n3\n4\n5\n";
        assert_eq!(
            apply_line_window(input, None, Some(3), None, &Language::Unknown),
            "1\n2\n3\n"
        );
    }

    /// The defect this window exists to fix: `--max-lines N` keeps only about
    /// N/2 lines, so it could never stand in for `head -N`.
    #[test]
    fn test_head_window_keeps_all_n_lines_unlike_max_lines() {
        let input = (1..=200)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let head = apply_line_window(&input, None, Some(10), None, &Language::Unknown);
        assert_eq!(head.lines().count(), 10);
        assert_eq!(head.lines().last(), Some("10"));
    }

    #[test]
    fn test_head_window_single_line() {
        let input = "1\n2\n3\n";
        assert_eq!(
            apply_line_window(input, None, Some(1), None, &Language::Unknown),
            "1\n"
        );
    }

    #[test]
    fn test_head_window_zero_is_empty() {
        assert_eq!(
            apply_line_window("a\nb\n", None, Some(0), None, &Language::Unknown),
            ""
        );
    }

    #[test]
    fn test_head_window_n_exceeds_line_count() {
        let input = "a\nb\n";
        assert_eq!(
            apply_line_window(input, None, Some(99), None, &Language::Unknown),
            input
        );
    }

    #[test]
    fn test_head_window_empty_input() {
        assert_eq!(
            apply_line_window("", None, Some(5), None, &Language::Unknown),
            ""
        );
    }

    #[test]
    fn test_head_window_unterminated_final_line() {
        assert_eq!(
            apply_line_window("a\nb\nc", None, Some(3), None, &Language::Unknown),
            "a\nb\nc"
        );
    }

    #[test]
    fn test_head_window_preserves_crlf() {
        assert_eq!(
            apply_line_window("a\r\nb\r\nc\r\n", None, Some(2), None, &Language::Unknown),
            "a\r\nb\r\n"
        );
    }

    #[test]
    fn test_tail_window_preserves_crlf() {
        assert_eq!(
            apply_line_window("a\r\nb\r\nc\r\n", None, None, Some(2), &Language::Unknown),
            "b\r\nc\r\n"
        );
    }

    /// Without discounting the terminal newline, counting separators backwards
    /// selects one line too few for newline-terminated input.
    #[test]
    fn test_tail_window_unterminated_single_line() {
        assert_eq!(
            apply_line_window("a\nb\nc", None, None, Some(1), &Language::Unknown),
            "c"
        );
    }

    #[test]
    fn test_tail_window_n_exceeds_line_count() {
        let input = "a\nb\n";
        assert_eq!(
            apply_line_window(input, None, None, Some(99), &Language::Unknown),
            input
        );
    }

    #[test]
    fn test_tail_window_empty_input() {
        assert_eq!(
            apply_line_window("", None, None, Some(5), &Language::Unknown),
            ""
        );
    }

    #[test]
    fn test_max_lines_zero_is_empty() {
        assert_eq!(
            apply_line_window("a\nb\nc\n", Some(0), None, None, &Language::Unknown),
            ""
        );
    }

    #[test]
    fn test_head_window_mixed_line_endings() {
        assert_eq!(
            apply_line_window("a\r\nb\nc\r\n", None, Some(2), None, &Language::Unknown),
            "a\r\nb\n"
        );
    }

    #[test]
    fn test_tail_window_mixed_line_endings() {
        assert_eq!(
            apply_line_window("a\r\nb\nc\r\n", None, None, Some(2), &Language::Unknown),
            "b\nc\r\n"
        );
    }

    #[test]
    fn test_windows_preserve_multibyte_utf8() {
        let input = "héllo\n日本語\nثالث\n";
        assert_eq!(
            apply_line_window(input, None, Some(2), None, &Language::Unknown),
            "héllo\n日本語\n"
        );
        assert_eq!(
            apply_line_window(input, None, None, Some(2), &Language::Unknown),
            "日本語\nثالث\n"
        );
    }

    #[test]
    fn test_windows_on_blank_lines_only() {
        assert_eq!(
            apply_line_window("\n\n\n", None, Some(2), None, &Language::Unknown),
            "\n\n"
        );
        assert_eq!(
            apply_line_window("\n\n\n", None, None, Some(2), &Language::Unknown),
            "\n\n"
        );
    }

    #[test]
    fn test_tail_window_zero_is_empty() {
        assert_eq!(
            apply_line_window("a\nb\n", None, None, Some(0), &Language::Unknown),
            ""
        );
    }

    #[test]
    fn test_apply_line_window_max_lines_still_works() {
        let input = "a\nb\nc\nd\n";
        let output = apply_line_window(input, Some(2), None, None, &Language::Unknown);
        assert!(output.starts_with("a\n"));
        assert!(output.contains("more lines"));
    }

    #[test]
    fn test_apply_line_window_zero_max_lines_is_empty() {
        let output = apply_line_window("a\nb\n", Some(0), None, None, &Language::Unknown);
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
        let output = apply_line_window(&selected, Some(1), None, Some(2), &Language::Unknown);
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
