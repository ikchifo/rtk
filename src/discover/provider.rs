//! Reads coding-agent session logs from disk and streams their command history.

// Rust guideline compliant 2026-02-21

use crate::hooks::init::resolve_claude_dir;
use anyhow::{Context, Result};
use clap::ValueEnum;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

/// Number of seconds in a 24-hour day for session age filtering.
const SECONDS_PER_DAY: u64 = 86_400;

/// Maximum number of characters retained from a tool result for error analysis.
const OUTPUT_PREVIEW_CHARS: usize = 1_000;

/// A command extracted from a session file.
#[derive(Debug)]
pub struct ExtractedCommand {
    pub command: String,
    pub output_len: Option<usize>,
    #[allow(dead_code)]
    pub session_id: String,
    /// Actual output content (first ~1000 chars for error detection)
    pub output_content: Option<String>,
    /// Whether the tool_result indicated an error
    pub is_error: bool,
    /// Chronological sequence index within the session
    #[allow(dead_code)]
    pub sequence_index: usize,
    /// Whether reports must omit arguments from this command.
    pub hide_arguments_in_reports: bool,
}

/// Trait for session providers (Claude Code, OpenCode, etc.).
///
/// Note: Cursor Agent transcripts use a text-only format without structured
/// tool_use/tool_result blocks, so command extraction is not possible.
/// Use `rtk gain` to track savings for Cursor sessions instead.
pub trait SessionProvider {
    fn discover_sessions(
        &self,
        project_filter: Option<&str>,
        since_days: Option<u64>,
    ) -> Result<Vec<PathBuf>>;
    fn extract_commands(&self, path: &Path) -> Result<Vec<ExtractedCommand>>;

    /// Returns the provider-specific default session scope.
    fn default_project_filter(&self) -> Result<Option<String>> {
        Ok(None)
    }

    /// Returns the user-facing name for this session source.
    fn display_name(&self) -> &'static str;
}

pub struct ClaudeProvider;

impl ClaudeProvider {
    /// Get the base directory for Claude Code projects.
    fn projects_dir() -> Result<PathBuf> {
        let claude_dir = resolve_claude_dir().context("could not determine claude directory")?;
        Ok(claude_dir.join("projects"))
    }

    fn discover_sessions_in_projects_dir(
        projects_dir: &Path,
        project_filter: Option<&str>,
        since_days: Option<u64>,
    ) -> Result<Vec<PathBuf>> {
        if !projects_dir
            .try_exists()
            .with_context(|| format!("failed to access {}", projects_dir.display()))?
        {
            return Ok(Vec::new());
        }

        let cutoff = since_days.map(|days| {
            SystemTime::now()
                .checked_sub(Duration::from_secs(days.saturating_mul(SECONDS_PER_DAY)))
                .unwrap_or(SystemTime::UNIX_EPOCH)
        });

        let mut sessions = Vec::new();

        // List project directories
        let entries = fs::read_dir(projects_dir)
            .with_context(|| format!("failed to read {}", projects_dir.display()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            // Apply project filter: substring match on directory name
            if let Some(filter) = project_filter {
                let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !dir_name.contains(filter) {
                    continue;
                }
            }

            // Walk the project directory recursively (catches subagents/)
            for walk_entry in WalkDir::new(&path)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let file_path = walk_entry.path();
                if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }

                // Apply mtime filter
                if let Some(cutoff_time) = cutoff {
                    if let Ok(meta) = fs::metadata(file_path) {
                        if let Ok(mtime) = meta.modified() {
                            if mtime < cutoff_time {
                                continue;
                            }
                        }
                    }
                }

                sessions.push(file_path.to_path_buf());
            }
        }

        Ok(sessions)
    }

    /// Encode a filesystem path to Claude Code's directory name format.
    ///
    /// Claude Code replaces `/`, `.`, `_`, `\`, and any non-ASCII character
    /// with `-` when computing the project directory slug under `~/.claude/projects/`.
    ///
    /// `/Users/foo/bar`          → `-Users-foo-bar`
    /// `/Users/first.last/bar`   → `-Users-first-last-bar`
    /// `/home/chris/2_project`   → `-home-chris-2-project`
    /// `C:\Users\foo\bar`        → `C:-Users-foo-bar`
    pub fn encode_project_path(path: &str) -> String {
        const SANITIZED_CHARS: &[char] = &['/', '.', '_', '\\', ' ', '[', ']'];

        path.chars()
            .map(|c| {
                if !c.is_ascii() || SANITIZED_CHARS.contains(&c) {
                    '-'
                } else {
                    c
                }
            })
            .collect()
    }
}

impl SessionProvider for ClaudeProvider {
    fn discover_sessions(
        &self,
        project_filter: Option<&str>,
        since_days: Option<u64>,
    ) -> Result<Vec<PathBuf>> {
        let projects_dir = Self::projects_dir()?;
        Self::discover_sessions_in_projects_dir(&projects_dir, project_filter, since_days)
    }

    fn extract_commands(&self, path: &Path) -> Result<Vec<ExtractedCommand>> {
        let file =
            fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let reader = BufReader::new(file);

        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        // First pass: collect all tool_use Bash commands with their IDs and sequence
        // Second pass (same loop): collect tool_result output lengths, content, and error status
        let mut pending_tool_uses: Vec<(String, String, usize)> = Vec::new(); // (tool_use_id, command, sequence)
        let mut tool_results: HashMap<String, (usize, String, bool)> = HashMap::new(); // (len, content, is_error)
        let mut commands = Vec::new();
        let mut sequence_counter = 0;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };

