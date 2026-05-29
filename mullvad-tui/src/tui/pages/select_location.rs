// SPDX-License-Identifier: GPL-3.0-or-later

//! `Status > Select location` sub-page renderer.
//!
//! ```text
//! [Filter location list (/ to focus)]   [Filter]
//! All locations
//!   ( ) Albania                             v
//!   ( ) Argentina                           ^
//!     ( ) Buenos Aires                      v
//!   ( ) Australia                           ^
//!     ( ) Brisbane                          ^
//!       ( ) au-bne-wg-301
//!       (•) au-bne-wg-302
//!       ( ) au-bne-wg-303
//!     ( ) Sydney                            v
//!   ( ) Austria                             v
//! ```
//!
//! One keyboard-focusable per tree row (the radio). The chevron glyph
//! is keyboard-only display, but its rect is registered as a
//! mouse-only click target so a click on `[▼]`/`[▲]` toggles
//! expansion instead of selecting the relay underneath. Indentation
//! is 2ch for cities, 4ch for relays.
//!
//! Selection: activating a country/city/relay radio (Enter or
//! clicking the row body) sets the corresponding `RelaySettings`
//! constraint via `App::select_*`. Expansion: ←/→ on a country or
//! city row drive expand/collapse and parent/child navigation in
//! standard tree-view style - see [`handle_tree_arrow`]; clicking
//! the row's chevron toggles expansion without changing the
//! selection. The expansion state lives in
//! [`crate::app::pages::select_location::PageState`] and persists
//! across navigation.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::{
    app::{
        App, ArrowDir, CurrentRelaySelection, FocusKind, FocusRegistry, FocusableWidget, WidgetId,
        pages::select_location::NodeKind,
    },
    integration::MullvadService,
    tui::{components, error::format_action_error},
};

/// Notification shown when the user tries to pick, for one multihop
/// node, the location already chosen for the other node. Entry and exit
/// must differ - you can't route in and out the same location.
const SAME_LOCATION_MSG: &str = "Can't use the same location for entry and exit";

mod tree;

pub use tree::{CityNode, CountryNode, filter_active, project_filtered_tree};

// ---- Widget id allocations ----
//
// Static rows (search anchor, filter button) live at the base of the
// SelectLocation slice. The dynamic per-tree-row focusables (one
// radio per country / city / relay) live in 256-id sub-slices well
// above the existing settings ranges so they don't collide. Each
// sub-slice has a `_MAX` cap; rows beyond the cap render display-only.

const SELECT_LOCATION_BASE: u32 = 0x1000;

/// Per-tree-row id capacity. The Mullvad relay list has ~50
/// countries / ~150 cities / ~700 relays; we cap at 256 / 256 / 1024
/// here, which gives breathing room without bloating the registry.
pub const COUNTRY_MAX: u32 = 256;
pub const CITY_MAX: u32 = 256;
pub const RELAY_MAX: u32 = 1024;

pub(crate) const COUNTRY_RADIO_BASE: u32 = SELECT_LOCATION_BASE + 0x100;
pub(crate) const CITY_RADIO_BASE: u32 = COUNTRY_RADIO_BASE + COUNTRY_MAX;
pub(crate) const RELAY_RADIO_BASE: u32 = CITY_RADIO_BASE + CITY_MAX;
/// Chevron click-target ids - same flat-row indices as the radios,
/// stacked above the relay-radio range so country/city `idx` decode
/// independently of their radio counterparts. Relay rows have no
/// chevron, so there is no `RELAY_CHEVRON_BASE`.
pub(crate) const COUNTRY_CHEVRON_BASE: u32 = RELAY_RADIO_BASE + RELAY_MAX;
pub(crate) const CITY_CHEVRON_BASE: u32 = COUNTRY_CHEVRON_BASE + COUNTRY_MAX;

pub mod widgets {
    use super::SELECT_LOCATION_BASE;
    use crate::app::WidgetId;

    /// Search anchor at the top of the panel.
    pub const SEARCH_ANCHOR: WidgetId = WidgetId(SELECT_LOCATION_BASE);
    /// `[Filter]` button right of the search anchor. Targets the
    /// `Status > Select location > Filter` sub-page.
    pub const FILTER_BUTTON: WidgetId = WidgetId(SELECT_LOCATION_BASE + 1);
    /// `[Entry]` tab of the Entry/Exit segmented control. Only rendered
    /// when multihop is enabled; switches the page to editing the
    /// multihop entry node.
    pub const ENTRY_TAB: WidgetId = WidgetId(SELECT_LOCATION_BASE + 2);
    /// `[Exit]` tab of the Entry/Exit segmented control. Switches the
    /// page back to editing the exit node.
    pub const EXIT_TAB: WidgetId = WidgetId(SELECT_LOCATION_BASE + 3);
}

// ---- Widget id decoders ----

/// Decode a country-radio widget id into a 0-based row index.
pub fn country_radio_index(widget: WidgetId) -> Option<usize> {
    decode(widget, COUNTRY_RADIO_BASE, COUNTRY_MAX)
}
pub fn city_radio_index(widget: WidgetId) -> Option<usize> {
    decode(widget, CITY_RADIO_BASE, CITY_MAX)
}
pub fn relay_radio_index(widget: WidgetId) -> Option<usize> {
    decode(widget, RELAY_RADIO_BASE, RELAY_MAX)
}
/// Decode a country-chevron click-target widget id into a 0-based
/// row index (the same index the matching radio uses).
pub fn country_chevron_index(widget: WidgetId) -> Option<usize> {
    decode(widget, COUNTRY_CHEVRON_BASE, COUNTRY_MAX)
}
pub fn city_chevron_index(widget: WidgetId) -> Option<usize> {
    decode(widget, CITY_CHEVRON_BASE, CITY_MAX)
}

fn decode(widget: WidgetId, base: u32, max: u32) -> Option<usize> {
    let id = widget.0;
    if id >= base && id < base + max {
        Some((id - base) as usize)
    } else {
        None
    }
}

/// True for any widget id this page registers - static or dynamic
/// (radio or chevron click target). Used by the run loop's Enter
/// dispatch to route activations to
/// [`crate::tui::activate_select_location_widget`].
pub fn owns_widget(widget: WidgetId) -> bool {
    widget == widgets::SEARCH_ANCHOR
        || widget == widgets::FILTER_BUTTON
        || widget == widgets::ENTRY_TAB
        || widget == widgets::EXIT_TAB
        || owns_tree_row(widget)
        || country_chevron_index(widget).is_some()
        || city_chevron_index(widget).is_some()
}

/// The node the page is effectively editing. With multihop off, entry
/// selection is meaningless, so the page is always editing the exit
/// node regardless of any stale `node_mode` left from a prior
/// multihop-on visit.
fn effective_mode(app: &App) -> NodeKind {
    if app.is_multihop_enabled() {
        app.select_location_page_state().node_mode()
    } else {
        NodeKind::Exit
    }
}

/// Coerce the stored `node_mode` to the effective mode and return it,
/// so the per-mode [`crate::app::pages::select_location::PageState`]
/// accessors (expansion, scroll) operate on the mode actually being
/// rendered/activated. Callable through `&App` because `node_mode` is a
/// `Cell`.
fn sync_effective_mode(app: &App) -> NodeKind {
    let mode = effective_mode(app);
    app.select_location_page_state().set_node_mode(mode);
    mode
}

/// The current daemon selection for `mode` (entry vs exit).
fn current_selection_for_mode(app: &App, mode: NodeKind) -> CurrentRelaySelection<'_> {
    match mode {
        NodeKind::Entry => app.current_entry_relay_selection(),
        NodeKind::Exit => app.current_relay_selection(),
    }
}

/// The selection of the *other* node - the one that must be excluded
/// while editing `mode` (can't route in and out the same location).
fn other_node_selection(app: &App, mode: NodeKind) -> CurrentRelaySelection<'_> {
    match mode {
        NodeKind::Entry => app.current_relay_selection(),
        NodeKind::Exit => app.current_entry_relay_selection(),
    }
}

/// Run the action bound to a focused Select-location widget. Static
/// widgets (search anchor, filter button) and the dynamic per-row
/// radio ranges are all dispatched here.
pub async fn activate<S: MullvadService>(app: &mut App, service: &S, widget: WidgetId) {
    if widget == widgets::SEARCH_ANCHOR {
        // The search anchor's keystroke-by-keystroke filter mode
        // lands in a follow-up; today the anchor is focusable so
        // arrow-key nav can land on it, but Enter is a no-op.
        return;
    }
    if widget == widgets::FILTER_BUTTON {
        app.enter_sub_page(crate::app::PageId::SelectLocationFilter);
        return;
    }
    // Entry/Exit tab: switch which node the page edits, then land focus
    // on that node's current selection (or the tab itself when there's
    // nothing to focus, e.g. the entry list is hidden because DAITA is
    // overriding the entry).
    if widget == widgets::ENTRY_TAB || widget == widgets::EXIT_TAB {
        let mode = if widget == widgets::ENTRY_TAB {
            NodeKind::Entry
        } else {
            NodeKind::Exit
        };
        app.select_location_page_state().set_node_mode(mode);
        let hidden_entry = matches!(mode, NodeKind::Entry) && app.daita_overrides_entry();
        if hidden_entry || !focus_current_selection(app, mode) {
            app.page_focus_mut().focused = Some(widget);
        }
        return;
    }

    // The node we're editing - exit, or (with multihop on) whichever
    // tab is active. Syncs the per-mode page state so the tree walks
    // below read the right expansion sets.
    let mode = sync_effective_mode(app);

    // Per-row focusables: re-project the *filtered* tree to map
    // indices back to (country / city / relay) entities. Must match
    // the renderer's `collect_visible_rows` walk - same filter, same
    // force-expand-when-filtering override - or activation would land
    // on the wrong row when a filter is in effect. Cheap: a few
    // BTreeMap walks over the cached relay list.
    let tree = project_filtered_tree(app);
    let force_expand = filter_active(app);

    // Country radio: select country (for the active node). Refuse the
    // selection if it collides with the other node's location.
    if let Some(idx) = country_radio_index(widget) {
        if let Some(country) = tree.get(idx) {
            let code = country.code.to_string();
            let blocked = app.is_multihop_enabled()
                && matches!(
                    other_node_selection(app, mode),
                    CurrentRelaySelection::Country(c) if c == code.as_str()
                );
            if blocked {
                app.show_notification(SAME_LOCATION_MSG);
                return;
            }
            let result = match mode {
                NodeKind::Entry => app.select_entry_relay_country(service, &code).await,
                NodeKind::Exit => app.select_relay_country(service, &code).await,
            };
            match result {
                Ok(()) => after_select_success(app, mode),
                Err(error) => {
                    app.show_notification(format_action_error("select country", &error));
                }
            }
        }
        return;
    }

    // Country chevron: toggle expansion. Mirrors the keyboard's
    // `←/→` semantics - when the country is force-expanded by an
    // active filter (`force_expand`), the chevron renders as ▲ but
    // the persisted bit may be false. Refuse to touch the persisted
    // bit so the user doesn't accidentally hide a country whose
    // descendants matched their query (cf. `handle_tree_arrow`'s
    // `Left, Country, force_expand` branch). Re-points focus at the
    // row's radio so subsequent arrow-key navigation behaves the
    // same as if the user had clicked the radio rather than the
    // chevron.
    if let Some(idx) = country_chevron_index(widget) {
        if let Some(country) = tree.get(idx) {
            let code = country.code.to_string();
            let radio_id = WidgetId(COUNTRY_RADIO_BASE + idx as u32);
            if !force_expand {
                if app.select_location_page_state().is_country_expanded(&code) {
                    app.select_location_page_state_mut().collapse_country(&code);
                } else {
                    app.select_location_page_state_mut().expand_country(&code);
                }
            }
            app.page_focus_mut().focused = Some(radio_id);
        }
        return;
    }

    // City rows are flattened across all expanded countries - walk
    // the tree in render order to find the (country, city) at `idx`.
    // The trailing `bool` carries `force_expand_under_filter` so the
    // chevron handler below can mirror the keyboard's "don't collapse
    // a force-expanded city" rule without re-walking the tree.
    let cities: Vec<(&str, &str, &str, &str, bool)> = tree
        .iter()
        .filter(|c| force_expand || app.select_location_page_state().is_country_expanded(c.code))
        .flat_map(|c| {
            c.cities.iter().map(move |city| {
                (
                    c.code,
                    c.name,
                    city.code,
                    city.name,
                    city.force_expand_under_filter,
                )
            })
        })
        .collect();

    if let Some(idx) = city_radio_index(widget) {
        if let Some(&(country_code, _, city_code, ..)) = cities.get(idx) {
            let cc = country_code.to_string();
            let ct = city_code.to_string();
            let blocked = app.is_multihop_enabled()
                && matches!(
                    other_node_selection(app, mode),
                    CurrentRelaySelection::City { city, .. } if city == ct.as_str()
                );
            if blocked {
                app.show_notification(SAME_LOCATION_MSG);
                return;
            }
            let result = match mode {
                NodeKind::Entry => app.select_entry_relay_city(service, &cc, &ct).await,
                NodeKind::Exit => app.select_relay_city(service, &cc, &ct).await,
            };
            match result {
                Ok(()) => after_select_success(app, mode),
                Err(error) => {
                    app.show_notification(format_action_error("select city", &error));
                }
            }
        }
        return;
    }

    // City chevron: toggle expansion. Same force-expand rule as the
    // country chevron, but checked per-city - a city kept open by a
    // pure-hostname filter match (`force_expand_under_filter`) refuses
    // to flip the persisted bit, so a click on its ▲ chevron is a
    // no-op rather than hiding the user's search hit.
    if let Some(idx) = city_chevron_index(widget) {
        if let Some(&(country_code, _, city_code, _, force_expand_city)) = cities.get(idx) {
            let cc = country_code.to_string();
            let ct = city_code.to_string();
            let radio_id = WidgetId(CITY_RADIO_BASE + idx as u32);
            if !force_expand_city {
                if app.select_location_page_state().is_city_expanded(&cc, &ct) {
                    app.select_location_page_state_mut().collapse_city(&cc, &ct);
                } else {
                    app.select_location_page_state_mut().expand_city(&cc, &ct);
                }
            }
            app.page_focus_mut().focused = Some(radio_id);
        }
        return;
    }

    // Relay rows: flatten across expanded cities. Compute the
    // hostname into an owned `String` before any `&mut app` call so
    // the immutable borrow held by `tree` / `state` is released first.
    if let Some(idx) = relay_radio_index(widget) {
        let host: Option<String> = {
            let state = app.select_location_page_state();
            tree.iter()
                .filter(|c| force_expand || state.is_country_expanded(c.code))
                .flat_map(|c| {
                    c.cities
                        .iter()
                        .filter(|city| {
                            // Same per-city expansion rule as the
                            // renderer: a city is expanded under a
                            // filter only when `force_expand_under_filter`
                            // is set (pure-hostname match) or when
                            // the user manually expanded it. The
                            // `force_expand` country-level guard is
                            // still in use by the outer country
                            // filter at the parent iterator.
                            city.force_expand_under_filter
                                || state.is_city_expanded(c.code, city.code)
                        })
                        .flat_map(|city| city.relays.iter().map(|r| r.hostname.as_str()))
                })
                .nth(idx)
                .map(String::from)
        };
        // `tree` (and the `state` borrow inside it) drops here; safe
        // to take `&mut app` next.
        drop(tree);
        if let Some(host) = host {
            let blocked = app.is_multihop_enabled()
                && matches!(
                    other_node_selection(app, mode),
                    CurrentRelaySelection::Hostname(h) if h == host.as_str()
                );
            if blocked {
                app.show_notification(SAME_LOCATION_MSG);
                return;
            }
            let result = match mode {
                NodeKind::Entry => app.select_entry_relay(service, &host).await,
                NodeKind::Exit => app.select_relay(service, &host).await,
            };
            match result {
                Ok(()) => after_select_success(app, mode),
                Err(error) => {
                    app.show_notification(format_action_error("select relay", &error));
                }
            }
        }
    }
}

