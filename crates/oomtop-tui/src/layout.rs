//! Layout classes by width (UX §2): wide ≥ 140 columns, standard 80–139, compact < 80. Layout is column-count
//! driven, never font-size driven. `Regions` records where things were drawn so mouse clicks can be mapped
//! back to rows, tabs and header meters (UX §7 "every visible thing clickable").

use crate::app::View;
use ratatui::layout::Rect;

pub const WIDE_MIN: u16 = 140;
pub const STANDARD_MIN: u16 = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutClass {
    Wide,
    Standard,
    Compact,
}

impl LayoutClass {
    pub fn for_width(w: u16) -> LayoutClass {
        if w >= WIDE_MIN {
            LayoutClass::Wide
        } else if w >= STANDARD_MIN {
            LayoutClass::Standard
        } else {
            LayoutClass::Compact
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            LayoutClass::Wide => "wide",
            LayoutClass::Standard => "standard",
            LayoutClass::Compact => "compact",
        }
    }
}

/// What a click on a header meter does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitTarget {
    View(View),
    WhySlow,
    Help,
    Chip(String),
    /// A function-key bar item: runs the keymap action.
    Action(String),
}

/// Screen regions from the last frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Regions {
    /// The scrolling list: area and the index of its first visible row.
    pub list: Option<(Rect, usize)>,
    /// Clickable spans (view tabs, header meters, chips).
    pub hits: Vec<(Rect, HitTarget)>,
    /// Timeline scrub area (x maps to a frame).
    pub timeline: Option<Rect>,
    /// Settings list: area and first visible row.
    pub settings: Option<(Rect, usize)>,
}

impl Regions {
    pub fn hit(&self, col: u16, row: u16) -> Option<&HitTarget> {
        self.hits
            .iter()
            .find(|(r, _)| contains(*r, col, row))
            .map(|(_, t)| t)
    }

    /// List row index under a point.
    pub fn list_index(&self, col: u16, row: u16) -> Option<usize> {
        let (r, off) = self.list?;
        contains(r, col, row).then(|| off + (row - r.y) as usize)
    }

    pub fn settings_index(&self, col: u16, row: u16) -> Option<usize> {
        let (r, off) = self.settings?;
        contains(r, col, row).then(|| off + (row - r.y) as usize)
    }
}

pub fn contains(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x.saturating_add(r.width) && row >= r.y && row < r.y.saturating_add(r.height)
}

/// Scroll offset that keeps `selected` visible, moving the window as little as possible.
pub fn scroll_offset(prev: usize, selected: usize, height: usize, len: usize) -> usize {
    if height == 0 {
        return 0;
    }
    let max_off = len.saturating_sub(height);
    let mut off = prev.min(max_off);
    if selected < off {
        off = selected;
    } else if selected >= off + height {
        off = selected + 1 - height;
    }
    off.min(max_off)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes() {
        assert_eq!(LayoutClass::for_width(200), LayoutClass::Wide);
        assert_eq!(LayoutClass::for_width(140), LayoutClass::Wide);
        assert_eq!(LayoutClass::for_width(139), LayoutClass::Standard);
        assert_eq!(LayoutClass::for_width(80), LayoutClass::Standard);
        assert_eq!(LayoutClass::for_width(79), LayoutClass::Compact);
        assert_eq!(LayoutClass::for_width(60), LayoutClass::Compact);
    }

    #[test]
    fn scrolling_is_minimal() {
        assert_eq!(scroll_offset(0, 3, 5, 20), 0);
        assert_eq!(scroll_offset(0, 7, 5, 20), 3);
        assert_eq!(
            scroll_offset(3, 5, 5, 20),
            3,
            "window stays while selection is inside"
        );
        assert_eq!(scroll_offset(3, 1, 5, 20), 1);
        assert_eq!(scroll_offset(10, 19, 5, 8), 3, "clamped to list length");
    }

    #[test]
    fn hits() {
        let mut r = Regions {
            list: Some((Rect::new(0, 10, 80, 5), 7)),
            ..Default::default()
        };
        r.hits.push((Rect::new(0, 0, 10, 1), HitTarget::Help));
        assert_eq!(r.list_index(3, 12), Some(9));
        assert_eq!(r.list_index(3, 15), None);
        assert_eq!(r.hit(5, 0), Some(&HitTarget::Help));
    }
}