            // Pre-filter: skip lines that can't contain Bash tool_use or tool_result
            if !line.contains("\"Bash\"") && !line.contains("\"tool_result\"") {
                continue;
            }

            let entry: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let entry_type = entry.get("type").and_then(|t| t.as_str()).unwrap_or("");

            match entry_type {
                "assistant" => {
                    // Look for tool_use Bash blocks in message.content
                    if let Some(content) =
                        entry.pointer("/message/content").and_then(|c| c.as_array())
                    {
                        for block in content {
                            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                                && block.get("name").and_then(|n| n.as_str()) == Some("Bash")
                            {
                                if let (Some(id), Some(cmd)) = (
                                    block.get("id").and_then(|i| i.as_str()),
                                    block.pointer("/input/command").and_then(|c| c.as_str()),
                                ) {
                                    pending_tool_uses.push((
                                        id.to_string(),
                                        cmd.to_string(),
                                        sequence_counter,
                                    ));
                                    sequence_counter += 1;
                                }
                            }
                        }
                    }
                }
                "user" => {
                    // Look for tool_result blocks
                    if let Some(content) =
                        entry.pointer("/message/content").and_then(|c| c.as_array())
                    {
                        for block in content {
                            if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                                if let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str())
                                {
                                    // Get content, length, and error status
                                    let content =
                                        block.get("content").and_then(|c| c.as_str()).unwrap_or("");

                                    let output_len = content.len();
                                    let is_error = block
                                        .get("is_error")
                                        .and_then(|e| e.as_bool())
                                        .unwrap_or(false);

                                    // Store first ~1000 chars of content for error detection
                                    let content_preview: String =
                                        content.chars().take(1000).collect();

                                    tool_results.insert(
                                        id.to_string(),
                                        (output_len, content_preview, is_error),
                                    );
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Match tool_uses with their results
        for (tool_id, command, sequence_index) in pending_tool_uses {
            let (output_len, output_content, is_error) = tool_results
                .get(&tool_id)
                .map(|(len, content, err)| (Some(*len), Some(content.clone()), *err))
                .unwrap_or((None, None, false));

            commands.push(ExtractedCommand {
                command,
                output_len,
                session_id: session_id.clone(),
                output_content,
                is_error,
                sequence_index,
                hide_arguments_in_reports: false,
            });
        }

        Ok(commands)
    }

    fn default_project_filter(&self) -> Result<Option<String>> {
        let cwd = env::current_dir().context("could not determine current directory")?;
        let cwd = cwd.to_string_lossy();
        Ok(Some(Self::encode_project_path(&cwd)))
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }
}

/// Reads Codex session transcripts from disk.
#[derive(Clone, Copy, Debug, Default)]
pub struct CodexProvider;

/// Commands extracted from one Codex session.
#[derive(Debug)]
pub struct CodexExtraction {
    pub commands: Vec<ExtractedCommand>,
}

struct CodexCommandCall {
    /// The outer tool call whose output belongs to this command, when that
    /// attribution is unambiguous.
    output_call_id: Option<String>,
    command: String,
    sequence_index: usize,
    session_id: Option<String>,
}

struct CodexContinuation {
    call_id: String,
    session_id: String,
}

/// Shell commands recovered from one custom Codex `exec` script.
struct CustomExecCommands {
    commands: Vec<String>,
    output_is_attributable: bool,
}

#[derive(Clone)]
struct ToolOutput {
    output_len: usize,
    output_content: String,
    is_error: bool,
    session_id: Option<String>,
}

impl ToolOutput {
    fn from_event(event: &serde_json::Value) -> Self {
        let output_content = output_text(event.get("output"));
        Self {
            output_len: output_content.len(),
            output_content: output_preview(&output_content),
            is_error: event
                .get("is_error")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
                || matches!(
                    event.get("status").and_then(|value| value.as_str()),
                    Some("failed" | "error")
                ),
            session_id: structured_session_id(event),
        }
    }

    fn append(&mut self, continuation: &Self) {
        self.output_len = self.output_len.saturating_add(continuation.output_len);
        append_preview(&mut self.output_content, &continuation.output_content);
        self.is_error |= continuation.is_error;
    }
}

impl CodexProvider {
    /// Returns Codex's configured sessions directory.
    fn sessions_dir() -> Result<PathBuf> {
        let home = dirs::home_dir().context("could not determine home directory")?;
        let fallback = home.join(".codex").join("sessions");

        if let Some(codex_home) = env::var_os("CODEX_HOME") {
            let configured = PathBuf::from(codex_home).join("sessions");
            if configured
                .try_exists()
                .with_context(|| format!("failed to access {}", configured.display()))?
            {
                return Ok(configured);
            }
        }

        Ok(fallback)
    }

    fn discover_sessions_in_sessions_dir(
        sessions_dir: &Path,
        project_filter: Option<&str>,
        since_days: Option<u64>,
    ) -> Result<Vec<PathBuf>> {
        if !sessions_dir
            .try_exists()
            .with_context(|| format!("failed to access {}", sessions_dir.display()))?
        {
            return Ok(Vec::new());
        }

        let cutoff = since_days.map(|days| {
            SystemTime::now()
                .checked_sub(Duration::from_secs(days.saturating_mul(SECONDS_PER_DAY)))
                .unwrap_or(SystemTime::UNIX_EPOCH)
        });
        let mut sessions = Vec::new();

        for entry in WalkDir::new(sessions_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }

            if let Some(cutoff_time) = cutoff {
                if let Ok(metadata) = fs::metadata(path) {
                    if let Ok(modified) = metadata.modified() {
                        if modified < cutoff_time {
                            continue;
                        }
                    }
                }
            }

            if let Some(project_filter) = project_filter {
                if !Self::session_matches_project(path, project_filter) {
                    continue;
                }
            }

            sessions.push(path.to_path_buf());
        }

        Ok(sessions)
    }

    /// Matches the project filter against the session metadata without reading
    /// the full transcript. Codex stores the working directory in its initial
    /// `session_meta` event.
    fn session_matches_project(path: &Path, project_filter: &str) -> bool {
        let Ok(file) = fs::File::open(path) else {
            return false;
        };
        let reader = BufReader::new(file);

        for line in reader.lines().take(16).flatten() {
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if entry.get("type").and_then(|value| value.as_str()) != Some("session_meta") {
                continue;
            }

            let cwd = entry
                .get("payload")
                .and_then(|payload| payload.get("cwd"))
                .or_else(|| entry.get("cwd"))
                .and_then(|value| value.as_str());
            return cwd.is_some_and(|cwd| cwd.contains(project_filter));
        }

        false
    }

    /// Extracts commands and safely attributable continuation output from a session.
    pub fn extract_session(&self, path: &Path) -> Result<CodexExtraction> {
        let file =
            fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let reader = BufReader::new(file);
        let session_id = session_id_from_path(path);

        let mut command_calls = Vec::new();
        let mut continuations = Vec::new();
        let mut outputs = HashMap::new();
        let mut sequence_index = 0;

        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(_) => continue,
            };
            let entry: serde_json::Value = match serde_json::from_str(&line) {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let Some(event) = codex_event(&entry) else {
                continue;
            };
            let Some(event_type) = event.get("type").and_then(|value| value.as_str()) else {
                continue;
            };

            match event_type {
                "function_call" => {
                    let name = event.get("name").and_then(|value| value.as_str());
                    match name {
                        Some("exec_command" | "shell_command") => {
                            if let Some(command) = command_from_arguments(event.get("arguments")) {
                                if let Some(call_id) = call_id(event) {
                                    command_calls.push(CodexCommandCall {
                                        output_call_id: Some(call_id),
                                        command,
                                        sequence_index,
                                        session_id: structured_session_id(event),
                                    });
                                    sequence_index += 1;
                                }
                            }
                        }
                        Some("write_stdin") => {
                            if let Some(continuation) =
                                continuation_from_arguments(event, event.get("arguments"))
                            {
                                continuations.push(continuation);
                            }
                        }
                        _ => {}
                    }
                }
                "custom_tool_call" => {
                    let name = event.get("name").and_then(|value| value.as_str());
                    match name {
                        Some("exec") => {
                            let extracted = custom_exec_commands(event.get("input"));
                            let output_call_id = extracted
                                .output_is_attributable
                                .then(|| call_id(event))
                                .flatten();
                            for command in extracted.commands {
                                command_calls.push(CodexCommandCall {
                                    output_call_id: output_call_id.clone(),
                                    command,
                                    sequence_index,
                                    session_id: structured_session_id(event),
                                });
                                sequence_index += 1;
                            }
                        }
                        Some("write_stdin") => {
                            if let Some(continuation) =
                                continuation_from_arguments(event, event.get("input"))
                            {
                                continuations.push(continuation);
                            }
                        }
                        _ => {}
                    }
                }
                "function_call_output" | "custom_tool_call_output" => {
                    if let Some(call_id) = call_id(event) {
                        outputs.insert(call_id, ToolOutput::from_event(event));
                    }
                }
                _ => {}
            }
        }

        let mut command_outputs: Vec<Option<ToolOutput>> = command_calls
            .iter()
            .map(|call| {
                call.output_call_id
                    .as_ref()
                    .and_then(|call_id| outputs.get(call_id).cloned())
            })
            .collect();
        let mut command_by_session = HashMap::new();

        for (index, call) in command_calls.iter().enumerate() {
            let session_id = call.session_id.as_ref().or_else(|| {
                command_outputs[index]
                    .as_ref()
                    .and_then(|output| output.session_id.as_ref())
            });
            let Some(session_id) = session_id else {
                continue;
            };

            if let Some(existing) = command_by_session.get_mut(session_id) {
                *existing = None;
            } else {
                command_by_session.insert(session_id.clone(), Some(index));
            }
        }

        for continuation in continuations {
            let Some(output) = outputs.get(&continuation.call_id) else {
                continue;
            };
            if let Some(command_index) = command_by_session
                .get(&continuation.session_id)
                .copied()
                .flatten()
            {
                if let Some(command_output) = &mut command_outputs[command_index] {
                    command_output.append(output);
                } else {
                    command_outputs[command_index] = Some(output.clone());
                }
            }
        }

        let commands = command_calls
            .into_iter()
            .zip(command_outputs)
            .map(|(call, output)| {
                let (output_len, output_content, is_error) = output
                    .map(|output| {
                        (
                            Some(output.output_len),
                            Some(output.output_content),
                            output.is_error,
                        )
                    })
                    .unwrap_or((None, None, false));

                ExtractedCommand {
                    command: call.command,
                    output_len,
                    session_id: session_id.clone(),
                    output_content,
                    is_error,
                    sequence_index: call.sequence_index,
                    hide_arguments_in_reports: true,
                }
            })
            .collect();

        Ok(CodexExtraction { commands })
    }
}

impl SessionProvider for CodexProvider {
    fn discover_sessions(
        &self,
        project_filter: Option<&str>,
        since_days: Option<u64>,
    ) -> Result<Vec<PathBuf>> {
        let sessions_dir = Self::sessions_dir()?;
        Self::discover_sessions_in_sessions_dir(&sessions_dir, project_filter, since_days)
    }

