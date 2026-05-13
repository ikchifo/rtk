//! Google Cloud CLI output compression.
//!
//! Filters verbose `gcloud` output into compact, token-efficient summaries.
//! Specialized handlers for compute, container, run, builds, and a generic
//! JSON fallback for unrecognized subcommands.

use crate::cmds::system::json_cmd;
use crate::core::stream::exec_capture;
use crate::core::tee::force_tee_hint;
use crate::core::tracking;
use crate::core::utils::{resolved_command, truncate_iso_date};
use anyhow::{Context, Result};

const MAX_INSTANCES: usize = 20;
const MAX_BUILDS: usize = 15;
const BUILD_LOG_TAIL: usize = 30;
const GENERIC_MAX_LINES: usize = 60;
const JSON_COMPRESS_DEPTH: usize = 4;

/// Result of a filter: text + whether items were truncated.
struct FilterResult {
    text: String,
    truncated: bool,
}

impl FilterResult {
    fn new(text: String) -> Self {
        Self {
            text,
            truncated: false,
        }
    }

    fn truncated(text: String) -> Self {
        Self {
            text,
            truncated: true,
        }
    }
}

/// Run a gcloud command with token-optimized output.
pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    if args.is_empty() {
        return run_passthrough(args, verbose);
    }

    let group = args[0].as_str();
    let sub = args.get(1).map(String::as_str).unwrap_or("");
    let rest = if args.len() > 2 { &args[2..] } else { &[] };

    match (group, sub) {
        ("compute", "instances") if has_action(rest, "list") => {
            let extra = skip_action(rest, "list");
            run_gcloud_filtered(
                &["compute", "instances", "list"],
                &extra,
                verbose,
                filter_compute_instances_list,
            )
        }
        ("compute", "instances") if has_action(rest, "describe") => {
            let extra = skip_action(rest, "describe");
            run_gcloud_filtered(
                &["compute", "instances", "describe"],
                &extra,
                verbose,
                filter_compute_instances_describe,
            )
        }
        ("container", "clusters") if has_action(rest, "list") => {
            let extra = skip_action(rest, "list");
            run_gcloud_filtered(
                &["container", "clusters", "list"],
                &extra,
                verbose,
                filter_container_clusters_list,
            )
        }
        ("container", "clusters") if has_action(rest, "describe") => {
            let extra = skip_action(rest, "describe");
            run_gcloud_filtered(
                &["container", "clusters", "describe"],
                &extra,
                verbose,
                filter_container_clusters_describe,
            )
        }
        ("run", "services") if has_action(rest, "list") => {
            let extra = skip_action(rest, "list");
            run_gcloud_filtered(
                &["run", "services", "list"],
                &extra,
                verbose,
                filter_run_services_list,
            )
        }
        ("run", "services") if has_action(rest, "describe") => {
            let extra = skip_action(rest, "describe");
            run_gcloud_filtered(
                &["run", "services", "describe"],
                &extra,
                verbose,
                filter_run_services_describe,
            )
        }
        ("builds", "list") => {
            run_gcloud_filtered(&["builds", "list"], rest, verbose, filter_builds_list)
        }
        ("builds", "log") => run_builds_log(rest, verbose),
        ("config", "list") | ("auth", "list") => run_passthrough(args, verbose),
        _ => run_generic(args, verbose),
    }
}

/// Check whether the first element of `rest` matches `action`.
fn has_action(rest: &[String], action: &str) -> bool {
    rest.first().map(String::as_str) == Some(action)
}

/// Return `rest` with the first element (the action verb) removed.
fn skip_action(rest: &[String], _action: &str) -> Vec<String> {
    if rest.is_empty() {
        Vec::new()
    } else {
        rest[1..].to_vec()
    }
}

// ---------------------------------------------------------------------------
// Shared execution helpers
// ---------------------------------------------------------------------------

