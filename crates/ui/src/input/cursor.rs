use std::ops::{Range, RangeBounds};

/// A selection in the text, represented by start and end byte indices.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct Selection {
    pub start: usize,
    pub end: usize,
}

/// One editor selection expressed as a normalized byte range plus its active end.
///
/// `range` always stores `start <= end`. When `reversed` is false the active
/// caret is at `range.end`; when true it is at `range.start`. This mirrors the
/// anchor/active distinction used by multi-cursor editors while preserving the
/// existing [`Selection`] representation.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct EditorSelection {
    pub range: Selection,
    pub reversed: bool,
}

impl EditorSelection {
    /// Construct a selection from its anchor and active caret byte offsets.
    pub fn from_anchor_and_head(anchor: usize, head: usize) -> Self {
        if head < anchor {
            Self {
                range: Selection::new(head, anchor),
                reversed: true,
            }
        } else {
            Self {
                range: Selection::new(anchor, head),
                reversed: false,
            }
        }
    }

    /// Construct a collapsed selection at `offset`.
    pub fn caret(offset: usize) -> Self {
        Self::from_anchor_and_head(offset, offset)
    }

    /// Return the stationary end of this selection.
    pub fn anchor(&self) -> usize {
        if self.reversed {
            self.range.end
        } else {
            self.range.start
        }
    }

    /// Return the active caret end of this selection.
    pub fn head(&self) -> usize {
        if self.reversed {
            self.range.start
        } else {
            self.range.end
        }
    }

    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
}

impl Selection {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Clears the selection, setting start and end to 0.
    pub fn clear(&mut self) {
        self.start = 0;
        self.end = 0;
    }

    /// Checks if the given offset is within the selection range.
    pub fn contains(&self, offset: usize) -> bool {
        offset >= self.start && offset < self.end
    }
}

impl From<Range<usize>> for Selection {
    fn from(value: Range<usize>) -> Self {
        Self::new(value.start, value.end)
    }
}
impl From<Selection> for Range<usize> {
    fn from(value: Selection) -> Self {
        value.start..value.end
    }
}
impl RangeBounds<usize> for Selection {
    fn start_bound(&self) -> std::ops::Bound<&usize> {
        std::ops::Bound::Included(&self.start)
    }

    fn end_bound(&self) -> std::ops::Bound<&usize> {
        std::ops::Bound::Excluded(&self.end)
    }
}

#[cfg(test)]
mod tests {
    use crate::input::Position;

    #[test]
    fn test_line_column_from_to() {
        assert_eq!(
            Position::new(1, 2),
            Position {
                line: 1,
                character: 2
            }
        );
    }
}
