//! Import opencode session transcripts into crabcode's local history.
//!
//! crabcode deliberately reads opencode's JSON interchange format (what
//! `opencode export` emits) instead of its SQLite database. The export payload
//! is a documented, versioned contract; `opencode.db` has already shipped
//! several lossy schema migrations (see anomalyco/opencode#13636, #13654).
//!
//! Typical use:
//!
//! ```text
//! opencode export ses_abc123 > session.json
//! crabcode import session.json
//! ```
//!
//! The imported session keeps opencode's identifier, so `crabcode import
//! session.json --force` is idempotent and re-running it replaces the row
//! instead of duplicating it.

use anyhow::{bail, Context, Result};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::persistence::{HistoryDAO, Message, MessagePart};

/// opencode stores wall-clock times in milliseconds; crabcode uses seconds.
fn ms_to_secs(value: i64) -> i64 {
    value.div_euclid(1000)
}

#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Path to the export JSON, or `-` to read stdin.
    pub source: String,
    /// Override the workspace root. Defaults to the session's recorded directory.
    pub workspace: Option<String>,
    /// Report what would be imported without touching the database.
    pub dry_run: bool,
    /// Replace an existing session with the same identifier.
    pub force: bool,
}

#[derive(Debug, Default)]
struct PartStats {
    text: usize,
    reasoning: usize,
    tools: usize,
    usage: usize,
    /// Parts with no crabcode equivalent (step-start/step-finish/file/…).
    skipped: usize,
    /// Messages dropped because their role has no crabcode equivalent.
    skipped_messages: usize,
    tools_by_name: BTreeMap<String, usize>,
}

impl PartStats {}

/// A session parsed out of the export file but not yet written to disk.
#[derive(Debug)]
struct ParsedSession {
    identifier: String,
    title: String,
    directory: Option<String>,
    parent: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    messages: Vec<ParsedMessage>,
    stats: PartStats,
}

#[derive(Debug)]
struct ParsedMessage {
    id: String,
    role: String,
    timestamp: i64,
    duration_ms: i64,
    model: Option<String>,
    provider: Option<String>,
    agent_mode: Option<String>,
    tokens_used: i64,
    output_tokens: Option<i64>,
    input_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    cost: Option<f64>,
    tokens_per_sec: Option<f64>,
    parts: Vec<MessagePart>,
}

// `persistence::MessagePart` is a plain row struct; build the three part
// shapes we emit by hand rather than leaning on session-type constructors.
fn text_part(text: &str) -> MessagePart {
    MessagePart {
        part_type: "text".to_string(),
        data: serde_json::json!({ "text": text }),
    }
}

fn reasoning_part(text: &str) -> MessagePart {
    MessagePart {
        part_type: "reasoning".to_string(),
        data: serde_json::json!({ "text": text }),
    }
}

fn usage_part(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
) -> MessagePart {
    MessagePart {
        part_type: "usage".to_string(),
        data: serde_json::json!({
            "input": input,
            "output": output,
            "cache_read": cache_read,
            "cache_write": cache_write,
            "cost": cost,
        }),
    }
}

fn read_source(source: &str) -> Result<String> {
    if source == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read export JSON from stdin")?;
        return Ok(buf);
    }

    let path = Path::new(source);
    std::fs::read_to_string(path)
        .with_context(|| format!("failed to read export JSON: {}", path.display()))
}

/// Map opencode's tool state status onto crabcode's `tool_result` status.
///
/// crabcode renders a `tool_call` with no matching `tool_result` as a still
/// running spinner, so an in-flight tool at export time is recorded as an
/// error instead of leaving the transcript permanently "busy".
fn map_tool_status(opencode_status: &str) -> (&'static str, bool) {
    match opencode_status {
        "completed" => ("ok", false),
        "error" => ("error", false),
        // running / pending / anything unknown: terminal for our purposes.
        _ => ("error", true),
    }
}

fn as_u64(value: Option<&JsonValue>) -> u64 {
    value.and_then(JsonValue::as_u64).unwrap_or(0)
}

/// Extract opencode's `{input, output, cache:{read,write}}` token block.
fn read_tokens(tokens: Option<&JsonValue>) -> (u64, u64, u64, u64) {
    let Some(tokens) = tokens else {
        return (0, 0, 0, 0);
    };
    let cache = tokens.get("cache");
    (
        as_u64(tokens.get("input")),
        as_u64(tokens.get("output")),
        as_u64(cache.and_then(|c| c.get("read"))),
        as_u64(cache.and_then(|c| c.get("write"))),
    )
}