/// Run gcloud with `--format=json`, capture output, apply a filter function.
fn run_gcloud_filtered(
    sub_args: &[&str],
    extra_args: &[String],
    verbose: u8,
    filter_fn: fn(&str) -> Option<FilterResult>,
) -> Result<i32> {
    let cmd_label = format!("gcloud {}", sub_args.join(" "));
    let rtk_label = format!("rtk {}", cmd_label);
    let slug = cmd_label.replace(' ', "_");
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("gcloud");
    for arg in sub_args {
        cmd.arg(arg);
    }

    let mut has_format = false;
    for arg in extra_args {
        if arg == "--format" || arg.starts_with("--format=") {
            has_format = true;
        }
        cmd.arg(arg);
    }
    if !has_format {
        cmd.arg("--format=json");
    }

    if verbose > 0 {
        eprintln!("Running: {}", cmd_label);
    }

    let result = exec_capture(&mut cmd).context("failed to run gcloud")?;
    let raw = if result.stderr.is_empty() {
        result.stdout.clone()
    } else {
        format!("{}\n{}", result.stdout, result.stderr)
    };

    if !result.success() {
        let cleaned = strip_noise(&result.stderr);
        eprintln!("{}", cleaned.trim());
        timer.track(&cmd_label, &rtk_label, &raw, &cleaned);
        return Ok(result.exit_code);
    }

    let filtered =
        filter_fn(&result.stdout).unwrap_or_else(|| FilterResult::new(result.stdout.clone()));

    if filtered.truncated {
        if let Some(hint) = force_tee_hint(&raw, &slug) {
            println!("{}\n{}", filtered.text, hint);
        } else {
            println!("{}", filtered.text);
        }
    } else {
        println!("{}", filtered.text);
    }

    timer.track(&cmd_label, &rtk_label, &raw, &filtered.text);
    Ok(0)
}

/// Pass-through: run gcloud, strip noise from stderr, print stdout as-is.
fn run_passthrough(args: &[String], verbose: u8) -> Result<i32> {
    let cmd_label = format!("gcloud {}", args.join(" "));
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("gcloud");
    cmd.args(args);

    if verbose > 0 {
        eprintln!("Running: {}", cmd_label);
    }

    let result = exec_capture(&mut cmd).context("failed to run gcloud")?;
    let raw = result.combined();

    let cleaned_stderr = strip_noise(&result.stderr);
    if !cleaned_stderr.trim().is_empty() {
        eprintln!("{}", cleaned_stderr.trim());
    }
    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }

    timer.track(
        &cmd_label,
        &format!("rtk {}", cmd_label),
        &raw,
        &format!("{}{}", result.stdout, cleaned_stderr),
    );
    Ok(result.exit_code)
}

/// Generic: try JSON compression, fall back to line truncation.
fn run_generic(args: &[String], verbose: u8) -> Result<i32> {
    let cmd_label = format!("gcloud {}", args.join(" "));
    let rtk_label = format!("rtk {}", cmd_label);
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("gcloud");
    cmd.args(args);

    if verbose > 0 {
        eprintln!("Running: {}", cmd_label);
    }

    let result = exec_capture(&mut cmd).context("failed to run gcloud")?;
    let raw = result.combined();

    if !result.success() {
        let cleaned = strip_noise(&result.stderr);
        eprintln!("{}", cleaned.trim());
        timer.track(&cmd_label, &rtk_label, &raw, &cleaned);
        return Ok(result.exit_code);
    }

    let stdout = strip_noise(&result.stdout);

    // Try JSON compression first
    if let Ok(compressed) = json_cmd::filter_json_string(&stdout, JSON_COMPRESS_DEPTH) {
        println!("{}", compressed);
        timer.track(&cmd_label, &rtk_label, &raw, &compressed);
        return Ok(0);
    }

    // Fall back to line truncation
    let filtered = truncate_lines(&stdout, GENERIC_MAX_LINES);
    println!("{}", filtered);
    timer.track(&cmd_label, &rtk_label, &raw, &filtered);
    Ok(0)
}

// ---------------------------------------------------------------------------
// Build log (special: streaming text, not JSON)
// ---------------------------------------------------------------------------

fn run_builds_log(args: &[String], verbose: u8) -> Result<i32> {
    let cmd_label = format!("gcloud builds log {}", args.join(" "));
    let rtk_label = format!("rtk {}", cmd_label);
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("gcloud");
    cmd.args(["builds", "log"]);
    cmd.args(args);

    if verbose > 0 {
        eprintln!("Running: {}", cmd_label);
    }

    let result = exec_capture(&mut cmd).context("failed to run gcloud builds log")?;
    let raw = result.combined();

    if !result.success() {
        let cleaned = strip_noise(&result.stderr);
        eprintln!("{}", cleaned.trim());
        timer.track(&cmd_label, &rtk_label, &raw, &cleaned);
        return Ok(result.exit_code);
    }

    let lines: Vec<&str> = result.stdout.lines().collect();
    let total = lines.len();

    let filtered = if total > BUILD_LOG_TAIL {
        let tail: Vec<&str> = lines[total - BUILD_LOG_TAIL..].to_vec();
        format!(
            "build log: {} total lines, showing last {}\n---\n{}",
            total,
            BUILD_LOG_TAIL,
            tail.join("\n")
        )
    } else {
        result.stdout.clone()
    };

    println!("{}", filtered);
    timer.track(&cmd_label, &rtk_label, &raw, &filtered);
    Ok(0)
}

