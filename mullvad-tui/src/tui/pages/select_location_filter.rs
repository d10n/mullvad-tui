// SPDX-License-Identifier: GPL-3.0-or-later

//! `Status > Select location > Filter` sub-page renderer.
//!
//! ```text
//! Ownership
//!   Any                                       (•)
//!   Mullvad owned only                        ( )
//!   Rented only                               ( )
//!
//! Providers
//!   All providers                             [x]
//!   100TB                                     [x]
//!   31173                                     [x]
//!   ...
//! ```
//!
//! The filter does **not** yet apply to the relay tree projection -
//! `RelayLocation` would need to carry `owned: bool` and
//! `provider: String` from the upstream `WireguardRelayEndpointData`.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    widgets::Paragraph,
};

use crate::{
    app::{
        App, FocusKind, FocusRegistry, FocusableWidget, WidgetId,
        pages::select_location_filter::Ownership,
    },
    tui::components,
};

// ---- Widget id allocations ----
//
// Static rows for the three Ownership options + the "All providers"
// master, plus a dynamic range for per-provider checkboxes. Lives
// well above the SelectLocation range so the two pages don't alias.

const FILTER_BASE: u32 = 0x2000;

pub mod widgets {
    use super::FILTER_BASE;
    use crate::app::WidgetId;

    pub const OWNERSHIP_ANY: WidgetId = WidgetId(FILTER_BASE);
    pub const OWNERSHIP_MULLVAD_OWNED: WidgetId = WidgetId(FILTER_BASE + 1);
    pub const OWNERSHIP_RENTED: WidgetId = WidgetId(FILTER_BASE + 2);
    pub const PROVIDERS_ALL: WidgetId = WidgetId(FILTER_BASE + 3);
}

pub const PROVIDER_RADIO_BASE: u32 = FILTER_BASE + 0x10;
pub const PROVIDER_MAX: u32 = 64;

pub fn provider_index(widget: WidgetId) -> Option<usize> {
    let id = widget.0;
    (PROVIDER_RADIO_BASE..PROVIDER_RADIO_BASE + PROVIDER_MAX)
        .contains(&id)
        .then(|| (id - PROVIDER_RADIO_BASE) as usize)
}

pub fn owns_widget(widget: WidgetId) -> bool {
    matches!(
        widget,
        widgets::OWNERSHIP_ANY
            | widgets::OWNERSHIP_MULLVAD_OWNED
            | widgets::OWNERSHIP_RENTED
            | widgets::PROVIDERS_ALL
    ) || provider_index(widget).is_some()
}

/// Run the action bound to a focused Select-location-filter widget.
/// All activations mutate `App.select_location_filter_page_state` -
/// the filter doesn't yet feed into the relay tree projection (that
/// wiring is a follow-up).
pub fn activate(app: &mut App, widget: WidgetId) {
    if let Some(idx) = provider_index(widget) {
        if let Some(provider) = provider_at(idx) {
            app.select_location_filter_page_state_mut()
                .toggle_provider(provider);
        }
        return;
    }
    match widget {
        widgets::OWNERSHIP_ANY => app
            .select_location_filter_page_state_mut()
            .set_ownership(Ownership::Any),
        widgets::OWNERSHIP_MULLVAD_OWNED => app
            .select_location_filter_page_state_mut()
            .set_ownership(Ownership::MullvadOwned),
        widgets::OWNERSHIP_RENTED => app
            .select_location_filter_page_state_mut()
            .set_ownership(Ownership::Rented),
        widgets::PROVIDERS_ALL => {
            app.select_location_filter_page_state_mut()
                .toggle_all_providers(PROVIDERS);
        }
        _ => {}
    }
}

/// Hardcoded provider list - a stub until the upstream
/// `provider: String` field is plumbed through `RelayLocation`. The
/// renderer iterates this; the dispatch maps an index back to a name
/// via [`provider_at`].
pub const PROVIDERS: &[&str] = &[
    "100TB",
    "31173",
    "Blix",
    "Creanova",
    "DataPacket",
    "HostRoyale",
    "hostuniversal",
    "iRegister",
    "m247",
    "PrivateLayer",
    "techfutures",
    "Tzulo",
    "Veloxserv",
    "xtom",
    "Zenlayer",
];