    fn extract_commands(&self, path: &Path) -> Result<Vec<ExtractedCommand>> {
        Ok(self.extract_session(path)?.commands)
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn default_project_filter(&self) -> Result<Option<String>> {
        let cwd = env::current_dir().context("could not determine current directory")?;
        Ok(Some(cwd.to_string_lossy().into_owned()))
    }
}

/// Selects the transcript format used by discovery and session analytics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum TranscriptProvider {
    /// Claude Code JSONL transcripts.
    #[default]
    Claude,
    /// Codex JSONL transcripts.
    Codex,
}

impl SessionProvider for TranscriptProvider {
    fn discover_sessions(
        &self,
        project_filter: Option<&str>,
        since_days: Option<u64>,
    ) -> Result<Vec<PathBuf>> {
        match self {
            Self::Claude => ClaudeProvider.discover_sessions(project_filter, since_days),
            Self::Codex => CodexProvider.discover_sessions(project_filter, since_days),
        }
    }

    fn extract_commands(&self, path: &Path) -> Result<Vec<ExtractedCommand>> {
        match self {
            Self::Claude => ClaudeProvider.extract_commands(path),
            Self::Codex => CodexProvider.extract_commands(path),
        }
    }

    fn default_project_filter(&self) -> Result<Option<String>> {
        match self {
            Self::Claude => ClaudeProvider.default_project_filter(),
            Self::Codex => CodexProvider.default_project_filter(),
        }
    }