// ---------------------------------------------------------------------------
// Filter functions -- each parses JSON array/object and returns compact text
// ---------------------------------------------------------------------------

/// `gcloud compute instances list --format=json`
fn filter_compute_instances_list(json_str: &str) -> Option<FilterResult> {
    let items: Vec<serde_json::Value> = serde_json::from_str(json_str).ok()?;
    let total = items.len();
    let show = items.iter().take(MAX_INSTANCES);

    let mut lines = Vec::with_capacity(total.min(MAX_INSTANCES) + 1);
    lines.push("NAME | ZONE | STATUS | INTERNAL_IP".to_string());

    for item in show {
        let name = json_str_field(item, "name");
        let zone = extract_last_segment(json_str_field(item, "zone"));
        let status = json_str_field(item, "status");
        let ip = item
            .pointer("/networkInterfaces/0/networkIP")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        lines.push(format!("{} | {} | {} | {}", name, zone, status, ip));
    }

    if total > MAX_INSTANCES {
        lines.push(format!("... +{} more instances", total - MAX_INSTANCES));
        Some(FilterResult::truncated(lines.join("\n")))
    } else {
        Some(FilterResult::new(lines.join("\n")))
    }
}

/// `gcloud compute instances describe --format=json`
fn filter_compute_instances_describe(json_str: &str) -> Option<FilterResult> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;

    let name = json_str_field(&v, "name");
    let zone = extract_last_segment(json_str_field(&v, "zone"));
    let status = json_str_field(&v, "status");
    let machine_type = extract_last_segment(json_str_field(&v, "machineType"));
    let ip = v
        .pointer("/networkInterfaces/0/networkIP")
        .and_then(|v| v.as_str())
        .unwrap_or("-");

    let disks = v
        .get("disks")
        .and_then(|d| d.as_array())
        .map(|arr| {
            let summaries: Vec<String> = arr
                .iter()
                .map(|d| {
                    let src = extract_last_segment(
                        d.get("source").and_then(|s| s.as_str()).unwrap_or("-"),
                    );
                    let size = d.get("diskSizeGb").and_then(|s| s.as_str()).unwrap_or("?");
                    format!("{}({}GB)", src, size)
                })
                .collect();
            summaries.join(", ")
        })
        .unwrap_or_else(|| "-".to_string());

    let text = format!(
        "name: {}\nzone: {}\nstatus: {}\nmachineType: {}\ninternalIP: {}\ndisks: {}",
        name, zone, status, machine_type, ip, disks
    );
    Some(FilterResult::new(text))
}

/// `gcloud container clusters list --format=json`
fn filter_container_clusters_list(json_str: &str) -> Option<FilterResult> {
    let items: Vec<serde_json::Value> = serde_json::from_str(json_str).ok()?;
    let total = items.len();

    let mut lines = Vec::with_capacity(total.min(MAX_INSTANCES) + 1);
    lines.push("NAME | LOCATION | STATUS | NODES".to_string());

    for item in items.iter().take(MAX_INSTANCES) {
        let name = json_str_field(item, "name");
        let location = json_str_field(item, "location");
        let status = json_str_field(item, "status");
        let nodes = item
            .get("currentNodeCount")
            .and_then(|n| n.as_u64())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".to_string());
        lines.push(format!("{} | {} | {} | {}", name, location, status, nodes));
    }

    if total > MAX_INSTANCES {
        lines.push(format!("... +{} more clusters", total - MAX_INSTANCES));
        Some(FilterResult::truncated(lines.join("\n")))
    } else {
        Some(FilterResult::new(lines.join("\n")))
    }
}

