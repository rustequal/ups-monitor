//! A single-line text editor with a caret and a selection.
//!
//! Modelled on how mature editors (the Win32 `EDIT` control, egui's
//! `TextEdit`, iced's `text_input`) hold their state: the text plus two
//! character indices — `cursor` and `anchor`. The selection is the range
//! between them; when they are equal there is no selection, only a caret.
//! Everything else — insert, delete, arrow movement, selection growth — is
//! derived from those two indices, so the special cases collapse into one
//! rule: *replace whatever is selected, then move the caret*.
//!
//! Indices count **characters, not bytes**, so a movement key can never land
//! in the middle of a multi-byte character. For the interval field the text
//! is ASCII digits and the two coincide, but the model is written correctly
//! regardless.
//!
//! This type owns no Windows state and is unit-tested without a window: the
//! window procedure translates keystrokes and clicks into [`super::row::Edit`]
//! events and the owner applies them here, so the caret logic is exercised as
//! plain data.

/// Which characters a field accepts.
///
/// An enum rather than a `digits_only` flag: a flag answers exactly one
/// question, and a second restriction later would mean a second flag beside
/// it. The enum names the intent at the call site — `CharFilter::Digits`
/// reads as what it is, where `true` would not — and a new rule is a new
/// variant, not a new parameter. iced takes the same shape with its input
/// validator; this is the closed-set version of it.
///
/// Only the variant the utility actually uses is defined. The rule is firm: a
/// variant kept "for a future filter" is dead code wearing an explanation, so
/// the set grows when a real caller needs it, not before.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CharFilter {
    /// ASCII digits `0`–`9` only.
    Digits,
}

impl CharFilter {
    fn accepts(self, c: char) -> bool {
        match self {
            CharFilter::Digits => c.is_ascii_digit(),
        }
    }
}

/// A single-line editable text field.
///
/// `cursor` and `anchor` are character offsets in `0..=chars`. They are equal
/// when nothing is selected. The selection, when present, spans the half-open
/// character range `min(cursor, anchor)..max(cursor, anchor)`.
#[derive(Clone, Debug)]
pub(crate) struct TextField {
    text: String,
    cursor: usize,
    anchor: usize,
    filter: CharFilter,
    max_len: usize,
}

impl TextField {
    /// Creates a field holding `text`, with the caret at the end and nothing
    /// selected. `max_len` counts characters.
    pub(crate) fn new(text: impl Into<String>, filter: CharFilter, max_len: usize) -> Self {
        let text = text.into();
        let end = text.chars().count();
        Self {
            text,
            cursor: end,
            anchor: end,
            filter,
            max_len,
        }
    }

    /// The current text.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Character count of the text — the highest valid caret index.
    ///
    /// Not `pub`: only this module edits and measures the field; callers read
    /// its content through `text()`. `is_empty` is kept beside it so the pair
    /// stays consistent and clippy's `len_without_is_empty` has nothing to flag
    /// even if either is exposed later.
    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The caret position, in characters.
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    /// The selected character range, or `None` when nothing is selected.
    pub(crate) fn selection(&self) -> Option<(usize, usize)> {
        if self.cursor == self.anchor {
            None
        } else {
            Some((self.cursor.min(self.anchor), self.cursor.max(self.anchor)))
        }
    }

    /// Byte offset of character index `at`, for slicing `text`.
    fn byte_of(&self, at: usize) -> usize {
        self.text
            .char_indices()
            .nth(at)
            .map_or(self.text.len(), |(b, _)| b)
    }