    fn display_name(&self) -> &'static str {
        match self {
            Self::Claude => ClaudeProvider.display_name(),
            Self::Codex => CodexProvider.display_name(),
        }
    }
}

fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn codex_event(entry: &serde_json::Value) -> Option<&serde_json::Value> {
    if entry.get("type").and_then(|value| value.as_str()) == Some("response_item") {
        entry.get("payload")
    } else {
        Some(entry)
    }
}

fn call_id(event: &serde_json::Value) -> Option<String> {
    event
        .get("call_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn command_from_arguments(arguments: Option<&serde_json::Value>) -> Option<String> {
    let arguments = parse_arguments(arguments)?;
    command_from_value(&arguments)
}

fn custom_exec_commands(input: Option<&serde_json::Value>) -> CustomExecCommands {
    let Some(source) = input
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|source| !source.is_empty())
    else {
        return CustomExecCommands {
            commands: Vec::new(),
            output_is_attributable: false,
        };
    };

    let commands = extract_embedded_exec_commands(source);
    if !commands.is_empty() {
        // A custom exec script can launch several shell commands. Its one
        // aggregate result cannot be divided safely, so only attach it when
        // the script has one nested tool call and one shell command.
        let output_is_attributable = commands.len() == 1 && source.matches("tools.").count() == 1;
        return CustomExecCommands {
            commands,
            output_is_attributable,
        };
    }

    if looks_like_javascript(source) {
        return CustomExecCommands {
            commands: Vec::new(),
            output_is_attributable: false,
        };
    }

    CustomExecCommands {
        commands: vec![source.to_string()],
        output_is_attributable: true,
    }
}

/// Parses JSON argument objects from nested `tools.exec_command` calls in the
/// JavaScript source recorded by Codex's custom `exec` tool.
fn extract_embedded_exec_commands(source: &str) -> Vec<String> {
    const EXEC_COMMAND: &str = "tools.exec_command";

    let mut commands = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = source[cursor..].find(EXEC_COMMAND) {
        let after_name = cursor + relative_start + EXEC_COMMAND.len();
        let bytes = source.as_bytes();
        let mut argument_start = after_name;
        while bytes
            .get(argument_start)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            argument_start += 1;
        }
        if bytes.get(argument_start) != Some(&b'(') {
            cursor = after_name;
            continue;
        }

        argument_start += 1;
        while bytes
            .get(argument_start)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            argument_start += 1;
        }
        let arguments = &source[argument_start..];
        let mut values =
            serde_json::Deserializer::from_str(arguments).into_iter::<serde_json::Value>();
        let value = match values.next() {
            Some(Ok(value)) => value,
            _ => {
                cursor = after_name;
                continue;
            }
        };
        let consumed = values.byte_offset();
        cursor = argument_start.saturating_add(consumed.max(1));

        if let Some(command) = command_from_value(&value) {
            commands.push(command);
        }
    }

    if commands.is_empty() && has_shorthand_exec_command(source) {
        commands = extract_declared_exec_commands(source);
    }

    commands
}

