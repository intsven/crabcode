use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_textarea::{CursorMove, TextArea};

pub(crate) fn has_command_modifier(modifiers: KeyModifiers) -> bool {
    modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::META)
}

/// The character produced by the AltGr (AltGr/Option) key, when one is present.
///
/// Several characters simply do not exist on a US layout and are typed with AltGr
/// on others: `@ \ | { } [ ] ~ < > " â‚¬`. crossterm reports the resulting
/// `KeyCode::Char` together with the modifier state AltGr implies -- CONTROL+ALT on
/// Windows, a bare ALT (ESC-prefixed) elsewhere -- but tui-textarea only inserts a
/// `Key::Char` when *neither* CONTROL nor ALT is set, so those keystrokes were
/// silently swallowed. Callers insert the returned character literally instead of
/// forwarding the event.
///
/// Letters stay excluded for a bare ALT so the Alt+b / Alt+f word motions, and any
/// future Alt+letter binding, keep winning.
pub(crate) fn altgr_char(event: &KeyEvent) -> Option<char> {
    let KeyCode::Char(c) = event.code else {
        return None;
    };
    if !event.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }

    if event.modifiers.contains(KeyModifiers::CONTROL) {
        // Windows AltGr, e.g. Ctrl+Alt+Q -> `@` on a German layout. No crabcode
        // binding uses Ctrl+Alt, so the character is always literal text.
        return Some(c);
    }

    // ESC-prefixed Alt elsewhere: only punctuation is safe to claim, and that is
    // exactly what AltGr is needed for on those layouts.
    if c.is_alphanumeric() || c.is_whitespace() || c.is_control() {
        return None;
    }

    Some(c)
}

fn line_end_col(textarea: &TextArea<'static>, row: usize) -> usize {
    textarea
        .lines()
        .get(row)
        .map(|line| line.chars().count())
        .unwrap_or(0)
}

pub(crate) fn delete_to_line_start(textarea: &mut TextArea<'static>) {
    if textarea.is_selecting() {
        textarea.delete_char();
        return;
    }

    let (cursor_row, cursor_col) = textarea.cursor();

    if let Some(line) = textarea.lines().get(cursor_row) {
        let delete_count = cursor_col.min(line.chars().count());
        for _ in 0..delete_count {
            textarea.delete_char();
        }
    }
}

pub(crate) fn command_backspace_to_line_start(textarea: &mut TextArea<'static>) {
    if textarea.is_selecting() {
        textarea.delete_char();
        return;
    }

    let (cursor_row, cursor_col) = textarea.cursor();

    if cursor_col == 0 {
        if cursor_row > 0 {
            let previous_row = cursor_row - 1;
            let previous_col = line_end_col(textarea, previous_row);
            textarea.move_cursor(CursorMove::Jump(previous_row as u16, previous_col as u16));
        }
        return;
    }

    delete_to_line_start(textarea);
}

pub(crate) fn input_textarea(textarea: &mut TextArea<'static>, event: KeyEvent) -> bool {
    let cmd = has_command_modifier(event.modifiers);
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);

    match event.code {
        KeyCode::Left if cmd => textarea.move_cursor(CursorMove::Head),
        KeyCode::Right if cmd => textarea.move_cursor(CursorMove::End),
        // macOS Option+Arrow, plus Ghostty's Alt+b / Alt+f equivalents.
        KeyCode::Left if event.modifiers.contains(KeyModifiers::ALT) => {
            textarea.move_cursor(CursorMove::WordBack)
        }
        KeyCode::Right if event.modifiers.contains(KeyModifiers::ALT) => {
            textarea.move_cursor(CursorMove::WordForward)
        }
        KeyCode::Char('b') | KeyCode::Char('B') if event.modifiers.contains(KeyModifiers::ALT) => {
            textarea.move_cursor(CursorMove::WordBack)
        }
        KeyCode::Char('f') | KeyCode::Char('F') if event.modifiers.contains(KeyModifiers::ALT) => {
            textarea.move_cursor(CursorMove::WordForward)
        }
        KeyCode::Backspace if cmd => {
            command_backspace_to_line_start(textarea);
        }
        KeyCode::Char('a') if ctrl => textarea.move_cursor(CursorMove::Head),
        KeyCode::Char('e') if ctrl => textarea.move_cursor(CursorMove::End),
        KeyCode::Char('u') if ctrl => {
            delete_to_line_start(textarea);
        }
        _ => match altgr_char(&event) {
            Some(c) => textarea.insert_char(c),
            None => return textarea.input(event),
        },
    };

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn windows_altgr_characters_are_recognized() {
        // German layout: AltGr+Q -> '@', AltGr+Shift+7 -> '\' (reported as
        // CONTROL+ALT by crossterm on Windows).
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(altgr_char(&key(KeyCode::Char('@'), altgr)), Some('@'));
        assert_eq!(altgr_char(&key(KeyCode::Char('\\'), altgr)), Some('\\'));
        assert_eq!(
            altgr_char(&key(KeyCode::Char('\u{20ac}'), altgr)),
            Some('\u{20ac}')
        );
    }

    #[test]
    fn esc_prefixed_alt_punctuation_is_recognized_but_letters_are_not() {
        assert_eq!(
            altgr_char(&key(KeyCode::Char('|'), KeyModifiers::ALT)),
            Some('|')
        );
        assert_eq!(
            altgr_char(&key(KeyCode::Char('q'), KeyModifiers::ALT)),
            None
        );
        assert_eq!(
            altgr_char(&key(KeyCode::Char(' '), KeyModifiers::ALT)),
            None
        );
    }

    #[test]
    fn plain_and_modified_chars_without_alt_are_not_altgr() {
        assert_eq!(
            altgr_char(&key(KeyCode::Char('@'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            altgr_char(&key(KeyCode::Char('@'), KeyModifiers::SHIFT)),
            None
        );
        assert_eq!(
            altgr_char(&key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(altgr_char(&key(KeyCode::Enter, KeyModifiers::ALT)), None);
    }

    #[test]
    fn input_textarea_inserts_altgr_characters() {
        for ch in ['@', '\\', '|', '{', '\u{20ac}'] {
            let mut textarea = TextArea::default();
            let inserted = input_textarea(
                &mut textarea,
                key(KeyCode::Char(ch), KeyModifiers::CONTROL | KeyModifiers::ALT),
            );
            assert!(inserted, "{ch} should be handled");
            assert_eq!(textarea.lines(), [ch.to_string()]);
        }
    }
}
