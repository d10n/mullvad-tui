// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistent state for the Logs page.
//!
//! The page has no focusable widgets; `↑/↓` and `Enter` are reserved
//! for the focus engine on neighboring tabs. The only state we keep
//! here is the user's manual scroll position; without it the panel
//! tails the most recent entry, which is the desired default for a
//! `tail -f`-style log viewer. `Home`/`End`/`PgUp`/`PgDn` switch the
//! page into a manually-pinned scroll position; `End` (or scrolling
//! all the way to the bottom) re-engages the auto-tail.

use std::cell::Cell;

#[derive(Debug, Default)]
pub struct PageState {
    /// Manually-set scroll offset (in wrapped rows from the top of
    /// the buffer). `None` means "tail the bottom" - the renderer
    /// computes `total_rows - viewport` itself so new entries appear
    /// without intervention. `Some(n)` pins the top of the visible
    /// area at row `n` so new entries below the user's position
    /// don't disturb the view.
    scroll_offset: Cell<Option<u16>>,
    /// Total wrapped row count from the most recent render, cached
    /// so the input dispatch (which doesn't have the panel rect
    /// handy) can clamp `Home`/`End`/`PgUp`/`PgDn` against current
    /// dimensions.
    last_total_rows: Cell<u16>,
    /// Visible row count from the most recent render. Same caching
    /// rationale as [`Self::last_total_rows`].
    last_viewport: Cell<u16>,
}

impl PageState {
    /// Resolve the scroll position for the current frame: either the
    /// user's pinned value (clamped to the valid range) or the
    /// auto-tail offset that pins the latest entry to the bottom.
    pub fn effective_scroll(&self, total_rows: u16, viewport: u16) -> u16 {
        let max_offset = total_rows.saturating_sub(viewport);
        match self.scroll_offset.get() {
            Some(n) => n.min(max_offset),
            None => max_offset,
        }
    }

    pub fn scroll_to_top(&self) {
        self.scroll_offset.set(Some(0));
    }

    /// Re-engage auto-tail. The next render will sit the latest entry
    /// at the bottom and continue to do so as new entries arrive.
    pub fn scroll_to_bottom(&self) {
        self.scroll_offset.set(None);
    }

    /// Move up by `lines` rows. Saturates at 0. Used by both `PgUp`
    /// (`lines = viewport`) and the mouse wheel (`lines = a few`).
    pub fn scroll_up_by(&self, lines: u16, total_rows: u16, viewport: u16) {
        let current = self.effective_scroll(total_rows, viewport);
        self.scroll_offset.set(Some(current.saturating_sub(lines)));
    }

    /// Move down by `lines` rows. If the move would land at (or past)
    /// the bottom, clear back to auto-tail so subsequent entries push
    /// into view automatically. Used by both `PgDn`
    /// (`lines = viewport`) and the mouse wheel.
    pub fn scroll_down_by(&self, lines: u16, total_rows: u16, viewport: u16) {
        let current = self.effective_scroll(total_rows, viewport);
        let max_offset = total_rows.saturating_sub(viewport);
        let next = current.saturating_add(lines);
        if next >= max_offset {
            self.scroll_offset.set(None);
        } else {
            self.scroll_offset.set(Some(next));
        }
    }

    /// `PgUp`: move up by a full viewport.
    pub fn page_up(&self, total_rows: u16, viewport: u16) {
        self.scroll_up_by(viewport, total_rows, viewport);
    }

    /// `PgDn`: move down by a full viewport, re-engaging auto-tail
    /// when it lands at the bottom.
    pub fn page_down(&self, total_rows: u16, viewport: u16) {
        self.scroll_down_by(viewport, total_rows, viewport);
    }

    /// Cache the dimensions of the most recent render so the
    /// dispatch can call [`Self::page_up`] / [`Self::page_down`] /
    /// [`Self::scroll_to_top`] / [`Self::scroll_to_bottom`] without
    /// having a `Frame` handy.
    pub fn record_dimensions(&self, total_rows: u16, viewport: u16) {
        self.last_total_rows.set(total_rows);
        self.last_viewport.set(viewport);
    }