/// After a successful selection, decide what happens to the page. An
/// exit selection closes the sub-page (today's behavior). An entry
/// selection keeps the page open and flips to the Exit tab so the
/// user can pick the exit next, mirroring the desktop GUI's
/// entry->exit flow; focus lands on the exit's current selection
/// (or the Exit tab when there's nothing to focus).
fn after_select_success(app: &mut App, mode: NodeKind) {
    match mode {
        NodeKind::Exit => app.leave_sub_page(),
        NodeKind::Entry => {
            app.select_location_page_state()
                .set_node_mode(NodeKind::Exit);
            if !focus_current_selection(app, NodeKind::Exit) {
                app.page_focus_mut().focused = Some(widgets::EXIT_TAB);
            }
        }
    }
}

/// True only for the per-tree-row radio widget ids (country / city /
/// relay). The run loop uses this to decide whether ←/→ should route
/// through the tree-arrow handler or fall through to the generic
/// focus-engine navigation (search anchor and filter button keep
/// today's within-row behavior).
pub fn owns_tree_row(widget: WidgetId) -> bool {
    country_radio_index(widget).is_some()
        || city_radio_index(widget).is_some()
        || relay_radio_index(widget).is_some()
}

/// Selection-driven snapshot of the row the page should focus when
/// it opens. Owned strings instead of borrows from `app` so the
/// caller can hold one across the mutable borrows that apply
/// expansions / set focus.
enum FocusTarget {
    Country(String),
    City { country: String, city: String },
    Hostname(String),
}

/// Open the Select-location sub-page focused on the daemon's current
/// relay selection. Expands the country / city that contains the
/// selection so its row is part of the tree the renderer registers,
/// then overrides `page_focus.focused` to the matching radio widget.
///
/// The order is deliberate: `enter_sub_page` runs first so it
/// captures the activating button (`[Select location]`) as
/// `return_focus`; only after that do we override the focused widget.
/// Pressing Esc therefore lands the user back on `[Select location]`,
/// not on the relay row they were just on.
///
/// Falls through to the renderer's default body-first focus snap
/// when the daemon hasn't reported a settings snapshot yet
/// (`Unknown`), the constraint is `Any` / a custom list / a custom
/// tunnel, or the relay list doesn't yet contain the selected entry.
pub fn enter_with_current_selection_focused(app: &mut App) {
    use crate::app::PageId;

    // The Status `[Switch location]` button is exit-centric, so always
    // open editing the exit node - even if the page was left on the
    // Entry tab from a previous visit.
    app.select_location_page_state()
        .set_node_mode(NodeKind::Exit);

    // Push the sub-page first so it captures the activating button as
    // `return_focus`; only after that do we override the focused
    // widget. Pressing Esc therefore lands the user back on
    // `[Switch location]`, not on the relay row they were just on.
    app.enter_sub_page(PageId::SelectLocation);
    focus_current_selection(app, NodeKind::Exit);
}

/// Expand the tree onto `mode`'s current daemon selection and focus its
/// row, centering it on the next frame. Assumes the page's `node_mode`
/// is already set to `mode` (the per-mode expansion sets are keyed off
/// it). Returns `true` when it set focus to a selection row, `false`
/// when there's nothing to focus - the daemon hasn't reported settings
/// yet (`Unknown`), the constraint is `Any` / a custom list / a custom
/// tunnel, or the relay list doesn't yet contain the selected entry.
/// Used both on page open (exit) and after an entry selection flips to
/// the Exit tab.
fn focus_current_selection(app: &mut App, mode: NodeKind) -> bool {
    // Snapshot the selection into owned strings before any mutable
    // borrow of `app` - the selection borrows from the cached settings.
    let target: Option<FocusTarget> = match current_selection_for_mode(app, mode) {
        CurrentRelaySelection::Country(c) => Some(FocusTarget::Country(c.to_string())),
        CurrentRelaySelection::City { country, city } => Some(FocusTarget::City {
            country: country.to_string(),
            city: city.to_string(),
        }),
        CurrentRelaySelection::Hostname(h) => Some(FocusTarget::Hostname(h.to_string())),
        CurrentRelaySelection::Unknown
        | CurrentRelaySelection::Any
        | CurrentRelaySelection::CustomList
        | CurrentRelaySelection::CustomTunnel => None,
    };

    // For Hostname selections we need the (country, city) the relay
    // belongs to so the parent rows can be expanded. Resolve from the
    // cached relay list now, before borrowing `app` mutably.
    let hostname_parents: Option<(String, String)> = match &target {
        Some(FocusTarget::Hostname(host)) => app
            .relay_locations()
            .iter()
            .find(|r| r.hostname.as_str() == host.as_str())
            .map(|r| (r.country_code.clone(), r.city_code.clone())),
        _ => None,
    };

    // Apply expansions so the target row will be visible in the
    // projected tree. The setters are idempotent so we can call them
    // unconditionally without risk of collapsing a row the user kept
    // open across earlier visits.
    if let Some(tgt) = &target {
        let state = app.select_location_page_state_mut();
        match tgt {
            FocusTarget::Country(_) => {}
            FocusTarget::City { country, .. } => {
                state.expand_country(country);
            }
            FocusTarget::Hostname(_) => {
                if let Some((cc, ct)) = &hostname_parents {
                    state.expand_country(cc);
                    state.expand_city(cc, ct);
                }
            }
        }
    }

    // Compute the widget id of the row to focus *after* expansions
    // have been applied - `collect_visible_rows`-style flat indices
    // depend on which countries/cities are currently expanded.
    let focus_widget = target
        .as_ref()
        .and_then(|tgt| focus_widget_for_target(app, tgt));

    if let Some(id) = focus_widget {
        app.page_focus_mut().focused = Some(id);
        // Ask the renderer to center the focused row vertically on
        // the next frame instead of just sliding it into view -
        // landing the selected location in the middle of the
        // visible window matches what the user expects from
        // "open the list on my current selection".
        app.select_location_page_state().request_center_focused();
        true
    } else {
        false
    }
}

/// Compute the widget id to focus for a given selection target,
/// using the same flat-index scheme `collect_visible_rows` uses so
/// the id matches whatever the next render registers. Returns
/// `None` if the target isn't in the current tree (e.g. the relay
/// list doesn't contain the daemon-reported hostname).
fn focus_widget_for_target(app: &App, target: &FocusTarget) -> Option<WidgetId> {
    // Walk the *filtered* tree so the flat indices computed here line
    // up with what the renderer's `collect_visible_rows` produces. The
    // page-open path runs before any new typing so the query is
    // whatever was persisted from the user's last visit; with no query
    // this is a passthrough.
    let tree = project_filtered_tree(app);
    let state = app.select_location_page_state();
    let force_expand = filter_active(app);
    let country_expanded = |code: &str| force_expand || state.is_country_expanded(code);
    // Match the renderer's per-city rule (see `collect_visible_rows`):
    // a city counts as expanded when `force_expand_under_filter` is
    // set (pure-hostname match) or when the user manually expanded it.
    let city_expanded = |country_code: &str, city_code: &str, city: &CityNode<'_>| {
        city.force_expand_under_filter || state.is_city_expanded(country_code, city_code)
    };
    match target {
        FocusTarget::Country(code) => {
            let idx = tree.iter().position(|c| c.code == code.as_str())?;
            row_id_or_none(idx, COUNTRY_MAX, COUNTRY_RADIO_BASE)
        }
        FocusTarget::City { country, city } => {
            let mut city_idx = 0usize;
            for c in &tree {
                if !country_expanded(c.code) {
                    continue;
                }
                for ct in &c.cities {
                    if c.code == country.as_str() && ct.code == city.as_str() {
                        return row_id_or_none(city_idx, CITY_MAX, CITY_RADIO_BASE);
                    }
                    city_idx += 1;
                }
            }
            None
        }
        FocusTarget::Hostname(host) => {
            let mut relay_idx = 0usize;
            for c in &tree {
                if !country_expanded(c.code) {
                    continue;
                }
                for ct in &c.cities {
                    if !city_expanded(c.code, ct.code, ct) {
                        continue;
                    }
                    for r in &ct.relays {
                        if r.hostname.as_str() == host.as_str() {
                            return row_id_or_none(relay_idx, RELAY_MAX, RELAY_RADIO_BASE);
                        }
                        relay_idx += 1;
                    }
                }
            }
            None
        }
    }
}

// ---- Renderer ----

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let multihop = app.is_multihop_enabled();
    // Coerce/read the node being edited and point the per-mode page
    // state at it before any tree walk reads expansion state.
    let mode = sync_effective_mode(app);

    // When multihop is on, carve an Entry/Exit tab row above the search
    // row. With multihop off there's only the exit node, so no tabs.
    let body = if multihop {
        let [tab_row, rest] = Layout::vertical([
            Constraint::Length(1), // Entry/Exit tabs
            Constraint::Min(0),    // search row + tree
        ])
        .areas(area);
        render_mode_tabs(frame, tab_row, mode, focused, registry);
        rest
    } else {
        area
    };

    // DAITA override guard: in Entry mode the daemon may insert its own
    // entry hop, overriding any configured entry, so an entry list would
    // be misleading. Show an explanation and register no tree rows; the
    // Entry/Exit tabs above stay focusable so the user can switch back.
    if matches!(mode, NodeKind::Entry) && app.daita_overrides_entry() {
        render_daita_override_message(frame, body);
        return;
    }

    let tree = project_filtered_tree(app);
    let force_expand = filter_active(app);
    // Highlight the active node's selection; dim the other node's
    // selection as non-selectable (only meaningful under multihop).
    let selection = current_selection_for_mode(app, mode);
    let disabled = multihop.then(|| other_node_selection(app, mode));
    let page_state = app.select_location_page_state();

    // Top: search anchor (filling) + [Filter] button.
    // Below: an "All locations" header + the tree.
    let [search_row, tree_body] = Layout::vertical([
        Constraint::Length(1), // search row
        Constraint::Min(0),    // tree body
    ])
    .areas(body);

    render_search_row(frame, search_row, page_state.query(), focused, registry);

    let rows = collect_visible_rows(&tree, page_state, force_expand);

    // Reserve the rightmost column of the tree body for a scrollbar
    // when the projected list overflows. Capacity has to be measured
    // against the tree-content area (after carving the bar out), so
    // the scroll-window math doesn't count cells the bar will paint.
    let (tree_content_area, scrollbar_area) =
        components::split_for_vertical_scrollbar(tree_body, rows.len(), tree_body.height as usize);
    let capacity = tree_content_area.height as usize;

    // Adjust the scroll offset so the focused tree row stays in view.
    // ratatui's constraint solver collapses overflowing `Length(1)`
    // constraints by overlapping y-coordinates, so we must never
    // hand it more constraints than fit.
    //
    // On the first frame after the page opens (the open-path sets
    // `center_focused_pending`), prefer a centering offset so the
    // user's currently-selected location lands near the middle of
    // the visible window. Subsequent frames fall back to the minimal
    // slide-to-keep-visible offset that arrow-key navigation needs.
    let focused_idx = focused.and_then(|f| row_index_for_widget(&rows, f));
    // Track focus change before reading `is_user_scrolled` so a focus
    // move (arrow keys, click) clears the wheel-scroll override on the
    // same frame. Record dimensions so the wheel handler can clamp.
    page_state.observe_focus_for_scroll(focused);
    page_state.record_dimensions(rows.len(), capacity);
    let scroll_offset = if page_state.take_center_focused_request() {
        centered_scroll_offset(focused_idx, rows.len(), capacity)
    } else if page_state.is_user_scrolled() {
        // Mouse-wheel scrolled the viewport - keep the offset where
        // the user put it, only clamping to the post-projection max
        // (rows.len() can shrink between frames if a country was
        // collapsed since the wheel event).
        let max_offset = rows.len().saturating_sub(capacity);
        page_state.scroll_offset().min(max_offset)
    } else {
        clamp_scroll_offset(
            page_state.scroll_offset(),
            focused_idx,
            rows.len(),
            capacity,
        )
    };
    page_state.set_scroll_offset(scroll_offset);

    let visible_end = (scroll_offset + capacity).min(rows.len());
    let visible = &rows[scroll_offset..visible_end];

    let row_areas = if visible.is_empty() {
        std::rc::Rc::<[Rect]>::from([] as [Rect; 0])
    } else {
        Layout::vertical(visible.iter().map(|_| Constraint::Length(1))).split(tree_content_area)
    };

    // Register every tree row in declaration order so arrow-key
    // navigation can reach off-screen rows; the next render will
    // scroll the newly-focused one into view. Visible rows get their
    // real screen rect; off-screen rows get a zero-sized placeholder
    // (the focus engine's row-oriented navigation ignores the rect).
    //
    // Wrap the registration in a scroll group so `Home`/`End`/`PgUp`
    // /`PgDn` clamp inside the tree rather than escaping up to the
    // search anchor / `[Filter]` button (registered before this
    // block) or below to whatever the next page's chrome registers.
    registry.begin_scroll_group();
    let off_screen = Rect::default();
    for (idx, row) in rows.iter().enumerate() {
        let (rect, draw_visible) = if idx >= scroll_offset && idx < visible_end {
            (row_areas[idx - scroll_offset], true)
        } else {
            (off_screen, false)
        };
        if draw_visible {
            render_tree_row_visual(frame, rect, row, &selection, disabled.as_ref(), focused);
        }
        register_tree_row_focus(registry, row, rect);
    }
    registry.end_scroll_group();

    if let Some(bar_area) = scrollbar_area {
        components::render_vertical_scrollbar(frame, bar_area, rows.len(), scroll_offset, capacity);
    }
}

