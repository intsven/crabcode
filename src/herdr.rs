//! Report agent state to herdr when running inside a herdr pane.
//!
//! Herdr injects `HERDR_PANE_ID`, `HERDR_SOCKET_PATH` and `HERDR_ENV` into every
//! process it spawns in a pane. When those are present we talk to herdr's socket
//! API so the pane shows up in the Agents panel with a live `idle` / `working` /
//! `blocked` state, carries a resume command, and drops out of the panel when
//! crabcode exits. See herdr's "Add Herdr support to your agent" docs.
//!
//! Transport: herdr's socket is a Unix socket on unix and a byte-mode named pipe
//! on Windows, addressed as `\\.\pipe\{HERDR_SOCKET_PATH}`. The value of
//! `HERDR_SOCKET_PATH` is passed through verbatim in both cases.
//!
//! Everything happens on a dedicated background thread. Reporting is called from
//! the TUI's state-transition path (`SessionManager::set_session_status`), so it
//! must never block on a socket connect, and a wedged herdr server must never
//! stall the UI or delay exit. The thread coalesces bursts (a turn start can
//! produce several transitions back to back) so only the newest state is sent.

use crate::session::types::SessionStatus;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SOURCE: &str = "crabcode";
const AGENT: &str = "crabcode";

/// Command name used for the resume argv. Must be a bare name on the user's
/// `PATH` (herdr rejects an absolute path with `invalid_resume_argv`) and must
/// not carry an extension or a directory separator.
const RESUME_COMMAND: &str = "crabcode";

/// herdr rejects resume argv that contains an apostrophe, a control character, or
/// control characters/arguments past its limits.
const MAX_RESUME_ARGS: usize = 64;
const MAX_RESUME_BYTES: usize = 8 * 1024;

/// Upper bound on how long the reporter thread may spend trying to reach herdr
/// before it gives up on the current report. Kept short because `Session::drop`
/// joins the thread and must not hold up process exit.
const CONNECT_BUDGET: Duration = Duration::from_millis(400);
const RETRY_DELAY: Duration = Duration::from_millis(40);
const IO_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Clone)]
struct HerdrEnv {
    pane_id: String,
    socket_path: String,
}

/// Identity of the crabcode conversation currently occupying the pane.
#[derive(Clone, PartialEq, Eq)]
struct SessionInfo {
    id: String,
    resume_argv: Vec<String>,
}

enum Command {
    State {
        state: &'static str,
        message: Option<String>,
        session: Option<SessionInfo>,
    },
    Release,
}

static ENV: OnceLock<Option<HerdrEnv>> = OnceLock::new();
static REPORTER: OnceLock<Option<Reporter>> = OnceLock::new();
/// Last state actually reported, paired with the session it described.
static LAST_STATE: OnceLock<Mutex<Option<(&'static str, String)>>> = OnceLock::new();
static NEXT_BLOCK_MESSAGE: OnceLock<Mutex<Option<&'static str>>> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);

struct Reporter {
    tx: Sender<Command>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

fn env() -> Option<&'static HerdrEnv> {
    ENV.get_or_init(|| {
        // herdr documents `HERDR_ENV=1` as the gate. Requiring it keeps a stray
        // pane id from being mistaken for a live pane.
        if std::env::var("HERDR_ENV").ok()? != "1" {
            return None;
        }
        let pane_id = std::env::var("HERDR_PANE_ID").ok()?;
        let socket_path = std::env::var("HERDR_SOCKET_PATH").ok()?;
        if pane_id.is_empty() || socket_path.is_empty() {
            return None;
        }
        Some(HerdrEnv {
            pane_id,
            socket_path,
        })
    })
    .as_ref()
}

/// Whether crabcode is running inside a herdr pane.
pub fn is_active() -> bool {
    env().is_some()
}

/// Map crabcode session status -> herdr agent state.
///
/// `Waiting` covers permission prompts, questions and terminal-session requests,
/// so it maps to `blocked`: herdr treats that as "needs a decision".
fn herdr_state(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Streaming => "working",
        SessionStatus::Waiting => "blocked",
        SessionStatus::Idle | SessionStatus::Failed | SessionStatus::Interrupted => "idle",
    }
}

/// Hint shown next to a `blocked` pane, set just before the transition that
/// blocks. Best-effort: it is a label for herdr's sidebar, not state.
pub fn set_block_message(message: &'static str) {
    if !is_active() {
        return;
    }
    if let Ok(mut slot) = NEXT_BLOCK_MESSAGE.get_or_init(|| Mutex::new(None)).lock() {
        *slot = Some(message);
    }
}

fn take_block_message() -> Option<String> {
    let slot = NEXT_BLOCK_MESSAGE.get_or_init(|| Mutex::new(None));
    let taken = slot.lock().ok().and_then(|mut guard| guard.take());
    taken.map(str::to_string)
}

