//! Reconstruct pasted text from a burst of individual key events.
//!
//! crossterm on Windows reads WinAPI `KEY_EVENT_RECORD`s (see
//! `crossterm-0.28.1/src/event/source/windows.rs`: `read_single_input_event`),
//! so the terminal's bracketed-paste markers are never parsed and
//! `Event::Paste` is never produced on this platform -- only the Unix parser
//! emits it. A paste therefore arrives as one `KeyCode::Char` per character,
//! with every embedded newline delivered as a real `Enter` key. That is why a
//! multi-line paste submits its first line immediately while the rest is still
//! being typed out, and why the characters appear one at a time.
//!
//! Two facts make a paste distinguishable from typing without any terminal
//! cooperation:
//!
//! * Human keystrokes are tens of milliseconds apart even when typing fast, so
//!   every *inter-key gap* inside a paste stays under [`QUIET_WINDOW`].
//! * crossterm's Windows reader is a separate thread feeding a channel, so a
//!   large paste does NOT land in one already-buffered chunk. It arrives in
//!   pieces. Deciding from "what was buffered at the instant the loop woke" --
//!   the obvious implementation -- therefore sees several small chunks and
//!   replays each one as typing, which is exactly the bug this module exists to
//!   prevent. `collect` instead keeps reading for as long as keys keep arriving
//!   inside the quiet window, so the burst is reassembled across chunks.
//!
//! A burst is delivered through the normal paste path, inserting it in one
//! shot; anything that is not a paste is replayed in its original order, so
//! ordinary typing is unchanged.

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long the input may stay quiet before a burst is considered finished.
/// This is the actual paste signal: a human is >=30ms between keystrokes even
/// when typing quickly, while every gap within a paste is far smaller. A burst
/// is collected while keys keep arriving sooner than this, so a paste that
/// trickles in over a slow link is still reassembled correctly.
pub(crate) const QUIET_WINDOW: Duration = Duration::from_millis(40);

/// Hard cap on how long a single burst may keep absorbing keys, so a held-down
/// key or a spamming console cannot extend the burst indefinitely.
pub(crate) const MAX_BURST_SPAN: Duration = Duration::from_millis(500);

/// Hard cap on how many events one burst may collect, so a stuck or spamming
/// console cannot grow the buffer without limit.
pub(crate) const MAX_BURST_KEYS: usize = 1024;

/// Fewest keys that can form a paste. A single character is indistinguishable
/// from typing and needs no special handling. Two is enough because routing
/// "ab" through the paste path is visually identical to typing it, and a
/// two-key paste such as "a\n" must NOT be replayed -- that would submit.
pub(crate) const MIN_PASTE_KEYS: usize = 2;

/// Upper bound on one reconstructed paste, so a stuck key cannot grow the
/// buffer without limit.
pub(crate) const MAX_PASTE_BURST_CHARS: usize = 64 * 1024;

/// What a drained burst turned out to be.
#[derive(Debug, PartialEq)]
pub(crate) enum Burst {
    /// A paste: deliver this text at once, newlines included.
    Paste(String),
    /// Ordinary typing: replay these keys in order, unchanged.
    Replay(Vec<KeyEvent>),
}

/// Whether a key can contribute a character to pasted text.
///
/// Control/Alt/Super-modified keys are excluded so every crabcode binding
/// (Ctrl+C, Alt+b, Ctrl+Enter, ...) keeps working, and so a burst that contains
/// a shortcut is replayed rather than silently turned into text. `Backspace` is
/// excluded for the same reason: dropping it would silently alter the paste.
pub(crate) fn is_paste_text_key(key: &KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return false;
    }
    if key.modifiers.intersects(
        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::META,
    ) {
        return false;
    }
    matches!(key.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab)
}

/// The character a key contributes to pasted text, if any.
fn key_text(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) => Some(c),
        // A newline in a paste must not become a submit.
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab => Some('\t'),
        _ => None,
    }
}

/// A collected run of key events, and the largest gap observed between two
/// consecutive keys inside it.
pub(crate) struct Collected {
    pub keys: Vec<KeyEvent>,
    /// Largest inter-key gap. Zero for a single key.
    pub max_gap: Duration,
}

/// Collect the burst that `first` belongs to, reassembling it across chunks.
///
/// Returns immediately -- with just `first` -- when nothing else is already
/// buffered, so an ordinary keystroke gains no latency. When more input *is*
/// pending it keeps reading for as long as keys keep arriving within
/// [`QUIET_WINDOW`], up to [`MAX_BURST_SPAN`] / [`MAX_BURST_KEYS`].
///
/// Non-key events encountered along the way are pushed onto `deferred` for the
/// caller to handle next; they are never dropped.
pub(crate) fn collect(first: KeyEvent, deferred: &mut VecDeque<Event>) -> Collected {
    let mut keys = vec![first];
    let mut max_gap = Duration::ZERO;

    // Zero-timeout probe first: a lone keystroke must not wait out the window.
    if !matches!(event::poll(Duration::ZERO), Ok(true)) {
        return Collected { keys, max_gap };
    }

    let burst_started = Instant::now();
    let mut last_arrival = burst_started;

    loop {
        if keys.len() >= MAX_BURST_KEYS || burst_started.elapsed() >= MAX_BURST_SPAN {
            break;
        }
        // Wait out the quiet window, but never past the overall burst cap.
        let until_quiet = QUIET_WINDOW.saturating_sub(last_arrival.elapsed());
        let until_cap = MAX_BURST_SPAN.saturating_sub(burst_started.elapsed());
        let timeout = until_quiet.min(until_cap);

        match event::poll(timeout) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if is_paste_text_key(&key) => {
                    let now = Instant::now();
                    max_gap = max_gap.max(now.duration_since(last_arrival));
                    last_arrival = now;
                    keys.push(key);
                }
                Ok(other) => {
                    // Mouse/resize/focus: hand it back to the caller, and stop
                    // absorbing into this burst.
                    deferred.push_back(other);
                    break;
                }
                Err(_) => break,
            },
            Ok(false) | Err(_) => break,
        }
    }

    Collected { keys, max_gap }
}