    /// Deletes the selected range if there is one, leaving the caret where the
    /// selection began. Returns whether anything was removed. The shared step
    /// behind typing over a selection and behind Backspace/Delete with a
    /// selection: each of those is "remove the selection, then maybe more".
    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        let (b0, b1) = (self.byte_of(start), self.byte_of(end));
        self.text.replace_range(b0..b1, "");
        self.cursor = start;
        self.anchor = start;
        true
    }

    /// Collapses the selection to the caret without moving the text. Used by
    /// an unshifted arrow when a selection is present: the platform behaviour
    /// is to jump to the near edge, not to step one character.
    fn collapse(&mut self, to_left_edge: bool) {
        if let Some((start, end)) = self.selection() {
            self.cursor = if to_left_edge { start } else { end };
        }
        self.anchor = self.cursor;
    }

    /// Applies one editing event, returning whether the field changed.
    pub(crate) fn apply(&mut self, edit: FieldEdit) -> bool {
        match edit {
            FieldEdit::Insert(c) => self.insert(c),
            FieldEdit::Backspace => self.backspace(),
            FieldEdit::Delete => self.delete_forward(),
            FieldEdit::Left { extend } => self.move_left(extend),
            FieldEdit::Right { extend } => self.move_right(extend),
            FieldEdit::Home { extend } => self.move_home(extend),
            FieldEdit::End { extend } => self.move_end(extend),
            FieldEdit::SelectAll => self.select_all(),
            FieldEdit::CaretTo { index, extend } => self.caret_to(index, extend),
        }
    }

    fn insert(&mut self, c: char) -> bool {
        if !self.filter.accepts(c) {
            return false;
        }
        // Typing over a selection replaces it; the removed span frees room, so
        // the length check is made against the text after the removal.
        let had_selection = self.delete_selection();
        if self.len() >= self.max_len {
            // The replacement above still counts as a change even if the
            // insert that follows is refused for length.
            return had_selection;
        }
        let at = self.byte_of(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
        self.anchor = self.cursor;
        true
    }

    fn backspace(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.cursor == 0 {
            return false;
        }
        let b1 = self.byte_of(self.cursor);
        let b0 = self.byte_of(self.cursor - 1);
        self.text.replace_range(b0..b1, "");
        self.cursor -= 1;
        self.anchor = self.cursor;
        true
    }

    fn delete_forward(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.cursor >= self.len() {
            return false;
        }
        let b0 = self.byte_of(self.cursor);
        let b1 = self.byte_of(self.cursor + 1);
        self.text.replace_range(b0..b1, "");
        // Caret stays put; anchor already equals it.
        true
    }

    fn move_left(&mut self, extend: bool) -> bool {
        if !extend && self.selection().is_some() {
            self.collapse(true);
            return true;
        }
        if self.cursor == 0 {
            // Still collapse a shift-selection's dangling anchor if pressed
            // against the edge, so the caret does not keep a stale selection.
            let changed = self.anchor != self.cursor;
            if !extend {
                self.anchor = self.cursor;
            }
            return changed;
        }
        self.cursor -= 1;
        if !extend {
            self.anchor = self.cursor;
        }
        true
    }

    fn move_right(&mut self, extend: bool) -> bool {
        if !extend && self.selection().is_some() {
            self.collapse(false);
            return true;
        }
        if self.cursor >= self.len() {
            let changed = self.anchor != self.cursor;
            if !extend {
                self.anchor = self.cursor;
            }
            return changed;
        }
        self.cursor += 1;
        if !extend {
            self.anchor = self.cursor;
        }
        true
    }

    fn move_home(&mut self, extend: bool) -> bool {
        let changed = self.cursor != 0 || (!extend && self.anchor != 0);
        self.cursor = 0;
        if !extend {
            self.anchor = 0;
        }
        changed
    }

    fn move_end(&mut self, extend: bool) -> bool {
        let end = self.len();
        let changed = self.cursor != end || (!extend && self.anchor != end);
        self.cursor = end;
        if !extend {
            self.anchor = end;
        }
        changed
    }

    fn select_all(&mut self) -> bool {
        if self.is_empty() {
            return false;
        }
        self.anchor = 0;
        self.cursor = self.len();
        true
    }

    /// Places the caret at character index `index`, clamped into range. With
    /// `extend`, the anchor is left where it is so a shift-click or a mouse
    /// drag grows the selection; without it, the anchor follows and the
    /// selection collapses.
    fn caret_to(&mut self, index: usize, extend: bool) -> bool {
        let index = index.min(self.len());
        let changed = index != self.cursor || (!extend && self.anchor != index);
        self.cursor = index;
        if !extend {
            self.anchor = index;
        }
        changed
    }
}