/// Build the command herdr runs to reopen a session after a server restart.
///
/// Returns `None` when argv would be rejected, which makes herdr keep the
/// report but drop the resume command rather than fail the whole report.
fn build_resume_argv(session_id: &str) -> Option<Vec<String>> {
    let argv = vec![
        RESUME_COMMAND.to_string(),
        "--session".to_string(),
        session_id.to_string(),
    ];

    if argv.len() > MAX_RESUME_ARGS {
        return None;
    }
    if argv.iter().any(|arg| {
        arg.contains('\'')
            || arg.chars().any(|ch| ch.is_control() || ch == '\u{7f}')
            || arg.is_empty()
    }) {
        return None;
    }
    if argv.iter().map(|arg| arg.len() + 1).sum::<usize>() > MAX_RESUME_BYTES {
        return None;
    }
    Some(argv)
}

/// Report the current session status to herdr (no-op outside herdr).
pub fn report_session_status(session_id: &str, status: SessionStatus) {
    let state = herdr_state(status);
    if !is_active() {
        return;
    }

    // Consume before the dedup check: a suppressed report must not leave a stale
    // hint behind to mislabel some later block.
    let message = if state == "blocked" {
        take_block_message()
    } else {
        None
    };

    if !mark_reported(state, session_id) {
        return;
    }

    let session = Some(SessionInfo {
        id: session_id.to_string(),
        resume_argv: build_resume_argv(session_id).unwrap_or_default(),
    });

    enqueue(Command::State {
        state,
        message,
        session,
    });
}

/// Record this (state, session) as reported, returning whether it is new enough
/// to send.
/// transition that changes nothing -- never touches the channel at all. The
/// session id belongs in the key: switching between two idle sessions leaves the
/// state untouched, but herdr must still learn the new id and resume command or
/// the pane would reopen the previous conversation.
fn mark_reported(state: &'static str, session_id: &str) -> bool {
    let key = (state, session_id.to_string());
    let last = LAST_STATE.get_or_init(|| Mutex::new(None));
    match last.lock() {
        Ok(mut guard) => {
            if guard.as_ref() == Some(&key) {
                return false;
            }
            *guard = Some(key);
            true
        }
        // A poisoned lock must not silence reporting for the rest of the session.
        Err(_) => true,
    }
}

/// Report idle on startup so the pane is classified as crabcode immediately.
fn report_startup() {
    let last = LAST_STATE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = last.lock() {
        // App construction runs before this guard is created and already reports
        // the session it opened. A session-less `idle` sent now would carry a
        // higher seq and strip the session id herdr is holding, so only classify
        // the pane when nothing has been reported yet.
        if guard.is_some() {
            return;
        }
        *guard = Some(("idle", String::new()));
    }
    enqueue(Command::State {
        state: "idle",
        message: None,
        session: None,
    });
}

/// Drop crabcode from herdr's agents panel. Custom (non-registry) agents are not
/// auto-cleared on process exit, so callers must release explicitly.
pub fn report_shutdown() {
    if !is_active() {
        return;
    }
    if let Ok(mut guard) = LAST_STATE.get_or_init(|| Mutex::new(None)).lock() {
        *guard = None;
    }
    enqueue(Command::Release);
}

fn enqueue(command: Command) {
    if let Some(reporter) = REPORTER.get_or_init(start_reporter).as_ref() {
        let _ = reporter.tx.send(command);
    }
}

fn start_reporter() -> Option<Reporter> {
    let env = env()?.clone();
    let (tx, rx) = channel::<Command>();
    let handle = std::thread::Builder::new()
        .name("herdr-reporter".to_string())
        .spawn(move || reporter_loop(env, rx))
        .ok()?;
    Some(Reporter {
        tx,
        handle: Mutex::new(Some(handle)),
    })
}

/// Collapse a burst of reports into the one worth sending. `first` is the
/// command already taken off the queue.
///
/// herdr treats the newest accepted report as the truth, so a superseded state
/// inside the same burst is pure waste.
///
/// `Release` is terminal and wins outright: `None` means "release the pane", and
/// any state queued before or after it is dropped. Sending a state after a
/// release would re-register an agent herdr is about to forget.
fn drain(rx: &Receiver<Command>, first: Command) -> Option<Command> {
    let mut latest = match first {
        Command::Release => return None,
        command => Some(command),
    };

    loop {
        match rx.try_recv() {
            Ok(Command::Release) => return None,
            Ok(next) => latest = Some(next),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return latest,
        }
    }
}