/// Convert one opencode tool part into crabcode's `tool_call` + `tool_result`
/// pair. `tool-invocation` is opencode v1's name for the same structure.
fn map_tool_part(part: &JsonValue, stats: &mut PartStats) -> Vec<MessagePart> {
    let call_id = part
        .get("callID")
        .or_else(|| part.get("callId"))
        .or_else(|| part.get("toolID"))
        .or_else(|| part.get("id"))
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    let name = part
        .get("tool")
        .and_then(JsonValue::as_str)
        .unwrap_or("tool")
        .to_string();

    if call_id.is_empty() {
        stats.skipped += 1;
        return Vec::new();
    }

    let empty = JsonValue::Object(Default::default());
    let state = part.get("state").unwrap_or(&empty);
    let args = state.get("input").cloned().unwrap_or(empty.clone());
    let opencode_status = state
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or("completed");
    let (status, was_in_flight) = map_tool_status(opencode_status);

    let mut payload = serde_json::Map::new();
    payload.insert("id".into(), JsonValue::String(call_id.clone()));
    payload.insert("name".into(), JsonValue::String(name.clone()));
    payload.insert("args".into(), args.clone());
    payload.insert("status".into(), JsonValue::String(status.into()));
    if let Some(title) = state.get("title").and_then(JsonValue::as_str) {
        payload.insert("title".into(), JsonValue::String(title.to_string()));
    }
    if let Some(metadata) = state.get("metadata") {
        if !metadata.is_null() {
            payload.insert("metadata".into(), metadata.clone());
        }
    }
    let output = state
        .get("output")
        .and_then(JsonValue::as_str)
        .or_else(|| state.get("error").and_then(JsonValue::as_str));
    let output = match (was_in_flight, output) {
        (true, Some(out)) => format!("Tool did not finish before export.\n\n{out}"),
        (true, None) => "Tool did not finish before export.".to_string(),
        (false, Some(out)) => out.to_string(),
        (false, None) => String::new(),
    };
    payload.insert("output_preview".into(), JsonValue::String(output));

    stats.tools += 1;
    *stats.tools_by_name.entry(name).or_insert(0) += 1;
    vec![
        MessagePart {
            part_type: "tool_call".into(),
            data: JsonValue::Object(payload.clone()),
        },
        MessagePart {
            part_type: "tool_result".into(),
            data: JsonValue::Object(payload),
        },
    ]
}