/// `gcloud container clusters describe --format=json`
fn filter_container_clusters_describe(json_str: &str) -> Option<FilterResult> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;

    let name = json_str_field(&v, "name");
    let location = json_str_field(&v, "location");
    let status = json_str_field(&v, "status");
    let nodes = v
        .get("currentNodeCount")
        .and_then(|n| n.as_u64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".to_string());

    let node_config = v.get("nodeConfig").map(|nc| {
        let machine = json_str_field(nc, "machineType");
        let disk_size = nc
            .get("diskSizeGb")
            .and_then(|d| d.as_u64())
            .map(|d| format!("{}GB", d))
            .unwrap_or_else(|| "?".to_string());
        let disk_type = json_str_field(nc, "diskType");
        format!("{}, {}, {}", machine, disk_size, disk_type)
    });

    let mut text = format!(
        "name: {}\nlocation: {}\nstatus: {}\nnodeCount: {}",
        name, location, status, nodes
    );
    if let Some(nc) = node_config {
        text.push_str(&format!("\nnodeConfig: {}", nc));
    }

    Some(FilterResult::new(text))
}

/// `gcloud run services list --format=json`
fn filter_run_services_list(json_str: &str) -> Option<FilterResult> {
    let items: Vec<serde_json::Value> = serde_json::from_str(json_str).ok()?;
    let total = items.len();

    let mut lines = Vec::with_capacity(total.min(MAX_INSTANCES) + 1);
    lines.push("SERVICE | REGION | URL | LAST_DEPLOYED".to_string());

    for item in items.iter().take(MAX_INSTANCES) {
        let name = item
            .pointer("/metadata/name")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        let region = item
            .pointer("/metadata/labels/cloud.googleapis.com~1location")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        let url = item
            .pointer("/status/url")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        let deployed = item
            .pointer("/metadata/creationTimestamp")
            .and_then(|v| v.as_str())
            .map(truncate_iso_date)
            .unwrap_or("-");
        lines.push(format!("{} | {} | {} | {}", name, region, url, deployed));
    }

    if total > MAX_INSTANCES {
        lines.push(format!("... +{} more services", total - MAX_INSTANCES));
        Some(FilterResult::truncated(lines.join("\n")))
    } else {
        Some(FilterResult::new(lines.join("\n")))
    }
}

/// `gcloud run services describe --format=json`
fn filter_run_services_describe(json_str: &str) -> Option<FilterResult> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;

    let name = v
        .pointer("/metadata/name")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let url = v
        .pointer("/status/url")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let ready = v
        .pointer("/status/conditions")
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("Ready"))
        })
        .and_then(|c| c.get("status"))
        .and_then(|s| s.as_str())
        .unwrap_or("-");

    let text = format!("name: {}\nurl: {}\nready: {}", name, url, ready);
    Some(FilterResult::new(text))
}

