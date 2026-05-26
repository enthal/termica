//! Native editor for the [`crate::block::Block::Prompt`] tail block.
//!
//! Phase 4B (see [spec/04 §"Editor model"](../spec/04-prompt-editor.md#the-editor-model)).
//! When the [`crate::shell::PromptController`] confirms a shell
//! prompt (via `Precmd` lifecycle event), the pane enters
//! `ShellPromptEditor` mode and the keystrokes start going through
//! this editor instead of straight to the PTY. The editor owns
//! cursor / selection / undo state; the [`crate::block`] model owns
//! lifecycle and submit semantics; the [`crate::render`] module
//! owns visuals. Each layer's tests cover its own surface.
//!
//! ## Scope for 4B
//!
//! - Plain text buffer with a UTF-8 **byte index** cursor.
//! - Insert (char or string), delete-before (`Backspace`),
//!   delete-after (`Delete`).
//! - Cursor moves: left/right (per grapheme — TODO 4H: proper
//!   grapheme clusters), home/end-of-line.
//! - Multiline: `Shift+Enter` inserts `\n`; rendering wraps soft
//!   lines at the pane's cell width.
//! - **`Enter` is a placeholder**: 4C wires submit + echo
//!   suppression.
//! - **`Esc` demotes** the pane back to `RawTerminal` via the
//!   controller's `leave_editor_esc`; render-pane handles that
//!   call.
//!
//! Selection / undo / history / completion live in later sub-PRs
//! (4F / 4H+ in the roadmap). The struct fields for those are
//! deliberately absent here so an out-of-scope feature can't quietly
//! land "for free" before its tests do.
//!
//! ## The cursor invariant
//!
//! `cursor` is a UTF-8 **byte index** that always lies on a `char`
//! boundary in `text`. Every operation preserves this; the
//! `assert_char_boundary` helper in tests validates it after each
//! mutation. Bypassing the operation API is a precondition violation,
//! not a runtime check — the type system + private fields prevent it.

#![forbid(unsafe_code)]

/// Native editor state inside a [`crate::block::Block::Prompt`].
///
/// Construct with [`PromptEditor::new`]; mutate via the operation
/// methods. Direct field access is hidden so the cursor invariant
/// stays unviolable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptEditor {
    text: String,
    /// UTF-8 byte index into `text`, lying on a `char` boundary.
    /// `0 <= cursor <= text.len()`.
    cursor: usize,
}

impl PromptEditor {
    /// Build a fresh editor with no text and the cursor at byte 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read-only access to the buffer.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Current cursor position as a UTF-8 byte index.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// True when no text has been typed.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Number of bytes in the buffer (not chars / not graphemes).
    pub fn len_bytes(&self) -> usize {
        self.text.len()
    }

    /// Clear the buffer and reset the cursor. Used by 4C's submit
    /// after the command has been sent to the PTY.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Insert one character at the cursor and advance the cursor
    /// past it. Maintains the char-boundary invariant.
    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Insert a string at the cursor. Each byte must form a valid
    /// UTF-8 sequence with its neighbours (it's `&str`, so it does).
    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Insert a newline at the cursor. Multiline support: a
    /// `Shift+Enter` keystroke routes here. Distinct from
    /// `insert_char('\n')` only by name — same semantics — so call
    /// sites are self-documenting.
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Delete the character immediately before the cursor. No-op
    /// when the cursor is at byte 0. Maintains the char-boundary
    /// invariant by walking back to the previous boundary, not by
    /// subtracting a fixed byte count.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    /// Delete the character immediately after the cursor. No-op
    /// when the cursor is at the end. The cursor position stays put.
    pub fn delete_forward(&mut self) {
        if self.cursor == self.text.len() {
            return;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
    }

    /// Move the cursor one character left. No-op at byte 0.
    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = prev_char_boundary(&self.text, self.cursor);
    }

    /// Move the cursor one character right. No-op at end of buffer.
    pub fn move_right(&mut self) {
        if self.cursor == self.text.len() {
            return;
        }
        self.cursor = next_char_boundary(&self.text, self.cursor);
    }

    /// Move the cursor to the start of the current line. A line is
    /// delimited by `\n`; cursor goes to either byte 0 or one past
    /// the most recent `\n` at or before the cursor.
    pub fn move_home(&mut self) {
        let line_start = self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.cursor = line_start;
    }

    /// Move the cursor to the end of the current line. End is either
    /// the next `\n` (cursor lands on it, not past it) or
    /// `text.len()`.
    pub fn move_end(&mut self) {
        let line_end = self.text[self.cursor..]
            .find('\n')
            .map(|off| self.cursor + off)
            .unwrap_or(self.text.len());
        self.cursor = line_end;
    }