/// Look up the position in `rows` of whichever entry registers
/// `widget` (radio or chevron). Returns `None` for non-tree widget ids
/// (search anchor, filter button) or for ids past the per-level cap.
fn row_index_for_widget(rows: &[TreeRow<'_>], widget: WidgetId) -> Option<usize> {
    rows.iter().position(|row| row_owns_widget(row, widget))
}

fn row_owns_widget(row: &TreeRow<'_>, widget: WidgetId) -> bool {
    let same = |id: Option<WidgetId>| id == Some(widget);
    match row.kind {
        TreeRowKind::Country { .. } => {
            same(row_id_or_none(row.index, COUNTRY_MAX, COUNTRY_RADIO_BASE))
        }
        TreeRowKind::City { .. } => same(row_id_or_none(row.index, CITY_MAX, CITY_RADIO_BASE)),
        TreeRowKind::Relay { .. } => same(row_id_or_none(row.index, RELAY_MAX, RELAY_RADIO_BASE)),
    }
}

/// Walk back from `child_idx` to the most recent ancestor row.
/// Returns the row index of the parent country for a `City`, the
/// parent city for a `Relay`, and `None` for a `Country` (no parent).
fn parent_row_index(rows: &[TreeRow<'_>], child_idx: usize) -> Option<usize> {
    let target_country = match rows.get(child_idx)?.kind {
        TreeRowKind::City { .. } => true,
        TreeRowKind::Relay { .. } => false,
        TreeRowKind::Country { .. } => return None,
    };
    rows[..child_idx].iter().rposition(|row| {
        if target_country {
            matches!(row.kind, TreeRowKind::Country { .. })
        } else {
            matches!(row.kind, TreeRowKind::City { .. })
        }
    })
}

/// First-child row of `parent_idx` in render order, if it has any
/// children currently expanded into the row list. Children always sit
/// immediately after the parent in `collect_visible_rows`'s output, so
/// "first child" reduces to "next row, if its kind is the expected
/// child kind".
fn first_child_row_index(rows: &[TreeRow<'_>], parent_idx: usize) -> Option<usize> {
    let parent = rows.get(parent_idx)?;
    let next = rows.get(parent_idx + 1)?;
    let ok = matches!(
        (parent.kind, next.kind),
        (TreeRowKind::Country { .. }, TreeRowKind::City { .. })
            | (TreeRowKind::City { .. }, TreeRowKind::Relay { .. })
    );
    ok.then_some(parent_idx + 1)
}

/// Radio WidgetId for `row` (the only focusable on a tree row, post-
/// chevron-removal). Mirrors `register_tree_row_focus`'s id choice.
fn widget_for_row(row: &TreeRow<'_>) -> Option<WidgetId> {
    match row.kind {
        TreeRowKind::Country { .. } => row_id_or_none(row.index, COUNTRY_MAX, COUNTRY_RADIO_BASE),
        TreeRowKind::City { .. } => row_id_or_none(row.index, CITY_MAX, CITY_RADIO_BASE),
        TreeRowKind::Relay { .. } => row_id_or_none(row.index, RELAY_MAX, RELAY_RADIO_BASE),
    }
}

/// Decision computed by [`handle_tree_arrow`] before any `&mut App`
/// borrow is taken - keeps the row vector (which borrows immutably
/// from the app's relay list and PageState) alive only as long as
/// it's needed to read.
enum TreeArrowAction {
    ExpandCountry(String),
    CollapseCountry(String),
    ExpandCity(String, String),
    CollapseCity(String, String),
    SetFocus(WidgetId),
    /// Key consumed but no state change (e.g. ← on a top-level
    /// collapsed country, or → on a country with no children).
    Noop,
    /// Key not consumed by the tree-arrow handler - let the run loop
    /// fall through to the generic focus-engine navigation.
    PassThrough,
}

/// Standard tree-view ←/→ semantics on a country / city / relay row.
/// Returns `true` when the key was consumed (state mutation, focus
/// move, or explicit no-op); `false` to let the run loop fall through
/// to the generic focus navigation. Truth table:
///
/// | Row state              | →                            | ←                  |
/// |------------------------|------------------------------|--------------------|
/// | Country, collapsed     | expand                       | no-op (no parent)  |
/// | Country, expanded      | focus first city             | collapse           |
/// | City, collapsed        | expand                       | focus parent       |
/// | City, expanded         | focus first relay            | collapse           |
/// | Relay (leaf)           | falls through (Down)         | focus parent       |
///
/// While a search filter is active, `collect_visible_rows` shows
/// children regardless of the persisted expansion bit (force-expand);
/// in that mode → on a country/city always behaves like "expanded"
/// (focus first child) and ← never collapses the persisted bit, so
/// clearing the filter restores the user's pre-filter expansion
/// state intact.
pub fn handle_tree_arrow(app: &mut App, focused: WidgetId, dir: ArrowDir) -> bool {
    use ArrowDir::{Left, Right};
    if !matches!(dir, Left | Right) {
        return false;
    }

    let action = {
        let tree = project_filtered_tree(app);
        let force_expand = filter_active(app);
        let rows = collect_visible_rows(
            tree.as_slice(),
            app.select_location_page_state(),
            force_expand,
        );
        let Some(row_idx) = row_index_for_widget(&rows, focused) else {
            return false;
        };

        match (dir, rows[row_idx].kind) {
            (Right, TreeRowKind::Country { code, expanded, .. }) => {
                if expanded {
                    first_child_row_index(&rows, row_idx)
                        .and_then(|i| widget_for_row(&rows[i]))
                        .map_or(TreeArrowAction::Noop, TreeArrowAction::SetFocus)
                } else {
                    TreeArrowAction::ExpandCountry(code.to_string())
                }
            }
            (Right, TreeRowKind::City { code, expanded, .. }) => {
                if expanded {
                    first_child_row_index(&rows, row_idx)
                        .and_then(|i| widget_for_row(&rows[i]))
                        .map_or(TreeArrowAction::Noop, TreeArrowAction::SetFocus)
                } else {
                    // The previous form short-circuited with
                    // `expanded || force_expand`, which broke the
                    // manual-expand path for cities kept in the
                    // filtered tree by a name-only match (e.g. typing
                    // `"us"` matches `"Austria"`, leaving Vienna in
                    // the tree but not force-expanded - the user
                    // could then never expand Vienna because the
                    // dispatch tried to navigate to a non-existent
                    // first child instead of expanding).
                    let country_code =
                        parent_row_index(&rows, row_idx).and_then(|p| match rows[p].kind {
                            TreeRowKind::Country { code, .. } => Some(code.to_string()),
                            _ => None,
                        });
                    match country_code {
                        Some(cc) => TreeArrowAction::ExpandCity(cc, code.to_string()),
                        None => TreeArrowAction::Noop,
                    }
                }
            }
            (Right, TreeRowKind::Relay { .. }) => TreeArrowAction::PassThrough,

            (Left, TreeRowKind::Country { code, expanded, .. }) => {
                // Country is force-expanded under any active filter;
                // refuse to collapse so the user doesn't accidentally
                // hide a country whose name (or descendants) just
                // matched their query.
                if expanded && !force_expand {
                    TreeArrowAction::CollapseCountry(code.to_string())
                } else {
                    // Top-level row, no parent. Consume the key so
                    // the generic Left doesn't snap us elsewhere.
                    TreeArrowAction::Noop
                }
            }
            (
                Left,
                TreeRowKind::City {
                    code,
                    expanded,
                    force_expand_under_filter,
                    ..
                },
            ) => {
                // Refuse collapse only when the filter itself is
                // forcing this city open (a pure-hostname match with
                // no country/city-name match). A city expanded
                // manually under a filter that didn't force-expand it
                // should still collapse on Left - the user opened it,
                // the user should be able to close it.
                if expanded && !force_expand_under_filter {
                    let country_code =
                        parent_row_index(&rows, row_idx).and_then(|p| match rows[p].kind {
                            TreeRowKind::Country { code, .. } => Some(code.to_string()),
                            _ => None,
                        });
                    match country_code {
                        Some(cc) => TreeArrowAction::CollapseCity(cc, code.to_string()),
                        None => TreeArrowAction::Noop,
                    }
                } else {
                    parent_row_index(&rows, row_idx)
                        .and_then(|i| widget_for_row(&rows[i]))
                        .map_or(TreeArrowAction::Noop, TreeArrowAction::SetFocus)
                }
            }
            (Left, TreeRowKind::Relay { .. }) => parent_row_index(&rows, row_idx)
                .and_then(|i| widget_for_row(&rows[i]))
                .map_or(TreeArrowAction::Noop, TreeArrowAction::SetFocus),

            // Up/Down already filtered out at the top of the function.
            _ => TreeArrowAction::PassThrough,
        }
    };

    match action {
        TreeArrowAction::ExpandCountry(cc) => {
            app.select_location_page_state_mut().expand_country(&cc);
            true
        }
        TreeArrowAction::CollapseCountry(cc) => {
            app.select_location_page_state_mut().collapse_country(&cc);
            true
        }
        TreeArrowAction::ExpandCity(cc, ct) => {
            app.select_location_page_state_mut().expand_city(&cc, &ct);
            true
        }
        TreeArrowAction::CollapseCity(cc, ct) => {
            app.select_location_page_state_mut().collapse_city(&cc, &ct);
            true
        }
        TreeArrowAction::SetFocus(widget) => {
            app.page_focus_mut().focused = Some(widget);
            true
        }
        TreeArrowAction::Noop => true,
        TreeArrowAction::PassThrough => false,
    }
}

/// Pick a scroll offset that places `focused_idx` near the vertical
/// center of the visible window - used on the first frame after the
/// page opens so the daemon's currently-selected location appears
/// roughly in the middle of the body rather than pinned to the top.
///
/// Returns `0` when the list fits without scrolling, when `capacity`
/// is `0`, or when there's no focused row to center on. Otherwise:
/// position the focus at `capacity / 2` rows from the top of the
/// window, then clamp into `[0, rows_len - capacity]` so the window
/// never extends past the end of the list.
fn centered_scroll_offset(focused_idx: Option<usize>, rows_len: usize, capacity: usize) -> usize {
    if rows_len <= capacity || capacity == 0 {
        return 0;
    }
    let Some(idx) = focused_idx else {
        return 0;
    };
    let max_offset = rows_len - capacity;
    let half = capacity / 2;
    idx.saturating_sub(half).min(max_offset)
}

/// Slide `current` so that `focused_idx` (if any) lies within
/// `[offset, offset + capacity)`, then clamp to the valid range for
/// `rows_len` items in `capacity` slots.
fn clamp_scroll_offset(
    current: usize,
    focused_idx: Option<usize>,
    rows_len: usize,
    capacity: usize,
) -> usize {
    if rows_len <= capacity || capacity == 0 {
        return 0;
    }
    let max_offset = rows_len - capacity;
    let mut offset = current.min(max_offset);
    if let Some(idx) = focused_idx {
        if idx < offset {
            offset = idx;
        } else if idx >= offset + capacity {
            offset = idx + 1 - capacity;
        }
    }
    offset.min(max_offset)
}

// ---- Row layout ----

/// One row in the rendered tree. `index` is the position in its
/// per-level sequence (country / city / relay), used to derive the
/// row's WidgetIds.
struct TreeRow<'a> {
    kind: TreeRowKind<'a>,
    index: usize,
}

#[derive(Clone, Copy)]
enum TreeRowKind<'a> {
    Country {
        name: &'a str,
        code: &'a str,
        expanded: bool,
    },
    City {
        name: &'a str,
        code: &'a str,
        expanded: bool,
        /// Mirrors [`CityNode::force_expand_under_filter`]. Carried
        /// down to [`handle_tree_arrow`] so a `Left` press on a city
        /// expanded *only* because the user manually expanded it
        /// (under a filter where this city wasn't force-expanded) can
        /// still collapse it.
        force_expand_under_filter: bool,
    },
    Relay {
        hostname: &'a str,
    },
}

fn collect_visible_rows<'a>(
    tree: &'a [CountryNode<'a>],
    state: &crate::app::pages::select_location::PageState,
    force_expand: bool,
) -> Vec<TreeRow<'a>> {
    let mut rows = Vec::new();
    // city_idx and relay_idx are flat counters across all expanded
    // sub-trees (same row index used at render and at dispatch); the
    // outer country counter naturally falls out of `.enumerate()`.
    let mut city_idx = 0usize;
    let mut relay_idx = 0usize;
    for (country_idx, country) in tree.iter().enumerate() {
        // `force_expand` overrides the user's collapse so a filtered
        // tree shows its matched cities/relays even when the user had
        // the country collapsed before they started typing.
        let expanded = force_expand || state.is_country_expanded(country.code);
        rows.push(TreeRow {
            kind: TreeRowKind::Country {
                name: country.name,
                code: country.code,
                expanded,
            },
            index: country_idx,
        });
        if expanded {
            for city in &country.cities {
                // City force-expansion is gated on
                // `force_expand_under_filter`, which `filter_tree`
                // sets only on pure-hostname matches (no country/city
                // name/code match shadowing the hostname hit). The
                // relay list itself is always preserved, so manual
                // `state.is_city_expanded` still works.
                let city_expanded = city.force_expand_under_filter
                    || state.is_city_expanded(country.code, city.code);
                rows.push(TreeRow {
                    kind: TreeRowKind::City {
                        name: city.name,
                        code: city.code,
                        expanded: city_expanded,
                        force_expand_under_filter: city.force_expand_under_filter,
                    },
                    index: city_idx,
                });
                city_idx += 1;
                if city_expanded {
                    for relay in &city.relays {
                        rows.push(TreeRow {
                            kind: TreeRowKind::Relay {
                                hostname: relay.hostname.as_str(),
                            },
                            index: relay_idx,
                        });
                        relay_idx += 1;
                    }
                }
            }
        }
    }
    rows
}

/// Paint a tree row at `area`. Does not touch the focus registry -
/// callers register the row's widget ids via [`register_tree_row_focus`]
/// so off-screen rows can also be navigated to.
fn render_tree_row_visual(
    frame: &mut Frame<'_>,
    area: Rect,
    row: &TreeRow<'_>,
    selection: &CurrentRelaySelection<'_>,
    disabled: Option<&CurrentRelaySelection<'_>>,
    focused: Option<WidgetId>,
) {
    match row.kind {
        TreeRowKind::Country {
            name,
            code,
            expanded,
        } => {
            let selected = matches!(selection, CurrentRelaySelection::Country(c) if *c == code);
            // Dimmed (cross-exclusion): this is the other node's
            // chosen location, which can't double as this node's.
            let dimmed = disabled
                .is_some_and(|d| matches!(d, CurrentRelaySelection::Country(c) if *c == code));
            render_radio_with_chevron_glyph(
                frame,
                area,
                // indent
                0,
                name,
                selected,
                dimmed,
                expanded,
                row_id_or_none(row.index, COUNTRY_MAX, COUNTRY_RADIO_BASE),
                focused,
            );
        }
        TreeRowKind::City {
            name,
            code,
            expanded,
            force_expand_under_filter: _,
        } => {
            let selected = matches!(
                selection,
                CurrentRelaySelection::City { city, .. } if *city == code
            );
            let dimmed = disabled.is_some_and(
                |d| matches!(d, CurrentRelaySelection::City { city, .. } if *city == code),
            );
            render_radio_with_chevron_glyph(
                frame,
                area,
                // indent
                2,
                name,
                selected,
                dimmed,
                expanded,
                row_id_or_none(row.index, CITY_MAX, CITY_RADIO_BASE),
                focused,
            );
        }
        TreeRowKind::Relay { hostname } => {
            let selected = matches!(
                selection,
                CurrentRelaySelection::Hostname(h) if *h == hostname
            );
            let dimmed = disabled
                .is_some_and(|d| matches!(d, CurrentRelaySelection::Hostname(h) if *h == hostname));
            render_relay_row_visual(
                frame,
                area,
                hostname,
                selected,
                dimmed,
                row_id_or_none(row.index, RELAY_MAX, RELAY_RADIO_BASE),
                focused,
            );
        }
    }
}