/// `gcloud builds list --format=json`
fn filter_builds_list(json_str: &str) -> Option<FilterResult> {
    let items: Vec<serde_json::Value> = serde_json::from_str(json_str).ok()?;
    let total = items.len();

    let mut lines = Vec::with_capacity(total.min(MAX_BUILDS) + 1);
    lines.push("BUILD_ID | STATUS | SOURCE | DURATION".to_string());

    for item in items.iter().take(MAX_BUILDS) {
        let id = json_str_field(item, "id");
        let short_id = if id.len() > 8 { &id[..8] } else { id };
        let status = json_str_field(item, "status");

        let source = item
            .pointer("/source/repoSource/repoName")
            .or_else(|| item.pointer("/source/storageSource/bucket"))
            .and_then(|v| v.as_str())
            .unwrap_or("-");

        let duration = json_str_field(item, "duration");

        lines.push(format!(
            "{} | {} | {} | {}",
            short_id, status, source, duration
        ));
    }

    if total > MAX_BUILDS {
        lines.push(format!("... +{} more builds", total - MAX_BUILDS));
        Some(FilterResult::truncated(lines.join("\n")))
    } else {
        Some(FilterResult::new(lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// Noise stripping
// ---------------------------------------------------------------------------

/// Strip common gcloud noise lines from output.
fn strip_noise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if is_noise_line(line) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn is_noise_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.starts_with("WARNING:") && trimmed.contains("API") && trimmed.contains("not enabled")
    {
        return true;
    }
    if trimmed.starts_with("Listed") && trimmed.ends_with("items.") {
        return true;
    }
    if trimmed.starts_with("Updates are available") || trimmed.starts_with("Update available") {
        return true;
    }
    if trimmed.starts_with("To take a quick anonymous survey") {
        return true;
    }
    if trimmed.starts_with("Run `gcloud components update`") {
        return true;
    }
    // Decorative table borders
    if trimmed
        .chars()
        .all(|c| c == '-' || c == '+' || c == '=' || c == ' ')
        && trimmed.len() > 3
    {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract a string field from a JSON value, returning "-" if missing.
fn json_str_field<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|f| f.as_str()).unwrap_or("-")
}

/// Extract the last `/`-separated segment from a GCP resource URL.
/// e.g. "projects/foo/zones/us-central1-a" -> "us-central1-a"
fn extract_last_segment(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Truncate text to `max` lines with a summary footer.
fn truncate_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total <= max {
        return text.to_string();
    }
    let mut out = String::with_capacity(max * 80);
    for line in lines.iter().take(max) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "... +{} more lines (total {})",
        total - max,
        total
    ));
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_noise_removes_warnings() {
        let input = "WARNING: API [compute.googleapis.com] not enabled on project\n\
                      NAME  ZONE  STATUS\n\
                      Listed 0 items.\n\
                      Updates are available for some components.\n\
                      Run `gcloud components update` to update.\n";
        let result = strip_noise(input);
        assert_eq!(result.trim(), "NAME  ZONE  STATUS");
    }

    #[test]
    fn test_strip_noise_removes_decorative_borders() {
        let input = "---+---+---\nreal data\n===  ===\n";
        let result = strip_noise(input);
        assert_eq!(result.trim(), "real data");
    }

    #[test]
    fn test_strip_noise_preserves_content() {
        let input = "name: my-instance\nzone: us-central1-a\n";
        let result = strip_noise(input);
        assert!(result.contains("name: my-instance"));
        assert!(result.contains("zone: us-central1-a"));
    }

    #[test]
    fn test_is_noise_line_empty() {
        assert!(is_noise_line(""));
        assert!(is_noise_line("   "));
    }

    #[test]
    fn test_extract_last_segment() {
        assert_eq!(
            extract_last_segment("projects/my-proj/zones/us-central1-a"),
            "us-central1-a"
        );
        assert_eq!(extract_last_segment("no-slash"), "no-slash");
        assert_eq!(extract_last_segment(""), "");
    }

    #[test]
    fn test_truncate_lines_short() {
        let text = "line1\nline2\nline3";
        assert_eq!(truncate_lines(text, 10), text);
    }

    #[test]
    fn test_truncate_lines_long() {
        let text = "a\nb\nc\nd\ne\nf";
        let result = truncate_lines(text, 3);
        assert!(result.contains("a\nb\nc\n"));
        assert!(result.contains("... +3 more lines (total 6)"));
    }

    #[test]
    fn test_filter_compute_instances_list_basic() {
        let json = r#"[
            {
                "name": "vm-1",
                "zone": "projects/my-proj/zones/us-central1-a",
                "status": "RUNNING",
                "networkInterfaces": [{"networkIP": "10.0.0.1"}]
            },
            {
                "name": "vm-2",
                "zone": "projects/my-proj/zones/europe-west1-b",
                "status": "TERMINATED",
                "networkInterfaces": [{"networkIP": "10.0.0.2"}]
            }
        ]"#;

        let result = filter_compute_instances_list(json).expect("should parse");
        assert!(!result.truncated);
        assert!(result
            .text
            .contains("vm-1 | us-central1-a | RUNNING | 10.0.0.1"));
        assert!(result
            .text
            .contains("vm-2 | europe-west1-b | TERMINATED | 10.0.0.2"));
        assert!(result
            .text
            .starts_with("NAME | ZONE | STATUS | INTERNAL_IP"));
    }

    #[test]
    fn test_filter_compute_instances_list_truncation() {
        let items: Vec<String> = (0..25)
            .map(|i| {
                format!(
                    r#"{{"name":"vm-{}","zone":"z","status":"RUNNING","networkInterfaces":[{{"networkIP":"10.0.0.{}"}}]}}"#,
                    i, i
                )
            })
            .collect();
        let json = format!("[{}]", items.join(","));

        let result = filter_compute_instances_list(&json).expect("should parse");
        assert!(result.truncated);
        assert!(result.text.contains("... +5 more instances"));
    }

    #[test]
    fn test_filter_compute_instances_describe() {
        let json = r#"{
            "name": "my-vm",
            "zone": "projects/p/zones/us-east1-b",
            "status": "RUNNING",
            "machineType": "projects/p/machineTypes/n1-standard-4",
            "networkInterfaces": [{"networkIP": "10.0.0.5"}],
            "disks": [
                {"source": "projects/p/disks/boot-disk", "diskSizeGb": "50"},
                {"source": "projects/p/disks/data-disk", "diskSizeGb": "200"}
            ]
        }"#;

        let result = filter_compute_instances_describe(json).expect("should parse");
        assert!(result.text.contains("name: my-vm"));
        assert!(result.text.contains("zone: us-east1-b"));
        assert!(result.text.contains("machineType: n1-standard-4"));
        assert!(result.text.contains("internalIP: 10.0.0.5"));
        assert!(result.text.contains("boot-disk(50GB)"));
        assert!(result.text.contains("data-disk(200GB)"));
    }

    #[test]
    fn test_filter_container_clusters_list() {
        let json = r#"[
            {
                "name": "prod-cluster",
                "location": "europe-west4-b",
                "status": "RUNNING",
                "currentNodeCount": 12
            }
        ]"#;

        let result = filter_container_clusters_list(json).expect("should parse");
        assert!(result
            .text
            .contains("prod-cluster | europe-west4-b | RUNNING | 12"));
    }

    #[test]
    fn test_filter_container_clusters_describe() {
        let json = r#"{
            "name": "staging",
            "location": "europe-west1-d",
            "status": "RUNNING",
            "currentNodeCount": 3,
            "nodeConfig": {
                "machineType": "e2-standard-4",
                "diskSizeGb": 100,
                "diskType": "pd-standard"
            }
        }"#;

        let result = filter_container_clusters_describe(json).expect("should parse");
        assert!(result.text.contains("name: staging"));
        assert!(result.text.contains("nodeCount: 3"));
        assert!(result
            .text
            .contains("nodeConfig: e2-standard-4, 100GB, pd-standard"));
    }

    #[test]
    fn test_filter_builds_list_basic() {
        let json = r#"[
            {
                "id": "abcdef12-3456-7890-abcd-ef1234567890",
                "status": "SUCCESS",
                "source": {"repoSource": {"repoName": "my-repo"}},
                "duration": "120s"
            }
        ]"#;

        let result = filter_builds_list(json).expect("should parse");
        assert!(result.text.contains("abcdef12 | SUCCESS | my-repo | 120s"));
    }

    #[test]
    fn test_filter_builds_list_truncation() {
        let items: Vec<String> = (0..20)
            .map(|i| {
                format!(
                    r#"{{"id":"id{:08}","status":"SUCCESS","duration":"10s"}}"#,
                    i
                )
            })
            .collect();
        let json = format!("[{}]", items.join(","));

        let result = filter_builds_list(&json).expect("should parse");
        assert!(result.truncated);
        assert!(result.text.contains("... +5 more builds"));
    }

    #[test]
    fn test_filter_run_services_list() {
        let json = r#"[
            {
                "metadata": {
                    "name": "my-svc",
                    "labels": {"cloud.googleapis.com/location": "us-central1"},
                    "creationTimestamp": "2025-06-01T12:00:00Z"
                },
                "status": {"url": "https://my-svc-xyz.a.run.app"}
            }
        ]"#;

        let result = filter_run_services_list(json).expect("should parse");
        assert!(result.text.contains("my-svc"));
        assert!(result.text.contains("https://my-svc-xyz.a.run.app"));
    }

    #[test]
    fn test_filter_run_services_describe() {
        let json = r#"{
            "metadata": {"name": "api-svc"},
            "status": {
                "url": "https://api-svc.a.run.app",
                "conditions": [
                    {"type": "Ready", "status": "True"}
                ]
            }
        }"#;

        let result = filter_run_services_describe(json).expect("should parse");
        assert!(result.text.contains("name: api-svc"));
        assert!(result.text.contains("url: https://api-svc.a.run.app"));
        assert!(result.text.contains("ready: True"));
    }

    #[test]
    fn test_filter_invalid_json_returns_none() {
        assert!(filter_compute_instances_list("not json").is_none());
        assert!(filter_compute_instances_describe("{invalid").is_none());
        assert!(filter_container_clusters_list("").is_none());
        assert!(filter_builds_list("null").is_none());
    }

    #[test]
    fn test_has_action() {
        let args = vec!["list".to_string(), "--project".to_string()];
        assert!(has_action(&args, "list"));
        assert!(!has_action(&args, "describe"));
        assert!(!has_action(&[], "list"));
    }

    #[test]
    fn test_skip_action() {
        let args = vec![
            "list".to_string(),
            "--project".to_string(),
            "my-proj".to_string(),
        ];
        let rest = skip_action(&args, "list");
        assert_eq!(rest, vec!["--project", "my-proj"]);
    }

    #[test]
    fn test_json_str_field_present() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"name": "test", "status": "ok"}"#).unwrap();
        assert_eq!(json_str_field(&v, "name"), "test");
        assert_eq!(json_str_field(&v, "missing"), "-");
    }
}