    /// Iterate the editor's content split on `\n`, yielding `(line,
    /// is_cursor_on_this_line, cursor_col_in_chars)`. Used by the
    /// renderer; column is a **char count**, not a byte count.
    pub fn lines_with_cursor(&self) -> Vec<EditorLine<'_>> {
        let mut out = Vec::new();
        let mut byte_start = 0usize;
        for line in self.text.split('\n') {
            let byte_end = byte_start + line.len();
            let cursor_on_line = self.cursor >= byte_start && self.cursor <= byte_end;
            let cursor_col =
                if cursor_on_line { line[..self.cursor - byte_start].chars().count() } else { 0 };
            out.push(EditorLine { text: line, cursor_col_chars: cursor_col, cursor_on_line });
            byte_start = byte_end + 1; // skip the '\n'
        }
        out
    }
}

/// One visual row of the editor's buffer, plus where the cursor
/// sits in chars if it lands on this row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorLine<'a> {
    pub text: &'a str,
    pub cursor_col_chars: usize,
    pub cursor_on_line: bool,
}

/// Walk back from `i` to the previous `char` boundary in `s`. `i`
/// must already be on one. Uses
/// [`str::floor_char_boundary`]-equivalent logic — without the unstable
/// API — by walking up to 4 bytes (UTF-8 max char length).
fn prev_char_boundary(s: &str, i: usize) -> usize {
    debug_assert!(s.is_char_boundary(i));
    let mut j = i.saturating_sub(1);
    while j > 0 && !s.is_char_boundary(j) {
        j -= 1;
    }
    j
}