fn map_parts(raw: &JsonValue, stats: &mut PartStats) -> Vec<MessagePart> {
    let Some(raw_parts) = raw.as_array() else {
        return Vec::new();
    };

    let mut parts = Vec::new();
    for part in raw_parts {
        let Some(kind) = part.get("type").and_then(JsonValue::as_str) else {
            stats.skipped += 1;
            continue;
        };
        match kind {
            "text" => {
                let Some(text) = part.get("text").and_then(JsonValue::as_str) else {
                    stats.skipped += 1;
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                stats.text += 1;
                parts.push(text_part(text));
            }
            "reasoning" => {
                let Some(text) = part.get("text").and_then(JsonValue::as_str) else {
                    stats.skipped += 1;
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                stats.reasoning += 1;
                parts.push(reasoning_part(text));
            }
            "tool" | "tool-invocation" => parts.extend(map_tool_part(part, stats)),
            // Step markers are opencode-internal turn boundaries; crabcode has
            // no equivalent. `step-finish` tokens are folded into the message's
            // usage part below instead of being dropped on the floor.
            "step-start" | "step-finish" => stats.skipped += 1,
            _ => stats.skipped += 1,
        }
    }
    parts
}

fn map_message(raw: &JsonValue, index: usize, stats: &mut PartStats) -> Option<ParsedMessage> {
    let info = raw.get("info")?;
    let role = info.get("role").and_then(JsonValue::as_str)?;
    if !matches!(role, "user" | "assistant" | "system" | "tool") {
        stats.skipped_messages += 1;
        return None;
    }

    let id = info
        .get("id")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        // Fall back to a positional id so a malformed export still imports.
        .unwrap_or_else(|| format!("imported-{index}"));

    let time = info.get("time");
    let created_ms = time
        .and_then(|t| t.get("created"))
        .and_then(JsonValue::as_i64);
    let completed_ms = time
        .and_then(|t| t.get("completed"))
        .and_then(JsonValue::as_i64);
    let timestamp = created_ms.map(ms_to_secs).unwrap_or(0);
    let duration_ms = match (created_ms, completed_ms) {
        (Some(start), Some(end)) => (end - start).max(0),
        _ => 0,
    };

    // opencode keeps the model under `modelID`/`providerID` on assistant
    // messages but nests it under `model` on user messages.
    let nested = info.get("model");
    let model = info
        .get("modelID")
        .or_else(|| nested.and_then(|m| m.get("modelID")))
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let provider = info
        .get("providerID")
        .or_else(|| nested.and_then(|m| m.get("providerID")))
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let agent_mode = info
        .get("mode")
        .or_else(|| info.get("agent"))
        .and_then(JsonValue::as_str)
        .map(str::to_string);

    let mut parts = map_parts(raw.get("parts").unwrap_or(&JsonValue::Null), stats);

    // Message-level usage. `info.tokens` is already the sum across the
    // message's steps, so it replaces the per-step `step-finish` parts.
    let (input, output, cache_read, cache_write) = read_tokens(info.get("tokens"));
    let cost = info.get("cost").and_then(JsonValue::as_f64).unwrap_or(0.0);
    let tokens_used = (input + output + cache_read + cache_write).min(i32::MAX as u64) as i64;
    if tokens_used > 0 || cost > 0.0 {
        stats.usage += 1;
        // Usage belongs after the visible parts; it is metadata, not content.
        parts.push(usage_part(input, output, cache_read, cache_write, cost));
    }

    let tokens_per_sec = if output > 0 && duration_ms > 0 {
        Some(output as f64 / (duration_ms as f64 / 1000.0))
    } else {
        None
    };

    Some(ParsedMessage {
        id,
        role: role.to_string(),
        timestamp,
        duration_ms,
        model,
        provider,
        agent_mode,
        tokens_used,
        output_tokens: (output > 0).then_some(output as i64),
        input_tokens: (input > 0).then_some(input as i64),
        cache_read_tokens: (cache_read > 0).then_some(cache_read as i64),
        cache_write_tokens: (cache_write > 0).then_some(cache_write as i64),
        // opencode reports cost per message in `info.cost`.
        cost: info.get("cost").and_then(JsonValue::as_f64),
        tokens_per_sec,
        parts,
    })
}

fn parse_export(raw: &JsonValue) -> Result<ParsedSession> {
    let info = raw
        .get("info")
        .context("not an opencode export: missing top-level `info`")?;
    let messages = raw
        .get("messages")
        .and_then(JsonValue::as_array)
        .context("not an opencode export: missing top-level `messages` array")?;

    let identifier = info
        .get("id")
        .and_then(JsonValue::as_str)
        .context("export is missing `info.id`")?
        .to_string();
    let title = info
        .get("title")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Imported opencode session {identifier}"));

    let directory = info
        .get("directory")
        .or_else(|| info.get("path").and_then(|p| p.get("cwd")))
        .and_then(JsonValue::as_str)
        .filter(|dir| !dir.is_empty())
        .map(str::to_string);

    let mut stats = PartStats::default();
    let mut parsed = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        if let Some(message) = map_message(message, index, &mut stats) {
            parsed.push(message);
        }
    }

    let time = info.get("time");
    Ok(ParsedSession {
        identifier,
        title,
        directory,
        parent: info
            .get("parentID")
            .and_then(JsonValue::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
        created_at: time
            .and_then(|t| t.get("created"))
            .and_then(JsonValue::as_i64)
            .map(ms_to_secs),
        updated_at: time
            .and_then(|t| t.get("updated"))
            .and_then(JsonValue::as_i64)
            .map(ms_to_secs),
        messages: parsed,
        stats,
    })
}

/// Match crabcode's own workspace keying.
///
/// `HistoryDAO::new_for_workspace` canonicalizes its argument before storing
/// it (on Windows that yields the `\\?\D:\...` verbatim form), and
/// `ensure_workspace` compares `root_path` as an exact string. An imported
/// session's raw `info.directory` must therefore be canonicalized too, or the
/// import lands in a duplicate workspace that the TUI never shows.
fn normalize_workspace_path(path: &str) -> String {
    Path::new(path)
        .canonicalize()
        .map(|resolved| resolved.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

fn format_secs(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn run(options: ImportOptions) -> Result<()> {
    let raw = read_source(&options.source)?;
    let json: JsonValue = serde_json::from_str(&raw).context("export file is not valid JSON")?;
    let parsed = parse_export(&json)?;

    let directory = options
        .workspace
        .clone()
        .or_else(|| parsed.directory.clone())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .to_string_lossy()
                .into_owned()
        });

    println!("Session   {}", parsed.identifier);
    println!("Title     {}", parsed.title);
    println!("Workspace {}", directory);
    if let Some(parent) = parsed.parent.as_deref() {
        println!("Parent    {parent} (subagent session)");
    }
    if let Some(created) = parsed.created_at {
        let updated = parsed.updated_at.unwrap_or(created);
        println!(
            "Created   {}  →  updated {}",
            format_secs(created),
            format_secs(updated)
        );
    }
    let stats = &parsed.stats;
    println!(
        "Messages  {} ({} text, {} reasoning, {} tool calls, {} usage records)",
        parsed.messages.len(),
        stats.text,
        stats.reasoning,
        stats.tools,
        stats.usage
    );
    if !stats.tools_by_name.is_empty() {
        let names: Vec<String> = stats
            .tools_by_name
            .iter()
            .map(|(name, count)| format!("{name} x{count}"))
            .collect();
        println!("Tools     {}", names.join(", "));
    }
    if stats.skipped > 0 || stats.skipped_messages > 0 {
        println!(
            "Skipped   {} part(s) with no equivalent, {} message(s) with no equivalent",
            stats.skipped, stats.skipped_messages
        );
    }

    if options.dry_run {
        println!("\nDry run: nothing was written.");
        return Ok(());
    }

    let workspace_root = normalize_workspace_path(&directory);
    let dao = HistoryDAO::new_for_workspace(&workspace_root)
        .with_context(|| format!("failed to open crabcode history for {directory}"))?;
    let workspace = dao.ensure_workspace_path(&workspace_root)?;

    if let Some(existing) = dao.get_session_by_identifier(&parsed.identifier)? {
        if !options.force {
            bail!(
                "session {} already exists in this database (workspace {:?}).\n\
                 Re-run with --force to replace it, or pass --workspace to import elsewhere.",
                parsed.identifier,
                workspace.display_name
            );
        }
        println!("Replacing existing session (--force)");
        // Empty replace_messages clears the old rows in one transaction.
        // Needed because this connection has foreign_keys off, so the
        // ON DELETE CASCADE would not fire and stale message ids would
        // collide on re-insert.
        dao.replace_messages(existing.id, &[])?;
        dao.delete_session(existing.id)?;
    }

    let session_id = dao.create_session_with_parent_in_workspace(
        &parsed.identifier,
        parsed.title.clone(),
        parsed.parent.as_deref(),
        workspace.id,
    )?;

    let messages: Vec<Message> = parsed
        .messages
        .iter()
        .map(|message| Message {
            id: message.id.clone(),
            session_id,
            role: message.role.clone(),
            parts: message.parts.clone(),
            timestamp: message.timestamp,
            tokens_used: message.tokens_used.min(i32::MAX as i64) as i32,
            model: message.model.clone(),
            provider: message.provider.clone(),
            agent_mode: message.agent_mode.clone(),
            duration_ms: message.duration_ms,
            t0_ms: None,
            t1_ms: None,
            tn_ms: None,
            output_tokens: message.output_tokens,
            input_tokens: message.input_tokens,
            cache_read_tokens: message.cache_read_tokens,
            cache_write_tokens: message.cache_write_tokens,
            cost: message.cost,
            // opencode's own export is authoritative about usage.
            usage_authoritative: true,
            tokens_per_sec: message.tokens_per_sec,
        })
        .collect();

    dao.replace_messages(session_id, &messages)?;

    // `create_session` stamps `now()`; restore the real opencode timestamps now
    // that the token/time totals `replace_messages` derived are final.
    if let Some(created) = parsed.created_at {
        let updated = parsed
            .updated_at
            .or_else(|| parsed.messages.iter().map(|m| m.timestamp).max())
            .unwrap_or(created);
        dao.set_session_times(session_id, created, updated)?;
    }

    println!(
        "\nImported {} message(s) into workspace {:?}.",
        messages.len(),
        workspace.display_name
    );
    println!("Open it with: crabcode   (then /sessions)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part_type_list(message: &ParsedMessage) -> Vec<&str> {
        message.parts.iter().map(|p| p.part_type.as_str()).collect()
    }

    fn assistant(parts: JsonValue, tokens: JsonValue) -> JsonValue {
        serde_json::json!({
            "info": {
                "id": "msg_1",
                "role": "assistant",
                "time": { "created": 1_700_000_000_000i64, "completed": 1_700_000_002_000i64 },
                "modelID": "space-bunny-free",
                "providerID": "opencode",
                "mode": "build",
                "tokens": tokens,
                "cost": 0.25,
            },
            "parts": parts,
        })
    }

    #[test]
    fn converts_millisecond_timestamps_to_seconds() {
        let raw = serde_json::json!({
            "info": { "id": "ses_1", "title": "t" },
            "messages": [assistant(
                serde_json::json!([{ "type": "text", "text": "hi" }]),
                serde_json::json!({ "input": 1, "output": 1 }),
            )],
        });
        let parsed = parse_export(&raw).unwrap();
        assert_eq!(parsed.messages[0].timestamp, 1_700_000_000);
        assert_eq!(parsed.messages[0].duration_ms, 2_000);
    }

    #[test]
    fn maps_text_reasoning_and_drops_step_markers() {
        let raw = serde_json::json!({
            "info": { "id": "ses_1", "title": "t" },
            "messages": [assistant(
                serde_json::json!([
                    { "type": "step-start" },
                    { "type": "reasoning", "text": "thinking" },
                    { "type": "text", "text": "hello" },
                    { "type": "step-finish", "tokens": { "input": 9, "output": 9 } },
                ]),
                serde_json::json!({ "input": 1, "output": 1 }),
            )],
        });
        let parsed = parse_export(&raw).unwrap();
        let message = &parsed.messages[0];
        assert_eq!(part_type_list(message), vec!["reasoning", "text", "usage"]);
        assert_eq!(parsed.stats.skipped, 2);
        // step-finish tokens must not double-count against the message total.
        assert_eq!(message.tokens_used, 2);
    }

    #[test]
    fn maps_tool_part_to_call_and_result_pair() {
        let raw = serde_json::json!({
            "info": { "id": "ses_1", "title": "t" },
            "messages": [assistant(
                serde_json::json!([{
                    "type": "tool",
                    "tool": "bash",
                    "callID": "call_1",
                    "state": {
                        "status": "completed",
                        "input": { "command": "ls" },
                        "output": "a.txt",
                        "title": "ls",
                        "metadata": { "exit": 0 },
                    },
                }]),
                serde_json::json!({ "input": 0, "output": 0 }),
            )],
        });
        let parsed = parse_export(&raw).unwrap();
        let message = &parsed.messages[0];
        let types = part_type_list(message);
        assert_eq!(&types[..2], &["tool_call", "tool_result"]);
        // trailing "usage" comes from the fixture's cost: 0.25
        assert_eq!(types.last(), Some(&"usage"));

        let result = &message.parts[1].data;
        assert_eq!(result["id"], "call_1");
        assert_eq!(result["name"], "bash");
        assert_eq!(result["status"], "ok");
        assert_eq!(result["output_preview"], "a.txt");
        assert_eq!(result["args"]["command"], "ls");
        assert_eq!(result["metadata"]["exit"], 0);
    }

    #[test]
    fn in_flight_tool_becomes_terminal_error() {
        let raw = serde_json::json!({
            "info": { "id": "ses_1", "title": "t" },
            "messages": [assistant(
                serde_json::json!([{
                    "type": "tool",
                    "tool": "bash",
                    "callID": "call_1",
                    "state": { "status": "running", "input": {} },
                }]),
                serde_json::json!({ "input": 0, "output": 0 }),
            )],
        });
        let parsed = parse_export(&raw).unwrap();
        let message = &parsed.messages[0];
        // Both halves are emitted so the transcript is never left "busy".
        let types = part_type_list(message);
        assert_eq!(&types[..2], &["tool_call", "tool_result"]);
        // trailing "usage" comes from the fixture's cost: 0.25
        assert_eq!(types.last(), Some(&"usage"));
        assert_eq!(message.parts[1].data["status"], "error");
        assert!(message.parts[1].data["output_preview"]
            .as_str()
            .unwrap()
            .contains("did not finish"));
    }

    #[test]
    fn v1_tool_invocation_shape_is_accepted() {
        let raw = serde_json::json!({
            "info": { "id": "ses_1", "title": "t" },
            "messages": [assistant(
                serde_json::json!([{
                    "type": "tool-invocation",
                    "tool": "read",
                    "toolID": "call_9",
                    "state": { "status": "completed", "input": { "path": "a" }, "output": "b" },
                }]),
                serde_json::json!({ "input": 0, "output": 0 }),
            )],
        });
        let parsed = parse_export(&raw).unwrap();
        assert_eq!(parsed.messages[0].parts[0].data["id"], "call_9");
        assert_eq!(parsed.messages[0].parts[0].data["name"], "read");
    }

    #[test]
    fn usage_part_carries_buckets_and_cache() {
        let raw = serde_json::json!({
            "info": { "id": "ses_1", "title": "t" },
            "messages": [assistant(
                serde_json::json!([{ "type": "text", "text": "done" }]),
                serde_json::json!({
                    "input": 1000, "output": 200, "cache": { "read": 500, "write": 50 },
                }),
            )],
        });
        let parsed = parse_export(&raw).unwrap();
        let message = &parsed.messages[0];
        assert_eq!(message.tokens_used, 1_750);
        assert_eq!(message.output_tokens, Some(200));

        let usage = message.parts.last().unwrap();
        assert_eq!(usage.part_type, "usage");
        assert_eq!(usage.data["input"], 1_000);
        assert_eq!(usage.data["cache_read"], 500);
        assert_eq!(usage.data["cache_write"], 50);
        assert_eq!(usage.data["cost"], 0.25);
        // 200 output tokens over 2s.
        assert!((message.tokens_per_sec.unwrap() - 100.0).abs() < 0.001);
    }

    #[test]
    fn user_message_model_is_read_from_nested_object() {
        let raw = serde_json::json!({
            "info": { "id": "ses_1", "title": "t" },
            "messages": [{
                "info": {
                    "id": "msg_0",
                    "role": "user",
                    "time": { "created": 1_700_000_000_000i64 },
                    "agent": "build",
                    "model": { "providerID": "opencode", "modelID": "space-bunny-free" },
                },
                "parts": [{ "type": "text", "text": "hello" }],
            }],
        });
        let parsed = parse_export(&raw).unwrap();
        let message = &parsed.messages[0];
        assert_eq!(message.model.as_deref(), Some("space-bunny-free"));
        assert_eq!(message.provider.as_deref(), Some("opencode"));
        assert_eq!(message.agent_mode.as_deref(), Some("build"));
        // No tokens on a user turn: no usage part.
        assert_eq!(part_type_list(message), vec!["text"]);
    }

    #[test]
    fn session_metadata_is_read_from_info() {
        let raw = serde_json::json!({
            "info": {
                "id": "ses_abc",
                "title": "Move herdr",
                "directory": "/home/dev/projects/example",
                "parentID": "ses_parent",
                "time": { "created": 1_790_859_536_942i64, "updated": 1_790_965_559_944i64 },
            },
            "messages": [],
        });
        let parsed = parse_export(&raw).unwrap();
        assert_eq!(parsed.identifier, "ses_abc");
        assert_eq!(parsed.title, "Move herdr");
        assert_eq!(
            parsed.directory.as_deref(),
            Some("/home/dev/projects/example")
        );
        assert_eq!(parsed.parent.as_deref(), Some("ses_parent"));
        assert_eq!(parsed.created_at, Some(1_790_859_536));
        assert_eq!(parsed.updated_at, Some(1_790_965_559));
    }

    #[test]
    fn falls_back_to_cwd_when_directory_is_absent() {
        let raw = serde_json::json!({
            "info": { "id": "ses_1", "title": "t", "path": { "cwd": "C:/work" } },
            "messages": [],
        });
        let parsed = parse_export(&raw).unwrap();
        assert_eq!(parsed.directory.as_deref(), Some("C:/work"));
    }

    #[test]
    fn blank_title_falls_back_to_identifier() {
        let raw = serde_json::json!({
            "info": { "id": "ses_1", "title": "   " },
            "messages": [],
        });
        let parsed = parse_export(&raw).unwrap();
        assert_eq!(parsed.title, "Imported opencode session ses_1");
    }

    #[test]
    fn unknown_role_messages_are_skipped() {
        let raw = serde_json::json!({
            "info": { "id": "ses_1", "title": "t" },
            "messages": [{
                "info": { "id": "msg_x", "role": "reviewer" },
                "parts": [{ "type": "text", "text": "?" }],
            }],
        });
        let parsed = parse_export(&raw).unwrap();
        assert!(parsed.messages.is_empty());
        assert_eq!(parsed.stats.skipped_messages, 1);
    }

    #[test]
    fn non_export_json_is_rejected_with_context() {
        let err = parse_export(&serde_json::json!({ "nope": true })).unwrap_err();
        assert!(err.to_string().contains("not an opencode export"));
    }
}