pub fn provider_at(index: usize) -> Option<&'static str> {
    PROVIDERS.get(index).copied()
}

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let state = app.select_location_filter_page_state();

    // Layout: Ownership header + 3 rows, blank, Providers header +
    // (1 master + N rows). Total height varies with PROVIDERS.len().
    let providers_count = PROVIDERS.len() as u16;
    let [
        ownership_label,
        any_row,
        mullvad_row,
        rented_row,
        _blank,
        providers_label,
        all_providers_row,
        provider_rows_area,
        _spacer,
    ] = Layout::vertical([
        Constraint::Length(1),               // "Ownership" label
        Constraint::Length(1),               // Any
        Constraint::Length(1),               // Mullvad-owned
        Constraint::Length(1),               // Rented
        Constraint::Length(1),               // blank
        Constraint::Length(1),               // "Providers" label
        Constraint::Length(1),               // All providers
        Constraint::Length(providers_count), // per-provider rows
        Constraint::Min(0),                  // spacer
    ])
    .areas(area);

    frame.render_widget(Paragraph::new("Ownership"), ownership_label);
    render_ownership_row(
        frame,
        any_row,
        "Any",
        state.ownership() == Ownership::Any,
        widgets::OWNERSHIP_ANY,
        focused,
        registry,
    );
    render_ownership_row(
        frame,
        mullvad_row,
        "Mullvad owned only",
        state.ownership() == Ownership::MullvadOwned,
        widgets::OWNERSHIP_MULLVAD_OWNED,
        focused,
        registry,
    );
    render_ownership_row(
        frame,
        rented_row,
        "Rented only",
        state.ownership() == Ownership::Rented,
        widgets::OWNERSHIP_RENTED,
        focused,
        registry,
    );

    frame.render_widget(Paragraph::new("Providers"), providers_label);
    render_provider_row(
        frame,
        all_providers_row,
        "All providers",
        state.all_providers_selected(),
        widgets::PROVIDERS_ALL,
        focused,
        registry,
    );

    // Per-provider rows. Cap by PROVIDER_MAX to stay within the
    // dynamic widget id range; rows beyond render display-only.
    let row_constraints: Vec<Constraint> = (0..providers_count)
        .map(|_| Constraint::Length(1))
        .collect();
    let row_areas = Layout::vertical(row_constraints).split(provider_rows_area);
    for (i, &provider) in PROVIDERS.iter().enumerate() {
        let id = if (i as u32) < PROVIDER_MAX {
            Some(WidgetId(PROVIDER_RADIO_BASE + i as u32))
        } else {
            None
        };
        render_provider_row_dynamic(
            frame,
            row_areas[i],
            provider,
            state.is_provider_selected(provider),
            id,
            focused,
            registry,
        );
    }
}

/// Whole-row focusable laid out as `<label>     <glyph>`. Used by
/// both the Ownership rows (radio glyph, `SelectOption` kind) and the
/// Providers rows (checkbox glyph, `Toggle` kind) - the rect math,
/// styling, and registration are identical, only the glyph and the
/// focus-engine semantics differ.
#[expect(
    clippy::too_many_arguments,
    reason = "row helper bundles label/glyph + 2 ids + frame/registry/kind; \
              splitting into a struct adds a layer for two callers"
)]
fn render_glyph_row(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    glyph: &str,
    id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
    kind: FocusKind,
) {
    let glyph_width = glyph.chars().count() as u16;
    let [label_area, _gap, glyph_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(glyph_width),
    ])
    .areas(area);
    let style = if focused == Some(id) {
        Style::new().yellow()
    } else {
        Style::new()
    };
    frame.render_widget(Paragraph::new(label.to_string()).style(style), label_area);
    frame.render_widget(Paragraph::new(glyph.to_string()).style(style), glyph_area);
    registry.register(FocusableWidget {
        id,
        rect: area,
        kind,
    });
    // Each row is a distinct vertical slot - close it so Up/Down
    // arrow keys move between rows instead of treating the whole page
    // as one horizontal strip.
    registry.end_row();
}

fn render_ownership_row(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    selected: bool,
    id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    render_glyph_row(
        frame,
        area,
        label,
        components::radio_glyph(selected),
        id,
        focused,
        registry,
        FocusKind::SelectOption,
    );
}

fn render_provider_row(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    checked: bool,
    id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    render_glyph_row(
        frame,
        area,
        label,
        components::checkbox_glyph(checked),
        id,
        focused,
        registry,
        FocusKind::Toggle,
    );
}

