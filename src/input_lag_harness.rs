//! Headless input-lag benchmark.
//!
//! The TUI is single threaded: a keystroke is handled and the whole frame is
//! redrawn on the same thread that polls for terminal events. When the agent
//! loop is running, that redraw also has to re-render the streaming
//! transcript, so the keystroke latency is dominated by the frame, not by the
//! input box.
//!
//! This harness measures that latency directly, without a terminal: it builds
//! a synthetic transcript, drives `App::handle_keys` + `App::render` through
//! ratatui's `TestBackend`, and reports the distribution of the time between
//! "a character was typed" and "the frame carrying it was drawn".
//!
//! Two phases are measured:
//!
//! * `idle`     - the agent loop is not running; the transcript is static.
//! * `streaming`- the agent loop is running; the streaming assistant message
//!                grows with new prose *and* with tool call/result rows, the
//!                way a real agent turn does.
//!
//! Tunables (environment):
//!   CRABCODE_LAG_TRANSCRIPT    number of transcript messages (default 60)
//!   CRABCODE_LAG_ITERATIONS    measured iterations per phase (default 60)
//!   CRABCODE_LAG_STREAM_BYTES  bytes of assistant prose per iteration
//!                               (default 600)
//!   CRABCODE_LAG_TOOL_PARTS    tool call/result rows in the streaming message
//!                               (default 0)
//!   CRABCODE_LAG_TOOL_BYTES    output size of every tool result (default 0)
//!   CRABCODE_LAG_PACE_MS       delay between iterations, mimicking the real
//!                               event loop cadence (default 40 = 25fps)
//!   CRABCODE_LAG_WIDTH         terminal width (default 120)
//!   CRABCODE_LAG_HEIGHT        terminal height (default 40)

use std::time::Instant;

use crate::app::{App, BaseFocus};
use crate::session::types::{Message, MessagePart};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use serde_json::json;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

/// A synthetic assistant reply with the shape real replies have: prose,
/// headings, a fenced code block and a list.
fn synthetic_reply(index: usize) -> String {
    format!(
        "## Step {index}\n\
         \n\
         I inspected the module and found that the render path rebuilds the\n\
         transcript on every frame. The fix is to keep the cached lines and\n\
         only rebuild the dirty tail.\n\
         \n\
         ```rust\n\
         pub fn ensure_render_cache(&mut self, max_width: usize) {{\n\
         \x20   if self.cache_valid() {{\n\
         \x20       return;\n\
         \x20   }}\n\
         \x20   self.rebuild_tail(self.render_dirty_from);\n\
         }}\n\
         ```\n\
         \n\
         - the cache is keyed by revision and width\n\
         - invalidation is incremental\n\
         - the tail rebuild is bounded by the dirty index\n"
    )
}

/// A tool row the way the agent loop produces it: a call plus its result.
fn tool_parts(index: usize, output_bytes: usize) -> Vec<MessagePart> {
    let id = format!("call_{index}");
    let call = MessagePart::tool_call(
        id.clone(),
        "read",
        json!({ "filePath": "src/ui/components/chat.rs" }),
    );
    let result = MessagePart::tool_result(json!({
        "id": id,
        "name": "read",
        "status": "ok",
        "output_preview": "x".repeat(output_bytes),
        "metadata": { "exitCode": 0 },
    }));
    vec![call, result]
}

struct Stats {
    label: String,
    samples_us: Vec<u128>,
    key_us: Vec<u128>,
    frame_us: Vec<u128>,
}

impl Stats {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            samples_us: Vec::new(),
            key_us: Vec::new(),
            frame_us: Vec::new(),
        }
    }

    fn report(&mut self) {
        fn percentile(values: &mut [u128], p: f64) -> u128 {
            if values.is_empty() {
                return 0;
            }
            values.sort_unstable();
            let idx = ((values.len() - 1) as f64 * p).round() as usize;
            values[idx.min(values.len() - 1)]
        }

        let total = percentile(&mut self.samples_us.clone(), 0.5);
        let p95 = percentile(&mut self.samples_us.clone(), 0.95);
        let max = percentile(&mut self.samples_us.clone(), 1.0);
        let key = percentile(&mut self.key_us.clone(), 0.5);
        let frame = percentile(&mut self.frame_us.clone(), 0.5);
        println!(
            "{:<12} n={:<4} total median={:>8.2}ms p95={:>8.2}ms max={:>8.2}ms  \
             (key median={:>7.3}ms, frame median={:>7.3}ms)",
            self.label,
            self.samples_us.len(),
            total as f64 / 1000.0,
            p95 as f64 / 1000.0,
            max as f64 / 1000.0,
            key as f64 / 1000.0,
            frame as f64 / 1000.0,
        );
    }
}

fn type_char(app: &mut App, terminal: &mut Terminal<TestBackend>, stats: &mut Stats) {
    let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    let started = Instant::now();
    app.handle_keys(key);
    let after_key = started.elapsed();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let total = started.elapsed();
    stats.key_us.push(after_key.as_micros());
    stats.frame_us.push(total.saturating_sub(after_key).as_micros());
    stats.samples_us.push(total.as_micros());
}