    /// Last rendered (total_rows, viewport) pair. Used by the
    /// `Home`/`End`/`PgUp`/`PgDn` dispatch.
    pub fn last_dimensions(&self) -> (u16, u16) {
        (self.last_total_rows.get(), self.last_viewport.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_follows_tail() {
        let s = PageState::default();
        assert_eq!(s.effective_scroll(100, 20), 80, "tail = total - viewport");
    }

    #[test]
    fn scroll_to_top_pins_at_zero() {
        let s = PageState::default();
        s.scroll_to_top();
        assert_eq!(s.effective_scroll(100, 20), 0);
        // After more entries arrive (total grows), the pinned 0 must
        // stay at 0 - auto-tail would have moved it.
        assert_eq!(s.effective_scroll(200, 20), 0);
    }

    #[test]
    fn page_up_moves_up_by_viewport_clamping_at_zero() {
        let s = PageState::default();
        // Total 100, viewport 20 -> tail offset 80.
        s.page_up(100, 20); // 80 - 20 = 60
        assert_eq!(s.effective_scroll(100, 20), 60);
        s.page_up(100, 20); // 60 - 20 = 40
        s.page_up(100, 20); // 40 - 20 = 20
        s.page_up(100, 20); // 20 - 20 = 0
        s.page_up(100, 20); // saturates at 0
        assert_eq!(s.effective_scroll(100, 20), 0);
    }

    #[test]
    fn page_down_re_engages_tail_when_reaching_the_bottom() {
        let s = PageState::default();
        s.scroll_to_top();
        s.page_down(100, 20); // 0 + 20 = 20, still pinned
        assert_eq!(s.effective_scroll(100, 20), 20);
        // The pinned offset survives a total-rows growth: still 20.
        assert_eq!(s.effective_scroll(200, 20), 20);
        // Walk back to the bottom (still on a 100-row buffer):
        // 40, 60, 80 (max_offset). Hits the bottom and re-engages
        // auto-tail, so growing the buffer afterwards keeps the
        // viewport at the new tail.
        s.page_down(100, 20);
        s.page_down(100, 20);
        s.page_down(100, 20);
        assert_eq!(
            s.effective_scroll(200, 20),
            180,
            "page_down at the bottom should re-engage auto-tail",
        );
    }

    #[test]
    fn scroll_up_by_moves_by_arbitrary_lines_clamping_at_zero() {
        let s = PageState::default();
        // Starting from auto-tail (offset 80 of 100/20).
        s.scroll_up_by(3, 100, 20); // 80 - 3 = 77
        assert_eq!(s.effective_scroll(100, 20), 77);
        // Big jump saturates.
        s.scroll_up_by(200, 100, 20);
        assert_eq!(s.effective_scroll(100, 20), 0);
    }

    #[test]
    fn scroll_down_by_re_engages_tail_at_the_bottom() {
        let s = PageState::default();
        s.scroll_to_top();
        s.scroll_down_by(3, 100, 20); // 0 + 3 = 3
        assert_eq!(s.effective_scroll(100, 20), 3);
        // Walk down past max_offset (= 80) to confirm the auto-tail
        // re-engages the same way `page_down` does.
        s.scroll_down_by(200, 100, 20);
        assert_eq!(s.effective_scroll(200, 20), 180);
    }

    #[test]
    fn out_of_range_pinned_offset_clamps_to_max() {
        // The renderer asks for the effective scroll given the
        // current frame's totals, which can shrink (entries evicted
        // from the bounded ring buffer). The state should clamp
        // rather than overshoot.
        let s = PageState::default();
        s.scroll_to_top();
        s.page_down(1000, 20); // pin at 20
        // Buffer shrinks: total_rows now 30, viewport 20 -> max 10.
        // 20 should clamp down to 10.
        assert_eq!(s.effective_scroll(30, 20), 10);
    }
}
