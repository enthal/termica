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
///
/// Selection model: a single contiguous selection anchored at
/// `selection_anchor` and extending to `cursor`. The two byte
/// indices always lie on char boundaries. `selection_anchor.is_none()`
/// means "no selection — just a cursor."
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptEditor {
    text: String,
    /// UTF-8 byte index into `text`, lying on a `char` boundary.
    /// `0 <= cursor <= text.len()`.
    cursor: usize,
    /// Other end of the selection (the fixed end; `cursor` is the
    /// active end that moves on extending operations). `None` ⇒ no
    /// selection. Always on a char boundary when `Some`.
    selection_anchor: Option<usize>,
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

    /// Effective selection range as `(min, max)` byte indices.
    /// `None` when there is no selection (or it is degenerate —
    /// anchor == cursor).
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        if anchor == self.cursor {
            None
        } else {
            Some((anchor.min(self.cursor), anchor.max(self.cursor)))
        }
    }

    /// Currently-selected text as a `&str`. `None` when no
    /// selection (or degenerate). Used by Cmd+C / Cmd+X.
    pub fn selected_text(&self) -> Option<&str> {
        let (start, end) = self.selection_range()?;
        Some(&self.text[start..end])
    }

    /// True when a non-degenerate selection exists.
    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    /// Clear any selection (cursor stays put).
    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    /// Anchor a selection at the current cursor position. Subsequent
    /// `*_extending` moves extend the selection from this anchor.
    /// Idempotent: re-anchoring at the same cursor is a no-op.
    fn begin_selection_if_absent(&mut self) {
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
    }

    /// Select the entire buffer. Cmd+A binding.
    pub fn select_all(&mut self) {
        if self.text.is_empty() {
            return;
        }
        self.selection_anchor = Some(0);
        self.cursor = self.text.len();
    }

    /// Delete the current selection. No-op when there is none.
    /// Cursor lands at the start of where the selection was.
    /// Used internally by insert/delete ops when a selection
    /// exists, and externally by Cmd+X cut.
    pub fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection_range() else {
            return false;
        };
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.selection_anchor = None;
        true
    }

    /// Move the cursor to an absolute byte index. Clamped to the
    /// nearest char boundary at or below `byte_idx`. Clears the
    /// selection. Mouse click handler routes here.
    pub fn set_cursor(&mut self, byte_idx: usize) {
        let clamped = clamp_to_char_boundary(&self.text, byte_idx);
        self.cursor = clamped;
        self.selection_anchor = None;
    }

    /// Move the cursor extending the selection. If no selection was
    /// active, anchor at the current cursor first. Mouse drag handler
    /// + Shift+arrow keys route here.
    pub fn set_cursor_extending(&mut self, byte_idx: usize) {
        self.begin_selection_if_absent();
        self.cursor = clamp_to_char_boundary(&self.text, byte_idx);
    }

    /// Clear the buffer and reset the cursor. Used by 4C's submit
    /// after the command has been sent to the PTY.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.selection_anchor = None;
    }

    /// Insert one character at the cursor and advance the cursor
    /// past it. If a selection exists, it's deleted first.
    /// Maintains the char-boundary invariant.
    pub fn insert_char(&mut self, c: char) {
        self.delete_selection();
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Insert a string at the cursor. If a selection exists, it's
    /// deleted first. Each byte must form a valid UTF-8 sequence
    /// with its neighbours (it's `&str`, so it does).
    pub fn insert_str(&mut self, s: &str) {
        self.delete_selection();
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

    /// Delete the character immediately before the cursor — OR, if
    /// a selection exists, delete the selection. No-op when the
    /// cursor is at byte 0 with no selection.
    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    /// Delete the character immediately after the cursor — OR, if a
    /// selection exists, delete the selection. No-op when the cursor
    /// is at the end with no selection.
    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor == self.text.len() {
            return;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
    }

    /// Move the cursor one character left. No-op at byte 0. Clears
    /// any selection.
    pub fn move_left(&mut self) {
        self.move_left_impl(false);
    }

    /// Move the cursor one character left, extending the selection.
    /// Shift+Left binding.
    pub fn move_left_extending(&mut self) {
        self.move_left_impl(true);
    }

    fn move_left_impl(&mut self, extend: bool) {
        if extend {
            self.begin_selection_if_absent();
        } else {
            self.selection_anchor = None;
        }
        if self.cursor == 0 {
            return;
        }
        self.cursor = prev_char_boundary(&self.text, self.cursor);
    }

    /// Move the cursor one character right. No-op at end of buffer.
    /// Clears any selection.
    pub fn move_right(&mut self) {
        self.move_right_impl(false);
    }

    /// Move the cursor one character right, extending the selection.
    /// Shift+Right binding.
    pub fn move_right_extending(&mut self) {
        self.move_right_impl(true);
    }

    fn move_right_impl(&mut self, extend: bool) {
        if extend {
            self.begin_selection_if_absent();
        } else {
            self.selection_anchor = None;
        }
        if self.cursor == self.text.len() {
            return;
        }
        self.cursor = next_char_boundary(&self.text, self.cursor);
    }

    /// Move the cursor to the start of the current line. A line is
    /// delimited by `\n`; cursor goes to either byte 0 or one past
    /// the most recent `\n` at or before the cursor. Clears
    /// selection.
    pub fn move_home(&mut self) {
        self.move_home_impl(false);
    }

    /// Move-home with selection extension.
    pub fn move_home_extending(&mut self) {
        self.move_home_impl(true);
    }

    fn move_home_impl(&mut self, extend: bool) {
        if extend {
            self.begin_selection_if_absent();
        } else {
            self.selection_anchor = None;
        }
        let line_start = self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.cursor = line_start;
    }

    /// Move the cursor to the end of the current line. End is either
    /// the next `\n` (cursor lands on it, not past it) or
    /// `text.len()`. Clears selection.
    pub fn move_end(&mut self) {
        self.move_end_impl(false);
    }

    /// Move-end with selection extension.
    pub fn move_end_extending(&mut self) {
        self.move_end_impl(true);
    }

    fn move_end_impl(&mut self, extend: bool) {
        if extend {
            self.begin_selection_if_absent();
        } else {
            self.selection_anchor = None;
        }
        let line_end = self.text[self.cursor..]
            .find('\n')
            .map(|off| self.cursor + off)
            .unwrap_or(self.text.len());
        self.cursor = line_end;
    }

    /// Move to the start of the previous word boundary. Macros
    /// macOS's Option+Left convention: skip any non-word chars
    /// behind the cursor, then skip word chars behind the cursor.
    /// Lands at the start of the word the cursor was in (or the
    /// previous word, if the cursor was on whitespace).
    pub fn move_word_left(&mut self) {
        self.move_word_left_impl(false);
    }

    /// Option+Shift+Left: word-left with selection extension.
    pub fn move_word_left_extending(&mut self) {
        self.move_word_left_impl(true);
    }

    fn move_word_left_impl(&mut self, extend: bool) {
        if extend {
            self.begin_selection_if_absent();
        } else {
            self.selection_anchor = None;
        }
        let mut i = self.cursor;
        // Skip non-word chars going back.
        while i > 0 {
            let prev = prev_char_boundary(&self.text, i);
            let c = self.text[prev..i].chars().next().unwrap();
            if is_word_char(c) {
                break;
            }
            i = prev;
        }
        // Then skip word chars going back.
        while i > 0 {
            let prev = prev_char_boundary(&self.text, i);
            let c = self.text[prev..i].chars().next().unwrap();
            if !is_word_char(c) {
                break;
            }
            i = prev;
        }
        self.cursor = i;
    }

    /// Move to the end of the next word boundary. macOS Option+Right.
    pub fn move_word_right(&mut self) {
        self.move_word_right_impl(false);
    }

    /// Option+Shift+Right: word-right with selection extension.
    pub fn move_word_right_extending(&mut self) {
        self.move_word_right_impl(true);
    }

    fn move_word_right_impl(&mut self, extend: bool) {
        if extend {
            self.begin_selection_if_absent();
        } else {
            self.selection_anchor = None;
        }
        let mut i = self.cursor;
        // Skip non-word chars going forward.
        while i < self.text.len() {
            let c = self.text[i..].chars().next().unwrap();
            if is_word_char(c) {
                break;
            }
            i += c.len_utf8();
        }
        // Then skip word chars going forward.
        while i < self.text.len() {
            let c = self.text[i..].chars().next().unwrap();
            if !is_word_char(c) {
                break;
            }
            i += c.len_utf8();
        }
        self.cursor = i;
    }

    /// Move to byte 0. macOS Cmd+Up convention.
    pub fn move_doc_start(&mut self) {
        self.selection_anchor = None;
        self.cursor = 0;
    }

    /// Cmd+Shift+Up: doc-start with selection extension.
    pub fn move_doc_start_extending(&mut self) {
        self.begin_selection_if_absent();
        self.cursor = 0;
    }

    /// Move to end of buffer. macOS Cmd+Down.
    pub fn move_doc_end(&mut self) {
        self.selection_anchor = None;
        self.cursor = self.text.len();
    }

    /// Cmd+Shift+Down: doc-end with selection extension.
    pub fn move_doc_end_extending(&mut self) {
        self.begin_selection_if_absent();
        self.cursor = self.text.len();
    }

    /// Set the selection to an explicit byte range. Both endpoints
    /// are clamped to char boundaries. `anchor == head` produces a
    /// degenerate (empty) selection and just moves the cursor. The
    /// editor renderer's drag handler uses this for word- and line-
    /// expansion drags.
    pub fn set_selection(&mut self, anchor: usize, head: usize) {
        let anchor = clamp_to_char_boundary(&self.text, anchor);
        let head = clamp_to_char_boundary(&self.text, head);
        if anchor == head {
            self.cursor = anchor;
            self.selection_anchor = None;
        } else {
            self.selection_anchor = Some(anchor);
            self.cursor = head;
        }
    }

    /// Select the word containing or touching `byte_idx`. If the
    /// position is on whitespace, the result is a degenerate
    /// (empty) selection at that position — matches the macOS
    /// double-click-on-space behaviour (no selection, just cursor).
    /// Double-click handler routes here.
    pub fn select_word_at(&mut self, byte_idx: usize) {
        let byte_idx = clamp_to_char_boundary(&self.text, byte_idx);
        let (start, end) = word_range_at(&self.text, byte_idx);
        if start == end {
            self.cursor = byte_idx;
            self.selection_anchor = None;
        } else {
            self.selection_anchor = Some(start);
            self.cursor = end;
        }
    }

    /// Select the entire line containing `byte_idx`. Includes any
    /// trailing `\n` so a triple-click + copy lifts a complete line.
    /// Triple-click handler routes here.
    pub fn select_line_at(&mut self, byte_idx: usize) {
        let byte_idx = clamp_to_char_boundary(&self.text, byte_idx);
        let (start, end) = line_range_at(&self.text, byte_idx);
        self.selection_anchor = Some(start);
        self.cursor = end;
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

/// Return the `(start, end)` byte indices of the word containing or
/// touching `byte_idx` in `text`. Returns `(byte_idx, byte_idx)`
/// (empty range) when the position is on whitespace / a non-word
/// char. `byte_idx` must already be on a char boundary.
///
/// Public because the drag-by-word handler in `render_pane` needs
/// to compute word ranges at the pointer position to extend a
/// double-click selection.
pub fn word_range_at(text: &str, byte_idx: usize) -> (usize, usize) {
    let mut start = byte_idx;
    while start > 0 {
        let prev = prev_char_boundary(text, start);
        let c = text[prev..start].chars().next().unwrap();
        if !is_word_char(c) {
            break;
        }
        start = prev;
    }
    let mut end = byte_idx;
    while end < text.len() {
        let c = text[end..].chars().next().unwrap();
        if !is_word_char(c) {
            break;
        }
        end += c.len_utf8();
    }
    (start, end)
}

/// Return the `(start, end)` byte indices of the line containing
/// `byte_idx` in `text`. End includes the trailing `\n` if present.
/// Public for the drag-by-line handler.
pub fn line_range_at(text: &str, byte_idx: usize) -> (usize, usize) {
    let start = text[..byte_idx].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = match text[byte_idx..].find('\n') {
        Some(off) => byte_idx + off + 1,
        None => text.len(),
    };
    (start, end)
}

/// True when `c` should count as part of a "word" for word-boundary
/// cursor moves and double-click word selection. Alphanumerics +
/// underscore — matches most text editors' default and avoids the
/// "what counts as punctuation" tar pit. Configurable later.
///
/// Public so the sealed-block selection module
/// ([`crate::block_selection`]) can apply the same predicate to
/// `StyledCell.c` and stay aligned with the editor's notion of
/// "word".
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Clamp `i` to a char boundary at or below itself. Returns `s.len()`
/// when `i >= s.len()`. The clamp direction (toward zero) is the
/// "land where the user pointed at or just before" convention that
/// hit-tests want.
pub fn clamp_to_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut j = i;
    while !s.is_char_boundary(j) {
        j -= 1;
    }
    j
}

/// Locate the byte index in `text` corresponding to the
/// `(row, col_chars)` pair, where `row` is the index of the
/// `\n`-delimited line and `col_chars` is the char-count within
/// that line. Used to translate mouse coordinates into a byte
/// index for [`PromptEditor::set_cursor`] /
/// [`PromptEditor::set_cursor_extending`].
///
/// Out-of-range inputs clamp: row beyond the last line falls to
/// the end of buffer; col beyond a line's length lands at the end
/// of that line.
pub fn byte_index_for_row_col(text: &str, row: usize, col_chars: usize) -> usize {
    let mut byte_start = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        let line_byte_len = line.len();
        if i == row {
            // Walk char-by-char counting columns.
            let mut byte = byte_start;
            for (col, (b, _c)) in line.char_indices().enumerate() {
                if col == col_chars {
                    return byte_start + b;
                }
                byte = byte_start + b + line[b..].chars().next().map_or(0, |c| c.len_utf8());
            }
            // Past the end of the line: land at the end (before \n
            // if there is one).
            return byte;
        }
        byte_start += line_byte_len + 1; // +1 for the \n
    }
    text.len()
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

    // ---- selection (Phase 4-editor-ergonomics) -----------------------

    #[test]
    fn fresh_editor_has_no_selection() {
        let e = PromptEditor::new();
        assert!(!e.has_selection());
        assert_eq!(e.selected_text(), None);
        assert_eq!(e.selection_range(), None);
    }

    #[test]
    fn shift_arrow_extends_selection_from_cursor() {
        let mut e = PromptEditor::new();
        e.insert_str("abcdef");
        e.move_home();
        e.move_right_extending();
        e.move_right_extending();
        e.move_right_extending();
        assert_eq!(e.selected_text(), Some("abc"));
        assert_eq!(e.selection_range(), Some((0, 3)));
        assert_invariant(&e);
    }

    #[test]
    fn non_extending_move_clears_selection() {
        let mut e = PromptEditor::new();
        e.insert_str("abcdef");
        e.move_home();
        e.move_right_extending();
        e.move_right_extending();
        assert!(e.has_selection());
        e.move_right();
        assert!(!e.has_selection(), "plain move clears selection");
    }

    #[test]
    fn select_all_covers_whole_buffer() {
        let mut e = PromptEditor::new();
        e.insert_str("hello world");
        e.select_all();
        assert_eq!(e.selected_text(), Some("hello world"));
        assert_eq!(e.cursor(), e.len_bytes(), "cursor lands at the end after select_all");
        assert_invariant(&e);
    }

    #[test]
    fn select_all_on_empty_is_noop() {
        let mut e = PromptEditor::new();
        e.select_all();
        assert!(!e.has_selection());
    }

    #[test]
    fn delete_selection_removes_range_and_lands_cursor_at_start() {
        let mut e = PromptEditor::new();
        e.insert_str("hello world");
        e.move_home();
        e.move_right_extending(); // select "h"
        e.move_right_extending(); // select "he"
        e.move_right_extending(); // select "hel"
        e.move_right_extending(); // select "hell"
        e.move_right_extending(); // select "hello"
        assert!(e.delete_selection());
        assert_eq!(e.text(), " world");
        assert_eq!(e.cursor(), 0);
        assert!(!e.has_selection());
        assert_invariant(&e);
    }

    #[test]
    fn insert_with_selection_replaces_selection() {
        let mut e = PromptEditor::new();
        e.insert_str("hello world");
        e.select_all();
        e.insert_str("bye");
        assert_eq!(e.text(), "bye");
        assert_eq!(e.cursor(), 3);
        assert_invariant(&e);
    }

    #[test]
    fn backspace_with_selection_deletes_selection_not_one_char() {
        let mut e = PromptEditor::new();
        e.insert_str("abcdef");
        e.move_home();
        e.move_right_extending();
        e.move_right_extending();
        // Selection "ab".
        e.backspace();
        assert_eq!(e.text(), "cdef", "selection removed, not one extra char");
        assert!(!e.has_selection());
    }

    #[test]
    fn set_cursor_clears_selection_and_clamps_to_char_boundary() {
        let mut e = PromptEditor::new();
        e.insert_str("hé"); // 'h' = 1 byte, 'é' = 2 bytes
        e.select_all();
        // Mouse-click lands mid-multi-byte char.
        e.set_cursor(2); // byte 2 is inside 'é' — should clamp to byte 1.
        assert_eq!(e.cursor(), 1);
        assert!(!e.has_selection());
    }

    #[test]
    fn set_cursor_extending_anchors_then_moves() {
        let mut e = PromptEditor::new();
        e.insert_str("hello");
        e.set_cursor(0);
        e.set_cursor_extending(3);
        assert_eq!(e.selected_text(), Some("hel"));
        // Extending again moves the head, anchor stays.
        e.set_cursor_extending(5);
        assert_eq!(e.selected_text(), Some("hello"));
    }

    #[test]
    fn clear_clears_selection_too() {
        let mut e = PromptEditor::new();
        e.insert_str("abc");
        e.select_all();
        e.clear();
        assert!(!e.has_selection());
        assert_eq!(e.cursor(), 0);
    }

    // ---- hit-test helpers ------------------------------------------

    #[test]
    fn byte_index_for_row_col_lands_at_char_boundary() {
        let s = "hé\nfoo";
        // (0, 0) -> 0; (0, 1) -> 1 (between 'h' and 'é'); (0, 2) ->
        // 3 (end of "hé").
        assert_eq!(byte_index_for_row_col(s, 0, 0), 0);
        assert_eq!(byte_index_for_row_col(s, 0, 1), 1);
        assert_eq!(byte_index_for_row_col(s, 0, 2), 3);
        // Row 1.
        assert_eq!(byte_index_for_row_col(s, 1, 0), 4);
        assert_eq!(byte_index_for_row_col(s, 1, 3), 7);
    }

    #[test]
    fn byte_index_for_row_col_clamps_past_line_end() {
        let s = "ab\ncd";
        // Col past line 0 end: lands at end of line 0 (before \n).
        assert_eq!(byte_index_for_row_col(s, 0, 99), 2);
    }

    // ---- word-boundary moves ---------------------------------------

    #[test]
    fn move_word_right_skips_then_lands_after_word() {
        let mut e = PromptEditor::new();
        e.insert_str("hello world foo");
        e.set_cursor(0);
        e.move_word_right();
        assert_eq!(e.cursor(), 5, "after 'hello'");
        e.move_word_right();
        assert_eq!(e.cursor(), 11, "after 'world'");
        e.move_word_right();
        assert_eq!(e.cursor(), 15, "after 'foo' (end)");
        assert_invariant(&e);
    }

    #[test]
    fn move_word_right_skips_leading_whitespace() {
        let mut e = PromptEditor::new();
        e.insert_str("   abc   def");
        e.set_cursor(0);
        e.move_word_right();
        assert_eq!(e.cursor(), 6, "after 'abc' (after skipping leading spaces and 'abc')");
    }

    #[test]
    fn move_word_left_lands_at_start_of_word() {
        let mut e = PromptEditor::new();
        e.insert_str("hello world foo");
        e.move_word_left();
        assert_eq!(e.cursor(), 12, "start of 'foo'");
        e.move_word_left();
        assert_eq!(e.cursor(), 6, "start of 'world'");
        e.move_word_left();
        assert_eq!(e.cursor(), 0, "start of 'hello'");
    }

    #[test]
    fn move_word_extending_builds_a_selection() {
        let mut e = PromptEditor::new();
        e.insert_str("hello world");
        e.set_cursor(0);
        e.move_word_right_extending();
        assert_eq!(e.selected_text(), Some("hello"));
        e.move_word_right_extending();
        assert_eq!(e.selected_text(), Some("hello world"));
    }

    #[test]
    fn move_word_handles_underscores_as_word_chars() {
        let mut e = PromptEditor::new();
        e.insert_str("foo_bar baz");
        e.set_cursor(0);
        e.move_word_right();
        assert_eq!(e.cursor(), 7, "underscore stays inside the word");
    }

    #[test]
    fn move_word_handles_punctuation_as_separator() {
        let mut e = PromptEditor::new();
        e.insert_str("foo.bar baz");
        e.set_cursor(0);
        e.move_word_right();
        assert_eq!(e.cursor(), 3, "dot is non-word; word ends here");
    }

    #[test]
    fn move_word_at_buffer_ends_is_clamped() {
        let mut e = PromptEditor::new();
        e.insert_str("abc");
        e.set_cursor(3);
        e.move_word_right();
        assert_eq!(e.cursor(), 3, "no-op at end");
        e.set_cursor(0);
        e.move_word_left();
        assert_eq!(e.cursor(), 0, "no-op at start");
    }

    // ---- doc-boundary moves ----------------------------------------

    #[test]
    fn move_doc_start_and_end() {
        let mut e = PromptEditor::new();
        e.insert_str("line one\nline two\nline three");
        // Land cursor mid-buffer.
        e.set_cursor(10);
        e.move_doc_start();
        assert_eq!(e.cursor(), 0);
        e.move_doc_end();
        assert_eq!(e.cursor(), e.len_bytes());
    }

    #[test]
    fn move_doc_extending_builds_selection_from_anchor() {
        let mut e = PromptEditor::new();
        e.insert_str("abcdef");
        e.set_cursor(3);
        e.move_doc_start_extending();
        assert_eq!(e.selected_text(), Some("abc"));
        e.move_doc_end_extending();
        assert_eq!(e.selected_text(), Some("def"), "anchor stays at 3; head moved to end");
    }

    // ---- click-selection helpers -----------------------------------

    #[test]
    fn select_word_at_grabs_surrounding_word() {
        let mut e = PromptEditor::new();
        e.insert_str("the quick brown fox");
        e.select_word_at(6); // position within "quick"
        assert_eq!(e.selected_text(), Some("quick"));
    }

    #[test]
    fn select_word_at_boundary_grabs_word_to_the_right() {
        let mut e = PromptEditor::new();
        e.insert_str("the quick brown");
        e.select_word_at(4); // exactly at 'q'
        assert_eq!(e.selected_text(), Some("quick"));
    }

    #[test]
    fn select_word_at_on_whitespace_makes_no_selection() {
        let mut e = PromptEditor::new();
        e.insert_str("the   quick");
        e.select_word_at(5); // mid-whitespace
        assert!(!e.has_selection());
        assert_eq!(e.cursor(), 5);
    }

    #[test]
    fn select_line_at_grabs_full_line_with_newline() {
        let mut e = PromptEditor::new();
        e.insert_str("first line\nsecond line\nthird");
        e.select_line_at(15); // somewhere in "second line"
        assert_eq!(e.selected_text(), Some("second line\n"));
    }

    #[test]
    fn select_line_at_on_last_line_grabs_to_end() {
        let mut e = PromptEditor::new();
        e.insert_str("only one line");
        e.select_line_at(4);
        assert_eq!(e.selected_text(), Some("only one line"));
    }

    // ---- helper range computations (used by drag-by-word/line) -----

    #[test]
    fn word_range_at_returns_word_bounds() {
        let s = "the quick brown fox";
        assert_eq!(word_range_at(s, 0), (0, 3)); // inside "the"
        assert_eq!(word_range_at(s, 5), (4, 9)); // inside "quick"
        assert_eq!(word_range_at(s, 10), (10, 15)); // start of "brown"
    }

    #[test]
    fn word_range_at_middle_of_whitespace_is_empty() {
        // Three spaces between words; byte 4 is truly in the middle
        // of the whitespace run with non-word chars on both sides.
        let s = "abc   def";
        assert_eq!(word_range_at(s, 4), (4, 4));
    }

    #[test]
    fn word_range_at_boundary_selects_left_word() {
        // Byte position immediately AFTER a word: walk-back grabs
        // the word. Matches macOS double-click-immediately-after-
        // word behaviour.
        let s = "abc def";
        assert_eq!(word_range_at(s, 3), (0, 3));
    }

    #[test]
    fn line_range_at_returns_line_bounds_with_trailing_newline() {
        let s = "first\nsecond\nthird";
        assert_eq!(line_range_at(s, 0), (0, 6)); // "first\n"
        assert_eq!(line_range_at(s, 8), (6, 13)); // "second\n"
        assert_eq!(line_range_at(s, 15), (13, 18)); // "third" (no newline at end)
    }

    // ---- set_selection -------------------------------------------------

    #[test]
    fn set_selection_anchors_and_heads_at_explicit_bytes() {
        let mut e = PromptEditor::new();
        e.insert_str("hello world");
        e.set_selection(2, 7);
        assert_eq!(e.selected_text(), Some("llo w"));
        assert_eq!(e.cursor(), 7);
    }

    #[test]
    fn set_selection_with_equal_bytes_clears_selection() {
        let mut e = PromptEditor::new();
        e.insert_str("hello");
        e.select_all();
        assert!(e.has_selection());
        e.set_selection(3, 3);
        assert!(!e.has_selection());
        assert_eq!(e.cursor(), 3);
    }

    #[test]
    fn set_selection_clamps_to_char_boundary() {
        let mut e = PromptEditor::new();
        e.insert_str("hé"); // 'h' = 1 byte, 'é' = 2 bytes; total 3
        // 2 is mid-é — clamp anchor to 1, head to 3 (end).
        e.set_selection(2, 99);
        assert_eq!(e.selection_range(), Some((1, 3)));
    }

    #[test]
    fn clamp_to_char_boundary_lands_at_or_below() {
        let s = "hé"; // bytes 0='h', 1=0xc3, 2=0xa9 (é).
        assert_eq!(clamp_to_char_boundary(s, 0), 0);
        assert_eq!(clamp_to_char_boundary(s, 1), 1);
        // 2 is mid-char — clamp down to 1.
        assert_eq!(clamp_to_char_boundary(s, 2), 1);
        // Past end clamps to end.
        assert_eq!(clamp_to_char_boundary(s, 99), 3);
    }
}
