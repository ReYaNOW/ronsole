#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SingleLineInput {
    pub(crate) text: String,
    pub(crate) cursor: usize,
    pub(crate) selection_anchor: Option<usize>,
    pub(crate) scroll_x: f32,
}

impl Default for SingleLineInput {
    fn default() -> Self {
        Self::with_capacity(0)
    }
}

impl SingleLineInput {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            text: String::with_capacity(capacity),
            cursor: 0,
            selection_anchor: None,
            scroll_x: 0.0,
        }
    }

    pub(crate) fn from_text(text: &str) -> Self {
        let mut input = Self::with_capacity(text.len());
        input.set_text(text);
        input
    }

    pub(crate) fn set_text(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        self.cursor = self.char_count();
        self.selection_anchor = None;
        self.scroll_x = 0.0;
    }

    #[inline]
    pub(crate) fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map_or(self.text.len(), |(index, _)| index)
    }

    pub(crate) fn selection(&self) -> Option<(usize, usize)> {
        self.selection_anchor
            .filter(|&anchor| anchor != self.cursor)
            .map(|anchor| (anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    pub(crate) fn selected_char_count(&self) -> usize {
        self.selection()
            .map_or(0, |(start, end)| end.saturating_sub(start))
    }

    pub(crate) fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        let start_byte = self.byte_index(start);
        let end_byte = self.byte_index(end);
        self.text.replace_range(start_byte..end_byte, "");
        self.cursor = start;
        self.selection_anchor = None;
        true
    }

    pub(crate) fn select_all(&mut self) {
        self.selection_anchor = Some(0);
        self.cursor = self.char_count();
    }

    pub(crate) fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        Some(self.text[self.byte_index(start)..self.byte_index(end)].to_string())
    }

    pub(crate) fn insert_text(&mut self, text: &str) -> bool {
        let had_selection = self.delete_selection();
        if text.is_empty() {
            return had_selection;
        }
        let byte = self.byte_index(self.cursor);
        self.text.insert_str(byte, text);
        self.cursor += text.chars().count();
        self.selection_anchor = None;
        true
    }

    pub(crate) fn backspace(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.cursor == 0 {
            return false;
        }
        let end = self.byte_index(self.cursor);
        let start = self.byte_index(self.cursor - 1);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
        true
    }

    pub(crate) fn delete_forward(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.cursor >= self.char_count() {
            return false;
        }
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.text.replace_range(start..end, "");
        true
    }

    pub(crate) fn move_cursor(&mut self, new_cursor: usize, selecting: bool) -> bool {
        let previous = (self.cursor, self.selection_anchor);
        let new_cursor = new_cursor.min(self.char_count());
        if selecting {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
        self.cursor = new_cursor;
        previous != (self.cursor, self.selection_anchor)
    }

    pub(crate) fn move_left(&mut self, selecting: bool) -> bool {
        self.move_cursor(self.cursor.saturating_sub(1), selecting)
    }

    pub(crate) fn move_right(&mut self, selecting: bool) -> bool {
        self.move_cursor((self.cursor + 1).min(self.char_count()), selecting)
    }

    pub(crate) fn move_home(&mut self, selecting: bool) -> bool {
        self.move_cursor(0, selecting)
    }

    pub(crate) fn move_end(&mut self, selecting: bool) -> bool {
        self.move_cursor(self.char_count(), selecting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_editor_supports_selection_navigation_and_replacement() {
        let mut input = SingleLineInput::from_text("abcdef");
        assert!(input.move_cursor(2, false));
        assert!(input.move_right(true));
        assert!(input.move_right(true));
        assert_eq!(input.selection(), Some((2, 4)));
        assert_eq!(input.selected_text().as_deref(), Some("cd"));
        assert!(input.insert_text("XY"));
        assert_eq!(input.text, "abXYef");
        assert_eq!(input.cursor, 4);
        assert_eq!(input.selection(), None);
        assert!(input.move_home(true));
        assert_eq!(input.selection(), Some((0, 4)));
        assert!(input.move_end(false));
        assert_eq!(input.selection(), None);
    }

    #[test]
    fn shared_editor_backspace_and_delete_replace_selected_ranges() {
        let mut input = SingleLineInput::from_text("abcdef");
        input.move_cursor(1, false);
        input.move_cursor(4, true);
        assert!(input.backspace());
        assert_eq!(input.text, "aef");
        input.set_text("abcdef");
        input.move_cursor(2, false);
        input.move_cursor(5, true);
        assert!(input.delete_forward());
        assert_eq!(input.text, "abf");
    }
}