/// Run the benchmark. Returns a non-zero exit code when a phase regressed
/// past its budget, so the harness can be used as a regression gate.
pub fn run(app: &mut App) -> i32 {
    let transcript = env_usize("CRABCODE_LAG_TRANSCRIPT", 60);
    let iterations = env_usize("CRABCODE_LAG_ITERATIONS", 60);
    let stream_bytes = env_usize("CRABCODE_LAG_STREAM_BYTES", 600);
    let tool_part_count = env_usize("CRABCODE_LAG_TOOL_PARTS", 0);
    let tool_bytes = env_usize("CRABCODE_LAG_TOOL_BYTES", 0);
    let pace = std::time::Duration::from_millis(env_usize("CRABCODE_LAG_PACE_MS", 40) as u64);
    let width = env_usize("CRABCODE_LAG_WIDTH", 120) as u16;
    let height = env_usize("CRABCODE_LAG_HEIGHT", 40) as u16;

    app.base_focus = BaseFocus::Chat;

    for index in 0..transcript {
        app.chat_state
            .chat
            .add_message(Message::user(format!("Please look at step {index}.")));
        app.chat_state
            .chat
            .add_message(Message::assistant(synthetic_reply(index)));
    }

    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();

    // Warm up: the first frame materialises every cache.
    for _ in 0..3 {
        terminal.draw(|frame| app.render(frame)).unwrap();
    }

    let mut idle = Stats::new("idle");
    for _ in 0..iterations {
        type_char(app, &mut terminal, &mut idle);
        std::thread::sleep(pace);
    }
    idle.report();

    // Now the agent loop runs: an incomplete assistant message streams in,
    // carrying the tool rows the loop produced so far.
    let mut streaming_message = Message::incomplete(String::new());
    for index in 0..tool_part_count {
        streaming_message.parts.extend(tool_parts(index, tool_bytes));
    }
    app.chat_state.chat.add_message(streaming_message);
    app.chat_state.chat.begin_streaming_turn();
    app.is_streaming = true;
    terminal.draw(|frame| app.render(frame)).unwrap();

    // Real agent prose: short lines, paragraphs, a fenced code block.
    let chunk = if stream_bytes == 0 {
        String::new()
    } else {
        // A realistic stream delta: a few words at a time.
        let mut chunk = String::new();
        while chunk.len() < stream_bytes {
            chunk.push_str("the render path is the bottleneck. ");
        }
        chunk.truncate(stream_bytes);
        chunk
    };
    let mut streaming = Stats::new("streaming");
    let tool_every = if tool_part_count > 0 { 4 } else { usize::MAX };
    for iteration in 0..iterations {
        // The agent produces prose; it lands in the last text part.
        app.chat_state.chat.append_to_last_assistant(&chunk);
        // A paragraph ends every few tokens, the way a model writes.
        if iteration % 10 == 9 {
            app.chat_state.chat.append_to_last_assistant("\n\n");
        }
        // ... and every few steps it also runs a tool, adding a row.
        if iteration % tool_every == tool_every - 1 {
            let last = app.chat_state.chat.messages.len().saturating_sub(1);
            if let Some(message) = app.chat_state.chat.messages.last_mut() {
                message
                    .parts
                    .extend(tool_parts(iteration / tool_every, tool_bytes));
            }
            app.chat_state.chat.mark_render_dirty_from(last);
        }
        type_char(app, &mut terminal, &mut streaming);
        std::thread::sleep(pace);
    }
    streaming.report();

    // Attribute the streaming cost: how long does a full re-render of the
    // streamed content take, and how long does the table scan take?
    {
        let content = app
            .chat_state
            .chat
            .messages
            .last()
            .map(|message| message.content.clone())
            .unwrap_or_default();
        let colors = crate::theme::Theme::load_builtin_default().get_colors(true);
        let started = Instant::now();
        let lines = crate::ui::markdown::streaming::render_markdown(&content, 118, &colors);
        let render_us = started.elapsed().as_micros();
        let started = Instant::now();
        let has_table = crate::ui::markdown::table::contains_markdown_table(&content);
        let table_us = started.elapsed().as_micros();
        let profile = crate::ui::markdown::streaming::profile_markdown(&content, 118, &colors);
        println!(
            "profile     content={} bytes lines={} render_full={:.2}ms table_scan={:.2}ms has_table={}",
            content.len(),
            lines.len(),
            render_us as f64 / 1000.0,
            table_us as f64 / 1000.0,
            has_table
        );
        println!(
            "profile     parse={:.2}ms convert={:.2}ms join={:.2}ms wrap={:.2}ms lines={}",
            profile.parse_us as f64 / 1000.0,
            profile.convert_us as f64 / 1000.0,
            profile.join_us as f64 / 1000.0,
            profile.wrap_us as f64 / 1000.0,
            profile.lines,
        );
    }

    // Budget: a keystroke must be drawn well inside one 60Hz frame, and the
    // streaming phase must not cost much more than the idle phase.
    const IDLE_BUDGET_US: u128 = 16_000;
    const STREAMING_BUDGET_US: u128 = 60_000;

    let idle_median = {
        let mut values = idle.samples_us.clone();
        values.sort_unstable();
        values[values.len() / 2]
    };
    let streaming_median = {
        let mut values = streaming.samples_us.clone();
        values.sort_unstable();
        values[values.len() / 2]
    };

    let mut failed = false;
    if idle_median > IDLE_BUDGET_US {
        println!(
            "FAIL: idle keystroke median {:.2}ms exceeds {:.2}ms budget",
            idle_median as f64 / 1000.0,
            IDLE_BUDGET_US as f64 / 1000.0
        );
        failed = true;
    }
    if streaming_median > STREAMING_BUDGET_US {
        println!(
            "FAIL: streaming keystroke median {:.2}ms exceeds {:.2}ms budget",
            streaming_median as f64 / 1000.0,
            STREAMING_BUDGET_US as f64 / 1000.0
        );
        failed = true;
    }
    if failed {
        1
    } else {
        0
    }
}
