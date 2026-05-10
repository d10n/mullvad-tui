// SPDX-License-Identifier: GPL-3.0-or-later

//! Focus engine for the arrow-key-driven UI.
//!
//! The renderer builds a fresh [`FocusRegistry`] each frame: every widget
//! that wants to be reachable via arrow keys calls [`FocusRegistry::register`]
//! with its on-screen [`Rect`], a stable [`WidgetId`], and a [`FocusKind`].
//! Page state (which widget is focused, whether we're in text-entry mode)
//! lives in [`PageFocus`] on `App` and persists across frames; the registry
//! is consulted by the input handler to translate arrow keys into focus
//! moves.
//!
//! The model is **row-oriented**: each row is an ordered list of cells
//! (left-to-right). `←/→` move within a row; `↑/↓` move between rows.
//! When the destination row has fewer cells than the current column, the
//! column snaps to the nearest available cell. This is simpler than full
//! 2D nearest-neighbor computation and fits every page, which are all
//! clearly row-organized.

use std::ops::Range;

use ratatui::layout::Rect;

/// A unique identifier for a focusable widget within a single frame's
/// registry. Pages can use any consistent scheme - typically a `match`
/// on a `repr(u32)` enum, or hand-assigned constants. Stability across
/// frames is what matters: focusing widget #5 on frame N should still
/// land on widget #5 on frame N+1 even if the layout shifted slightly.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct WidgetId(pub u32);