/// Register the focusables for `row` so arrow-key navigation can
/// reach it. `area` is the row's drawn rect for visible rows or a
/// zero-sized placeholder for off-screen rows; the focus engine's
/// row-oriented navigation ignores the rect, so a placeholder is
/// safe.
///
/// For visible country/city rows, also registers the chevron's
/// rightmost-3-cell rect as a mouse-only click target so a click on
/// `[▼]`/`[▲]` toggles expansion instead of selecting the relay
/// underneath. The chevron click target is intentionally **not** a
/// keyboard-navigable widget - `←/→` continues to drive expansion
/// via [`handle_tree_arrow`] on the radio.
fn register_tree_row_focus(registry: &mut FocusRegistry, row: &TreeRow<'_>, area: Rect) {
    let id = match row.kind {
        TreeRowKind::Country { .. } => row_id_or_none(row.index, COUNTRY_MAX, COUNTRY_RADIO_BASE),
        TreeRowKind::City { .. } => row_id_or_none(row.index, CITY_MAX, CITY_RADIO_BASE),
        TreeRowKind::Relay { .. } => row_id_or_none(row.index, RELAY_MAX, RELAY_RADIO_BASE),
    };
    if let Some(id) = id {
        registry.register(FocusableWidget {
            id,
            rect: area,
            kind: FocusKind::SelectOption,
        });
    }
    if area.height > 0 && area.width >= CHEVRON_LABEL_WIDTH {
        let chevron_id = match row.kind {
            TreeRowKind::Country { .. } => {
                row_id_or_none(row.index, COUNTRY_MAX, COUNTRY_CHEVRON_BASE)
            }
            TreeRowKind::City { .. } => row_id_or_none(row.index, CITY_MAX, CITY_CHEVRON_BASE),
            TreeRowKind::Relay { .. } => None,
        };
        if let Some(chevron_id) = chevron_id {
            let chevron_rect = Rect::new(
                area.x + area.width - CHEVRON_LABEL_WIDTH,
                area.y,
                CHEVRON_LABEL_WIDTH,
                1,
            );
            registry.register_click_target(chevron_id, chevron_rect);
        }
    }
    registry.end_row();
}

/// Width of the bracketed chevron glyph (`[▼]` or `[▲]`) in terminal
/// cells. Both glyphs are single-cell + 2 brackets - kept as a
/// `const` so the renderer's [`Layout::horizontal`] reservation and
/// the click-target rect computation can't drift apart.
const CHEVRON_LABEL_WIDTH: u16 = 3;

fn row_id_or_none(index: usize, max: u32, base: u32) -> Option<WidgetId> {
    if (index as u32) < max {
        Some(WidgetId(base + index as u32))
    } else {
        None
    }
}

/// Paint a `( ) <label>     [v]` row at `area`. The chevron is purely
/// indicative - it is only interactable with the mouse; ←/→ on the
/// row's radio drive expand/collapse and parent/child navigation.
#[expect(
    clippy::too_many_arguments,
    reason = "row helper bundles indent + label + radio id + focus + selection \
              flags; splitting into a struct produces a single-call-site type"
)]
fn render_radio_with_chevron_glyph(
    frame: &mut Frame<'_>,
    area: Rect,
    indent: u16,
    label: &str,
    radio_selected: bool,
    dimmed: bool,
    expanded: bool,
    radio_id: Option<WidgetId>,
    focused: Option<WidgetId>,
) {
    if area.height == 0 {
        return;
    }
    let chevron_glyph = if expanded {
        components::chevron(components::Chevron::Up)
    } else {
        components::chevron(components::Chevron::Down)
    };
    debug_assert_eq!(
        chevron_glyph.chars().count() as u16 + 2,
        CHEVRON_LABEL_WIDTH,
        "CHEVRON_LABEL_WIDTH must stay in sync with the chevron glyph + brackets",
    );

    let [radio_area, _gap, chevron_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(CHEVRON_LABEL_WIDTH),
    ])
    .areas(Rect::new(area.x, area.y, area.width, 1));

    let radio_text = format!(
        "{:indent$}{} {label}",
        "",
        components::radio_glyph(radio_selected),
        indent = indent as usize,
    );
    let radio_style = if radio_id.is_some() && focused == radio_id {
        // Focus wins over the dim so the user can still see where they
        // are while navigating through a non-selectable row.
        Style::new().yellow()
    } else if dimmed {
        Style::new().dark_gray()
    } else {
        Style::new()
    };
    frame.render_widget(Paragraph::new(radio_text).style(radio_style), radio_area);

    let chevron_text = format!("[{chevron_glyph}]");
    frame.render_widget(Paragraph::new(chevron_text), chevron_area);
}

fn render_relay_row_visual(
    frame: &mut Frame<'_>,
    area: Rect,
    hostname: &str,
    selected: bool,
    dimmed: bool,
    id: Option<WidgetId>,
    focused: Option<WidgetId>,
) {
    if area.height == 0 {
        return;
    }
    // Indented 4ch. Whole-row focusable.
    let text = format!("    {} {hostname}", components::radio_glyph(selected));
    let style = if id.is_some() && focused == id {
        // Focus wins over the dim so a non-selectable row still shows
        // where the cursor is.
        Style::new().yellow()
    } else if dimmed {
        Style::new().dark_gray()
    } else {
        Style::new()
    };
    frame.render_widget(
        Paragraph::new(text).style(style),
        Rect::new(area.x, area.y, area.width, 1),
    );
}

/// `[Entry] [Exit]` segmented control shown above the search row when
/// multihop is enabled. The active node is underlined (matching the
/// tab-bar's active-tab convention); a focused tab renders yellow. Both
/// tabs live on one focus row, so ←/→ move between them and ↓ drops to
/// the search row.
fn render_mode_tabs(
    frame: &mut Frame<'_>,
    area: Rect,
    active: NodeKind,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    const ENTRY: &str = "[Entry]";
    const EXIT: &str = "[Exit]";
    const GAP: u16 = 2;
    let total = ENTRY.len() as u16 + GAP + EXIT.len() as u16;
    let x = area.x + area.width.saturating_sub(total) / 2;
    let row = Rect::new(x, area.y, total.min(area.width), area.height.min(1));
    let [entry_area, _gap, exit_area] = Layout::horizontal([
        Constraint::Length(ENTRY.len() as u16),
        Constraint::Length(GAP),
        Constraint::Length(EXIT.len() as u16),
    ])
    .areas(row);
    render_mode_tab(
        frame,
        entry_area,
        ENTRY,
        widgets::ENTRY_TAB,
        matches!(active, NodeKind::Entry),
        focused,
        registry,
    );
    render_mode_tab(
        frame,
        exit_area,
        EXIT,
        widgets::EXIT_TAB,
        matches!(active, NodeKind::Exit),
        focused,
        registry,
    );
    registry.end_row();
}

/// One `[Entry]`/`[Exit]` tab cell: underlined when it's the active
/// node, yellow when focused, registered as a focusable button.
fn render_mode_tab(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    id: WidgetId,
    active: bool,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let mut style = if focused == Some(id) {
        Style::new().yellow()
    } else {
        Style::new()
    };
    if active {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    frame.render_widget(Paragraph::new(label).style(style), area);
    registry.register(FocusableWidget {
        id,
        rect: area,
        kind: FocusKind::Button,
    });
}

/// Replaces the entry list when DAITA is overriding the multihop entry
/// node. There's nothing to select, so explain why and point the user
/// at the DAITA settings. Registers no focusables - the Entry/Exit tabs
/// above remain the way back.
fn render_daita_override_message(frame: &mut Frame<'_>, area: Rect) {
    let text = "The multihop entry server is currently chosen by DAITA. To pick an entry \
                location, enable \"Direct only\" or disable DAITA in Settings.";
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .style(Style::new().dark_gray()),
        area,
    );
}