/// Walk forward from `i` to the next `char` boundary in `s`. `i`
/// must already be on one.
fn next_char_boundary(s: &str, i: usize) -> usize {
    debug_assert!(s.is_char_boundary(i));
    let mut j = (i + 1).min(s.len());
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_invariant(e: &PromptEditor) {
        assert!(
            e.text.is_char_boundary(e.cursor),
            "cursor {} not on char boundary in {:?}",
            e.cursor,
            e.text
        );
    }

    // ---- fresh editor ------------------------------------------------

    #[test]
    fn new_editor_is_empty_with_cursor_at_zero() {
        let e = PromptEditor::new();
        assert!(e.is_empty());
        assert_eq!(e.cursor(), 0);
        assert_eq!(e.text(), "");
        assert_invariant(&e);
    }

    // ---- insert ------------------------------------------------------

    #[test]
    fn insert_char_appends_and_advances_cursor() {
        let mut e = PromptEditor::new();
        e.insert_char('a');
        e.insert_char('b');
        assert_eq!(e.text(), "ab");
        assert_eq!(e.cursor(), 2);
        assert_invariant(&e);
    }

    #[test]
    fn insert_char_handles_multibyte_utf8() {
        let mut e = PromptEditor::new();
        e.insert_char('é'); // 2 bytes
        e.insert_char('日'); // 3 bytes
        e.insert_char('😀'); // 4 bytes
        assert_eq!(e.text(), "é日😀");
        assert_eq!(e.cursor(), 2 + 3 + 4);
        assert_invariant(&e);
    }

    #[test]
    fn insert_str_at_middle_splits_existing_text() {
        let mut e = PromptEditor::new();
        e.insert_str("hello");
        e.move_left();
        e.move_left();
        e.insert_str("XY");
        // After "hello" cursor was at 5. Two move_left → 3. Then
        // insert "XY" → "helXYlo", cursor at 5.
        assert_eq!(e.text(), "helXYlo");
        assert_eq!(e.cursor(), 5);
        assert_invariant(&e);
    }

    #[test]
    fn insert_newline_creates_multiline_buffer() {
        let mut e = PromptEditor::new();
        e.insert_str("first");
        e.insert_newline();
        e.insert_str("second");
        assert_eq!(e.text(), "first\nsecond");
        assert_invariant(&e);
    }

    // ---- delete ------------------------------------------------------

    #[test]
    fn backspace_at_byte_zero_is_noop() {
        let mut e = PromptEditor::new();
        e.insert_str("abc");
        e.move_home();
        e.backspace();
        assert_eq!(e.text(), "abc");
        assert_eq!(e.cursor(), 0);
        assert_invariant(&e);
    }

    #[test]
    fn backspace_removes_previous_char() {
        let mut e = PromptEditor::new();
        e.insert_str("abc");
        e.backspace();
        assert_eq!(e.text(), "ab");
        assert_eq!(e.cursor(), 2);
        assert_invariant(&e);
    }

    #[test]
    fn backspace_walks_back_one_multibyte_char() {
        let mut e = PromptEditor::new();
        e.insert_char('😀'); // 4 bytes
        assert_eq!(e.cursor(), 4);
        e.backspace();
        assert_eq!(e.text(), "");
        assert_eq!(e.cursor(), 0);
        assert_invariant(&e);
    }

    #[test]
    fn delete_forward_at_end_is_noop() {
        let mut e = PromptEditor::new();
        e.insert_str("abc");
        e.delete_forward();
        assert_eq!(e.text(), "abc");
        assert_eq!(e.cursor(), 3);
        assert_invariant(&e);
    }

    #[test]
    fn delete_forward_removes_next_char_cursor_unchanged() {
        let mut e = PromptEditor::new();
        e.insert_str("abc");
        e.move_home();
        e.delete_forward();
        assert_eq!(e.text(), "bc");
        assert_eq!(e.cursor(), 0);
        assert_invariant(&e);
    }

    #[test]
    fn delete_forward_walks_one_multibyte_char() {
        let mut e = PromptEditor::new();
        e.insert_str("ab😀cd");
        e.move_home();
        e.move_right();
        e.move_right();
        // Cursor now at "ab|😀cd", byte 2.
        e.delete_forward();
        assert_eq!(e.text(), "abcd");
        assert_eq!(e.cursor(), 2);
        assert_invariant(&e);
    }

    // ---- cursor moves -----------------------------------------------

    #[test]
    fn move_left_at_byte_zero_is_noop() {
        let mut e = PromptEditor::new();
        e.move_left();
        assert_eq!(e.cursor(), 0);
        assert_invariant(&e);
    }

    #[test]
    fn move_right_at_end_is_noop() {
        let mut e = PromptEditor::new();
        e.insert_str("abc");
        e.move_right();
        assert_eq!(e.cursor(), 3);
        assert_invariant(&e);
    }

    #[test]
    fn move_left_walks_multibyte_chars() {
        let mut e = PromptEditor::new();
        e.insert_char('😀');
        e.insert_char('a');
        // Cursor at 5 (4 + 1).
        e.move_left();
        assert_eq!(e.cursor(), 4);
        e.move_left();
        assert_eq!(e.cursor(), 0);
        assert_invariant(&e);
    }

    #[test]
    fn move_home_jumps_to_start_of_current_line() {
        let mut e = PromptEditor::new();
        e.insert_str("first line\nsecond line");
        // Cursor at end. move_home should land at start of "second line".
        e.move_home();
        assert_eq!(e.cursor(), 11, "should land after the \\n");
        // Now move_home on first line.
        e.move_left();
        e.move_left();
        e.move_home();
        assert_eq!(e.cursor(), 0);
        assert_invariant(&e);
    }

    #[test]
    fn move_end_jumps_to_end_of_current_line() {
        let mut e = PromptEditor::new();
        e.insert_str("first line\nsecond line");
        e.move_home();
        e.move_home(); // sanity
        e.move_left(); // into first line
        e.move_end();
        assert_eq!(e.cursor(), 10, "should land at the \\n position");
        // From end of second line, move_end is a no-op.
        e.move_right(); // past \n
        e.move_end();
        assert_eq!(e.cursor(), e.len_bytes());
        assert_invariant(&e);
    }

    // ---- lines / rendering helper ------------------------------------

    #[test]
    fn lines_with_cursor_splits_on_newlines() {
        let mut e = PromptEditor::new();
        e.insert_str("alpha\nbeta\ngamma");
        let lines = e.lines_with_cursor();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "alpha");
        assert_eq!(lines[1].text, "beta");
        assert_eq!(lines[2].text, "gamma");
    }

    #[test]
    fn lines_with_cursor_locates_cursor_on_correct_line() {
        let mut e = PromptEditor::new();
        e.insert_str("alpha\nbeta\ngamma");
        // Cursor at end → last line, col == "gamma".chars().count() == 5.
        let lines = e.lines_with_cursor();
        assert!(!lines[0].cursor_on_line);
        assert!(!lines[1].cursor_on_line);
        assert!(lines[2].cursor_on_line);
        assert_eq!(lines[2].cursor_col_chars, 5);
    }

    #[test]
    fn lines_with_cursor_reports_chars_not_bytes_for_cursor_col() {
        let mut e = PromptEditor::new();
        e.insert_str("😀😀x");
        // Cursor at end: text.len() = 4+4+1 = 9 bytes; chars = 3.
        let lines = e.lines_with_cursor();
        assert_eq!(lines[0].cursor_col_chars, 3);
    }

    // ---- clear -------------------------------------------------------

    #[test]
    fn clear_resets_text_and_cursor() {
        let mut e = PromptEditor::new();
        e.insert_str("anything goes here");
        e.clear();
        assert!(e.is_empty());
        assert_eq!(e.cursor(), 0);
        assert_invariant(&e);
    }
}
