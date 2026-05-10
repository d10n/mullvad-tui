// SPDX-License-Identifier: GPL-3.0-or-later

//! `Settings > API access methods` sub-page renderer.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Paragraph, Wrap},
};

use crate::{
    app::{App, FocusRegistry, WidgetId},
    integration::{AccessMethodId, AccessMethodSetting},
    tui::components,
};

use super::{checkbox_label, widgets};

/// Render the API access sub-page. Layout:
///
/// 1. Heading + descriptive blurb.
/// 2. Per-method rows (built-ins first: Direct, Mullvad Bridges, Encrypted DNS proxy; then any
///    custom methods). Each row shows `*<name>  [Enable]/[Disable]  [Use]` where `*` flags the
///    daemon's currently-active method (read once on entry via
///    [`App::refresh_current_access_method`] - the daemon doesn't push changes for this).
///
/// Custom proxy add/edit and reachability test are deferred to
/// follow-up sessions; the placeholder built-ins display every
/// supported method.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let active_id = app.current_api_access_id().cloned();

    let description = Line::from("Manage and add custom methods to access the Mullvad API.");
    let description_height = Paragraph::new(description.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width) as u16;

    let methods: Vec<AccessMethodSetting> = app
        .settings()
        .map(|s| s.api_access_methods.iter().cloned().collect())
        .unwrap_or_default();

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // Title
        Constraint::Length(1), // Blank
        Constraint::Length(description_height),
        Constraint::Length(1), // Blank
    ];
    if methods.is_empty() {
        constraints.push(Constraint::Length(1)); // "(loading…)"
    } else {
        for _ in &methods {
            constraints.push(Constraint::Length(1));
        }
    }
    constraints.push(Constraint::Min(0)); // spacer

    let chunks = Layout::vertical(constraints).split(area);

    frame.render_widget(Paragraph::new("API access"), chunks[0]);
    frame.render_widget(
        Paragraph::new(description).wrap(Wrap { trim: false }),
        chunks[2],
    );

    if methods.is_empty() {
        frame.render_widget(Paragraph::new("    (loading methods…)"), chunks[4]);
        return;
    }

    for (i, setting) in methods.iter().enumerate() {
        let row = chunks[4 + i];
        let active = active_id.as_ref() == Some(&setting.get_id());
        render_row(frame, row, setting, i, active, focused, registry);
    }
}

/// One `*<name>  [Enable]/[Disable]  [Use]` row. Rows beyond
/// [`widgets::API_ACCESS_MAX`] render display-only - same backstop as
/// the Custom DNS remove range.
fn render_row(
    frame: &mut Frame<'_>,
    area: Rect,
    setting: &AccessMethodSetting,
    index: usize,
    active: bool,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let prefix = if active { "* " } else { "  " };
    let label_text = format!("{prefix}{}", setting.name);

    if (index as u32) >= widgets::API_ACCESS_MAX {
        frame.render_widget(Paragraph::new(label_text), area);
        return;
    }

    let toggle_label = checkbox_label(setting.enabled);
    let toggle_text = format!("[{toggle_label}]");
    let toggle_width = toggle_text.len() as u16;
    let use_text = "[Use]";
    let use_width = use_text.len() as u16;
    // ` ` separators between label / toggle / use.
    let [label_area, _gap1, toggle_area, _gap2, use_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(toggle_width),
        Constraint::Length(1),
        Constraint::Length(use_width),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(label_text), label_area);

    let toggle_id = WidgetId(widgets::API_ACCESS_TOGGLE_BASE.0 + index as u32);
    components::render_button(
        frame,
        toggle_area,
        toggle_label,
        focused == Some(toggle_id),
        registry,
        toggle_id,
    );

    let use_id = WidgetId(widgets::API_ACCESS_USE_BASE.0 + index as u32);
    components::render_button(
        frame,
        use_area,
        "Use",
        focused == Some(use_id),
        registry,
        use_id,
    );
    registry.end_row();
}

/// Look up the cached [`AccessMethodId`] for a given row index on the
/// API access sub-page. Returns `None` when the index is out of range
/// or when the settings cache hasn't been primed yet - both cases the
/// activation handler treats as a no-op.
pub fn method_id_at(app: &App, index: usize) -> Option<AccessMethodId> {
    app.settings()
        .and_then(|s| s.api_access_methods.iter().nth(index).map(|m| m.get_id()))
}