/// Decide what a collected burst is.
///
/// `max_gap` is the largest interval between two consecutive keys, which is the
/// timing signal that separates a paste from typing. Keys are always replayed in
/// their original order when the burst is not a paste, so nothing observable
/// changes for ordinary typing.
pub(crate) fn resolve(collected: Collected) -> Burst {
    let Collected { keys, max_gap } = collected;

    if keys.len() < MIN_PASTE_KEYS {
        return Burst::Replay(keys);
    }
    if max_gap > QUIET_WINDOW {
        return Burst::Replay(keys);
    }
    if !keys.iter().all(is_paste_text_key) {
        return Burst::Replay(keys);
    }

    let mut text = String::with_capacity(keys.len());
    for key in &keys {
        if let Some(c) = key_text(key) {
            text.push(c);
            if text.len() >= MAX_PASTE_BURST_CHARS {
                break;
            }
        }
    }
    Burst::Paste(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn collected(keys: Vec<KeyEvent>, max_gap: Duration) -> Collected {
        Collected { keys, max_gap }
    }

    fn tight(keys: Vec<KeyEvent>) -> Collected {
        collected(keys, Duration::from_millis(1))
    }

    #[test]
    fn multi_line_paste_becomes_one_text_burst_with_newlines() {
        let burst = resolve(tight(vec![
            key(KeyCode::Char('a')),
            key(KeyCode::Char('b')),
            key(KeyCode::Enter),
            key(KeyCode::Char('c')),
        ]));
        assert_eq!(burst, Burst::Paste("ab\nc".to_string()));
    }

    #[test]
    fn paste_is_not_split_into_a_submit() {
        // The regression: "one\ntwo" used to submit on the Enter.
        let burst = resolve(tight(vec![
            key(KeyCode::Char('o')),
            key(KeyCode::Char('n')),
            key(KeyCode::Char('e')),
            key(KeyCode::Enter),
            key(KeyCode::Char('t')),
            key(KeyCode::Char('w')),
            key(KeyCode::Char('o')),
        ]));
        assert_eq!(burst, Burst::Paste("one\ntwo".to_string()));
    }

    #[test]
    fn two_key_paste_with_newline_is_not_replayed() {
        // Replaying this would submit on the Enter.
        let burst = resolve(tight(vec![key(KeyCode::Char('a')), key(KeyCode::Enter)]));
        assert_eq!(burst, Burst::Paste("a\n".to_string()));
    }

    #[test]
    fn single_key_is_always_typing() {
        let burst = resolve(tight(vec![key(KeyCode::Char('a'))]));
        match burst {
            Burst::Replay(keys) => assert_eq!(keys.len(), 1),
            other => panic!("expected replay, got {other:?}"),
        }
    }

    #[test]
    fn a_paste_that_trickles_over_a_slow_link_is_still_one_paste() {
        // Total span is long, but every individual gap is short: that is a
        // paste arriving in chunks, which is the bug this module fixes.
        let burst = resolve(collected(
            vec![
                key(KeyCode::Char('h')),
                key(KeyCode::Char('i')),
                key(KeyCode::Enter),
                key(KeyCode::Char('y')),
                key(KeyCode::Char('o')),
            ],
            Duration::from_millis(35),
        ));
        assert_eq!(burst, Burst::Paste("hi\nyo".to_string()));
    }

    #[test]
    fn slow_typing_with_a_long_gap_is_replayed() {
        let burst = resolve(collected(
            vec![
                key(KeyCode::Char('h')),
                key(KeyCode::Char('i')),
                key(KeyCode::Char('!')),
                key(KeyCode::Char(' ')),
            ],
            Duration::from_millis(300),
        ));
        match burst {
            Burst::Replay(keys) => assert_eq!(keys.len(), 4),
            other => panic!("expected replay, got {other:?}"),
        }
    }

    #[test]
    fn modified_keys_are_never_turned_into_text() {
        let burst = resolve(tight(vec![
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            key(KeyCode::Char('x')),
        ]));
        match burst {
            Burst::Replay(keys) => assert_eq!(keys.len(), 2),
            other => panic!("expected replay, got {other:?}"),
        }
    }

    #[test]
    fn backspace_in_a_burst_forces_replay() {
        let burst = resolve(tight(vec![
            key(KeyCode::Backspace),
            key(KeyCode::Char('a')),
            key(KeyCode::Char('b')),
            key(KeyCode::Char('c')),
        ]));
        assert!(matches!(burst, Burst::Replay(_)));
    }

    #[test]
    fn released_events_are_not_paste_text() {
        let mut released = key(KeyCode::Char('a'));
        released.kind = KeyEventKind::Release;
        assert!(!is_paste_text_key(&released));
    }

    #[test]
    fn paste_text_is_length_capped() {
        let drained: Vec<KeyEvent> = (0..MAX_PASTE_BURST_CHARS + 10)
            .map(|_| key(KeyCode::Char('x')))
            .collect();
        match resolve(tight(drained)) {
            Burst::Paste(text) => {
                assert!(text.len() <= MAX_PASTE_BURST_CHARS + 1, "{}", text.len());
            }
            other => panic!("expected paste, got {other:?}"),
        }
    }
}