fn has_shorthand_exec_command(source: &str) -> bool {
    const EXEC_COMMAND: &str = "tools.exec_command";

    let mut cursor = 0;
    while let Some(relative_start) = source[cursor..].find(EXEC_COMMAND) {
        let after_name = cursor + relative_start + EXEC_COMMAND.len();
        let mut arguments = source[after_name..].trim_start();
        let Some(after_open_paren) = arguments.strip_prefix('(') else {
            cursor = after_name;
            continue;
        };
        arguments = after_open_paren.trim_start();
        let Some(after_open_brace) = arguments.strip_prefix('{') else {
            cursor = after_name;
            continue;
        };
        let arguments = after_open_brace.trim_start();
        let Some(after_cmd) = arguments.strip_prefix("cmd") else {
            cursor = after_name;
            continue;
        };
        if after_cmd
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b',' | b'}' | b':'))
        {
            return true;
        }

        cursor = after_name;
    }

    false
}

fn extract_declared_exec_commands(source: &str) -> Vec<String> {
    let mut commands = Vec::new();

    if source.contains("cmds.map") {
        commands.extend(commands_from_declaration(source, "cmds"));
    }
    if source.contains("commands.map") {
        commands.extend(commands_from_declaration(source, "commands"));
    }
    if commands.is_empty() {
        commands.extend(commands_from_declaration(source, "cmd"));
    }

    commands
}

fn commands_from_declaration(source: &str, name: &str) -> Vec<String> {
    let declaration = format!("const {name}");
    let mut commands = Vec::new();
    let mut cursor = 0;

    while let Some(relative_start) = source[cursor..].find(&declaration) {
        let after_name = cursor + relative_start + declaration.len();
        let mut value_start = after_name;
        let bytes = source.as_bytes();
        while bytes
            .get(value_start)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            value_start += 1;
        }
        if bytes.get(value_start) != Some(&b'=') {
            cursor = after_name;
            continue;
        }

        value_start += 1;
        while bytes
            .get(value_start)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            value_start += 1;
        }

        let mut values = serde_json::Deserializer::from_str(&source[value_start..])
            .into_iter::<serde_json::Value>();
        let value = match values.next() {
            Some(Ok(value)) => value,
            _ => {
                cursor = after_name;
                continue;
            }
        };
        cursor = value_start.saturating_add(values.byte_offset().max(1));
        commands.extend(command_strings_from_declared_value(&value));
    }

    commands
}

fn command_strings_from_declared_value(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(command) if !command.trim().is_empty() => {
            vec![command.to_string()]
        }
        serde_json::Value::Array(values) => values
            .iter()
            .filter_map(|value| match value {
                serde_json::Value::String(command) if !command.trim().is_empty() => {
                    Some(command.to_string())
                }
                serde_json::Value::Array(parts) => parts
                    .first()
                    .and_then(|value| value.as_str())
                    .filter(|command| !command.trim().is_empty())
                    .map(str::to_string),
                serde_json::Value::Object(_) => command_from_value(value),
                _ => None,
            })
            .collect(),
        serde_json::Value::Object(_) => command_from_value(value).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn looks_like_javascript(source: &str) -> bool {
    let source = source.trim_start();
    source.starts_with("const ")
        || source.starts_with("let ")
        || source.starts_with("await ")
        || source.starts_with("tools.")
}

fn continuation_from_arguments(
    event: &serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> Option<CodexContinuation> {
    let call_id = call_id(event)?;
    let arguments = parse_arguments(arguments)?;
    let session_id = arguments.get("session_id").and_then(identifier)?;
    Some(CodexContinuation {
        call_id,
        session_id,
    })
}

fn parse_arguments(arguments: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let arguments = arguments?;
    match arguments {
        serde_json::Value::String(value) => serde_json::from_str(value).ok(),
        serde_json::Value::Object(_) => Some(arguments.clone()),
        _ => None,
    }
}

fn command_from_value(arguments: &serde_json::Value) -> Option<String> {
    arguments
        .get("cmd")
        .or_else(|| arguments.get("command"))
        .and_then(|value| value.as_str())
        .filter(|command| !command.trim().is_empty())
        .map(str::to_string)
}

fn structured_session_id(event: &serde_json::Value) -> Option<String> {
    // Do not parse terminal output: it is command-controlled and not trustworthy.
    event.get("session_id").and_then(identifier).or_else(|| {
        event
            .get("output")
            .and_then(|output| output.get("session_id"))
            .and_then(identifier)
    })
}

fn identifier(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) if !value.is_empty() => Some(value.clone()),
        serde_json::Value::Number(value) => value.as_u64().map(|value| value.to_string()),
        _ => None,
    }
}

fn output_text(output: Option<&serde_json::Value>) -> String {
    let mut text = String::new();
    if let Some(output) = output {
        append_output_text(&mut text, output);
    }
    text
}

fn append_output_text(text: &mut String, output: &serde_json::Value) {
    match output {
        serde_json::Value::String(value) => text.push_str(value),
        serde_json::Value::Array(blocks) => {
            for block in blocks {
                append_output_text(text, block);
            }
        }
        serde_json::Value::Object(block) => {
            if let Some(value) = block.get("text") {
                append_output_text(text, value);
            } else if let Some(value) = block.get("content") {
                append_output_text(text, value);
            }
        }
        _ => {}
    }
}

fn output_preview(output: &str) -> String {
    output.chars().take(OUTPUT_PREVIEW_CHARS).collect()
}