fn reporter_loop(env: HerdrEnv, rx: Receiver<Command>) {
    // Remembers the session herdr currently holds for this pane, so a session
    // switch is reported even when the state itself has not moved.
    let mut reported_session: Option<String> = None;

    loop {
        // Block for work, then fold in everything already queued: herdr keeps
        // the newest accepted report, so superseded states need not go out.
        let first = match rx.recv() {
            Ok(command) => command,
            // Every sender is gone and nothing is queued: nothing left to report.
            Err(_) => return,
        };

        // `None` means a release is pending: it is terminal, so drop the states
        // queued with it and let the pane go.
        let (state, message, session) = match drain(&rx, first) {
            Some(Command::State {
                state,
                message,
                session,
            }) => (state, message, session),
            Some(Command::Release) | None => {
                send_release(&env);
                return;
            }
        };

        let session_changed = session
            .as_ref()
            .is_some_and(|info| reported_session.as_deref() != Some(info.id.as_str()));

        send_state(&env, state, message.as_deref(), session.as_ref());

        if session_changed {
            if let Some(info) = session.as_ref() {
                if !info.resume_argv.is_empty() {
                    send_session(&env, info);
                }
                reported_session = Some(info.id.clone());
            }
        }
    }
}

fn next_seq() -> u64 {
    // A millisecond timestamp keeps the sequence increasing across restarts of
    // crabcode itself; the counter breaks ties within one millisecond.
    let from_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    from_time.saturating_add(n)
}

fn send_state(env: &HerdrEnv, state: &str, message: Option<&str>, session: Option<&SessionInfo>) {
    let seq = next_seq();
    let mut params = serde_json::json!({
        "pane_id": env.pane_id,
        "source": SOURCE,
        "agent": AGENT,
        "state": state,
        "seq": seq,
    });
    if let Some(message) = message {
        params["message"] = serde_json::Value::String(message.to_string());
    }
    if let Some(info) = session {
        params["agent_session_id"] = serde_json::Value::String(info.id.clone());
        if !info.resume_argv.is_empty() {
            params["resume_argv"] = serde_json::to_value(&info.resume_argv).unwrap_or_default();
        }
    }

    let _ = send_rpc(
        &env.socket_path,
        &serde_json::json!({
            "id": format!("{SOURCE}:{seq}"),
            "method": "pane.report_agent",
            "params": params,
        }),
    );
}

fn send_session(env: &HerdrEnv, info: &SessionInfo) {
    let seq = next_seq();
    let _ = send_rpc(
        &env.socket_path,
        &serde_json::json!({
            "id": format!("{SOURCE}:{seq}"),
            "method": "pane.report_agent_session",
            "params": {
                "pane_id": env.pane_id,
                "source": SOURCE,
                "agent": AGENT,
                "seq": seq,
                "agent_session_id": info.id,
                "resume_argv": info.resume_argv,
            },
        }),
    );
}

fn send_release(env: &HerdrEnv) {
    let seq = next_seq();
    let _ = send_rpc(
        &env.socket_path,
        &serde_json::json!({
            "id": format!("{SOURCE}:{seq}"),
            "method": "pane.release_agent",
            "params": {
                "pane_id": env.pane_id,
                "source": SOURCE,
                "agent": AGENT,
                "seq": seq,
            },
        }),
    );
}

fn send_rpc(socket_path: &str, payload: &serde_json::Value) -> std::io::Result<()> {
    let mut body = serde_json::to_vec(payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    body.push(b'\n');

    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        stream.write_all(&body)?;
        stream.flush()?;

        // Drain one response line so herdr does not see a reset mid-write.
        let mut buf = [0u8; 512];
        let _ = stream.read(&mut buf);
        Ok(())
    }

    #[cfg(windows)]
    {
        send_rpc_windows(socket_path, &body)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (socket_path, body);
        Ok(())
    }
}

#[cfg(windows)]
/// Send one newline-delimited JSON request to herdr's named pipe.
///
/// herdr answers every request, but we deliberately do not read the reply: a
/// blocking read on a byte-mode pipe has no portable timeout in `std`, and the
/// report is fire-and-forget. A peer that reads one line and closes is what
/// herdr's own bundled integrations do on Windows.
fn send_rpc_windows(socket_path: &str, body: &[u8]) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::Instant;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;

    let pipe_path = format!(r"\\.\pipe\{socket_path}");
    let deadline = Instant::now() + CONNECT_BUDGET;

    let mut stream = loop {
        let attempt = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&pipe_path);

        match attempt {
            Ok(stream) => break stream,
            Err(err) => {
                // Every pipe instance busy: wait for one to free up, but never
                // past the budget or exit stalls with it.
                let retryable = matches!(
                    err.raw_os_error(),
                    Some(231) |   // ERROR_PIPE_BUSY
                    Some(32) // ERROR_SHARING_VIOLATION
                );
                if !retryable || Instant::now() >= deadline {
                    return Err(err);
                }
                std::thread::sleep(RETRY_DELAY);
            }
        }
    };

    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

