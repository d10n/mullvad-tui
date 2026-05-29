// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistent state for the `Status > Select location` sub-page.
//!
//! Survives navigation away and back to the page so the user's
//! expanded country/city tree and search query don't reset every time
//! they pop into Status: a country tree with per-country and per-city
//! expansion toggles.

use std::{cell::Cell, collections::BTreeSet};

use crate::app::WidgetId;

/// Which node the relay-selector page is currently editing. Only
/// meaningful when multihop is enabled; with multihop off the page is
/// always editing the exit node (see the renderer's "effective mode").
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum NodeKind {
    /// The multihop entry node (`wireguard_constraints.entry_location`).
    Entry,
    /// The exit node (`relay_constraints.location`). Default and the
    /// only selectable node when multihop is off.
    #[default]
    Exit,
}

#[derive(Debug, Default)]
pub struct PageState {
    /// Country codes (e.g. `"se"`, `"us"`) currently expanded - their
    /// city rows render below the country row.
    expanded_countries: BTreeSet<String>,
    /// `(country_code, city_code)` pairs currently expanded - their
    /// relay-hostname rows render below the city row. The country
    /// must also be expanded for the city's expansion to show; we
    /// keep them as an independent set so collapsing a country
    /// doesn't lose the user's per-city expansion preference.
    expanded_cities: BTreeSet<(String, String)>,
    /// Search/filter query bound to the search-anchor row at the top
    /// of the page. Empty string means no filter.
    query: String,
    /// Which node the page is currently editing. A `Cell` so the
    /// renderer (holding `&App`/`&PageState`) can coerce it to
    /// [`NodeKind::Exit`] when multihop is off without a `&mut`.
    node_mode: Cell<NodeKind>,
    /// First visible tree-row index when the projected list is taller
    /// than the body area. Stored in a `Cell` so the renderer (which
    /// holds `&App`) can adjust it as focus moves through rows that
    /// fall outside the current window - without scrolling, ratatui's
    /// constraint solver assigns overlapping y-coordinates to the
    /// overflowing `Length(1)` rows and the renderer paints them on
    /// top of each other.
    scroll_offset: Cell<usize>,
    /// One-shot request from the page-open path: on the next render,
    /// position `scroll_offset` so the focused row sits near the
    /// vertical center of the visible window rather than just being
    /// nudged into view. Cleared once consumed so subsequent frames
    /// fall back to the standard "minimum slide to keep focus
    /// visible" behavior driven by arrow-key navigation.
    center_focused_pending: Cell<bool>,
    /// Set when the user moved the viewport directly (mouse wheel) so
    /// the renderer must NOT slide the offset to pull focus back into
    /// view - that's exactly what wheel-scrolling away from the
    /// focused row asks the page to allow. Cleared by
    /// [`Self::observe_focus_for_scroll`] the next time focus moves,
    /// so arrow-key navigation resumes the standard "keep focus
    /// visible" behavior on its first keypress.
    user_scrolled: Cell<bool>,
    /// Focused widget id seen by the renderer on the previous frame.
    /// Comparing against the current frame's focused id is how we
    /// detect that the user navigated and need to clear
    /// `user_scrolled`.
    last_focused: Cell<Option<WidgetId>>,
    /// `(rows_len, capacity)` recorded by the renderer on each frame
    /// so the wheel handler can clamp [`Self::scroll_by`] without
    /// re-projecting the row list. `(0, 0)` means "no frame rendered
    /// yet" - `scroll_by` is a no-op in that state.
    last_dimensions: Cell<(usize, usize)>,
}

impl PageState {
    /// Which node the page is currently editing.
    pub fn node_mode(&self) -> NodeKind {
        self.node_mode.get()
    }

    /// Switch the page to editing `mode`. Interior mutability (`Cell`)
    /// so both the `&mut App` activation path and the `&App` renderer's
    /// multihop-off coercion can call it.
    pub fn set_node_mode(&self, mode: NodeKind) {
        self.node_mode.set(mode);
    }

    pub fn is_country_expanded(&self, country_code: &str) -> bool {
        self.expanded_countries.contains(country_code)
    }

    pub fn is_city_expanded(&self, country_code: &str, city_code: &str) -> bool {
        self.expanded_cities
            .contains(&(country_code.to_string(), city_code.to_string()))
    }

    /// Mark `country_code` as expanded. Idempotent. Returns `true`
    /// when the state actually changed (i.e. the country was previously
    /// collapsed) so callers can skip work on no-ops if they want.
    pub fn expand_country(&mut self, country_code: &str) -> bool {
        self.expanded_countries.insert(country_code.to_string())
    }

    /// Mark `country_code` as collapsed. Idempotent. Returns `true`
    /// when the state actually changed.
    pub fn collapse_country(&mut self, country_code: &str) -> bool {
        self.expanded_countries.remove(country_code)
    }

    /// Mark `(country_code, city_code)` as expanded. Idempotent.
    /// Returns `true` when the state actually changed.
    pub fn expand_city(&mut self, country_code: &str, city_code: &str) -> bool {
        self.expanded_cities
            .insert((country_code.to_string(), city_code.to_string()))
    }

