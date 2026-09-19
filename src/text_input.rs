//! A single-line text field with a cursor that can sit anywhere in the string, not just at
//! the end.
//!
//! kanstack's commit-message and branch-name prompts both need this (GitHub issue #1: "the
//! only way to edit a commit message or a branch name is by deletion, arrowing not
//! functional") — before this, both were bare `String`s edited with `.push`/`.pop`, so
//! typing was append-only and the arrow keys did nothing. One implementation shared by
//! both, rather than two copies of insert/delete/move logic that would drift apart.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    value: String,
    /// A *char* index, not a byte index — every mutator keeps this in `0..=char count`,
    /// and it is converted to a byte offset only at the point of slicing `value`, so a
    /// multi-byte character (a commit message is free-form text) is never split.
    cursor: usize,
}

impl TextInput {
    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// What `confirm_commit`/`confirm_branch` actually want: trimmed, owned.
    pub fn trimmed(&self) -> String {
        self.value.trim().to_string()
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    /// Replaces the value outright, cursor at the end — for restoring a field from a
    /// value that was handed off elsewhere (see `App::back_to_branch_name`), as opposed
    /// to `insert`'s one-character-at-a-time typing.
    pub fn set(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.char_count();
    }

    fn char_count(&self) -> usize {
        self.value.chars().count()
    }

    fn byte_offset(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.value.len())
    }

    /// Applies an editing key — typing, `Backspace`/`Delete`, `Left`/`Right`/`Home`/`End`,
    /// and `Ctrl-U` (clear the whole field) — and reports whether it was one. Callers try
    /// their own navigation keys first and hand whatever's left to this, so the six text
    /// fields don't each carry their own copy of this match.
    ///
    /// A `Ctrl`-modified character is consumed without being typed: terminals deliver
    /// `Ctrl-U` as `Char('u')` plus a modifier, so matching on the char alone would insert
    /// a literal `u` for every control chord.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(c) = key.code {
                if c.eq_ignore_ascii_case(&'u') {
                    self.clear();
                }
                return true;
            }
        }
        match key.code {
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Home => self.move_home(),
            KeyCode::End => self.move_end(),
            KeyCode::Char(c) => self.insert(c),
            _ => return false,
        }
        true
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte_offset(self.cursor);
        self.value.insert(at, c);
        self.cursor += 1;
    }

    /// Removes the character *before* the cursor, same as a terminal's own backspace.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at = self.byte_offset(self.cursor - 1);
        self.value.remove(at);
        self.cursor -= 1;
    }

    /// Removes the character *under* the cursor, leaving the cursor where it is.
    pub fn delete_forward(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let at = self.byte_offset(self.cursor);
        self.value.remove(at);
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.char_count();
    }

    /// The text split at the cursor, for rendering a block cursor between the two halves
    /// rather than always after the whole string.
    pub fn split_at_cursor(&self) -> (&str, &str) {
        let at = self.byte_offset(self.cursor);
        self.value.split_at(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_goes_at_the_cursor_not_always_the_end() {
        let mut t = TextInput::default();
        for c in "helo".chars() {
            t.insert(c);
        }
        t.move_left();
        t.move_left();
        t.insert('l');
        assert_eq!(t.as_str(), "hello");
    }

    #[test]
    fn backspace_removes_before_the_cursor() {
        let mut t = TextInput::default();
        for c in "hello".chars() {
            t.insert(c);
        }
        t.move_left();
        t.backspace();
        assert_eq!(t.as_str(), "helo");
    }

    #[test]
    fn cursor_movement_is_bounded() {
        let mut t = TextInput::default();
        t.move_left(); // before anything exists
        t.insert('a');
        t.move_right();
        t.move_right(); // past the end
        t.insert('b');
        assert_eq!(t.as_str(), "ab");
    }

    #[test]
    fn split_at_cursor_respects_char_not_byte_boundaries() {
        let mut t = TextInput::default();
        for c in "héllo".chars() {
            t.insert(c);
        }
        // 5 chars, cursor starts at the end (5); three lefts lands it between 'é' and 'l'.
        t.move_left();
        t.move_left();
        t.move_left();
        let (before, after) = t.split_at_cursor();
        assert_eq!(before, "hé");
        assert_eq!(after, "llo");
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_u_clears_the_whole_field_wherever_the_cursor_is() {
        let mut t = TextInput::default();
        t.set("hello world");
        t.move_home();
        t.move_right();
        assert!(t.handle_key(ctrl('u')));
        assert_eq!(t.as_str(), "");
        t.insert('x');
        assert_eq!(t.as_str(), "x", "the cursor must be reset along with the text");
    }

    #[test]
    fn other_ctrl_chords_are_swallowed_not_typed() {
        let mut t = TextInput::default();
        t.set("abc");
        assert!(t.handle_key(ctrl('w')));
        assert_eq!(t.as_str(), "abc");
    }

    #[test]
    fn handle_key_leaves_keys_it_does_not_own_to_the_caller() {
        let mut t = TextInput::default();
        assert!(!t.handle_key(KeyEvent::from(KeyCode::Up)));
        assert!(!t.handle_key(KeyEvent::from(KeyCode::Enter)));
    }

    #[test]
    fn delete_forward_removes_the_character_under_the_cursor() {
        let mut t = TextInput::default();
        for c in "hello".chars() {
            t.insert(c);
        }
        t.move_home();
        t.delete_forward();
        assert_eq!(t.as_str(), "ello");
    }
}