/// RAII guard: reports startup on create, releases on drop (including panic
/// unwind), and joins the reporter thread so the release reaches herdr.
pub struct Session {
    active: bool,
}

impl Session {
    pub fn start() -> Self {
        let active = is_active();
        if active {
            start_reporter();
            report_startup();
        }
        Self { active }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        report_shutdown();
        if let Some(Some(reporter)) = REPORTER.get() {
            let handle = reporter
                .handle
                .lock()
                .ok()
                .and_then(|mut guard| guard.take());
            if let Some(handle) = handle {
                // The thread returns as soon as it drains the release; the join
                // only waits out an in-flight connect, which is bounded.
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_session_status_to_herdr_state() {
        assert_eq!(herdr_state(SessionStatus::Streaming), "working");
        assert_eq!(herdr_state(SessionStatus::Waiting), "blocked");
        assert_eq!(herdr_state(SessionStatus::Idle), "idle");
        assert_eq!(herdr_state(SessionStatus::Failed), "idle");
        assert_eq!(herdr_state(SessionStatus::Interrupted), "idle");
    }

    #[test]
    fn inactive_without_env() {
        // Tests run outside herdr; env should be unset.
        assert!(!is_active() || env().is_some());
    }

    #[test]
    fn resume_argv_is_bare_command_plus_session() {
        let argv = build_resume_argv("abc123").expect("resume argv");
        assert_eq!(argv, vec!["crabcode", "--session", "abc123"]);
        // herdr spawns argv[0] through a shell-free PATH lookup.
        assert!(!argv[0].contains('/'));
        assert!(!argv[0].contains('\\'));
    }

    #[test]
    fn resume_argv_rejects_apostrophes_and_control_chars() {
        // herdr answers invalid_resume_argv and drops the whole report, so
        // these must never reach the wire.
        assert!(build_resume_argv("ses_bad'quote").is_none());
        assert!(build_resume_argv("ses_with\nnewline").is_none());
        assert!(build_resume_argv("ses_with\ttab").is_none());
    }

    #[test]
    fn coalescing_keeps_only_the_newest_state() {
        let (tx, rx) = channel();
        let first = Command::State {
            state: "working",
            message: None,
            session: None,
        };
        tx.send(Command::State {
            state: "blocked",
            message: None,
            session: None,
        })
        .unwrap();
        tx.send(Command::State {
            state: "idle",
            message: None,
            session: None,
        })
        .unwrap();

        match drain(&rx, first) {
            Some(Command::State { state, .. }) => assert_eq!(state, "idle"),
            _ => panic!("expected the newest state"),
        }
    }

    #[test]
    fn release_wins_over_queued_states() {
        let (tx, rx) = channel();
        let first = Command::State {
            state: "working",
            message: None,
            session: None,
        };
        tx.send(Command::Release).unwrap();
        // A state queued *after* the release must not resurrect the agent.
        tx.send(Command::State {
            state: "idle",
            message: None,
            session: None,
        })
        .unwrap();

        // A release must be terminal, so nothing queued alongside it may be
        // sent first -- herdr would re-register the agent we are releasing.
        assert!(drain(&rx, first).is_none());
    }

    #[test]
    fn dedup_key_includes_the_session() {
        // Regression: deduping on state alone meant switching between two idle
        // sessions reported nothing, so herdr kept the first session's id and
        // resume command forever.
        let slot = Mutex::new(None);

        // Mirrors mark_reported's contract against a local slot, so the test
        // does not race the process-wide LAST_STATE used by other tests.
        fn mark(slot: &Mutex<Option<(&'static str, String)>>, state: &'static str, id: &str) -> bool {
            let key = (state, id.to_string());
            let mut guard = slot.lock().unwrap();
            if guard.as_ref() == Some(&key) {
                return false;
            }
            *guard = Some(key);
            true
        }

        assert!(mark(&slot, "idle", "session_a"));
        // Same state, same session: suppressed, so we do not spam herdr.
        assert!(!mark(&slot, "idle", "session_a"));
        // Same state, different session: a switch must always go out.
        assert!(mark(&slot, "idle", "session_b"));
        // And back again.
        assert!(mark(&slot, "idle", "session_a"));
    }

    #[test]
    fn release_already_dequeued_is_still_terminal() {
        let (tx, rx) = channel();
        tx.send(Command::State {
            state: "working",
            message: None,
            session: None,
        })
        .unwrap();

        // The release was the first command taken, with a state behind it.
        assert!(drain(&rx, Command::Release).is_none());
    }
}