    /// Mark `(country_code, city_code)` as collapsed. Idempotent.
    /// Returns `true` when the state actually changed.
    pub fn collapse_city(&mut self, country_code: &str, city_code: &str) -> bool {
        self.expanded_cities
            .remove(&(country_code.to_string(), city_code.to_string()))
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Append a character typed into the search anchor. Reserved for
    /// `Char` key events that survived the run loop's modifier filter
    /// (`Ctrl`/`Alt` are excluded to avoid hijacking terminal
    /// shortcuts), so the caller has already decided the keystroke
    /// belongs to the buffer.
    pub fn push_query_char(&mut self, c: char) {
        self.query.push(c);
    }

    /// Pop the last character (one Backspace press). No-op on an empty
    /// buffer so the caller doesn't need to gate the call.
    pub fn pop_query_char(&mut self) {
        self.query.pop();
    }

    /// Drop the entire query in one shot. Used by `Esc` on the search
    /// anchor - first press clears, second press leaves the sub-page
    /// (the run loop only routes Esc here when the buffer is non-empty;
    /// an already-empty buffer falls through to the global Esc).
    pub fn clear_query(&mut self) {
        self.query.clear();
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset.get()
    }

    pub fn set_scroll_offset(&self, offset: usize) {
        self.scroll_offset.set(offset);
    }

    /// Ask the renderer to center the focused tree row on the next
    /// frame. Called by the page-open path so the user lands with
    /// their currently-selected location near the middle of the
    /// visible window instead of pinned to the top edge.
    pub fn request_center_focused(&self) {
        self.center_focused_pending.set(true);
    }

    /// Atomically read-and-clear the center-on-next-render request.
    /// Renderer calls this once per frame; once consumed, the standard
    /// "slide minimally to keep focus in view" behavior resumes.
    pub fn take_center_focused_request(&self) -> bool {
        self.center_focused_pending.replace(false)
    }

    /// Record the projected list length and the body's row capacity
    /// from this frame. Lets [`Self::scroll_by`] clamp wheel events
    /// without re-deriving the projection.
    pub fn record_dimensions(&self, rows_len: usize, capacity: usize) {
        self.last_dimensions.set((rows_len, capacity));
    }

    /// Compare this frame's focused widget against the previous
    /// frame's. When focus moved (arrow keys, mouse click, page open)
    /// the user-scroll override is cleared so the next frame slides
    /// the offset to keep focus visible. When focus stayed put -
    /// which is what mouse-wheel scrolling produces - the override is
    /// preserved.
    pub fn observe_focus_for_scroll(&self, focused: Option<WidgetId>) {
        if self.last_focused.get() != focused {
            self.last_focused.set(focused);
            self.user_scrolled.set(false);
        }
    }

    /// `true` when the most recent viewport movement came from
    /// [`Self::scroll_by`] (and focus hasn't changed since). The
    /// renderer reads this to decide whether to clamp the offset to
    /// the focused row.
    pub fn is_user_scrolled(&self) -> bool {
        self.user_scrolled.get()
    }

    /// Shift the viewport by `delta` rows (positive = down). Clamps
    /// to `[0, rows_len - capacity]` based on the dimensions stored
    /// by the most recent [`Self::record_dimensions`] call. No-op
    /// when the list fits in the viewport, when no frame has been
    /// rendered yet, or when the offset wouldn't actually change
    /// (e.g. wheeling further past the bottom edge). Sets
    /// `user_scrolled` so the next render leaves the offset alone
    /// instead of pulling focus back into view.
    pub fn scroll_by(&self, delta: isize) {
        let (rows_len, capacity) = self.last_dimensions.get();
        if capacity == 0 || rows_len <= capacity {
            return;
        }
        let max_offset = (rows_len - capacity) as isize;
        let current = self.scroll_offset.get() as isize;
        let new = (current + delta).clamp(0, max_offset) as usize;
        if new != self.scroll_offset.get() {
            self.scroll_offset.set(new);
            self.user_scrolled.set(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_expansion_setters_are_idempotent() {
        let mut s = PageState::default();
        assert!(!s.is_country_expanded("se"));
        assert!(s.expand_country("se"));
        assert!(s.is_country_expanded("se"));
        // Second expand is a no-op - returns false.
        assert!(!s.expand_country("se"));
        assert!(s.is_country_expanded("se"));
        assert!(s.collapse_country("se"));
        assert!(!s.is_country_expanded("se"));
        // Second collapse is a no-op too.
        assert!(!s.collapse_country("se"));
    }

    #[test]
    fn city_expansion_is_country_independent() {
        // Collapsing a country doesn't drop its cities' expansion
        // state - re-expanding the country brings the previously-
        // open cities back.
        let mut s = PageState::default();
        s.expand_country("se");
        s.expand_city("se", "got");
        assert!(s.is_city_expanded("se", "got"));
        s.collapse_country("se");
        assert!(!s.is_country_expanded("se"));
        assert!(
            s.is_city_expanded("se", "got"),
            "city stays expanded; renderer just won't show it",
        );
    }

    #[test]
    fn distinct_country_city_pairs_are_independent() {
        let mut s = PageState::default();
        s.expand_city("se", "got");
        assert!(s.is_city_expanded("se", "got"));
        // Same city code in different country -> different key.
        assert!(!s.is_city_expanded("us", "got"));
    }

    #[test]
    fn node_mode_defaults_to_exit_and_is_switchable() {
        let s = PageState::default();
        assert_eq!(s.node_mode(), NodeKind::Exit);
        s.set_node_mode(NodeKind::Entry);
        assert_eq!(s.node_mode(), NodeKind::Entry);
    }
}