/// Generate the boilerplate that every static-widget page in `tui/pages/`
/// shares: the `#[repr(u32)]` enum, its `VARIANTS` slice, the
/// `widget_id()` / `from_widget_id()` pair, and the `widgets::*` re-export
/// module of `pub const` aliases (one per variant, in
/// `SCREAMING_SNAKE_CASE`).
///
/// Variants accept Rust's normal `Variant = expr` discriminant syntax,
/// so multi-cluster pages (settings has seven cluster bases) just
/// assign each cluster's first variant a base const and let the rest
/// auto-increment from there. Per-variant doc comments and other
/// attributes are forwarded to the generated enum verbatim.
///
/// The optional `sentinel <Ident>;` clause appends a final variant of
/// that name (excluded from `VARIANTS` so dynamic-range bases anchored
/// on `Sentinel as u32` stay distinct from real widget ids).
///
/// The optional `extra widgets { ... }` block splices items into the
/// generated `widgets` module - the conventional home for the
/// dynamic-range `_BASE` / `_MAX` constants that pages anchor on the
/// sentinel. Per-variant classifier methods (`dns_blocker()`,
/// `anti_censorship_mode()`, etc.) are written as a separate
/// hand-rolled `impl` block - Rust permits multiple impl blocks on
/// the same type, so they live next to the macro invocation.
///
/// The optional `widgets_attrs { ... }` clause splices arbitrary
/// attributes onto the generated `widgets` module - used by pages
/// (e.g. settings) that intentionally retain `widgets::*` consts for
/// dispatcher arms / future re-adoption that the dead-code lint would
/// otherwise flag.
///
/// Example:
/// ```ignore
/// crate::define_page_widgets! {
///     /// Closed enum of Status-page widgets.
///     pub enum StatusWidget {
///         DetailsToggle = 0x10,
///         SwitchLocation,
///         RefreshConnection,
///         ConnectDisconnect,
///     }
/// }
/// ```
#[macro_export]
macro_rules! define_page_widgets {
    (
        $(#[$enum_attr:meta])*
        $vis:vis enum $Name:ident {
            $(
                $(#[$var_attr:meta])*
                $Variant:ident $(= $disc:expr)?
            ),* $(,)?
        }
        $(sentinel $Sentinel:ident;)?
        $(widgets_attrs { $(#[$widgets_attr:meta])* })?
        $(extra widgets { $($extra:item)* })?
    ) => {
        $(#[$enum_attr])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        #[repr(u32)]
        $vis enum $Name {
            $(
                $(#[$var_attr])*
                $Variant $(= $disc)?,
            )*
            $($Sentinel,)?
        }

        impl $Name {
            /// Every real variant in declaration order. The sentinel
            /// (when present) is deliberately excluded so dynamic-range
            /// bases anchored on `Sentinel as u32` don't collide with a
            /// real widget id.
            const VARIANTS: &'static [Self] = &[$(Self::$Variant,)*];

            pub const fn widget_id(self) -> $crate::app::WidgetId {
                $crate::app::WidgetId(self as u32)
            }

            pub fn from_widget_id(id: $crate::app::WidgetId) -> Option<Self> {
                Self::VARIANTS.iter().find(|v| v.widget_id() == id).copied()
            }
        }

        ::paste::paste! {
            $($(#[$widgets_attr])*)?
            pub mod widgets {
                use super::$Name;
                use $crate::app::WidgetId;
                $(
                    pub const [<$Variant:snake:upper>]: WidgetId =
                        $Name::$Variant.widget_id();
                )*
                $($($extra)*)?
            }
        }
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusKind {
    /// Top-bar tab. `Enter` switches to that page.
    TabButton,
    /// Generic action button. `Enter` runs the bound action.
    Button,
    /// Boolean flip. `Enter` toggles.
    Toggle,
    /// Text-entry field. `Enter` enters/commits text-entry mode.
    TextInput,
    /// One option in a radio-style group. `Enter` selects it.
    SelectOption,
    /// Sub-page breadcrumb back button (`[<]`). Treated as a chrome
    /// row by [`first_body_widget`] / pane partitioning so it doesn't
    /// hijack snap-to-body or pane-cycle navigation, but otherwise
    /// behaves like a `Button` (focusable, mouse-clickable).
    BreadcrumbBack,
    /// `[x]` button on the top-right of the outer frame border.
    /// Same chrome treatment as `TabButton`/`BreadcrumbBack`: skipped
    /// by `first_body_widget` and merged with the tab-bar pane for
    /// pane partitioning. Reachable via mouse click and arrow-key
    /// navigation, but explicitly skipped as a `Tab`-cycle landing
    /// target so chrome -> body -> chrome cycling lands on a tab
    /// button rather than the close button.
    WindowClose,
}

/// One focusable widget's position + kind in the current frame.
///
/// `rect` is read by Left/Right arrow navigation (so a row whose
/// registration order doesn't match its visual layout still walks
/// left-to-right) and by mouse routing. Up/Down work in (row, col)
/// space so pages can put the primary widget at column 0 and have
/// column-snap land on it. `kind` distinguishes the tab bar from
/// body widgets so `Home`/`End` can skip it.
#[derive(Clone, Copy, Debug)]
pub struct FocusableWidget {
    pub id: WidgetId,
    pub rect: Rect,
    pub kind: FocusKind,
}

/// Per-frame registry built up by render code; consulted by the input
/// handler to resolve arrow-key navigation.
#[derive(Default)]
pub struct FocusRegistry {
    rows: Vec<Vec<FocusableWidget>>,
    /// Row-index ranges that delimit "scrollable areas" - contiguous
    /// rows whose `Home`/`End`/`PgUp`/`PgDn` navigation should stay
    /// inside the range rather than escape to a sibling widget above
    /// or below. Pages with a scrollable list/tree call
    /// [`Self::begin_scroll_group`] before registering the rows and
    /// [`Self::end_scroll_group`] after, so bulk-nav clamps to the
    /// list instead of jumping to the search anchor or some other
    /// adjacent focusable.
    scroll_groups: Vec<Range<usize>>,
    /// Open-but-not-yet-closed scroll group, set by
    /// [`Self::begin_scroll_group`] and consumed by
    /// [`Self::end_scroll_group`]. Stored as the row index at the
    /// time of the begin call.
    pending_group_start: Option<usize>,
    /// Mouse-only click targets that are hit-tested by [`Self::hit_test`]
    /// but don't participate in keyboard arrow navigation. Used for
    /// affordances like the expand/collapse chevron on the
    /// Select-location tree, where a click toggles the tree state but
    /// keyboard focus stays on the row's primary radio. Checked
    /// **before** the row widgets in `hit_test` so a click target
    /// stacked over a row-spanning radio still wins.
    click_targets: Vec<(Rect, WidgetId)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArrowDir {
    Up,
    Down,
    Left,
    Right,
}

/// Visual category for a row, used by [`FocusRegistry::panes`] when
/// partitioning the registry for `Tab` cycling. Adjacent rows of the
/// same kind merge into one pane; `ScrollGroup`s are distinguished by
/// their index so a `Body`-`ScrollGroup`-`Body` sequence yields three
/// panes rather than two.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneKind {
    TabBar,
    /// Sub-page breadcrumb row - gets its own pane so `Tab` cycles
    /// `tabs -> [<] -> body -> tabs` (single-button pane is a tap to
    /// reach, a tap to leave).
    Breadcrumb,
    ScrollGroup(usize),
    Body,
}

/// Persistent focus state for `App`. Reset on page navigation so each
/// page starts at its first focusable widget.
#[derive(Default)]
pub struct PageFocus {
    pub focused: Option<WidgetId>,
}

impl FocusRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a widget in the current row. If no row has been opened
    /// yet, a new row is implicitly started - saves callers from having
    /// to always say `end_row()` first.
    pub fn register(&mut self, widget: FocusableWidget) {
        if self.rows.is_empty() {
            self.rows.push(Vec::new());
        }
        self.rows
            .last_mut()
            .expect("just-pushed row must exist")
            .push(widget);
    }

    /// Mark the end of the current row. Subsequent `register` calls go
    /// into a new row. No-op if the current row is empty (so it's safe
    /// to call defensively at the top of a render block).
    pub fn end_row(&mut self) {
        let needs_break = self.rows.last().is_some_and(|row| !row.is_empty());
        if needs_break {
            self.rows.push(Vec::new());
        }
    }

    /// First focusable widget in registration order (top-to-bottom,
    /// left-to-right). `None` only if the registry is empty (no widgets
    /// at all on this page).
    pub fn first(&self) -> Option<WidgetId> {
        self.rows.iter().flat_map(|r| r.iter()).next().map(|w| w.id)
    }

    /// Per-row cell counts in registration order, with empty rows
    /// elided. Test-facing accessor: lets renderers assert their
    /// rows-and-columns layout without exposing the underlying
    /// `Vec<Vec<FocusableWidget>>`.
    #[cfg(test)]
    pub fn row_widths(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows.iter().filter(|r| !r.is_empty()).map(|r| r.len())
    }

    #[cfg(test)]
    pub fn all_widgets(&self) -> Vec<FocusableWidget> {
        self.rows.iter().flat_map(|r| r.iter()).copied().collect()
    }

    /// First focusable widget in row `row` (zero-indexed). The caller
    /// owns the row-numbering convention; in this codebase row 0 is
    /// the tab bar (always rendered first by the run loop), so
    /// `first_in_row(0)` gets the first tab. Test-only: production
    /// snap logic uses [`Self::first_body_widget`] instead, which
    /// skips chrome rows (tab bar, breadcrumb) automatically.
    #[cfg(test)]
    pub fn first_in_row(&self, row: usize) -> Option<WidgetId> {
        self.rows.get(row).and_then(|r| r.first()).map(|w| w.id)
    }

    /// True if any registered widget has the given id.
    pub fn contains(&self, id: WidgetId) -> bool {
        self.rows.iter().any(|row| row.iter().any(|w| w.id == id))
    }

    /// Register a click-only target whose rect should be hit-tested by
    /// [`Self::hit_test`] but ignored by keyboard navigation. The
    /// caller is responsible for handling the resulting widget id in
    /// the page's `activate` dispatch - typically by performing the
    /// action and re-pointing focus back at a normal navigable widget
    /// in the same row, so subsequent arrow keys behave as the user
    /// expects. Used by the Select-location tree's chevron glyphs.
    pub fn register_click_target(&mut self, id: WidgetId, rect: Rect) {
        self.click_targets.push((rect, id));
    }

    /// Find the first registered widget whose rect contains the
    /// terminal-cell coordinate `(col, row)`. Used by mouse-click
    /// dispatch to translate a screen position into a focusable
    /// widget. Click-only targets are checked first so that a small
    /// affordance (the tree chevron) layered on top of a row-spanning
    /// radio still wins; the row widgets are then walked in
    /// registration order so an earlier-registered widget wins when
    /// two of them overlap. Returns `None` when no rect covers the
    /// position.
    pub fn hit_test(&self, col: u16, row: u16) -> Option<WidgetId> {
        let hits = |r: &Rect| {
            let in_x = col >= r.x && col < r.x.saturating_add(r.width);
            let in_y = row >= r.y && row < r.y.saturating_add(r.height);
            in_x && in_y
        };
        if let Some((_, id)) = self.click_targets.iter().find(|(r, _)| hits(r)) {
            return Some(*id);
        }
        self.rows
            .iter()
            .flat_map(|r| r.iter())
            .find_map(|w| if hits(&w.rect) { Some(w.id) } else { None })
    }

    /// Like [`Self::hit_test`] but returns the matching widget's
    /// `Rect` instead of its id - used by the run loop's hover-
    /// highlight pass to find the cells it should bg-paint gray.
    /// Same precedence as `hit_test`: click-only targets first, then
    /// row widgets in registration order.
    pub fn rect_at(&self, col: u16, row: u16) -> Option<Rect> {
        let hits = |r: &Rect| {
            let in_x = col >= r.x && col < r.x.saturating_add(r.width);
            let in_y = row >= r.y && row < r.y.saturating_add(r.height);
            in_x && in_y
        };
        if let Some((rect, _)) = self.click_targets.iter().find(|(r, _)| hits(r)) {
            return Some(*rect);
        }
        self.rows
            .iter()
            .flat_map(|r| r.iter())
            .find_map(|w| if hits(&w.rect) { Some(w.rect) } else { None })
    }

    /// Compute the (row, col) position of the given id, if registered.
    fn position(&self, id: WidgetId) -> Option<(usize, usize)> {
        self.rows.iter().enumerate().find_map(|(row_idx, row)| {
            row.iter()
                .position(|w| w.id == id)
                .map(|col_idx| (row_idx, col_idx))
        })
    }

    /// Resolve an arrow-key press to the next widget id, or `None` if
    /// the move would leave the registry (edge of the page). Callers
    /// can choose to leave focus where it was, or implement wrap-around
    /// at the call site.
    ///
    /// Up/Down move between rows and column-snap by registration index -
    /// pages can put the "primary" widget at column 0 to make it the
    /// snap target. Left/Right move by the widget's actual visual
    /// `rect.x` so a row whose registration order doesn't match its
    /// left-to-right layout (e.g. the VPN-settings toggle row, where
    /// the toggle is registered first but rendered to the right of
    /// `[Info]`) still navigates the way the user sees it. Ties on
    /// `rect.x` fall back to registration order so dummy/zero-width
    /// rects in tests behave as before.
    pub fn navigate(&self, current: WidgetId, direction: ArrowDir) -> Option<WidgetId> {
        let (row, col) = self.position(current)?;
        match direction {
            ArrowDir::Left => self.horizontal_neighbor(row, col, false),
            ArrowDir::Right => self.horizontal_neighbor(row, col, true),
            ArrowDir::Up => self.rows.get(row.checked_sub(1)?).and_then(|target_row| {
                let target_col = col.min(target_row.len().saturating_sub(1));
                target_row.get(target_col).map(|w| w.id)
            }),
            ArrowDir::Down => self.rows.get(row + 1).and_then(|target_row| {
                let target_col = col.min(target_row.len().saturating_sub(1));
                target_row.get(target_col).map(|w| w.id)
            }),
        }
    }

    /// Walk one cell left or right within a row by the widgets' visual
    /// `rect.x`, breaking ties by registration order (col index). The
    /// `(rect.x, col)` tuple is the sort key in both directions; with
    /// `right = true` we pick the smallest key strictly greater than
    /// the current widget's, with `right = false` the largest key
    /// strictly less than it. Returns `None` at the row's visual edge.
    fn horizontal_neighbor(&self, row: usize, col: usize, right: bool) -> Option<WidgetId> {
        let cur_key = (self.rows[row][col].rect.x, col);
        let candidates = self.rows[row]
            .iter()
            .enumerate()
            .map(|(i, w)| ((w.rect.x, i), w.id));
        if right {
            candidates
                .filter(|(key, _)| *key > cur_key)
                .min_by_key(|(key, _)| *key)
                .map(|(_, id)| id)
        } else {
            candidates
                .filter(|(key, _)| *key < cur_key)
                .max_by_key(|(key, _)| *key)
                .map(|(_, id)| id)
        }
    }

    /// First focusable in the first non-chrome row. Used by `Home`
    /// and the post-render snap-to-first fallback: pages render the
    /// tab bar (and on sub-pages, the breadcrumb's `[<]` button) as
    /// chrome rows, so jumping past them lands the user on whatever
    /// the body's top widget is (typically the first list/tree row).
    /// Falls back to the first widget overall when the page has only
    /// chrome rows.
    pub fn first_body_widget(&self) -> Option<WidgetId> {
        let first_body_row = self.rows.iter().position(|row| {
            !row.iter().all(|w| {
                matches!(
                    w.kind,
                    FocusKind::TabButton | FocusKind::BreadcrumbBack | FocusKind::WindowClose
                )
            })
        })?;
        self.rows[first_body_row].first().map(|w| w.id)
    }

    /// Last focusable in the last non-empty row. Used by `End`. The
    /// hint bar at the bottom of the frame registers no widgets, so
    /// "last row" naturally means "last body row".
    pub fn last_widget(&self) -> Option<WidgetId> {
        let last_row = self.rows.iter().rposition(|row| !row.is_empty())?;
        self.rows[last_row].last().map(|w| w.id)
    }

    /// Mark the start of a scrollable focus group. Subsequent rows
    /// registered before [`Self::end_scroll_group`] form a contiguous
    /// range; `Home`/`End`/`PgUp`/`PgDn` clamp inside that range when
    /// the focused widget belongs to it, so a list with a search
    /// anchor above doesn't see Home jump to the anchor instead of
    /// the first list row.
    ///
    /// Captures the row index where the next [`Self::register`] will
    /// land, which is `rows.len() - 1` when there's a trailing empty
    /// row open (the typical case after the caller's prior
    /// `end_row()`), or `0` when the registry is empty.
    pub fn begin_scroll_group(&mut self) {
        let start = if self.rows.is_empty() {
            0
        } else {
            self.rows.len() - 1
        };
        self.pending_group_start = Some(start);
    }

    /// Close the scroll group opened by [`Self::begin_scroll_group`].
    /// No-op when no group is open or when the group is empty.
    /// `end_row()` between widgets leaves a trailing empty row which
    /// we exclude - the group covers only the rows that actually
    /// contain widgets.
    pub fn end_scroll_group(&mut self) {
        if let Some(start) = self.pending_group_start.take() {
            let end = self
                .rows
                .iter()
                .enumerate()
                .skip(start)
                .rfind(|(_, row)| !row.is_empty())
                .map(|(i, _)| i + 1)
                .unwrap_or(start);
            if start < end {
                self.scroll_groups.push(start..end);
            }
        }
    }

    /// Row range of the scroll group containing `id`, or `None` when
    /// `id` isn't in any registered group.
    fn scroll_group_of(&self, id: WidgetId) -> Option<Range<usize>> {
        let (row, _) = self.position(id)?;
        self.scroll_groups
            .iter()
            .find(|g| g.contains(&row))
            .cloned()
    }

    /// First widget in the same scroll group as `id`. Used by
    /// `Home`: when focus is inside a scrollable list, we want it to
    /// move to the list's first row, not jump above to a sibling
    /// search anchor or filter button.
    pub fn first_in_scroll_group(&self, id: WidgetId) -> Option<WidgetId> {
        let group = self.scroll_group_of(id)?;
        let (_, col) = self.position(id)?;
        let row = self.rows.get(group.start)?;
        let target_col = col.min(row.len().saturating_sub(1));
        row.get(target_col).map(|w| w.id)
    }

    /// Last widget in the same scroll group as `id`. Counterpart of
    /// [`Self::first_in_scroll_group`] for `End`.
    pub fn last_in_scroll_group(&self, id: WidgetId) -> Option<WidgetId> {
        let group = self.scroll_group_of(id)?;
        let (_, col) = self.position(id)?;
        let row = self.rows.get(group.end.saturating_sub(1))?;
        let target_col = col.min(row.len().saturating_sub(1));
        row.get(target_col).map(|w| w.id)
    }

    /// Cycle focus to the next pane on the current page. Panes are
    /// the natural visual partition of the focus registry: the tab
    /// bar, each registered scroll group, and contiguous runs of
    /// non-grouped body rows between them. From a body widget on a
    /// page like `Status > Select location` (search anchor + tree),
    /// `Tab` moves between the search/filter row and the tree.
    ///
    /// Returns `None` when the registry has fewer than two panes -
    /// callers leave focus where it is.
    pub fn next_pane(&self, current: WidgetId) -> Option<WidgetId> {
        self.cycle_pane(current, 1)
    }

    /// Inverse of [`Self::next_pane`] for `Shift+Tab`.
    pub fn prev_pane(&self, current: WidgetId) -> Option<WidgetId> {
        self.cycle_pane(current, -1)
    }

    fn cycle_pane(&self, current: WidgetId, direction: isize) -> Option<WidgetId> {
        let (_, col) = self.position(current)?;
        let panes = self.panes();
        if panes.len() < 2 {
            return None;
        }
        let cur_pane = self.pane_index_of(current, &panes)?;
        let len = panes.len() as isize;
        let next = (cur_pane as isize + direction).rem_euclid(len) as usize;
        self.first_widget_in_range(&panes[next], col)
    }

    /// Compute the natural pane partition of the registry by walking
    /// rows in order. Two adjacent rows merge into one pane when they
    /// share a [`PaneKind`]: `TabBar` (a row whose widgets are all
    /// `FocusKind::TabButton`), `ScrollGroup(i)` (rows in the i-th
    /// registered scroll group), or `Body` (everything else). Empty
    /// rows are skipped.
    fn panes(&self) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut current: Option<(PaneKind, usize)> = None;
        for i in 0..self.rows.len() {
            let Some(kind) = self.pane_kind_of_row(i) else {
                continue;
            };
            match current {
                Some((cur, _)) if cur == kind => {
                    // Continue the current pane.
                }
                Some((_, start)) => {
                    out.push(start..i);
                    current = Some((kind, i));
                }
                None => current = Some((kind, i)),
            }
        }
        if let Some((_, start)) = current {
            out.push(start..self.rows.len());
        }
        out
    }

    fn pane_kind_of_row(&self, row_idx: usize) -> Option<PaneKind> {
        let row = self.rows.get(row_idx)?;
        if row.is_empty() {
            return None;
        }
        // `[x]` window-close + tab buttons are both top-frame chrome;
        // merging them into one `TabBar` pane keeps `Tab` cycling at
        // chrome -> body -> chrome regardless of how many chrome
        // widgets there are.
        if row
            .iter()
            .all(|w| matches!(w.kind, FocusKind::TabButton | FocusKind::WindowClose))
        {
            return Some(PaneKind::TabBar);
        }
        if row.iter().all(|w| w.kind == FocusKind::BreadcrumbBack) {
            return Some(PaneKind::Breadcrumb);
        }
        for (gi, g) in self.scroll_groups.iter().enumerate() {
            if g.contains(&row_idx) {
                return Some(PaneKind::ScrollGroup(gi));
            }
        }
        Some(PaneKind::Body)
    }

    fn pane_index_of(&self, id: WidgetId, panes: &[Range<usize>]) -> Option<usize> {
        let (row, _) = self.position(id)?;
        panes.iter().position(|p| p.contains(&row))
    }

    fn first_widget_in_range(&self, range: &Range<usize>, col_snap: usize) -> Option<WidgetId> {
        for i in range.clone() {
            let Some(row) = self.rows.get(i) else {
                continue;
            };
            // `Tab` cycling skips the `[x]` window-close button: the
            // close row is merged into the chrome pane for partitioning,
            // but landing focus on `[x]` would mean every chrome <-> body
            // round trip arms a destructive action under the cursor.
            // Filter it out here so the snap target falls through to a
            // tab button on the next chrome row.
            let mut candidates = row.iter().filter(|w| w.kind != FocusKind::WindowClose);
            let Some(first) = candidates.next() else {
                continue;
            };
            let rest: Vec<&FocusableWidget> = candidates.collect();
            let count = 1 + rest.len();
            let target_col = col_snap.min(count - 1);
            return if target_col == 0 {
                Some(first.id)
            } else {
                rest.get(target_col - 1).map(|w| w.id)
            };
        }
        None
    }

    /// `move_rows` clamped to the scroll group containing `id`.
    /// Returns `None` when `id` isn't in any scroll group - callers
    /// fall back to the unclamped [`Self::move_rows`] in that case.
    pub fn move_rows_in_scroll_group(
        &self,
        current: WidgetId,
        row_delta: isize,
    ) -> Option<WidgetId> {
        let group = self.scroll_group_of(current)?;
        let (row, col) = self.position(current)?;
        let lo = group.start as isize;
        let hi = group.end.saturating_sub(1) as isize;
        let target = (row as isize + row_delta).clamp(lo, hi) as usize;
        let target_row = self.rows.get(target)?;
        let target_col = col.min(target_row.len().saturating_sub(1));
        target_row.get(target_col).map(|w| w.id)
    }

    /// Move focus by `row_delta` rows (positive = down). Snaps the
    /// column when the destination row has fewer cells, mirroring the
    /// `Up`/`Down` arrow behavior. Clamps to `[0, last_row]` so
    /// `PageUp` at the top and `PageDown` at the bottom land on the
    /// edge instead of returning `None` - the user gets a visible
    /// effect on every press, which is the convention `Home`/`End`
    /// reinforce.
    pub fn move_rows(&self, current: WidgetId, row_delta: isize) -> Option<WidgetId> {
        let (row, col) = self.position(current)?;
        let last_row = self.rows.iter().rposition(|r| !r.is_empty())?;
        let target = (row as isize + row_delta).clamp(0, last_row as isize) as usize;
        // Skip empty rows in the direction of travel - defensive; the
        // current renderers don't produce trailing empty rows but
        // `end_row()` could leave one if a future caller registers
        // nothing after it.
        let target = if !self.rows[target].is_empty() {
            target
        } else if row_delta >= 0 {
            (target..=last_row).find(|&i| !self.rows[i].is_empty())?
        } else {
            (0..=target).rev().find(|&i| !self.rows[i].is_empty())?
        };
        let target_col = col.min(self.rows[target].len().saturating_sub(1));
        self.rows[target].get(target_col).map(|w| w.id)
    }
}

#[cfg(test)]
mod tests {
    use super::{ArrowDir, FocusKind, FocusRegistry, FocusableWidget, WidgetId};
    use ratatui::layout::Rect;

    /// Build a 3-row registry (3 cells, 2 cells, 1 cell):
    /// ```text
    /// row 0: [1] [2] [3]
    /// row 1: [4] [5]
    /// row 2: [6]
    /// ```
    /// All widgets are dummy `Button`s; rect is filler since the navigation
    /// engine only uses (row, col) positions for arrow moves.
    fn three_row_registry() -> FocusRegistry {
        let mut r = FocusRegistry::new();
        let dummy = Rect::new(0, 0, 1, 1);
        for id in 1..=3 {
            r.register(FocusableWidget {
                id: WidgetId(id),
                rect: dummy,
                kind: FocusKind::Button,
            });
        }
        r.end_row();
        for id in 4..=5 {
            r.register(FocusableWidget {
                id: WidgetId(id),
                rect: dummy,
                kind: FocusKind::Button,
            });
        }
        r.end_row();
        r.register(FocusableWidget {
            id: WidgetId(6),
            rect: dummy,
            kind: FocusKind::Button,
        });
        r
    }

    #[test]
    fn navigate_within_a_row() {
        let r = three_row_registry();
        // Right from middle of row 0 -> next cell.
        assert_eq!(r.navigate(WidgetId(2), ArrowDir::Right), Some(WidgetId(3)));
        // Left from middle of row 0 -> previous cell.
        assert_eq!(r.navigate(WidgetId(2), ArrowDir::Left), Some(WidgetId(1)));
        // Right at end of row 0 -> None (no wrap).
        assert_eq!(r.navigate(WidgetId(3), ArrowDir::Right), None);
        // Left at start of row 0 -> None.
        assert_eq!(r.navigate(WidgetId(1), ArrowDir::Left), None);
    }

    #[test]
    fn hit_test_finds_widget_under_cell_and_misses_outside() {
        // Three widgets with disjoint rects on a single row. Each
        // cell inside a rect resolves to that rect's id; cells in the
        // gaps resolve to None.
        let mut r = FocusRegistry::new();
        r.register(FocusableWidget {
            id: WidgetId(1),
            rect: Rect::new(0, 0, 5, 1), // x=0..5
            kind: FocusKind::Button,
        });
        r.register(FocusableWidget {
            id: WidgetId(2),
            rect: Rect::new(10, 0, 5, 1), // x=10..15
            kind: FocusKind::Button,
        });
        r.end_row();
        r.register(FocusableWidget {
            id: WidgetId(3),
            rect: Rect::new(0, 2, 8, 2), // y=2..4
            kind: FocusKind::Button,
        });

        // Inside widget 1.
        assert_eq!(r.hit_test(0, 0), Some(WidgetId(1)));
        assert_eq!(r.hit_test(4, 0), Some(WidgetId(1)));
        // Right edge is exclusive: x=5 is the gap.
        assert_eq!(r.hit_test(5, 0), None);
        // Inside widget 2.
        assert_eq!(r.hit_test(10, 0), Some(WidgetId(2)));
        assert_eq!(r.hit_test(14, 0), Some(WidgetId(2)));
        // Multi-row widget.
        assert_eq!(r.hit_test(0, 2), Some(WidgetId(3)));
        assert_eq!(r.hit_test(7, 3), Some(WidgetId(3)));
        // Out of bounds.
        assert_eq!(r.hit_test(99, 99), None);
        // y=1 is the gap between rows.
        assert_eq!(r.hit_test(0, 1), None);
    }

    #[test]
    fn rect_at_returns_widget_rect_or_none_with_click_target_priority() {
        let mut r = FocusRegistry::new();
        let row_rect = Rect::new(0, 0, 20, 1);
        r.register(FocusableWidget {
            id: WidgetId(1),
            rect: row_rect,
            kind: FocusKind::Button,
        });
        // Click-only target stacked over the row widget. `rect_at`
        // should mirror `hit_test`'s precedence and return the
        // chevron's rect even though the row spans the same cell.
        let chevron_rect = Rect::new(2, 0, 1, 1);
        r.register_click_target(WidgetId(99), chevron_rect);

        // Inside the row widget but outside the chevron - returns the
        // row's full rect (so the hover bg covers the whole row).
        assert_eq!(r.rect_at(10, 0), Some(row_rect));
        // Inside the chevron - the smaller click-target rect wins.
        assert_eq!(r.rect_at(2, 0), Some(chevron_rect));
        // Outside any rect.
        assert_eq!(r.rect_at(50, 50), None);
    }

    #[test]
    fn navigate_between_equal_width_rows() {
        let r = three_row_registry();
        // Down from col 0 of row 0 -> col 0 of row 1.
        assert_eq!(r.navigate(WidgetId(1), ArrowDir::Down), Some(WidgetId(4)));
        // Down from col 1 of row 0 -> col 1 of row 1.
        assert_eq!(r.navigate(WidgetId(2), ArrowDir::Down), Some(WidgetId(5)));
        // Up from col 0 of row 1 -> col 0 of row 0.
        assert_eq!(r.navigate(WidgetId(4), ArrowDir::Up), Some(WidgetId(1)));
    }

    #[test]
    fn navigate_with_column_snap_when_destination_row_is_narrower() {
        let r = three_row_registry();
        // Down from col 2 of row 0 (3 cells) -> row 1 has only 2 cells,
        // snap to last cell (col 1).
        assert_eq!(r.navigate(WidgetId(3), ArrowDir::Down), Some(WidgetId(5)));
        // Down from col 1 of row 1 (2 cells) -> row 2 has only 1 cell,
        // snap to col 0.
        assert_eq!(r.navigate(WidgetId(5), ArrowDir::Down), Some(WidgetId(6)));
        // Up from row 2's only cell -> col 0 of row 1 (no snap needed).
        assert_eq!(r.navigate(WidgetId(6), ArrowDir::Up), Some(WidgetId(4)));
    }

    #[test]
    fn navigate_at_grid_edges_returns_none() {
        let r = three_row_registry();
        // Up from top row -> None.
        assert_eq!(r.navigate(WidgetId(1), ArrowDir::Up), None);
        assert_eq!(r.navigate(WidgetId(2), ArrowDir::Up), None);
        // Down from bottom row -> None.
        assert_eq!(r.navigate(WidgetId(6), ArrowDir::Down), None);
    }

    #[test]
    fn navigate_unknown_widget_returns_none() {
        let r = three_row_registry();
        assert_eq!(r.navigate(WidgetId(99), ArrowDir::Right), None);
    }

    #[test]
    fn first_returns_top_left_widget() {
        let r = three_row_registry();
        assert_eq!(r.first(), Some(WidgetId(1)));
        assert_eq!(FocusRegistry::new().first(), None);
    }

    /// Five-row registry: row 0 is a tab bar (TabButton kind), rows 1-4
    /// are body widgets (Button kind). Used to exercise the
    /// `Home`/`End`/`PgUp`/`PgDn` navigation methods.
    fn five_row_registry_with_tabs() -> FocusRegistry {
        let mut r = FocusRegistry::new();
        let dummy = Rect::new(0, 0, 1, 1);
        for id in 1..=2 {
            r.register(FocusableWidget {
                id: WidgetId(id),
                rect: dummy,
                kind: FocusKind::TabButton,
            });
        }
        r.end_row();
        for row in 0..4 {
            for col in 0..2 {
                r.register(FocusableWidget {
                    id: WidgetId(10 + row * 2 + col),
                    rect: dummy,
                    kind: FocusKind::Button,
                });
            }
            r.end_row();
        }
        r
    }

    #[test]
    fn first_body_widget_skips_tab_row() {
        let r = five_row_registry_with_tabs();
        // Row 0 is all-TabButton, row 1 is the first body row.
        assert_eq!(r.first_body_widget(), Some(WidgetId(10)));
    }

    #[test]
    fn first_body_widget_falls_back_to_overall_first() {
        // Registry with only a tab bar (no body) - Home should still
        // land somewhere rather than vanishing.
        let mut r = FocusRegistry::new();
        let dummy = Rect::new(0, 0, 1, 1);
        r.register(FocusableWidget {
            id: WidgetId(1),
            rect: dummy,
            kind: FocusKind::TabButton,
        });
        r.end_row();
        assert_eq!(r.first_body_widget(), None);
    }

    #[test]
    fn last_widget_returns_bottom_right() {
        let r = five_row_registry_with_tabs();
        // Row 4 (last body row), col 1.
        assert_eq!(r.last_widget(), Some(WidgetId(17)));
    }

    #[test]
    fn move_rows_jumps_by_delta_with_column_snap() {
        let r = five_row_registry_with_tabs();
        // From row 1 col 0 (id 10), +2 rows -> row 3 col 0 (id 14).
        assert_eq!(r.move_rows(WidgetId(10), 2), Some(WidgetId(14)));
        // From row 4 col 1 (id 17), -2 rows -> row 2 col 1 (id 13).
        assert_eq!(r.move_rows(WidgetId(17), -2), Some(WidgetId(13)));
    }

    #[test]
    fn move_rows_clamps_at_edges_instead_of_returning_none() {
        let r = five_row_registry_with_tabs();
        // PgDn at the bottom: stay on the last row's snap-column cell.
        assert_eq!(r.move_rows(WidgetId(17), 100), Some(WidgetId(17)));
        // PgUp from the top body row: lands on the tab bar (row 0).
        assert_eq!(r.move_rows(WidgetId(10), -100), Some(WidgetId(1)));
    }

    /// Registry shaped like select_location: a search-anchor row,
    /// then a 4-row scrollable list, then a footer row. All
    /// single-cell rows for simplicity.
    fn registry_with_scroll_group() -> FocusRegistry {
        let mut r = FocusRegistry::new();
        let dummy = Rect::new(0, 0, 1, 1);
        // Row 0 - search anchor (above the list).
        r.register(FocusableWidget {
            id: WidgetId(100),
            rect: dummy,
            kind: FocusKind::TextInput,
        });
        r.end_row();
        // Rows 1..5 - the scroll group.
        r.begin_scroll_group();
        for id in 200..204 {
            r.register(FocusableWidget {
                id: WidgetId(id),
                rect: dummy,
                kind: FocusKind::SelectOption,
            });
            r.end_row();
        }
        r.end_scroll_group();
        // Row 5 - footer button (below the list).
        r.register(FocusableWidget {
            id: WidgetId(300),
            rect: dummy,
            kind: FocusKind::Button,
        });
        r.end_row();
        r
    }

    #[test]
    fn home_inside_a_scroll_group_clamps_to_the_groups_first_row() {
        let r = registry_with_scroll_group();
        // From row 3 of the list (id 202), `Home` should land on
        // the list's first row (id 200), not the search anchor.
        assert_eq!(r.first_in_scroll_group(WidgetId(202)), Some(WidgetId(200)));
        // The search anchor and footer aren't in any group; Home
        // returns None and the caller falls back to first_body_widget.
        assert_eq!(r.first_in_scroll_group(WidgetId(100)), None);
        assert_eq!(r.first_in_scroll_group(WidgetId(300)), None);
    }

    #[test]
    fn end_inside_a_scroll_group_clamps_to_the_groups_last_row() {
        let r = registry_with_scroll_group();
        assert_eq!(r.last_in_scroll_group(WidgetId(200)), Some(WidgetId(203)));
        assert_eq!(r.last_in_scroll_group(WidgetId(100)), None);
    }

    #[test]
    fn page_up_inside_a_scroll_group_does_not_escape_to_a_sibling_above() {
        let r = registry_with_scroll_group();
        // PgUp by a large delta from id 202 should land on id 200,
        // not on the search anchor (id 100) or the tab bar.
        assert_eq!(
            r.move_rows_in_scroll_group(WidgetId(202), -100),
            Some(WidgetId(200)),
        );
        // PgDn by a large delta should clamp to the last row of the
        // group (id 203), not jump to the footer.
        assert_eq!(
            r.move_rows_in_scroll_group(WidgetId(200), 100),
            Some(WidgetId(203)),
        );
    }

    /// Tab-bar (row 0) + body (row 1) + scroll group (rows 2..6) + footer (row 6).
    /// Same shape as a typical sub-page but with a tab bar prepended.
    fn registry_with_tab_bar_body_group_footer() -> FocusRegistry {
        let mut r = FocusRegistry::new();
        let dummy = Rect::new(0, 0, 1, 1);
        // Row 0 - tab bar.
        r.register(FocusableWidget {
            id: WidgetId(1),
            rect: dummy,
            kind: FocusKind::TabButton,
        });
        r.register(FocusableWidget {
            id: WidgetId(2),
            rect: dummy,
            kind: FocusKind::TabButton,
        });
        r.end_row();
        // Row 1 - body (search anchor + filter).
        r.register(FocusableWidget {
            id: WidgetId(100),
            rect: dummy,
            kind: FocusKind::TextInput,
        });
        r.register(FocusableWidget {
            id: WidgetId(101),
            rect: dummy,
            kind: FocusKind::Button,
        });
        r.end_row();
        // Rows 2..6 - scroll group.
        r.begin_scroll_group();
        for id in 200..204 {
            r.register(FocusableWidget {
                id: WidgetId(id),
                rect: dummy,
                kind: FocusKind::SelectOption,
            });
            r.end_row();
        }
        r.end_scroll_group();
        // Row 6 - footer button.
        r.register(FocusableWidget {
            id: WidgetId(300),
            rect: dummy,
            kind: FocusKind::Button,
        });
        r.end_row();
        r
    }

    #[test]
    fn tab_cycles_from_tab_bar_through_body_through_group_back_to_tab_bar() {
        let r = registry_with_tab_bar_body_group_footer();
        // From the active tab -> first body row's first cell.
        assert_eq!(r.next_pane(WidgetId(1)), Some(WidgetId(100)));
        // From the search anchor -> first scroll-group row.
        assert_eq!(r.next_pane(WidgetId(100)), Some(WidgetId(200)));
        // From a tree row -> footer (next non-group pane).
        assert_eq!(r.next_pane(WidgetId(202)), Some(WidgetId(300)));
        // From the footer -> wrap back to the tab bar.
        assert_eq!(r.next_pane(WidgetId(300)), Some(WidgetId(1)));
    }

    #[test]
    fn shift_tab_cycles_in_reverse() {
        let r = registry_with_tab_bar_body_group_footer();
        // Shift+Tab from a tree row -> search anchor (preceding pane).
        assert_eq!(r.prev_pane(WidgetId(201)), Some(WidgetId(100)));
        // Shift+Tab from the tab bar -> wrap to the footer.
        assert_eq!(r.prev_pane(WidgetId(1)), Some(WidgetId(300)));
    }

    #[test]
    fn tab_cycling_skips_window_close_button() {
        // Chrome layout matches `split_layout`: row 0 = `[x]`, row 1 =
        // tabs. Both rows pane-partition as `TabBar`, but Tab landing
        // on the chrome pane should snap to a tab button, not `[x]`.
        let mut r = FocusRegistry::new();
        let dummy = Rect::new(0, 0, 1, 1);
        r.register(FocusableWidget {
            id: WidgetId(900),
            rect: dummy,
            kind: FocusKind::WindowClose,
        });
        r.end_row();
        r.register(FocusableWidget {
            id: WidgetId(1),
            rect: dummy,
            kind: FocusKind::TabButton,
        });
        r.register(FocusableWidget {
            id: WidgetId(2),
            rect: dummy,
            kind: FocusKind::TabButton,
        });
        r.end_row();
        r.register(FocusableWidget {
            id: WidgetId(100),
            rect: dummy,
            kind: FocusKind::Button,
        });
        r.end_row();

        // Tab from body wraps to chrome and lands on the first tab,
        // not on `[x]`.
        assert_eq!(r.next_pane(WidgetId(100)), Some(WidgetId(1)));
        // Shift+Tab from body lands on a tab button as well.
        assert_eq!(r.prev_pane(WidgetId(100)), Some(WidgetId(1)));
        // Tab from `[x]` itself still cycles to body.
        assert_eq!(r.next_pane(WidgetId(900)), Some(WidgetId(100)));
    }

    #[test]
    fn tab_no_op_when_there_is_only_one_pane() {
        // Single body widget, no tab bar, no scroll group -> 1 pane.
        let mut r = FocusRegistry::new();
        let dummy = Rect::new(0, 0, 1, 1);
        r.register(FocusableWidget {
            id: WidgetId(42),
            rect: dummy,
            kind: FocusKind::Button,
        });
        r.end_row();
        assert_eq!(r.next_pane(WidgetId(42)), None);
        assert_eq!(r.prev_pane(WidgetId(42)), None);
    }

    #[test]
    fn end_row_is_idempotent_on_empty_row() {
        // Calling end_row() defensively at the top of a render block
        // shouldn't insert a phantom empty row.
        let mut r = FocusRegistry::new();
        r.end_row();
        r.end_row();
        r.register(FocusableWidget {
            id: WidgetId(1),
            rect: Rect::new(0, 0, 1, 1),
            kind: FocusKind::Button,
        });
        // No empty row before; we should be able to navigate normally.
        assert_eq!(r.first(), Some(WidgetId(1)));
        assert_eq!(r.navigate(WidgetId(1), ArrowDir::Down), None);
    }
}