/// Search-anchor + filter-button row. The anchor is a focusable
/// styled like a black input field with placeholder text, mirroring
/// `logs.html`'s pattern.
fn render_search_row(
    frame: &mut Frame<'_>,
    area: Rect,
    query: &str,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    // Filter button hugs the right; search anchor fills the rest.
    let filter_label = "[Filter]";
    let filter_width = filter_label.len() as u16;
    let [anchor_area, filter_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(filter_width + 1)]).areas(area);

    // Search anchor: dark background, placeholder when query empty,
    // current query when non-empty. Focus highlight via `:focus` ->
    // yellow fg, plus an end-of-buffer cursor block so typing has
    // visual feedback.
    let is_focused = focused == Some(widgets::SEARCH_ANCHOR);
    let bg = Color::Indexed(234);
    let base_style = if is_focused {
        Style::new().yellow().bg(bg)
    } else if query.is_empty() {
        Style::new().dark_gray().bg(bg)
    } else {
        Style::new().white().bg(bg)
    };
    // Bracket color follows the focus state so the focusable
    // affordance reads the same as `[Filter]` to its right and the
    // page's other buttons (yellow when focused).
    let bracket_style = if is_focused {
        Style::new().yellow().bg(bg)
    } else {
        Style::new().white().bg(bg)
    };
    let line = if is_focused {
        // While focused, show the live query (no placeholder) plus a
        // block-cursor glyph at the caret. Reverse-video the cursor
        // so it stays visible against the dark anchor bg even when
        // the buffer is empty.
        let cursor = components::cursor_glyph_span(bg);
        Line::from(vec![Span::styled(query.to_string(), base_style), cursor])
    } else if query.is_empty() {
        Line::from(Span::styled(
            "Filter location list (/ to focus)".to_string(),
            base_style,
        ))
    } else {
        Line::from(Span::styled(query.to_string(), base_style))
    };
    // Reserve 1 cell on each edge of `anchor_area` for the brackets;
    // the inner band carries the existing content render. The whole
    // `anchor_area` (brackets included) is registered for focus so a
    // click anywhere on the pill, including the brackets, lands on
    // the search anchor.
    let [left_bracket_area, content_area, right_bracket_area] = Layout::horizontal([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(anchor_area);
    frame.render_widget(
        Paragraph::new(Span::styled("[", bracket_style)),
        left_bracket_area,
    );
    frame.render_widget(
        Paragraph::new(line).style(Style::new().bg(bg)),
        content_area,
    );
    frame.render_widget(
        Paragraph::new(Span::styled("]", bracket_style)),
        right_bracket_area,
    );
    registry.register(FocusableWidget {
        id: widgets::SEARCH_ANCHOR,
        rect: anchor_area,
        kind: FocusKind::TextInput,
    });

    // Filter button: standard bracketed button to the right of the
    // anchor, separated by a 1-cell gap inside `chunks[1]`.
    let button_area = Rect::new(filter_area.x + 1, filter_area.y, filter_width, 1);
    components::render_button(
        frame,
        button_area,
        "Filter",
        focused == Some(widgets::FILTER_BUTTON),
        registry,
        widgets::FILTER_BUTTON,
    );
    registry.end_row();
}

#[cfg(test)]
mod tests {
    use super::{
        tree::{filter_tree, hostname_matches, project_tree},
        *,
    };
    use crate::integration::RelayLocation;

    #[test]
    fn widget_id_decoders_are_disjoint() {
        // Each base+index decodes through its own helper and
        // returns None on the others. Catches accidental overlap
        // when MAXes change.
        let country_radio = WidgetId(COUNTRY_RADIO_BASE + 5);
        assert_eq!(country_radio_index(country_radio), Some(5));
        assert_eq!(city_radio_index(country_radio), None);
        assert_eq!(relay_radio_index(country_radio), None);

        let city_radio = WidgetId(CITY_RADIO_BASE + 12);
        assert_eq!(city_radio_index(city_radio), Some(12));
        assert_eq!(country_radio_index(city_radio), None);
        assert_eq!(relay_radio_index(city_radio), None);

        let relay_radio = WidgetId(RELAY_RADIO_BASE + 100);
        assert_eq!(relay_radio_index(relay_radio), Some(100));
        assert_eq!(country_radio_index(relay_radio), None);
        assert_eq!(city_radio_index(relay_radio), None);
    }

    #[test]
    fn owns_widget_recognizes_static_and_dynamic_ids() {
        assert!(owns_widget(widgets::SEARCH_ANCHOR));
        assert!(owns_widget(widgets::FILTER_BUTTON));
        assert!(owns_widget(widgets::ENTRY_TAB));
        assert!(owns_widget(widgets::EXIT_TAB));
        assert!(owns_widget(WidgetId(COUNTRY_RADIO_BASE)));
        assert!(owns_widget(WidgetId(RELAY_RADIO_BASE + RELAY_MAX - 1)));
        // Chevron click-target ids sit just past the relay range.
        assert!(owns_widget(WidgetId(COUNTRY_CHEVRON_BASE)));
        assert!(owns_widget(WidgetId(CITY_CHEVRON_BASE + CITY_MAX - 1)));
        // Outside any range - not owned.
        assert!(!owns_widget(WidgetId(0x40))); // settings range
        assert!(!owns_widget(WidgetId(CITY_CHEVRON_BASE + CITY_MAX)));
    }

    fn test_app_with_relays() -> App {
        let mut app = App::new();
        app.set_relay_locations(vec![
            RelayLocation {
                hostname: "se-got-wg-001".to_string(),
                country_name: "Sweden".to_string(),
                country_code: "se".to_string(),
                city_name: "Gothenburg".to_string(),
                city_code: "got".to_string(),
            },
            RelayLocation {
                hostname: "us-nyc-wg-001".to_string(),
                country_name: "USA".to_string(),
                country_code: "us".to_string(),
                city_name: "New York".to_string(),
                city_code: "nyc".to_string(),
            },
            RelayLocation {
                hostname: "us-lax-wg-001".to_string(),
                country_name: "USA".to_string(),
                country_code: "us".to_string(),
                city_name: "Los Angeles".to_string(),
                city_code: "lax".to_string(),
            },
        ]);
        app
    }

    /// Settings with multihop and an optional DAITA-overrides-entry
    /// state, for the Entry/Exit render tests. Exit/entry default to
    /// `se` (the daemon defaults).
    fn settings_multihop_daita(
        multihop: bool,
        daita_overrides_entry: bool,
    ) -> crate::integration::Settings {
        use mullvad_types::relay_constraints::{
            RelayConstraints, RelaySettings, WireguardConstraints,
        };
        let mut s = crate::integration::Settings {
            relay_settings: RelaySettings::Normal(RelayConstraints {
                wireguard_constraints: WireguardConstraints {
                    use_multihop: multihop,
                    ..WireguardConstraints::default()
                },
                ..RelayConstraints::default()
            }),
            ..crate::integration::Settings::default()
        };
        if daita_overrides_entry {
            // "Direct only" off = `use_multihop_if_necessary` on.
            s.tunnel_options.wireguard.daita.enabled = true;
            s.tunnel_options.wireguard.daita.use_multihop_if_necessary = true;
        }
        s
    }

    #[test]
    fn entry_exit_tabs_render_only_with_multihop() {
        let mut app = test_app_with_relays();

        let lines = render_screen(&app, 50, 20);
        assert!(
            !lines.iter().any(|l| l.contains("[Entry]")),
            "no Entry/Exit tabs when multihop is off",
        );

        app.set_settings(settings_multihop_daita(true, false));
        let lines = render_screen(&app, 50, 20);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("[Entry]") && l.contains("[Exit]")),
            "Entry/Exit tabs appear when multihop is on",
        );
    }

    #[test]
    fn entry_mode_renders_daita_override_message_without_a_tree() {
        let mut app = test_app_with_relays();
        app.set_settings(settings_multihop_daita(true, true));
        app.select_location_page_state()
            .set_node_mode(super::NodeKind::Entry);

        let lines = render_screen(&app, 60, 20);
        assert!(
            lines.iter().any(|l| l.contains("DAITA")),
            "Entry tab shows the DAITA-override message",
        );
        assert!(
            !lines.iter().any(|l| l.contains("Sweden")),
            "the entry tree is hidden while DAITA overrides the entry",
        );
        // The tabs are still present so the user can switch back to Exit.
        assert!(
            lines
                .iter()
                .any(|l| l.contains("[Entry]") && l.contains("[Exit]")),
        );
    }

    #[test]
    fn search_anchor_is_wrapped_with_focusable_brackets() {
        // The search anchor pill is wrapped with `[` and `]` so it
        // reads as a focusable widget, matching the convention used
        // by `[Filter]` to its right and the page's other buttons.
        // Brackets render yellow when the anchor has focus, default
        // otherwise.
        use ratatui::{Terminal, backend::TestBackend, layout::Rect, style::Color};

        for focused_now in [true, false] {
            let mut registry = FocusRegistry::default();
            let backend = TestBackend::new(50, 1);
            let mut terminal = Terminal::new(backend).unwrap();
            let buf = terminal
                .draw(|frame| {
                    render_search_row(
                        frame,
                        Rect::new(0, 0, 50, 1),
                        "",
                        focused_now.then_some(widgets::SEARCH_ANCHOR),
                        &mut registry,
                    );
                })
                .unwrap();

            // Iterate cells (not bytes) - the focused-empty render
            // includes a `█` cursor glyph that occupies 1 cell but
            // takes 3 UTF-8 bytes, so a byte-offset `String::find`
            // would point to the wrong column.
            let cell_at = |x: u16| buf.buffer[(x, 0)].symbol();
            let open = (0..buf.area.width)
                .find(|&x| cell_at(x) == "[")
                .unwrap_or_else(|| panic!("anchor `[` missing (focused={focused_now})"));
            let close = ((open + 1)..buf.area.width)
                .find(|&x| cell_at(x) == "]")
                .unwrap_or_else(|| panic!("anchor `]` missing (focused={focused_now})"));
            assert_eq!(open, 0, "anchor `[` must sit at the row's left edge");
            assert!(close > open, "brackets out of order");

            let expected_fg = if focused_now {
                Color::Yellow
            } else {
                Color::White
            };
            assert_eq!(
                buf.buffer[(open, 0)].fg,
                expected_fg,
                "open bracket fg should track focus (focused={focused_now})",
            );
            assert_eq!(
                buf.buffer[(close, 0)].fg,
                expected_fg,
                "close bracket fg should track focus (focused={focused_now})",
            );
        }
    }

    fn render_screen(app: &App, width: u16, height: u16) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut registry = FocusRegistry::default();
        let buf = terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                render(frame, area, app, &mut registry);
            })
            .unwrap();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Variant of [`render_screen`] that also persists the built focus
    /// registry on `app` (mirroring what the run loop does between
    /// frames) so subsequent input dispatch can read it via
    /// `app.last_focus_registry()`.
    fn render_screen_persisting_registry(app: &mut App, width: u16, height: u16) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut registry = FocusRegistry::default();
        let buf = terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                render(frame, area, app, &mut registry);
            })
            .unwrap();
        app.set_focus_registry(registry, None);
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn test_app_with_many_relays() -> App {
        // 5 countries, alphabetical by code: ar, au, de, se, us.
        // Some have multiple cities; us has multiple relays per city.
        let mut app = App::new();
        app.set_relay_locations(vec![
            RelayLocation {
                hostname: "ar-bue-wg-001".into(),
                country_name: "Argentina".into(),
                country_code: "ar".into(),
                city_name: "Buenos Aires".into(),
                city_code: "bue".into(),
            },
            RelayLocation {
                hostname: "au-bne-wg-301".into(),
                country_name: "Australia".into(),
                country_code: "au".into(),
                city_name: "Brisbane".into(),
                city_code: "bne".into(),
            },
            RelayLocation {
                hostname: "au-syd-wg-001".into(),
                country_name: "Australia".into(),
                country_code: "au".into(),
                city_name: "Sydney".into(),
                city_code: "syd".into(),
            },
            RelayLocation {
                hostname: "de-fra-wg-001".into(),
                country_name: "Germany".into(),
                country_code: "de".into(),
                city_name: "Frankfurt".into(),
                city_code: "fra".into(),
            },
            RelayLocation {
                hostname: "se-got-wg-001".into(),
                country_name: "Sweden".into(),
                country_code: "se".into(),
                city_name: "Gothenburg".into(),
                city_code: "got".into(),
            },
            RelayLocation {
                hostname: "us-lax-wg-001".into(),
                country_name: "USA".into(),
                country_code: "us".into(),
                city_name: "Los Angeles".into(),
                city_code: "lax".into(),
            },
            RelayLocation {
                hostname: "us-nyc-wg-001".into(),
                country_name: "USA".into(),
                country_code: "us".into(),
                city_name: "New York".into(),
                city_code: "nyc".into(),
            },
            RelayLocation {
                hostname: "us-nyc-wg-002".into(),
                country_name: "USA".into(),
                country_code: "us".into(),
                city_name: "New York".into(),
                city_code: "nyc".into(),
            },
        ]);
        app
    }

    #[test]
    fn overflowing_tree_renders_only_visible_window_in_order() {
        // 50 countries each with 2 cities; expand one country to force
        // overflow. Without scroll-windowing, ratatui's constraint
        // solver collapses overlapping y-coordinates and the renderer
        // overpaints rows on top of each other (the bug user reported).
        let mut app = App::new();
        let mut relays = Vec::new();
        for i in 0..50u32 {
            let code = format!("c{i:02}");
            let country_name = format!("Country{i:02}");
            relays.push(RelayLocation {
                hostname: format!("{code}-aa-wg-001"),
                country_name: country_name.clone(),
                country_code: code.clone(),
                city_name: "AlphaCity".into(),
                city_code: "aa".into(),
            });
            relays.push(RelayLocation {
                hostname: format!("{code}-bb-wg-001"),
                country_name,
                country_code: code,
                city_name: "BetaCity".into(),
                city_code: "bb".into(),
            });
        }
        app.set_relay_locations(relays);
        app.select_location_page_state_mut().expand_country("c01");

        let screen = render_screen(&app, 50, 30);

        // Drop the search anchor row and trim trailing blanks
        // (overflow rows past the body cap render empty, that's the
        // desired behavior).
        let body: Vec<&String> = screen
            .iter()
            .skip(1)
            .filter(|line| !line.is_empty())
            .collect();

        // The first six rendered rows MUST match the prefix of the
        // tree in declaration order: Country00, Country01, AlphaCity,
        // BetaCity, Country02, Country03 - no skipped rows, no bleed.
        let firsts: Vec<&String> = body.iter().take(6).copied().collect();
        assert!(firsts[0].contains("Country00"), "row 0: {}", firsts[0]);
        assert!(firsts[1].contains("Country01"), "row 1: {}", firsts[1]);
        assert!(firsts[2].contains("AlphaCity"), "row 2: {}", firsts[2]);
        assert!(firsts[3].contains("BetaCity"), "row 3: {}", firsts[3]);
        assert!(firsts[4].contains("Country02"), "row 4: {}", firsts[4]);
        assert!(firsts[5].contains("Country03"), "row 5: {}", firsts[5]);
        // No bleed-through: a country row never carries leftover text
        // from the city row that previously occupied that y.
        for (idx, line) in firsts.iter().enumerate() {
            assert!(
                !line.contains("ity") || line.contains("Cit"),
                "row {idx} has city-name fragment bleed: {line}",
            );
        }
    }

    #[test]
    fn overflow_renders_a_scrollbar_in_the_rightmost_column() {
        let mut app = App::new();
        let mut relays = Vec::new();
        for i in 0..50u32 {
            relays.push(RelayLocation {
                hostname: format!("c{i:02}-aa-wg-001"),
                country_name: format!("Country{i:02}"),
                country_code: format!("c{i:02}"),
                city_name: "AlphaCity".into(),
                city_code: "aa".into(),
            });
        }
        app.set_relay_locations(relays);
        let screen = render_screen(&app, 50, 30);

        // Body starts at y=2 (search anchor + label). The bar lives in
        // the rightmost column of the tree body. Look for any of
        // ratatui's vertical-scrollbar glyphs there.
        let bar_glyphs = ['║', '█', '▌', '▐', '┃', '│'];
        let bar_present = (2..screen.len()).any(|y| {
            screen[y]
                .chars()
                .last()
                .is_some_and(|c| bar_glyphs.contains(&c))
        });
        assert!(
            bar_present,
            "scrollbar should render in the rightmost column when the tree overflows; \
             screen:\n{}",
            screen.join("\n"),
        );
    }

    #[test]
    fn no_overflow_means_no_scrollbar() {
        // With only 3 countries the tree fits entirely; no bar.
        let app = test_app_with_relays();
        let screen = render_screen(&app, 50, 30);
        let bar_glyphs = ['║', '█', '▌', '▐', '┃'];
        let bar_present = (2..screen.len()).any(|y| {
            screen[y]
                .chars()
                .last()
                .is_some_and(|c| bar_glyphs.contains(&c))
        });
        assert!(
            !bar_present,
            "scrollbar should NOT appear when the tree fits; screen:\n{}",
            screen.join("\n"),
        );
    }

    #[test]
    fn page_down_then_render_scrolls_far_down_the_tree() {
        let mut app = App::new();
        let mut relays = Vec::new();
        for i in 0..50u32 {
            relays.push(RelayLocation {
                hostname: format!("c{i:02}-aa-wg-001"),
                country_name: format!("Country{i:02}"),
                country_code: format!("c{i:02}"),
                city_name: "AlphaCity".into(),
                city_code: "aa".into(),
            });
        }
        app.set_relay_locations(relays);

        // Seed focus on the first country so PgDn has something to
        // anchor against, then render once to populate
        // `last_focus_registry` (the dispatch path reads from the
        // *previous* frame's registry).
        app.page_focus_mut().focused = Some(WidgetId(COUNTRY_RADIO_BASE));
        let _ = render_screen_persisting_registry(&mut app, 50, 30);

        // Three PgDn presses should advance focus by 3 *
        // PAGE_STEP_ROWS = 30 rows, well past the body's 28-row
        // capacity. The scroll-on-focus logic then pulls the
        // newly-focused row into view, scrolling earlier countries
        // off the top.
        let mut cursor = WidgetId(COUNTRY_RADIO_BASE);
        for _ in 0..3 {
            cursor = app
                .last_focus_registry()
                .move_rows(cursor, crate::tui::keybindings::PAGE_STEP_ROWS as isize)
                .expect("PgDn should land somewhere");
        }
        app.page_focus_mut().focused = Some(cursor);
        let screen = render_screen(&app, 50, 30);

        // After 3 PgDns the focused row is Country30; the window
        // should scroll so Country30 appears, and Country00 is no
        // longer visible.
        let saw_country30 = screen.iter().any(|line| line.contains("Country30"));
        let saw_country00 = screen.iter().any(|line| line.contains("Country00"));
        assert!(
            saw_country30 && !saw_country00,
            "PgDn x 3 should scroll Country30 into view and Country00 out; \
             screen:\n{}",
            screen.join("\n"),
        );
    }

    #[test]
    fn home_in_the_tree_does_not_escape_to_the_search_anchor() {
        let app = test_app_with_relays();
        // Render once to seed the focus registry, then place focus
        // on the second country (a tree row inside the scroll
        // group).
        let mut app = app;
        let _ = render_screen_persisting_registry(&mut app, 50, 30);
        let second_country_id = WidgetId(COUNTRY_RADIO_BASE + 1);
        app.page_focus_mut().focused = Some(second_country_id);

        // `Home` from inside the tree should clamp to the first
        // tree row, not jump above to the search anchor.
        let registry = app.last_focus_registry();
        let target = registry
            .first_in_scroll_group(second_country_id)
            .or_else(|| registry.first_body_widget());
        assert_eq!(
            target,
            Some(WidgetId(COUNTRY_RADIO_BASE)),
            "Home should clamp to the first tree row, not the search anchor",
        );
    }

    #[test]
    fn page_up_from_top_tree_row_does_not_escape_above_the_list() {
        // 50 countries; expand none. Place focus on the first
        // country and PgUp by a large delta. The clamped move
        // should keep us on the first country, not bubble up to
        // the search anchor.
        let mut app = App::new();
        let mut relays = Vec::new();
        for i in 0..50u32 {
            relays.push(RelayLocation {
                hostname: format!("c{i:02}-aa-wg-001"),
                country_name: format!("Country{i:02}"),
                country_code: format!("c{i:02}"),
                city_name: "AlphaCity".into(),
                city_code: "aa".into(),
            });
        }
        app.set_relay_locations(relays);
        app.page_focus_mut().focused = Some(WidgetId(COUNTRY_RADIO_BASE));
        let _ = render_screen_persisting_registry(&mut app, 50, 30);
        let registry = app.last_focus_registry();
        let target = registry
            .move_rows_in_scroll_group(WidgetId(COUNTRY_RADIO_BASE), -100)
            .or_else(|| registry.move_rows(WidgetId(COUNTRY_RADIO_BASE), -100));
        assert_eq!(
            target,
            Some(WidgetId(COUNTRY_RADIO_BASE)),
            "PgUp at the top of the tree should stay on the first country, \
             not escape to the search anchor",
        );
    }

    #[test]
    fn focused_off_screen_row_scrolls_into_view() {
        let mut app = App::new();
        let mut relays = Vec::new();
        for i in 0..50u32 {
            relays.push(RelayLocation {
                hostname: format!("c{i:02}-aa-wg-001"),
                country_name: format!("Country{i:02}"),
                country_code: format!("c{i:02}"),
                city_name: "AlphaCity".into(),
                city_code: "aa".into(),
            });
        }
        app.set_relay_locations(relays);
        // Focus the radio of the 40th country (index 40, far past
        // body capacity). The renderer should scroll so it appears.
        app.page_focus_mut().focused = Some(WidgetId(COUNTRY_RADIO_BASE + 40));
        let screen = render_screen(&app, 50, 30);
        let saw_country40 = screen.iter().any(|line| line.contains("Country40"));
        assert!(
            saw_country40,
            "Country40 should be visible after scroll; screen:\n{}",
            screen.join("\n"),
        );
    }

    #[test]
    fn dump_screen_when_country_and_city_are_expanded() {
        let mut app = test_app_with_many_relays();
        app.select_location_page_state_mut().expand_country("au");
        app.select_location_page_state_mut()
            .expand_city("au", "bne");
        // Drive the renderer at the actual page area the run loop
        // hands it (~50 x ~30 post-chrome). Keep the area
        // generous so overflow isn't masking the row layout.
        let screen = render_screen(&app, 50, 30);
        eprintln!("=== screen (ar/au-bne expanded) ===");
        for (i, line) in screen.iter().enumerate() {
            eprintln!("{i:>3}: {line}");
        }
        eprintln!("===");

        // What we expect, in order, starting after the header rows:
        // Argentina / Australia / Brisbane / au-bne-wg-301 / Sydney / Germany / Sweden / USA
        let mut order: Vec<&str> = Vec::new();
        for line in &screen {
            for needle in [
                "Argentina",
                "Australia",
                "Brisbane",
                "au-bne-wg-301",
                "Sydney",
                "Germany",
                "Sweden",
                "USA",
            ] {
                if line.contains(needle) {
                    order.push(needle);
                }
            }
        }
        assert_eq!(
            order,
            vec![
                "Argentina",
                "Australia",
                "Brisbane",
                "au-bne-wg-301",
                "Sydney",
                "Germany",
                "Sweden",
                "USA",
            ],
            "rows should appear in tree order, screen:\n{}",
            screen.join("\n"),
        );
    }

    // --- filter query plumbing ---

    #[test]
    fn empty_query_is_passthrough_and_keeps_full_tree() {
        let app = test_app_with_relays();
        let unfiltered = project_tree(&app);
        let filtered = filter_tree(project_tree(&app), "");
        assert_eq!(unfiltered.len(), filtered.len());
        assert_eq!(
            unfiltered.iter().map(|c| c.code).collect::<Vec<_>>(),
            filtered.iter().map(|c| c.code).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn filter_by_country_name_keeps_full_relay_lists_without_force_expanding() {
        // Matching the country ("usa") keeps every city under it so
        // the user can see what their search hit. Each city retains
        // its **full** relay list - manual expansion needs to render
        // those relays - but `force_expand_under_filter` is `false`
        // because the country-name match shadows the hostname match,
        // and the renderer reads the flag to leave the city collapsed
        // by default.
        let app = test_app_with_relays();
        let filtered = filter_tree(project_tree(&app), "usa");
        let codes: Vec<&str> = filtered.iter().map(|c| c.code).collect();
        assert_eq!(codes, vec!["us"]);
        let us = &filtered[0];
        let city_codes: Vec<&str> = us.cities.iter().map(|c| c.code).collect();
        assert_eq!(city_codes, vec!["lax", "nyc"]);
        for city in &us.cities {
            assert!(
                !city.force_expand_under_filter,
                "country match should not force-expand {:?}",
                city.code,
            );
            assert!(
                !city.relays.is_empty(),
                "city {:?} should keep its full relay list for manual expansion",
                city.code,
            );
        }
    }

    #[test]
    fn filter_by_city_name_keeps_only_matching_subtree() {
        // City match keeps only the matched city under its country;
        // sibling cities are pruned. The country wraps it for context.
        let app = test_app_with_relays();
        let filtered = filter_tree(project_tree(&app), "gothen");
        assert_eq!(filtered.len(), 1, "only Sweden survives");
        let se = &filtered[0];
        assert_eq!(se.code, "se");
        let cities: Vec<&str> = se.cities.iter().map(|c| c.code).collect();
        assert_eq!(cities, vec!["got"]);
    }

    #[test]
    fn filter_by_relay_hostname_keeps_only_matching_relay() {
        // Hostname match keeps just that relay; sibling relays in the
        // same city are pruned.
        let app = test_app_with_relays();
        let filtered = filter_tree(project_tree(&app), "lax");
        assert_eq!(filtered.len(), 1);
        let us = &filtered[0];
        assert_eq!(
            us.cities.len(),
            1,
            "only Los Angeles survives, not New York"
        );
        let lax = &us.cities[0];
        let hosts: Vec<&str> = lax.relays.iter().map(|r| r.hostname.as_str()).collect();
        assert_eq!(hosts, vec!["us-lax-wg-001"]);
    }

    #[test]
    fn filter_with_no_matches_yields_empty_tree() {
        let app = test_app_with_relays();
        let filtered = filter_tree(project_tree(&app), "zzz-no-such-relay");
        assert!(filtered.is_empty());
    }

    #[test]
    fn filter_with_single_letter_inside_wg_only_does_not_match_wireguard_relay() {
        // Build a fixture where the country / city / their codes have
        // **no** "w" or "g" at all, so the only possible match for "w"
        // or "g" is inside `-wg-`. The relay must therefore be
        // filtered out - confirming the user-reported bug doesn't
        // reproduce here.
        let mut app = App::new();
        app.set_relay_locations(vec![RelayLocation {
            hostname: "fr-mrs-wg-001".to_string(), // Marseille, France
            country_name: "France".to_string(),
            country_code: "fr".to_string(),
            city_name: "Marseille".to_string(),
            city_code: "mrs".to_string(),
        }]);
        // Sanity: nothing in the names/codes contains "w" or "g".
        for piece in ["france", "fr", "marseille", "mrs"] {
            assert!(!piece.contains('w') && !piece.contains('g'));
        }

        for needle in ["w", "g", "wg"] {
            let filtered = filter_tree(project_tree(&app), needle);
            let total: usize = filtered
                .iter()
                .map(|c| c.cities.iter().map(|ci| ci.relays.len()).sum::<usize>())
                .sum();
            assert_eq!(
                total, 0,
                "needle {needle:?} should not match a relay whose only \
                 `w`/`g` characters live inside `-wg-`; got {total} relay(s)",
            );
        }
    }

    #[test]
    fn filter_with_query_wg_does_not_trivially_match_every_wireguard_relay() {
        // The literal `-wg-` protocol segment is identical on every
        // WireGuard relay's hostname. Stripping it from the matchable
        // form means a query like "wg" only matches if some *other*
        // part of the hostname (or the country/city) contains "wg" too.
        // The test fixture has none, so the result is empty.
        let app = test_app_with_relays();
        let filtered = filter_tree(project_tree(&app), "wg");
        assert!(
            filtered.is_empty(),
            "`wg` shouldn't match every WireGuard relay; got {} country(ies) back",
            filtered.len(),
        );
    }

    #[test]
    fn filter_still_matches_hostnames_outside_the_protocol_segment() {
        // The `-wg-` strip must not break legitimate hostname matches
        // on the city code or relay number - the fixture has three
        // relays and they all share the `-001` suffix, so `001` should
        // surface all of them.
        let app = test_app_with_relays();
        let filtered = filter_tree(project_tree(&app), "001");
        let count: usize = filtered
            .iter()
            .map(|c| c.cities.iter().map(|ci| ci.relays.len()).sum::<usize>())
            .sum();
        assert_eq!(
            count, 3,
            "`001` should match the relay-number suffix on every fixture relay",
        );
    }

    #[test]
    fn hostname_matches_rejects_query_entirely_inside_wg_segment() {
        // The "wg"-only query is the canonical "trivially matches
        // every WireGuard relay" case the filter is meant to suppress.
        assert!(!hostname_matches("se-got-wg-001", "wg"));
        // -wg, -w, wg: all fit inside `-wg-` and so are rejected.
        assert!(!hostname_matches("se-got-wg-001", "-wg"));
        assert!(!hostname_matches("se-got-wg-001", "wg-"));
        assert!(!hostname_matches("se-got-wg-001", "-wg-"));
    }

    #[test]
    fn hostname_matches_accepts_query_that_extends_beyond_wg_segment() {
        // The match range covers some non-`-wg-` character (`"got"`,
        // `"001"`, etc.), so `-wg-` is allowed to be *part* of the
        // match - just not the *only* part.
        assert!(hostname_matches("se-got-wg-001", "got-wg"));
        assert!(hostname_matches("se-got-wg-001", "wg-001"));
        assert!(hostname_matches("se-got-wg-001", "got"));
        assert!(hostname_matches("se-got-wg-001", "001"));
        // Whole hostname, of course.
        assert!(hostname_matches("se-got-wg-001", "se-got-wg-001"));
    }

    #[test]
    fn hostname_matches_is_case_insensitive() {
        // Caller passes a lower-cased needle; the helper lower-cases
        // the hostname.
        assert!(hostname_matches("US-LAX-WG-001", "lax"));
        assert!(!hostname_matches("US-LAX-WG-001", "wg"));
    }

    #[test]
    fn hostname_matches_falls_back_to_plain_substring_without_wg_segment() {
        // No `-wg-` segment -> no special carving; any substring is
        // a regular match.
        assert!(hostname_matches("ovpn-bridge-12", "bridge"));
        assert!(!hostname_matches("ovpn-bridge-12", "wg"));
    }

    #[test]
    fn country_name_match_shows_cities_collapsed_when_no_hostname_matches() {
        // Filter "usa" matches the country name. Cities under USA
        // appear (so the user sees the country opened up), but
        // since no relay hostname matches "usa" the cities are NOT
        // force-expanded - i.e., the relay rows underneath are not
        // rendered. The user can still manually expand a city to
        // browse it.
        let mut app = test_app_with_relays();
        app.navigate_to(crate::app::PageId::SelectLocation);
        for ch in "usa".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }
        let screen = render_screen(&app, 50, 12);
        let any_line_with = |needle: &str| screen.iter().any(|line| line.contains(needle));
        assert!(
            any_line_with("USA"),
            "country header rendered:\n{}",
            screen.join("\n")
        );
        assert!(
            any_line_with("Los Angeles"),
            "city visible:\n{}",
            screen.join("\n")
        );
        assert!(
            any_line_with("New York"),
            "sibling city visible:\n{}",
            screen.join("\n")
        );
        // Critically, no relay rows are rendered - the cities are
        // collapsed because the filter matched only the country name.
        assert!(
            !any_line_with("us-lax-wg-001"),
            "city should not be force-expanded when only the country name matched:\n{}",
            screen.join("\n"),
        );
        assert!(
            !any_line_with("us-nyc-wg-001"),
            "city should not be force-expanded when only the country name matched:\n{}",
            screen.join("\n"),
        );
    }

    #[test]
    fn manually_expanded_name_matched_city_renders_its_full_relay_list() {
        // After typing "usa", the cities under USA appear collapsed
        // (no hostname match). If the user then manually expands one,
        // the renderer must paint that city's relays - `filter_tree`
        // preserves the full relay list for exactly this reason.
        let mut app = test_app_with_relays();
        app.navigate_to(crate::app::PageId::SelectLocation);
        for ch in "usa".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }
        let before = render_screen(&app, 50, 12);
        assert!(
            !before.iter().any(|line| line.contains("us-lax-wg-001")),
            "city is collapsed by default under a name-only match:\n{}",
            before.join("\n"),
        );

        app.select_location_page_state_mut()
            .expand_city("us", "lax");
        let after = render_screen(&app, 50, 12);
        assert!(
            after.iter().any(|line| line.contains("us-lax-wg-001")),
            "manual expansion must render the city's full relay list:\n{}",
            after.join("\n"),
        );
    }

    #[test]
    fn city_name_match_shows_city_collapsed_when_no_hostname_matches() {
        // Filter "gothen" matches the city name "Gothenburg". The
        // city is visible but its relays are not rendered - the user
        // can manually expand to browse them.
        // (Note: `hostname_matches("se-got-wg-001", "gothen")` is
        // false - "gothen" doesn't appear in the hostname; only the
        // city name has it.)
        let mut app = test_app_with_relays();
        app.navigate_to(crate::app::PageId::SelectLocation);
        for ch in "gothen".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }
        let screen = render_screen(&app, 50, 12);
        let any_line_with = |needle: &str| screen.iter().any(|line| line.contains(needle));
        assert!(any_line_with("Sweden"));
        assert!(any_line_with("Gothenburg"));
        assert!(
            !any_line_with("se-got-wg-001"),
            "city should not be force-expanded when only the city name matched:\n{}",
            screen.join("\n"),
        );
    }

    #[test]
    fn typing_a_pure_hostname_match_renders_subtree_force_expanded() {
        // End-to-end: countries start collapsed, the user types a
        // query that hits *only* a relay hostname (not a country/city
        // name or code), and the matched relay's city is auto-
        // expanded so the user sees the match without drilling in.
        // We use the larger fixture so the test query (`"301"`,
        // matching `au-bne-wg-301`) doesn't double up on a city or
        // country name.
        let mut app = test_app_with_many_relays();
        app.navigate_to(crate::app::PageId::SelectLocation);
        assert!(!app.select_location_page_state().is_country_expanded("au"));
        for ch in "301".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }
        let screen = render_screen(&app, 50, 18);
        let any_line_with = |needle: &str| screen.iter().any(|line| line.contains(needle));
        assert!(
            any_line_with("Australia"),
            "country row visible:\n{}",
            screen.join("\n"),
        );
        assert!(
            any_line_with("Brisbane"),
            "matched relay's city row visible:\n{}",
            screen.join("\n"),
        );
        assert!(
            any_line_with("au-bne-wg-301"),
            "matched relay row force-expanded:\n{}",
            screen.join("\n"),
        );
        assert!(
            !any_line_with("Sydney"),
            "sibling city without a match is pruned:\n{}",
            screen.join("\n"),
        );
    }

    #[test]
    fn typing_a_city_code_match_keeps_city_collapsed_until_manual_expand() {
        // Counterpart to `typing_a_pure_hostname_match_*`: when the
        // query hits a city *code* (here `"lax"`), the user is
        // signaling "browse this city" - the relays underneath stay
        // collapsed. The user can still drill in by manually
        // expanding (`Right`), tested separately.
        let mut app = test_app_with_relays();
        app.navigate_to(crate::app::PageId::SelectLocation);
        for ch in "lax".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }
        let screen = render_screen(&app, 50, 12);
        let any_line_with = |needle: &str| screen.iter().any(|line| line.contains(needle));
        assert!(any_line_with("USA"));
        assert!(any_line_with("Los Angeles"));
        assert!(
            !any_line_with("us-lax-wg-001"),
            "city-code match must not auto-expand the relay row:\n{}",
            screen.join("\n"),
        );
        assert!(!any_line_with("Sweden"));
        assert!(!any_line_with("New York"));
    }

    #[test]
    fn expanding_country_lists_cities_directly_below_it() {
        let mut app = test_app_with_relays();
        app.select_location_page_state_mut().expand_country("us");
        let screen = render_screen(&app, 50, 12);
        // Header row (search anchor) then the tree:
        //   ( ) Sweden                                  [v]
        //   ( ) USA                                     [^]
        //     ( ) Los Angeles                           [v]
        //     ( ) New York                              [v]
        // The bug the user reported is rows landing in nonsensical
        // order; this asserts USA's cities appear directly under USA.
        let usa_idx = screen
            .iter()
            .position(|line| line.contains("USA"))
            .expect("USA row present");
        assert!(
            screen[usa_idx + 1].contains("Los Angeles"),
            "Los Angeles should land on the row directly below USA, screen:\n{}",
            screen.join("\n"),
        );
        assert!(
            screen[usa_idx + 2].contains("New York"),
            "New York should land on the second row below USA, screen:\n{}",
            screen.join("\n"),
        );
    }

    /// Build `Settings` whose `relay_settings` is `Normal` with a
    /// given `LocationConstraint`. Test-only sibling of the helper
    /// in `app::tests`.
    fn settings_with_location(
        location: mullvad_types::constraints::Constraint<
            mullvad_types::relay_constraints::LocationConstraint,
        >,
    ) -> mullvad_types::settings::Settings {
        use mullvad_types::{
            relay_constraints::{RelayConstraints, RelaySettings},
            settings::Settings,
        };
        Settings {
            relay_settings: RelaySettings::Normal(RelayConstraints {
                location,
                ..RelayConstraints::default()
            }),
            ..Settings::default()
        }
    }

    #[test]
    fn enter_with_country_selection_focuses_country_row_without_expansions() {
        use mullvad_types::{
            constraints::Constraint,
            relay_constraints::{GeographicLocationConstraint, LocationConstraint},
        };

        let mut app = test_app_with_relays();
        app.set_settings(settings_with_location(Constraint::Only(
            LocationConstraint::Location(GeographicLocationConstraint::Country("us".to_string())),
        )));

        enter_with_current_selection_focused(&mut app);

        // "us" is the second country in the projected tree (alpha by code).
        let tree = project_tree(&app);
        let us_idx = tree.iter().position(|c| c.code == "us").unwrap();
        assert_eq!(
            app.page_focus().focused,
            Some(WidgetId(COUNTRY_RADIO_BASE + us_idx as u32))
        );
        // Country selection doesn't expand anything - the user can
        // open the country themselves if they want to drill in.
        assert!(!app.select_location_page_state().is_country_expanded("us"));
    }

    #[test]
    fn enter_with_city_selection_expands_country_and_focuses_city_row() {
        use mullvad_types::{
            constraints::Constraint,
            relay_constraints::{GeographicLocationConstraint, LocationConstraint},
        };

        let mut app = test_app_with_many_relays();
        app.set_settings(settings_with_location(Constraint::Only(
            LocationConstraint::Location(GeographicLocationConstraint::City(
                "au".to_string(),
                "syd".to_string(),
            )),
        )));

        enter_with_current_selection_focused(&mut app);

        // The country was expanded so the city row is part of the tree.
        assert!(app.select_location_page_state().is_country_expanded("au"));

        // Sydney is the second city in expanded "au" (alphabetical:
        // Brisbane "bne", Sydney "syd"); no other countries are
        // expanded, so its flat city index is 1.
        assert_eq!(
            app.page_focus().focused,
            Some(WidgetId(CITY_RADIO_BASE + 1))
        );
    }

    #[test]
    fn enter_with_hostname_selection_expands_country_and_city_and_focuses_relay() {
        use mullvad_types::{
            constraints::Constraint,
            relay_constraints::{GeographicLocationConstraint, LocationConstraint},
        };

        let mut app = test_app_with_many_relays();
        app.set_settings(settings_with_location(Constraint::Only(
            LocationConstraint::Location(GeographicLocationConstraint::Hostname(
                "us".to_string(),
                "nyc".to_string(),
                "us-nyc-wg-002".to_string(),
            )),
        )));

        enter_with_current_selection_focused(&mut app);

        let state = app.select_location_page_state();
        assert!(state.is_country_expanded("us"));
        assert!(state.is_city_expanded("us", "nyc"));

        // No other countries / cities are expanded, so the flat relay
        // index over expanded cities is just the position within
        // nyc's relay list. nyc has us-nyc-wg-001 then us-nyc-wg-002,
        // so us-nyc-wg-002 is index 1.
        assert_eq!(
            app.page_focus().focused,
            Some(WidgetId(RELAY_RADIO_BASE + 1))
        );
    }

    #[test]
    fn enter_with_no_concrete_selection_leaves_focus_for_default_snap() {
        // CurrentRelaySelection::Any -> no row to focus. The function
        // should still push the sub-page; focus is left to the
        // renderer's body-first snap on the next render.
        let mut app = test_app_with_relays();
        app.set_settings(settings_with_location(
            mullvad_types::constraints::Constraint::Any,
        ));
        // Pre-set focus to something distinct so we can detect that
        // `enter_with_current_selection_focused` doesn't override it.
        app.page_focus_mut().focused = Some(widgets::SEARCH_ANCHOR);

        enter_with_current_selection_focused(&mut app);

        assert_eq!(app.page_focus().focused, Some(widgets::SEARCH_ANCHOR));
        assert!(!app.select_location_page_state().is_country_expanded("se"));
        assert!(!app.select_location_page_state().is_country_expanded("us"));
    }

    #[test]
    fn entering_then_leaving_select_location_restores_status_button_focus() {
        use mullvad_types::{
            constraints::Constraint,
            relay_constraints::{GeographicLocationConstraint, LocationConstraint},
        };

        // Focus the activating button on the parent page first, the
        // way the run loop would have it before dispatching Enter.
        let mut app = test_app_with_relays();
        app.set_settings(settings_with_location(Constraint::Only(
            LocationConstraint::Location(GeographicLocationConstraint::Country("us".to_string())),
        )));
        let activating = WidgetId(0x10 + 1); // StatusWidget::SwitchLocation
        app.page_focus_mut().focused = Some(activating);

        enter_with_current_selection_focused(&mut app);
        // Sub-page focus moved onto the matching country row...
        let tree = project_tree(&app);
        let us_idx = tree.iter().position(|c| c.code == "us").unwrap();
        assert_eq!(
            app.page_focus().focused,
            Some(WidgetId(COUNTRY_RADIO_BASE + us_idx as u32))
        );

        // ...and Esc-equivalent back-out restores the activating button.
        app.leave_sub_page();
        assert_eq!(app.page_focus().focused, Some(activating));
    }

    #[test]
    fn centered_scroll_offset_places_focus_at_window_midline() {
        // 100 rows, capacity 11 -> max_offset 89, half-window 5.
        // Focused at row 50 -> offset 45 puts focus on the 6th visible
        // row (50 - 45 = 5), which is the middle of an 11-row window.
        assert_eq!(centered_scroll_offset(Some(50), 100, 11), 45);
    }

    #[test]
    fn centered_scroll_offset_clamps_at_top_and_bottom() {
        // Focus near the top can't center because there's nothing
        // above to scroll past - offset clamps to 0.
        assert_eq!(centered_scroll_offset(Some(2), 100, 11), 0);
        // Focus near the bottom clamps to max_offset (89) so the
        // visible window doesn't run past the end of the list.
        assert_eq!(centered_scroll_offset(Some(98), 100, 11), 89);
    }

    #[test]
    fn centered_scroll_offset_returns_zero_when_list_fits() {
        // No scrolling possible / no focus -> start at row 0.
        assert_eq!(centered_scroll_offset(Some(3), 5, 10), 0);
        assert_eq!(centered_scroll_offset(None, 100, 11), 0);
        assert_eq!(centered_scroll_offset(Some(50), 100, 0), 0);
    }

    #[test]
    fn opening_with_hostname_selection_centres_relay_in_visible_window() {
        // Build a tree with enough relays-under-one-city that the
        // expanded list overflows the body, so centring actually
        // shifts the visible window. 30 relays in one city is
        // comfortably more than the body height we'll render at.
        use mullvad_types::{
            constraints::Constraint,
            relay_constraints::{GeographicLocationConstraint, LocationConstraint},
        };

        let mut app = App::new();
        let mut relays = Vec::with_capacity(30);
        for i in 0..30u32 {
            relays.push(RelayLocation {
                hostname: format!("us-nyc-wg-{i:03}"),
                country_name: "USA".into(),
                country_code: "us".into(),
                city_name: "New York".into(),
                city_code: "nyc".into(),
            });
        }
        app.set_relay_locations(relays);

        // Pick a relay near the middle of the list so the ideal
        // centring offset isn't clamped at either end.
        app.set_settings(settings_with_location(Constraint::Only(
            LocationConstraint::Location(GeographicLocationConstraint::Hostname(
                "us".to_string(),
                "nyc".to_string(),
                "us-nyc-wg-015".to_string(),
            )),
        )));

        enter_with_current_selection_focused(&mut app);

        // First render after open: centring is requested.
        let _ = render_screen_persisting_registry(&mut app, 50, 12);

        // Tree body height after the search row + outer chrome ends
        // up around 9-10 rows. The focused tree row (USA expanded ->
        // New York expanded -> relay 015) lives at rows index:
        // 1 (country) + 1 (city) + 15 = 17.
        // For body capacity ~ 9, half ~ 4, so the centered offset is
        // ~13 - i.e. far from 0. The exact body capacity depends on
        // ratatui's chrome math, so just assert "moved off the top".
        let offset = app.select_location_page_state().scroll_offset();
        assert!(
            offset > 0,
            "expected centered offset to scroll past the top; got {offset}",
        );

        // And the centring request is consumed: a second render
        // should fall back to clamping (offset stays where it is, no
        // jump). We verify by re-rendering and checking the offset
        // doesn't reset to a different "centered" value.
        let _ = render_screen_persisting_registry(&mut app, 50, 12);
        assert!(
            !app.select_location_page_state()
                .take_center_focused_request(),
            "center request should be one-shot; renderer must clear it",
        );
    }

    #[test]
    fn project_tree_groups_by_country_then_city() {
        // Build a small App with relays; assert the tree shape.
        let mut app = App::new();
        app.set_relay_locations(vec![
            RelayLocation {
                hostname: "se-got-wg-001".to_string(),
                country_name: "Sweden".to_string(),
                country_code: "se".to_string(),
                city_name: "Gothenburg".to_string(),
                city_code: "got".to_string(),
            },
            RelayLocation {
                hostname: "us-nyc-wg-001".to_string(),
                country_name: "USA".to_string(),
                country_code: "us".to_string(),
                city_name: "New York".to_string(),
                city_code: "nyc".to_string(),
            },
            RelayLocation {
                hostname: "se-got-wg-002".to_string(),
                country_name: "Sweden".to_string(),
                country_code: "se".to_string(),
                city_name: "Gothenburg".to_string(),
                city_code: "got".to_string(),
            },
        ]);
        let tree = project_tree(&app);
        assert_eq!(tree.len(), 2);
        // Alphabetical by country code: "se" before "us".
        assert_eq!(tree[0].code, "se");
        assert_eq!(tree[0].cities.len(), 1);
        assert_eq!(tree[0].cities[0].relays.len(), 2);
        assert_eq!(tree[1].code, "us");
    }

    // ---- Tree-view ←/→ navigation tests ----
    //
    // The fixture (`test_app_with_many_relays`) projects to:
    //   idx 0: ar (Argentina)        - 1 city  (bue)
    //   idx 1: au (Australia)        - 2 cities (bne, syd)
    //   idx 2: de (Germany)          - 1 city  (fra)
    //   idx 3: se (Sweden)           - 1 city  (got)
    //   idx 4: us (USA)              - 2 cities (lax, nyc) with 1 / 2 relays
    //
    // City and relay flat indices are assigned in render order across
    // all expanded countries (see `collect_visible_rows`).

    fn focus_country_radio(app: &mut App, country_idx: u32) {
        app.page_focus_mut().focused = Some(WidgetId(COUNTRY_RADIO_BASE + country_idx));
    }

    fn focus_city_radio(app: &mut App, city_idx: u32) {
        app.page_focus_mut().focused = Some(WidgetId(CITY_RADIO_BASE + city_idx));
    }

    fn focus_relay_radio(app: &mut App, relay_idx: u32) {
        app.page_focus_mut().focused = Some(WidgetId(RELAY_RADIO_BASE + relay_idx));
    }

    fn dispatch(app: &mut App, dir: ArrowDir) -> bool {
        let focused = app.page_focus().focused.expect("focus set in test setup");
        handle_tree_arrow(app, focused, dir)
    }

    #[test]
    fn right_on_collapsed_city_under_country_name_match_expands_it() {
        // Regression for the "us -> Austria -> Vienna" bug. Filtering
        // `"us"` matches the country name "Australia" (a-U-S-tralia)
        // but no city or hostname under it. Brisbane and Sydney show
        // up in the filtered tree but aren't force-expanded; the
        // user must still be able to expand them with `Right`. The
        // old dispatch short-circuited on `expanded || force_expand`
        // and tried to navigate to a non-existent first child
        // instead of expanding.
        let mut app = test_app_with_many_relays();
        for ch in "us".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }
        // Sanity: under "us", Australia is in the filtered tree
        // (country name matches) but Brisbane has no hostname match.
        let tree = project_filtered_tree(&app);
        let au = tree
            .iter()
            .find(|c| c.code == "au")
            .expect("Australia kept by country-name match");
        let bne = au
            .cities
            .iter()
            .find(|c| c.code == "bne")
            .expect("Brisbane kept under Australia");
        assert!(
            !bne.force_expand_under_filter,
            "Brisbane has no hostname match for `us` (and the country match \
             would shadow it anyway), so it should not force-expand",
        );

        // Filtered tree under "us" prunes Argentina/Germany/Sweden
        // (nothing in those countries contains "us"). What's left is
        // Australia (Brisbane, Sydney) and USA (LA, NYC). Brisbane is
        // the first city in render order, so city_idx = 0.
        focus_city_radio(&mut app, 0);

        let consumed = dispatch(&mut app, ArrowDir::Right);

        assert!(consumed);
        assert!(
            app.select_location_page_state()
                .is_city_expanded("au", "bne"),
            "Right should toggle the city's manual-expand state",
        );
    }

    #[test]
    fn left_on_manually_expanded_city_under_country_name_match_collapses_it() {
        // The flip side of `right_on_collapsed_city_under_country_...`:
        // once the user has manually expanded a name-only-matched
        // city, `Left` should collapse it. The previous dispatch
        // refused (`expanded && !force_expand` was false because
        // force_expand was true under any filter), so the user could
        // open Brisbane but never close it again without dropping the
        // filter.
        let mut app = test_app_with_many_relays();
        for ch in "us".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }
        // Manually expand Brisbane (the bug scenario).
        app.select_location_page_state_mut()
            .expand_city("au", "bne");
        focus_city_radio(&mut app, 0);

        let consumed = dispatch(&mut app, ArrowDir::Left);

        assert!(consumed);
        assert!(
            !app.select_location_page_state()
                .is_city_expanded("au", "bne"),
            "Left should collapse a manually-expanded name-only-matched city",
        );
    }

    #[test]
    fn left_on_force_expanded_city_does_not_collapse() {
        // A city that's force-expanded by the filter (e.g. LA under
        // "lax" - the hostname matches) must NOT be collapsible by
        // Left, otherwise the user would just hide their match. Left
        // should fall through to "move to parent" instead.
        let mut app = test_app_with_many_relays();
        for ch in "lax".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }
        // Under "lax", LA is the only filtered city in render order
        // (LA's hostname has "lax", NYC's doesn't, others pruned).
        focus_city_radio(&mut app, 0);

        let consumed = dispatch(&mut app, ArrowDir::Left);

        assert!(consumed);
        assert!(
            !app.select_location_page_state()
                .is_city_expanded("us", "lax"),
            "Left must not toggle the manual-collapse state of a force-expanded city",
        );
    }

    #[test]
    fn right_on_collapsed_country_expands_it() {
        let mut app = test_app_with_many_relays();
        focus_country_radio(&mut app, 1); // au
        let before = app.page_focus().focused;

        let consumed = dispatch(&mut app, ArrowDir::Right);

        assert!(consumed);
        assert!(app.select_location_page_state().is_country_expanded("au"));
        assert_eq!(
            app.page_focus().focused,
            before,
            "focus stays on the country it just expanded",
        );
    }

    #[test]
    fn right_on_expanded_country_focuses_first_city() {
        let mut app = test_app_with_many_relays();
        app.select_location_page_state_mut().expand_country("au");
        focus_country_radio(&mut app, 1); // au

        let consumed = dispatch(&mut app, ArrowDir::Right);

        assert!(consumed);
        // au has 2 cities; flat city_idx 0 = bne, 1 = syd. Only au is
        // expanded so bne is the first (and only) city in the row list.
        assert_eq!(
            app.page_focus().focused,
            Some(WidgetId(CITY_RADIO_BASE)),
            "focus moved to first child city",
        );
    }

    #[test]
    fn right_on_collapsed_city_expands_it() {
        let mut app = test_app_with_many_relays();
        app.select_location_page_state_mut().expand_country("au");
        focus_city_radio(&mut app, 0); // bne - first (and only) expanded city

        let consumed = dispatch(&mut app, ArrowDir::Right);

        assert!(consumed);
        assert!(
            app.select_location_page_state()
                .is_city_expanded("au", "bne")
        );
    }

    #[test]
    fn right_on_expanded_city_focuses_first_relay() {
        let mut app = test_app_with_many_relays();
        app.select_location_page_state_mut().expand_country("au");
        app.select_location_page_state_mut()
            .expand_city("au", "bne");
        focus_city_radio(&mut app, 0); // bne

        let consumed = dispatch(&mut app, ArrowDir::Right);

        assert!(consumed);
        // Only bne is expanded, so the first relay flat index is 0.
        assert_eq!(app.page_focus().focused, Some(WidgetId(RELAY_RADIO_BASE)));
    }

    #[test]
    fn right_on_relay_falls_through_to_down() {
        let mut app = test_app_with_many_relays();
        app.select_location_page_state_mut().expand_country("us");
        app.select_location_page_state_mut()
            .expand_city("us", "nyc");
        // us-nyc has 2 relays. After lax (1 relay), city_idx for nyc is 1
        // and relay_idx 0 = us-nyc-wg-001, relay_idx 1 = us-nyc-wg-002.
        focus_relay_radio(&mut app, 0);
        let before = app.page_focus().focused;

        let consumed = dispatch(&mut app, ArrowDir::Right);

        assert!(!consumed, "leaf relay falls through so run loop runs Down");
        assert_eq!(
            app.page_focus().focused,
            before,
            "handle_tree_arrow doesn't move focus on pass-through",
        );
    }

    #[test]
    fn left_on_expanded_country_collapses_it() {
        let mut app = test_app_with_many_relays();
        app.select_location_page_state_mut().expand_country("au");
        focus_country_radio(&mut app, 1);
        let before = app.page_focus().focused;

        let consumed = dispatch(&mut app, ArrowDir::Left);

        assert!(consumed);
        assert!(!app.select_location_page_state().is_country_expanded("au"));
        assert_eq!(app.page_focus().focused, before);
    }

    #[test]
    fn left_on_expanded_city_collapses_it() {
        let mut app = test_app_with_many_relays();
        app.select_location_page_state_mut().expand_country("au");
        app.select_location_page_state_mut()
            .expand_city("au", "bne");
        focus_city_radio(&mut app, 0); // bne
        let before = app.page_focus().focused;

        let consumed = dispatch(&mut app, ArrowDir::Left);

        assert!(consumed);
        assert!(
            !app.select_location_page_state()
                .is_city_expanded("au", "bne")
        );
        assert_eq!(app.page_focus().focused, before);
    }

    #[test]
    fn left_on_collapsed_city_focuses_parent_country() {
        let mut app = test_app_with_many_relays();
        app.select_location_page_state_mut().expand_country("au");
        // au: bne (city_idx 0), syd (city_idx 1) - neither expanded.
        focus_city_radio(&mut app, 0);

        let consumed = dispatch(&mut app, ArrowDir::Left);

        assert!(consumed);
        assert_eq!(
            app.page_focus().focused,
            Some(WidgetId(COUNTRY_RADIO_BASE + 1)),
            "focus moves to parent country (au)",
        );
    }

    #[test]
    fn left_on_relay_focuses_parent_city() {
        let mut app = test_app_with_many_relays();
        app.select_location_page_state_mut().expand_country("au");
        app.select_location_page_state_mut()
            .expand_city("au", "bne");
        focus_relay_radio(&mut app, 0); // au-bne-wg-301

        let consumed = dispatch(&mut app, ArrowDir::Left);

        assert!(consumed);
        // bne is the only expanded city -> city_idx 0.
        assert_eq!(app.page_focus().focused, Some(WidgetId(CITY_RADIO_BASE)));
        assert!(
            app.select_location_page_state()
                .is_city_expanded("au", "bne"),
            "city stays expanded; we only moved focus, not collapsed",
        );
    }

    #[test]
    fn left_on_collapsed_top_level_country_is_noop() {
        let mut app = test_app_with_many_relays();
        focus_country_radio(&mut app, 1); // au, collapsed
        let before = app.page_focus().focused;

        let consumed = dispatch(&mut app, ArrowDir::Left);

        assert!(consumed, "key consumed so generic Left doesn't fire");
        assert!(!app.select_location_page_state().is_country_expanded("au"));
        assert_eq!(app.page_focus().focused, before);
    }

    #[test]
    fn arrows_on_search_anchor_are_not_consumed_by_tree_handler() {
        // The run loop only calls handle_tree_arrow when
        // owns_tree_row(focused) is true; the search anchor isn't a
        // tree row, so it would never enter this path. Sanity-check
        // owns_tree_row's contract here so the run-loop guard stays
        // honest if either side moves.
        assert!(!owns_tree_row(widgets::SEARCH_ANCHOR));
        assert!(!owns_tree_row(widgets::FILTER_BUTTON));
        assert!(owns_tree_row(WidgetId(COUNTRY_RADIO_BASE)));
        assert!(owns_tree_row(WidgetId(CITY_RADIO_BASE)));
        assert!(owns_tree_row(WidgetId(RELAY_RADIO_BASE)));
    }

    #[test]
    fn under_filter_left_on_country_does_not_collapse_persisted_bit() {
        // With a search filter active, the renderer force-expands
        // matching subtrees. Pressing ← on a country in that mode
        // must not collapse the persisted expansion bit, so clearing
        // the filter restores the user's pre-filter state intact.
        let mut app = test_app_with_many_relays();
        // Persistently expand au, then apply a query that matches
        // only Australia ("au" -> "Australia"/"au"). The filtered tree
        // has au at idx 0.
        app.select_location_page_state_mut().expand_country("au");
        app.select_location_page_state_mut().push_query_char('a');
        app.select_location_page_state_mut().push_query_char('u');
        focus_country_radio(&mut app, 0);

        let consumed = dispatch(&mut app, ArrowDir::Left);

        assert!(consumed);
        assert!(
            app.select_location_page_state().is_country_expanded("au"),
            "<- under filter does not clear the persisted bit",
        );
    }

    #[test]
    fn under_filter_right_on_country_focuses_first_child() {
        // Force-expand override means → on a country whose persisted
        // bit is collapsed should still treat it as expanded and move
        // focus to the first child city (not flip the persisted bit).
        let mut app = test_app_with_many_relays();
        // au is collapsed in persisted state. Filter to Australia only.
        app.select_location_page_state_mut().push_query_char('a');
        app.select_location_page_state_mut().push_query_char('u');
        focus_country_radio(&mut app, 0);

        let consumed = dispatch(&mut app, ArrowDir::Right);

        assert!(consumed);
        assert!(
            !app.select_location_page_state().is_country_expanded("au"),
            "persisted bit unchanged under filter",
        );
        assert_eq!(
            app.page_focus().focused,
            Some(WidgetId(CITY_RADIO_BASE)),
            "focus moved to first city (force-expand treats it as already expanded)",
        );
    }

    // ---- Chevron click target tests ----
    //
    // The chevron is registered as a mouse-only click target (separate
    // from the radio's keyboard-focusable rect). A click on the
    // rightmost three cells of a country/city row toggles expansion
    // instead of selecting the relay underneath.

    #[test]
    fn chevron_widget_id_decoders_are_disjoint_from_radio_ids() {
        // Country / city chevron ids must not overlap with any radio
        // range - `owns_widget` distinguishes them in dispatch.
        let cc = WidgetId(COUNTRY_CHEVRON_BASE + 3);
        assert_eq!(country_chevron_index(cc), Some(3));
        assert_eq!(country_radio_index(cc), None);
        assert_eq!(city_radio_index(cc), None);
        assert_eq!(relay_radio_index(cc), None);

        let cy = WidgetId(CITY_CHEVRON_BASE + 7);
        assert_eq!(city_chevron_index(cy), Some(7));
        assert_eq!(country_chevron_index(cy), None);
        assert_eq!(city_radio_index(cy), None);
    }

    #[test]
    fn owns_widget_recognizes_chevron_ids() {
        assert!(owns_widget(WidgetId(COUNTRY_CHEVRON_BASE)));
        assert!(owns_widget(WidgetId(CITY_CHEVRON_BASE)));
        assert!(owns_widget(WidgetId(
            COUNTRY_CHEVRON_BASE + COUNTRY_MAX - 1
        )));
        assert!(!owns_widget(WidgetId(CITY_CHEVRON_BASE + CITY_MAX)));
    }

    #[tokio::test]
    async fn chevron_click_on_collapsed_country_expands_it_and_focuses_radio() {
        use crate::test_support::StubService;
        let mut app = test_app_with_many_relays();
        let service = StubService::default();
        // au is the second country in the projected tree (idx 1) and
        // starts collapsed.
        assert!(!app.select_location_page_state().is_country_expanded("au"));

        activate(&mut app, &service, WidgetId(COUNTRY_CHEVRON_BASE + 1)).await;

        assert!(app.select_location_page_state().is_country_expanded("au"));
        assert_eq!(
            app.page_focus().focused,
            Some(WidgetId(COUNTRY_RADIO_BASE + 1)),
            "focus snaps to the row's radio so subsequent arrow nav makes sense",
        );
        // No selection-changing RPC fired - the chevron toggles state
        // only, it doesn't pick a relay.
        assert!(service.set_relay_country_calls.borrow().is_empty());
        assert!(service.set_relay_calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn chevron_click_on_expanded_country_collapses_it() {
        use crate::test_support::StubService;
        let mut app = test_app_with_many_relays();
        let service = StubService::default();
        app.select_location_page_state_mut().expand_country("au");

        activate(&mut app, &service, WidgetId(COUNTRY_CHEVRON_BASE + 1)).await;

        assert!(!app.select_location_page_state().is_country_expanded("au"));
        assert_eq!(
            app.page_focus().focused,
            Some(WidgetId(COUNTRY_RADIO_BASE + 1)),
        );
    }

    #[tokio::test]
    async fn chevron_click_on_force_expanded_country_under_filter_is_noop() {
        // Mirrors `under_filter_left_on_country_does_not_collapse_persisted_bit`:
        // when a filter is forcing the country open, refusing the
        // click avoids hiding the user's search hit.
        use crate::test_support::StubService;
        let mut app = test_app_with_many_relays();
        let service = StubService::default();
        // Filter "au" matches the country name; au is at idx 0 in the
        // filtered tree.
        app.select_location_page_state_mut().push_query_char('a');
        app.select_location_page_state_mut().push_query_char('u');
        // Persisted bit starts collapsed and must remain so.
        assert!(!app.select_location_page_state().is_country_expanded("au"));

        activate(&mut app, &service, WidgetId(COUNTRY_CHEVRON_BASE)).await;

        assert!(
            !app.select_location_page_state().is_country_expanded("au"),
            "click on a force-expanded country must not flip the persisted bit",
        );
    }

    #[tokio::test]
    async fn chevron_click_on_collapsed_city_expands_it() {
        use crate::test_support::StubService;
        let mut app = test_app_with_many_relays();
        let service = StubService::default();
        app.select_location_page_state_mut().expand_country("au");
        // bne is the first city in the only expanded country -> city_idx 0.
        assert!(
            !app.select_location_page_state()
                .is_city_expanded("au", "bne")
        );

        activate(&mut app, &service, WidgetId(CITY_CHEVRON_BASE)).await;

        assert!(
            app.select_location_page_state()
                .is_city_expanded("au", "bne")
        );
        assert_eq!(app.page_focus().focused, Some(WidgetId(CITY_RADIO_BASE)),);
        assert!(service.set_relay_city_calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn chevron_click_on_expanded_city_collapses_it() {
        use crate::test_support::StubService;
        let mut app = test_app_with_many_relays();
        let service = StubService::default();
        app.select_location_page_state_mut().expand_country("au");
        app.select_location_page_state_mut()
            .expand_city("au", "bne");

        activate(&mut app, &service, WidgetId(CITY_CHEVRON_BASE)).await;

        assert!(
            !app.select_location_page_state()
                .is_city_expanded("au", "bne")
        );
    }

    #[tokio::test]
    async fn chevron_click_on_force_expanded_city_under_filter_is_noop() {
        // A pure-hostname filter match force-expands the matched relay's
        // city. Clicking the chevron on that city must not flip the
        // persisted bit - same rule as the keyboard ←
        // (`left_on_force_expanded_city_does_not_collapse`).
        //
        // "301" matches `au-bne-wg-301` and nothing in country/city
        // names or codes, so Brisbane gets `force_expand_under_filter`
        // set true. Brisbane is the only city in the filtered tree ->
        // city_idx 0.
        use crate::test_support::StubService;
        let mut app = test_app_with_many_relays();
        let service = StubService::default();
        for ch in "301".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }
        assert!(
            !app.select_location_page_state()
                .is_city_expanded("au", "bne")
        );

        activate(&mut app, &service, WidgetId(CITY_CHEVRON_BASE)).await;

        assert!(
            !app.select_location_page_state()
                .is_city_expanded("au", "bne"),
            "click on a force-expanded city must not flip the persisted bit",
        );
    }

    #[test]
    fn rendered_chevron_rect_is_hit_testable_to_chevron_widget_id() {
        // End-to-end check: render the page, then issue a `hit_test`
        // at the rightmost cells of a country row and confirm the
        // returned widget id is the chevron's, not the radio's. The
        // page is centered (`PAGE_COLUMN_WIDTH`) so the chevron sits
        // at the inner-right edge, not the absolute right of the
        // terminal - derive both columns from the trimmed line so the
        // test stays correct if the centring math changes.
        let mut app = test_app_with_relays();
        let _ = render_screen_persisting_registry(&mut app, 60, 12);

        let registry = app.last_focus_registry();
        let screen = render_screen(&app, 60, 12);
        let se_row = screen
            .iter()
            .position(|line| line.contains("Sweden"))
            .expect("Sweden row visible") as u16;
        let line = &screen[se_row as usize];
        let chevron_col = line.chars().count() as u16 - 1;
        let chevron_hit = registry.hit_test(chevron_col, se_row);
        assert_eq!(
            chevron_hit,
            Some(WidgetId(COUNTRY_CHEVRON_BASE)),
            "rightmost cell of Sweden row should hit-test to the country-0 chevron",
        );

        // A click on the row's label area (well left of the chevron
        // column, but inside the centered page) lands on the radio so
        // the row body still selects. Find the leading-padding width
        // by counting spaces before the radio glyph "(".
        let leading = line.chars().take_while(|c| *c == ' ').count() as u16;
        let radio_hit = registry.hit_test(leading + 2, se_row);
        assert_eq!(
            radio_hit,
            Some(WidgetId(COUNTRY_RADIO_BASE)),
            "label column on Sweden row should still hit the country radio",
        );
    }
}