fn append_preview(preview: &mut String, additional: &str) {
    let remaining = OUTPUT_PREVIEW_CHARS.saturating_sub(preview.chars().count());
    preview.extend(additional.chars().take(remaining));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        f.flush().unwrap();
        f
    }

    #[test]
    fn test_extract_assistant_bash() {
        let jsonl = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_abc","name":"Bash","input":{"command":"git status"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"On branch master\nnothing to commit"}]}}"#,
        ]);

        let provider = ClaudeProvider;
        let cmds = provider.extract_commands(jsonl.path()).unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].command, "git status");
        assert!(cmds[0].output_len.is_some());
        assert_eq!(
            cmds[0].output_len.unwrap(),
            "On branch master\nnothing to commit".len()
        );
    }

    #[test]
    fn test_extract_non_bash_ignored() {
        let jsonl = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_abc","name":"Read","input":{"file_path":"/tmp/foo"}}]}}"#,
        ]);

        let provider = ClaudeProvider;
        let cmds = provider.extract_commands(jsonl.path()).unwrap();
        assert_eq!(cmds.len(), 0);
    }

    #[test]
    fn test_extract_non_message_ignored() {
        let jsonl =
            make_jsonl(&[r#"{"type":"file-history-snapshot","messageId":"abc","snapshot":{}}"#]);

        let provider = ClaudeProvider;
        let cmds = provider.extract_commands(jsonl.path()).unwrap();
        assert_eq!(cmds.len(), 0);
    }

    #[test]
    fn test_extract_multiple_tools() {
        let jsonl = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"git status"}},{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"git diff"}}]}}"#,
        ]);

        let provider = ClaudeProvider;
        let cmds = provider.extract_commands(jsonl.path()).unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].command, "git status");
        assert_eq!(cmds[1].command, "git diff");
    }

    #[test]
    fn test_extract_malformed_line() {
        let jsonl = make_jsonl(&[
            "this is not json at all",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_ok","name":"Bash","input":{"command":"ls"}}]}}"#,
        ]);

        let provider = ClaudeProvider;
        let cmds = provider.extract_commands(jsonl.path()).unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].command, "ls");
    }

    #[test]
    fn test_encode_project_path() {
        assert_eq!(
            ClaudeProvider::encode_project_path("/Users/foo/bar"),
            "-Users-foo-bar"
        );
    }

    #[test]
    fn test_encode_project_path_trailing_slash() {
        assert_eq!(
            ClaudeProvider::encode_project_path("/Users/foo/bar/"),
            "-Users-foo-bar-"
        );
    }

    #[test]
    fn test_encode_project_path_dot_in_username() {
        // Claude Code replaces both '/' and '.' with '-'.
        // A cwd like /Users/first.last must produce the same slug as
        // Claude's projects directory (-Users-first-last), otherwise
        // `rtk discover` finds zero sessions for that project.
        assert_eq!(
            ClaudeProvider::encode_project_path("/Users/first.last/my-project"),
            "-Users-first-last-my-project"
        );
    }

    #[test]
    fn test_encode_project_path_multiple_dots() {
        assert_eq!(
            ClaudeProvider::encode_project_path("/Users/a.b.c/proj"),
            "-Users-a-b-c-proj"
        );
    }

    #[test]
    fn test_encode_project_path_underscore() {
        // Claude Code also replaces '_' with '-' (https://github.com/anthropics/claude-code/issues/24067)
        assert_eq!(
            ClaudeProvider::encode_project_path("/home/chris/2_project-files/proj"),
            "-home-chris-2-project-files-proj"
        );
    }

    #[test]
    fn test_encode_project_path_non_ascii() {
        // Non-ASCII characters are each replaced with '-' (https://github.com/anthropics/claude-code/issues/40946)
        // '/home/user/' + '外' + '主' + '/app' -> '-home-user' + '-' + '-' + '-' + '-' + 'app'
        assert_eq!(
            ClaudeProvider::encode_project_path("/home/user/\u{5916}\u{4e3b}/app"),
            "-home-user----app"
        );
    }

    #[test]
    fn test_encode_project_path_windows() {
        // Windows backslashes are also replaced with '-'
        assert_eq!(
            ClaudeProvider::encode_project_path(r"C:\Users\foo\bar"),
            "C:-Users-foo-bar"
        );
    }

    #[test]
    fn test_match_project_filter() {
        let encoded = ClaudeProvider::encode_project_path("/Users/foo/Sites/rtk");
        assert!(encoded.contains("rtk"));
        assert!(encoded.contains("Sites"));
    }

    #[test]
    fn test_encode_path_with_spaces() {
        // Even if run on Unix, encoding should replace backslashes to match Claude's behavior
        assert_eq!(
            ClaudeProvider::encode_project_path(
                r"/home/user/projects/[QZX-7K42] - Análise Genérica de Exemplo"
            ),
            "-home-user-projects--QZX-7K42----An-lise-Gen-rica-de-Exemplo"
        );
    }

    #[test]
    fn test_discover_sessions_missing_projects_dir_returns_empty() {
        let temp_home = tempfile::tempdir().unwrap();
        let missing_projects_dir = temp_home
            .path()
            .join(crate::hooks::constants::CLAUDE_DIR)
            .join("projects");

        let sessions = ClaudeProvider::discover_sessions_in_projects_dir(
            &missing_projects_dir,
            None,
            Some(30),
        )
        .unwrap();

        assert!(sessions.is_empty());
    }

    #[test]
    fn test_discover_sessions_applies_project_filter() {
        let projects_dir = tempfile::tempdir().unwrap();
        let matching_project = projects_dir.path().join("-Users-test-rtk");
        let other_project = projects_dir.path().join("-Users-test-other");
        std::fs::create_dir_all(&matching_project).unwrap();
        std::fs::create_dir_all(&other_project).unwrap();
        std::fs::write(matching_project.join("matching.jsonl"), "").unwrap();
        std::fs::write(other_project.join("other.jsonl"), "").unwrap();

        let sessions = ClaudeProvider::discover_sessions_in_projects_dir(
            projects_dir.path(),
            Some("rtk"),
            None,
        )
        .unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].file_name().and_then(|name| name.to_str()),
            Some("matching.jsonl")
        );
    }

    #[test]
    fn test_discover_sessions_existing_non_directory_returns_error() {
        let projects_file = tempfile::NamedTempFile::new().unwrap();

        let err =
            ClaudeProvider::discover_sessions_in_projects_dir(projects_file.path(), None, None)
                .unwrap_err();

        assert!(err.to_string().contains("failed to read"));
    }

    #[test]
    fn test_extract_output_content() {
        let jsonl = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_abc","name":"Bash","input":{"command":"git commit --ammend"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"error: unexpected argument '--ammend'","is_error":true}]}}"#,
        ]);

        let provider = ClaudeProvider;
        let cmds = provider.extract_commands(jsonl.path()).unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].command, "git commit --ammend");
        assert!(cmds[0].is_error);
        assert!(cmds[0].output_content.is_some());
        assert_eq!(
            cmds[0].output_content.as_ref().unwrap(),
            "error: unexpected argument '--ammend'"
        );
    }

    #[test]
    fn test_extract_is_error_flag() {
        let jsonl = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}},{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"invalid_cmd"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"file1.txt","is_error":false},{"type":"tool_result","tool_use_id":"toolu_2","content":"command not found","is_error":true}]}}"#,
        ]);

        let provider = ClaudeProvider;
        let cmds = provider.extract_commands(jsonl.path()).unwrap();
        assert_eq!(cmds.len(), 2);
        assert!(!cmds[0].is_error);
        assert!(cmds[1].is_error);
    }

    #[test]
    fn test_extract_sequence_ordering() {
        let jsonl = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"first"}},{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"second"}},{"type":"tool_use","id":"toolu_3","name":"Bash","input":{"command":"third"}}]}}"#,
        ]);

        let provider = ClaudeProvider;
        let cmds = provider.extract_commands(jsonl.path()).unwrap();
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0].sequence_index, 0);
        assert_eq!(cmds[1].sequence_index, 1);
        assert_eq!(cmds[2].sequence_index, 2);
        assert_eq!(cmds[0].command, "first");
        assert_eq!(cmds[1].command, "second");
        assert_eq!(cmds[2].command, "third");
    }

    #[test]
    fn codex_extracts_historical_exec_command_and_output() {
        let jsonl = make_jsonl(&[
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call-1","arguments":"{\"cmd\":\"rtk git status\"}"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"clean"}}"#,
        ]);

        let commands = CodexProvider.extract_commands(jsonl.path()).unwrap();

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "rtk git status");
        assert_eq!(commands[0].output_len, Some("clean".len()));
        assert!(commands[0].hide_arguments_in_reports);
    }

    #[test]
    fn codex_extracts_historical_shell_command_without_result() {
        let jsonl = make_jsonl(&[
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell_command","call_id":"call-2","arguments":"{\"command\":\"git log -1\"}"}}"#,
        ]);

        let commands = CodexProvider.extract_commands(jsonl.path()).unwrap();

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "git log -1");
        assert_eq!(commands[0].output_len, None);
    }

    #[test]
    fn codex_extracts_custom_exec_text_blocks() {
        let jsonl = make_jsonl(&[
            r#"{"type":"custom_tool_call","name":"exec","call_id":"call-3","input":"rtk git diff"}"#,
            r#"{"type":"custom_tool_call_output","call_id":"call-3","output":[{"type":"input_text","text":"one"},{"type":"input_text","text":"two"}]}"#,
        ]);

        let commands = CodexProvider.extract_commands(jsonl.path()).unwrap();

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "rtk git diff");
        assert_eq!(commands[0].output_len, Some("onetwo".len()));
    }

    #[test]
    fn codex_extracts_shell_command_embedded_in_custom_exec_script() {
        let jsonl = make_jsonl(&[
            r#"{"type":"custom_tool_call","name":"exec","call_id":"call-js","input":"const r = await tools.exec_command({\"cmd\":\"git status\"});\ntext(r.output);"}"#,
            r#"{"type":"custom_tool_call_output","call_id":"call-js","output":"clean"}"#,
        ]);

        let commands = CodexProvider.extract_commands(jsonl.path()).unwrap();

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "git status");
        assert_eq!(commands[0].output_len, Some("clean".len()));
    }

    #[test]
    fn codex_extracts_multiple_embedded_shell_commands_without_duplicate_output() {
        let jsonl = make_jsonl(&[
            r#"{"type":"custom_tool_call","name":"exec","call_id":"call-js-many","input":"const results = await Promise.all([tools.exec_command({\"cmd\":\"git status\"}), tools.exec_command({\"cmd\":\"git diff\"})]);"}"#,
            r#"{"type":"custom_tool_call_output","call_id":"call-js-many","output":"aggregate output"}"#,
        ]);

        let commands = CodexProvider.extract_commands(jsonl.path()).unwrap();

        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].command, "git status");
        assert_eq!(commands[1].command, "git diff");
        assert_eq!(commands[0].output_len, None);
        assert_eq!(commands[1].output_len, None);
    }

    #[test]
    fn codex_extracts_commands_from_common_cmds_array_wrapper() {
        let jsonl = make_jsonl(&[
            r#"{"type":"custom_tool_call","name":"exec","call_id":"call-js-cmds","input":"const cmds = [[\"git status\", \"/work/repo\"], [\"git diff --stat\", \"/work/repo\"]];\nconst results = await Promise.all(cmds.map(([cmd, workdir]) => tools.exec_command({cmd, workdir})));"}"#,
            r#"{"type":"custom_tool_call_output","call_id":"call-js-cmds","output":"aggregate output"}"#,
        ]);

        let commands = CodexProvider.extract_commands(jsonl.path()).unwrap();

        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].command, "git status");
        assert_eq!(commands[1].command, "git diff --stat");
        assert_eq!(commands[0].output_len, None);
        assert_eq!(commands[1].output_len, None);
    }

    #[test]
    fn codex_ignores_custom_exec_scripts_without_shell_commands() {
        let jsonl = make_jsonl(&[
            r#"{"type":"custom_tool_call","name":"exec","call_id":"call-js-web","input":"const r = await tools.web__run({\"search_query\":[]});"}"#,
            r#"{"type":"custom_tool_call_output","call_id":"call-js-web","output":"web result"}"#,
        ]);

        let commands = CodexProvider.extract_commands(jsonl.path()).unwrap();

        assert!(commands.is_empty());
    }

    #[test]
    fn codex_ignores_malformed_and_unrelated_records() {
        let jsonl = make_jsonl(&[
            "not json",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call-4","arguments":"not json"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"read_file","call_id":"call-5","arguments":"{\"path\":\"notes.txt\"}"}}"#,
            r#"{"type":"custom_tool_call","name":"exec","call_id":"call-6","input":42}"#,
            r#"{"type":"custom_tool_call","name":"exec","call_id":"call-7","input":"git status"}"#,
        ]);

        let commands = CodexProvider.extract_commands(jsonl.path()).unwrap();

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "git status");
    }

    #[test]
    fn codex_does_not_create_commands_from_output_only_records() {
        let jsonl = make_jsonl(&[
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call-7","output":"orphan"}}"#,
            r#"{"type":"custom_tool_call_output","call_id":"call-8","output":[{"type":"input_text","text":"later"}]}"#,
        ]);

        let extraction = CodexProvider.extract_session(jsonl.path()).unwrap();

        assert!(extraction.commands.is_empty());
    }

    #[test]
    fn codex_appends_trustworthy_continuation_output() {
        let jsonl = make_jsonl(&[
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call-9","arguments":"{\"cmd\":\"rtk git status\"}"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call-9","output":{"session_id":"session-1","text":"start"}}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"write_stdin","call_id":"call-10","arguments":"{\"session_id\":\"session-1\",\"chars\":\"\"}"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call-10","output":"end"}}"#,
        ]);

        let extraction = CodexProvider.extract_session(jsonl.path()).unwrap();

        assert_eq!(extraction.commands.len(), 1);
        assert_eq!(extraction.commands[0].output_len, Some("startend".len()));
    }

    #[test]
    fn codex_ignores_unattributable_continuation_output() {
        let jsonl = make_jsonl(&[
            r#"{"type":"response_item","payload":{"type":"function_call","name":"write_stdin","call_id":"call-11","arguments":"{\"session_id\":17,\"chars\":\"\"}"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call-11","output":"more"}}"#,
        ]);

        let extraction = CodexProvider.extract_session(jsonl.path()).unwrap();

        assert!(extraction.commands.is_empty());
    }

    #[test]
    fn codex_discovers_nested_session_files() {
        let sessions_dir = tempfile::tempdir().unwrap();
        let nested_dir = sessions_dir.path().join("2026/06/22");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(nested_dir.join("session.jsonl"), "").unwrap();
        std::fs::write(nested_dir.join("ignored.txt"), "").unwrap();

        let sessions =
            CodexProvider::discover_sessions_in_sessions_dir(sessions_dir.path(), None, None)
                .unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].file_name().and_then(|name| name.to_str()),
            Some("session.jsonl")
        );
    }

    #[test]
    fn codex_filters_sessions_by_metadata_cwd() {
        let sessions_dir = tempfile::tempdir().unwrap();
        let nested_dir = sessions_dir.path().join("2026/06/22");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(
            nested_dir.join("matching.jsonl"),
            r#"{"type":"session_meta","payload":{"cwd":"/work/rtk"}}"#,
        )
        .unwrap();
        std::fs::write(
            nested_dir.join("other.jsonl"),
            r#"{"type":"session_meta","payload":{"cwd":"/work/other"}}"#,
        )
        .unwrap();

        let sessions = CodexProvider::discover_sessions_in_sessions_dir(
            sessions_dir.path(),
            Some("rtk"),
            None,
        )
        .unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].file_name().and_then(|name| name.to_str()),
            Some("matching.jsonl")
        );
    }

    #[test]
    fn codex_default_project_filter_uses_current_working_directory() {
        let current_dir = std::env::current_dir().unwrap();
        let filter = CodexProvider.default_project_filter().unwrap().unwrap();

        assert_eq!(PathBuf::from(filter), current_dir);
    }
}