/// One editing operation against a [`TextField`].
///
/// Kept distinct from the window layer's `Edit` (which also carries list
/// navigation and scrolling) so the field's own vocabulary stays small and
/// the mapping from keystrokes to edits is explicit at the boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FieldEdit {
    Insert(char),
    Backspace,
    Delete,
    Left {
        extend: bool,
    },
    Right {
        extend: bool,
    },
    Home {
        extend: bool,
    },
    End {
        extend: bool,
    },
    SelectAll,
    /// Place the caret at this character index; `extend` grows the selection.
    CaretTo {
        index: usize,
        extend: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digits(text: &str) -> TextField {
        TextField::new(text, CharFilter::Digits, 6)
    }

    #[test]
    fn new_places_caret_at_end_with_no_selection() {
        let f = digits("3000");
        assert_eq!(f.cursor(), 4);
        assert_eq!(f.selection(), None);
    }

    #[test]
    fn digits_filter_rejects_non_digits() {
        let mut f = digits("");
        assert!(f.apply(FieldEdit::Insert('4')));
        assert!(!f.apply(FieldEdit::Insert('x')));
        assert!(f.apply(FieldEdit::Insert('2')));
        assert_eq!(f.text(), "42");
    }

    #[test]
    fn control_characters_are_rejected() {
        let mut f = digits("");
        assert!(f.apply(FieldEdit::Insert('4')));
        assert!(!f.apply(FieldEdit::Insert('\n')));
        assert!(!f.apply(FieldEdit::Insert('\t')));
        assert_eq!(f.text(), "4");
    }

    #[test]
    fn insert_happens_at_the_caret_not_the_end() {
        let mut f = digits("300");
        f.apply(FieldEdit::Home { extend: false });
        f.apply(FieldEdit::Right { extend: false });
        f.apply(FieldEdit::Insert('9'));
        assert_eq!(f.text(), "3900");
        assert_eq!(f.cursor(), 2);
    }

    #[test]
    fn max_len_is_enforced() {
        let mut f = digits("123456");
        assert!(!f.apply(FieldEdit::Insert('7')));
        assert_eq!(f.text(), "123456");
    }

    #[test]
    fn backspace_removes_char_left_of_caret() {
        let mut f = digits("300");
        f.apply(FieldEdit::Backspace);
        assert_eq!(f.text(), "30");
        assert_eq!(f.cursor(), 2);
    }

    #[test]
    fn backspace_at_start_does_nothing() {
        let mut f = digits("30");
        f.apply(FieldEdit::Home { extend: false });
        assert!(!f.apply(FieldEdit::Backspace));
        assert_eq!(f.text(), "30");
    }

    #[test]
    fn delete_removes_char_right_of_caret() {
        let mut f = digits("300");
        f.apply(FieldEdit::Home { extend: false });
        f.apply(FieldEdit::Delete);
        assert_eq!(f.text(), "00");
        assert_eq!(f.cursor(), 0);
    }

    #[test]
    fn delete_at_end_does_nothing() {
        let mut f = digits("30");
        assert!(!f.apply(FieldEdit::Delete));
        assert_eq!(f.text(), "30");
    }

    #[test]
    fn arrows_move_the_caret() {
        let mut f = digits("300");
        f.apply(FieldEdit::Left { extend: false });
        assert_eq!(f.cursor(), 2);
        f.apply(FieldEdit::Left { extend: false });
        assert_eq!(f.cursor(), 1);
        f.apply(FieldEdit::Right { extend: false });
        assert_eq!(f.cursor(), 2);
    }

    #[test]
    fn home_and_end_jump_to_the_extremes() {
        let mut f = digits("3000");
        f.apply(FieldEdit::Home { extend: false });
        assert_eq!(f.cursor(), 0);
        f.apply(FieldEdit::End { extend: false });
        assert_eq!(f.cursor(), 4);
    }

    #[test]
    fn shift_arrow_grows_a_selection() {
        let mut f = digits("3000");
        f.apply(FieldEdit::Home { extend: false });
        f.apply(FieldEdit::Right { extend: true });
        f.apply(FieldEdit::Right { extend: true });
        assert_eq!(f.selection(), Some((0, 2)));
    }

    #[test]
    fn shift_home_selects_to_the_start() {
        let mut f = digits("3000");
        f.apply(FieldEdit::Home { extend: true });
        assert_eq!(f.selection(), Some((0, 4)));
    }

    #[test]
    fn unshifted_arrow_collapses_selection_to_near_edge() {
        let mut f = digits("3000");
        f.apply(FieldEdit::SelectAll);
        f.apply(FieldEdit::Left { extend: false });
        assert_eq!(f.selection(), None);
        assert_eq!(f.cursor(), 0);

        let mut g = digits("3000");
        g.apply(FieldEdit::SelectAll);
        g.apply(FieldEdit::Right { extend: false });
        assert_eq!(g.selection(), None);
        assert_eq!(g.cursor(), 4);
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut f = digits("3000");
        f.apply(FieldEdit::SelectAll);
        f.apply(FieldEdit::Insert('5'));
        assert_eq!(f.text(), "5");
        assert_eq!(f.selection(), None);
        assert_eq!(f.cursor(), 1);
    }

    #[test]
    fn backspace_deletes_the_selection() {
        let mut f = digits("3000");
        f.apply(FieldEdit::Home { extend: false });
        f.apply(FieldEdit::Right { extend: true });
        f.apply(FieldEdit::Right { extend: true });
        f.apply(FieldEdit::Backspace);
        assert_eq!(f.text(), "00");
        assert_eq!(f.selection(), None);
    }

    #[test]
    fn delete_deletes_the_selection() {
        let mut f = digits("3000");
        f.apply(FieldEdit::SelectAll);
        f.apply(FieldEdit::Delete);
        assert_eq!(f.text(), "");
    }

    #[test]
    fn select_all_covers_the_whole_text() {
        let mut f = digits("3000");
        assert!(f.apply(FieldEdit::SelectAll));
        assert_eq!(f.selection(), Some((0, 4)));
    }

    #[test]
    fn select_all_on_empty_does_nothing() {
        let mut f = digits("");
        assert!(!f.apply(FieldEdit::SelectAll));
        assert_eq!(f.selection(), None);
    }

    #[test]
    fn caret_to_places_and_clamps() {
        let mut f = digits("3000");
        f.apply(FieldEdit::CaretTo {
            index: 2,
            extend: false,
        });
        assert_eq!(f.cursor(), 2);
        f.apply(FieldEdit::CaretTo {
            index: 99,
            extend: false,
        });
        assert_eq!(f.cursor(), 4);
    }

    #[test]
    fn caret_to_with_extend_selects_from_anchor() {
        let mut f = digits("3000");
        f.apply(FieldEdit::Home { extend: false });
        f.apply(FieldEdit::CaretTo {
            index: 3,
            extend: true,
        });
        assert_eq!(f.selection(), Some((0, 3)));
    }

    /// What a caret move reports as a change, when the caret does not move.
    ///
    /// The return value is what decides whether the dialog repaints, and it is
    /// the half of these three functions nothing was asking about: every test
    /// above them checks where the caret ended up, which is the same for a
    /// great many wrong answers to "did anything change". Two cases are the
    /// whole of it, and they pull in opposite directions.
    ///
    /// Unshifted, the move also drops the anchor, so a field with a dangling
    /// selection changes even when the caret was already at the end it is
    /// being sent to. Reporting no change there leaves the selection
    /// highlighted on screen after the keystroke that cleared it.
    ///
    /// Shifted, the anchor stays put, so the same keystroke against the same
    /// field changes nothing at all — and repainting anyway would restart the
    /// caret blink on a key that did nothing.
    #[test]
    fn a_caret_move_that_stays_put_reports_the_anchor_it_drops() {
        // Home, with the caret already at the start and the anchor out at 4.
        let mut f = digits("3000");
        f.apply(FieldEdit::Home { extend: true });
        assert_eq!(f.selection(), Some((0, 4)));
        assert!(
            f.apply(FieldEdit::Home { extend: false }),
            "dropping the anchor is a change even though the caret stays"
        );
        assert_eq!(f.selection(), None);
        assert_eq!(f.cursor(), 0);
        assert!(
            !f.apply(FieldEdit::Home { extend: false }),
            "and once there is nothing to drop, Home changes nothing"
        );

        // The same field again, shifted: the anchor survives, so nothing
        // changes and the selection is still there afterwards.
        let mut g = digits("3000");
        g.apply(FieldEdit::Home { extend: true });
        assert!(!g.apply(FieldEdit::Home { extend: true }));
        assert_eq!(g.selection(), Some((0, 4)));

        // End is the mirror image, measured against the text's length rather
        // than against zero.
        let mut h = digits("3000");
        h.apply(FieldEdit::Home { extend: false });
        h.apply(FieldEdit::End { extend: true });
        assert_eq!(h.selection(), Some((0, 4)));
        assert!(h.apply(FieldEdit::End { extend: false }));
        assert_eq!(h.selection(), None);
        assert_eq!(h.cursor(), 4);
        assert!(!h.apply(FieldEdit::End { extend: false }));

        // And a click that lands where the caret already is: it still clears
        // the selection, and says so.
        let mut k = digits("3000");
        k.apply(FieldEdit::SelectAll);
        assert_eq!(k.cursor(), 4);
        assert!(k.apply(FieldEdit::CaretTo {
            index: 4,
            extend: false
        }));
        assert_eq!(k.selection(), None);
        assert!(!k.apply(FieldEdit::CaretTo {
            index: 4,
            extend: false
        }));
    }

    /// A click past the end of the text lands on the end, and the clamp
    /// happens before the comparison rather than after it.
    ///
    /// Otherwise a click in the empty space right of a field whose caret is
    /// already at the end reports a change on every press, repainting and
    /// restarting the blink each time.
    #[test]
    fn a_click_past_the_end_clamps_before_deciding_it_changed_anything() {
        let mut f = digits("3000");
        assert_eq!(f.cursor(), 4);
        assert!(!f.apply(FieldEdit::CaretTo {
            index: 99,
            extend: false
        }));
        assert_eq!(f.cursor(), 4);
    }
}