fn render_provider_row_dynamic(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    checked: bool,
    id: Option<WidgetId>,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let Some(id) = id else {
        // Out of widget id range - render display-only.
        let glyph = components::checkbox_glyph(checked);
        frame.render_widget(Paragraph::new(format!("{label}     {glyph}")), area);
        return;
    };
    render_provider_row(frame, area, label, checked, id, focused, registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_index_decodes_in_range() {
        assert_eq!(provider_index(WidgetId(PROVIDER_RADIO_BASE)), Some(0));
        assert_eq!(provider_index(WidgetId(PROVIDER_RADIO_BASE + 5)), Some(5));
        assert_eq!(
            provider_index(WidgetId(PROVIDER_RADIO_BASE + PROVIDER_MAX - 1)),
            Some((PROVIDER_MAX - 1) as usize),
        );
    }

    #[test]
    fn provider_index_returns_none_outside_range() {
        assert_eq!(provider_index(WidgetId(PROVIDER_RADIO_BASE - 1)), None);
        assert_eq!(
            provider_index(WidgetId(PROVIDER_RADIO_BASE + PROVIDER_MAX)),
            None,
        );
    }

    #[test]
    fn rendered_registry_dump_shows_one_row_per_focusable() {
        // Belt-and-braces visibility: snapshot the row layout the
        // renderer actually produces so future changes that
        // accidentally collapse rows (e.g. a missing `end_row`) fail
        // here with a clear "all on row 0" diff instead of a
        // navigation symptom.
        use crate::app::App;
        use ratatui::{Terminal, backend::TestBackend};

        let app = App::new();
        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut registry = FocusRegistry::default();
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, 60, 30), &app, &mut registry);
            })
            .unwrap();
        let row_widths: Vec<usize> = registry.row_widths().collect();
        // First three rows are the Ownership radios (one each), then
        // PROVIDERS_ALL, then one row per provider. Every row should
        // be exactly one cell.
        assert!(
            row_widths.iter().all(|w| *w == 1),
            "every focusable row should have exactly one cell, got widths {row_widths:?}",
        );
        // 3 ownership + 1 PROVIDERS_ALL + N providers, all separate.
        assert_eq!(
            row_widths.len(),
            4 + PROVIDERS.len(),
            "row count should equal focusable count",
        );
    }

    #[test]
    fn rows_register_as_separate_focus_rows_so_up_down_navigates() {
        // Each Ownership / Providers row is a standalone vertical
        // slot. Without closing the row in `render_glyph_row` they
        // collapse into a single horizontal strip, and Up/Down become
        // no-ops while Left/Right walks through the rows - the bug
        // this test guards against.
        use crate::app::{App, ArrowDir};
        use ratatui::{Terminal, backend::TestBackend};

        let app = App::new();
        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut registry = FocusRegistry::default();
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, 60, 30), &app, &mut registry);
            })
            .unwrap();

        // Down walks Any -> Mullvad -> Rented -> All providers.
        assert_eq!(
            registry.navigate(widgets::OWNERSHIP_ANY, ArrowDir::Down),
            Some(widgets::OWNERSHIP_MULLVAD_OWNED),
        );
        assert_eq!(
            registry.navigate(widgets::OWNERSHIP_MULLVAD_OWNED, ArrowDir::Down),
            Some(widgets::OWNERSHIP_RENTED),
        );
        assert_eq!(
            registry.navigate(widgets::OWNERSHIP_RENTED, ArrowDir::Down),
            Some(widgets::PROVIDERS_ALL),
        );
        // Up walks the same rows in reverse.
        assert_eq!(
            registry.navigate(widgets::PROVIDERS_ALL, ArrowDir::Up),
            Some(widgets::OWNERSHIP_RENTED),
        );
        // Left/Right stay put - every row has only one cell.
        assert_eq!(
            registry.navigate(widgets::OWNERSHIP_ANY, ArrowDir::Right),
            None,
        );
        assert_eq!(
            registry.navigate(widgets::OWNERSHIP_ANY, ArrowDir::Left),
            None,
        );
    }

    #[test]
    fn owns_widget_recognizes_static_and_dynamic() {
        assert!(owns_widget(widgets::OWNERSHIP_ANY));
        assert!(owns_widget(widgets::OWNERSHIP_MULLVAD_OWNED));
        assert!(owns_widget(widgets::OWNERSHIP_RENTED));
        assert!(owns_widget(widgets::PROVIDERS_ALL));
        assert!(owns_widget(WidgetId(PROVIDER_RADIO_BASE)));
        assert!(!owns_widget(WidgetId(0x40))); // settings range
    }
}
