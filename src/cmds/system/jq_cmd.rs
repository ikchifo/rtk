use crate::cmds::system::json_cmd;
use crate::core::stream::exec_capture;
use crate::core::tracking;
use crate::core::utils::resolved_command;
use anyhow::{Context, Result};

const PASSTHROUGH_THRESHOLD: usize = 2000;
const MAX_JSONL_LINES: usize = 10;
const MAX_RAW_LINES: usize = 50;
const JSON_COMPRESS_DEPTH: usize = 4;

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    if verbose > 0 {
        eprintln!("jq: {}", args.join(" "));
    }

    let mut cmd = resolved_command("jq");
    cmd.args(args);
    let result = exec_capture(&mut cmd).context("failed to run jq")?;

    let raw_output = format!("{}{}", result.stdout, result.stderr);

    if !result.success() {
        // Error output from jq is already compact; show as-is.
        let filtered = result.stderr.clone();
        print!("{}", filtered);
        timer.track(
            &format!("jq {}", args.join(" ")),
            "rtk jq",
            &raw_output,
            &filtered,
        );
        return Ok(result.exit_code);
    }

    let stdout = &result.stdout;
    let filtered = compress_output(stdout);

    print!("{}", filtered);
    timer.track(
        &format!("jq {}", args.join(" ")),
        "rtk jq",
        &raw_output,
        &filtered,
    );

    Ok(0)
}

fn compress_output(stdout: &str) -> String {
    if stdout.len() <= PASSTHROUGH_THRESHOLD {
        return stdout.to_string();
    }

    // Try parsing as a single JSON value first.
    if let Ok(compressed) = json_cmd::filter_json_compact(stdout, JSON_COMPRESS_DEPTH) {
        return format!("{}\n", compressed);
    }

    // Check for JSONL (multiple JSON objects, one per line).
    let lines: Vec<&str> = stdout.lines().collect();
    if is_jsonl(&lines) {
        return compress_jsonl(&lines);
    }

    // Fallback: raw line truncation.
    truncate_lines(&lines)
}

fn is_jsonl(lines: &[&str]) -> bool {
    lines.len() > 10
        && lines
            .iter()
            .all(|l| l.starts_with('{') || l.starts_with('['))
}

fn compress_jsonl(lines: &[&str]) -> String {
    let total = lines.len();
    let total_chars: usize = lines.iter().map(|l| l.len()).sum();
    let mut out = String::with_capacity(512);
    for line in lines.iter().take(MAX_JSONL_LINES) {
        out.push_str(line);
        out.push('\n');
    }
    if total > MAX_JSONL_LINES {
        out.push_str(&format!(
            "... +{} more lines ({} total chars)\n",
            total - MAX_JSONL_LINES,
            total_chars
        ));
    }
    out
}

fn truncate_lines(lines: &[&str]) -> String {
    let total = lines.len();
    let mut out = String::with_capacity(512);
    for line in lines.iter().take(MAX_RAW_LINES) {
        out.push_str(line);
        out.push('\n');
    }
    if total > MAX_RAW_LINES {
        out.push_str(&format!(
            "--- [output truncated: showing {} of {} lines] ---\n",
            MAX_RAW_LINES, total
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_passthrough() {
        let input = r#"{"key": "value"}"#;
        assert_eq!(compress_output(input), input);
    }

    #[test]
    fn empty_output_passthrough() {
        assert_eq!(compress_output(""), "");
    }

    #[test]
    fn single_line_json_under_threshold() {
        let input = r#"{"name":"test","count":42}"#;
        assert_eq!(compress_output(input), input);
    }

    #[test]
    fn large_json_gets_compressed() {
        // Build a JSON object exceeding PASSTHROUGH_THRESHOLD.
        let mut entries = Vec::with_capacity(50);
        for i in 0..50 {
            entries.push(format!(r#""key_{}": "{}""#, i, "x".repeat(60)));
        }
        let json = format!("{{{}}}", entries.join(","));
        assert!(json.len() > PASSTHROUGH_THRESHOLD);

        let result = compress_output(&json);
        // The compressed result must be shorter than the original.
        assert!(
            result.len() < json.len(),
            "expected compression, got {} vs {}",
            result.len(),
            json.len()
        );
    }

    #[test]
    fn jsonl_detection_and_truncation() {
        let big_jsonl: String = (0..15)
            .map(|i| format!(r#"{{"id":{},"data":"{}"}}"#, i, "a".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(big_jsonl.len() > PASSTHROUGH_THRESHOLD);

        let result = compress_output(&big_jsonl);
        assert!(result.contains("... +5 more lines"));
        assert!(result.contains("total chars"));
    }

    #[test]
    fn raw_string_truncation() {
        // 80 plain-text lines, each long enough to exceed threshold.
        let lines: Vec<String> = (0..80).map(|i| format!("line {} {}", i, "z".repeat(40))).collect();
        let input = lines.join("\n");
        assert!(input.len() > PASSTHROUGH_THRESHOLD);

        let result = compress_output(&input);
        assert!(result.contains("output truncated"));
        assert!(result.contains("showing 50 of 80 lines"));
    }

    #[test]
    fn is_jsonl_requires_more_than_10_lines() {
        let few: Vec<&str> = vec![r#"{"a":1}"#; 5];
        assert!(!is_jsonl(&few));
    }

    #[test]
    fn is_jsonl_rejects_non_json_starts() {
        let lines: Vec<&str> = vec!["hello world"; 15];
        assert!(!is_jsonl(&lines));
    }

    #[test]
    fn is_jsonl_accepts_array_lines() {
        let lines: Vec<&str> = vec!["[1,2,3]"; 15];
        assert!(is_jsonl(&lines));
    }

    #[test]
    fn compress_jsonl_shows_count() {
        let lines: Vec<&str> = (0..20).map(|_| r#"{"x":1}"#).collect();
        let result = compress_jsonl(&lines);
        assert!(result.contains("... +10 more lines"));
    }

    #[test]
    fn truncate_lines_short_input() {
        let lines: Vec<&str> = vec!["a", "b", "c"];
        let result = truncate_lines(&lines);
        assert_eq!(result, "a\nb\nc\n");
        assert!(!result.contains("truncated"));
    }
}
