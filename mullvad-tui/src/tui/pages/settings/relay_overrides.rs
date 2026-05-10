// SPDX-License-Identifier: GPL-3.0-or-later

//! `Settings > VPN > Server IP overrides` sub-page renderer.
//!
//! Lists every per-relay IPv4/IPv6 in-address override the daemon
//! currently has configured. Each override gets a 3-row block: the
//! hostname with a `[Remove]` button, then one row per protocol showing
//! either the override value or `—`. The footer pins
//! `[Add override…]` and the danger `[Clear all]` button.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Paragraph, Wrap},
};

use crate::{
    app::{App, FocusRegistry, WidgetId},
    integration::RelayOverride,
    tui::components,
};

use super::widgets;

/// Rows per override block: hostname + remove on row 1, v4 detail on
/// row 2, v6 detail on row 3.
const ROWS_PER_OVERRIDE: u16 = 3;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let overrides = app.relay_overrides();

    let description = Line::from(
        "Override the IPv4/IPv6 in-address of a specific relay when the upstream-published address is blocked or unreachable.",
    );
    let description_height = Paragraph::new(description.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width) as u16;

    // Layout:
    //   Title
    //   blank
    //   description (variable height)
    //   blank
    //   "Configured overrides:"
    //   blank
    //   per-override blocks (3 rows each + 1 blank separator between)
    //   spacer
    //   footer row
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(description_height),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ];
    if overrides.is_empty() {
        constraints.push(Constraint::Length(1));
    } else {
        for i in 0..overrides.len() {
            for _ in 0..ROWS_PER_OVERRIDE {
                constraints.push(Constraint::Length(1));
            }
            if i + 1 < overrides.len() {
                constraints.push(Constraint::Length(1));
            }
        }
    }
    constraints.push(Constraint::Min(1));
    constraints.push(Constraint::Length(1));

    let chunks = Layout::vertical(constraints).split(area);

    frame.render_widget(Paragraph::new("Server IP overrides"), chunks[0]);
    frame.render_widget(
        Paragraph::new(description).wrap(Wrap { trim: false }),
        chunks[2],
    );
    frame.render_widget(Paragraph::new("Configured overrides:"), chunks[4]);

    if overrides.is_empty() {
        frame.render_widget(Paragraph::new("    (no overrides configured)"), chunks[6]);
    } else {
        // Walk the per-override blocks, skipping the separator slots
        // between them.
        registry.begin_scroll_group();
        let mut row_idx = 6usize;
        for (i, ov) in overrides.iter().enumerate() {
            render_override_block(
                frame,
                &chunks[row_idx..row_idx + 3],
                ov,
                i,
                focused,
                registry,
            );
            row_idx += ROWS_PER_OVERRIDE as usize;
            if i + 1 < overrides.len() {
                row_idx += 1; // separator
            }
        }
        registry.end_scroll_group();
    }

    let footer_row = chunks[chunks.len() - 1];
    render_footer(frame, footer_row, focused, registry);
}

/// Render one override's three-row block: hostname + `[Remove]`, then
/// the v4 and v6 detail rows. Rows beyond the [`widgets::RELAY_OVERRIDE_REMOVE_MAX`]
/// cap render display-only (no `[Remove]` button).
fn render_override_block(
    frame: &mut Frame<'_>,
    rows: &[Rect],
    relay_override: &RelayOverride,
    index: usize,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    debug_assert_eq!(rows.len(), ROWS_PER_OVERRIDE as usize);
    let header_row = rows[0];
    let v4_row = rows[1];
    let v6_row = rows[2];

    if (index as u32) < widgets::RELAY_OVERRIDE_REMOVE_MAX {
        let remove_id = WidgetId(widgets::RELAY_OVERRIDE_REMOVE_BASE.0 + index as u32);
        components::render_label_button_row(
            frame,
            header_row,
            relay_override.hostname.clone(),
            "Remove",
            remove_id,
            focused,
            registry,
        );
    } else {
        frame.render_widget(Paragraph::new(relay_override.hostname.clone()), header_row);
    }

    let v4 = relay_override
        .ipv4_addr_in
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "\u{2014}".to_string()); // em dash
    let v6 = relay_override
        .ipv6_addr_in
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "\u{2014}".to_string());
    frame.render_widget(Paragraph::new(format!("    v4 {v4}")), v4_row);
    frame.render_widget(Paragraph::new(format!("    v6 {v6}")), v6_row);
}

/// Footer row: `[Add override…]` on the left, danger `[Clear all]` on
/// the right. The two buttons are pinned to the row's edges with a
/// `Min(1)` gap so the row scales with available width.
fn render_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let add_label = "Add override\u{2026}";
    let clear_label = "Clear all";
    let add_w = (add_label.chars().count() as u16) + 2;
    let clear_w = (clear_label.len() as u16) + 2;

    let [add_area, _gap, clear_area] = Layout::horizontal([
        Constraint::Length(add_w),
        Constraint::Min(1),
        Constraint::Length(clear_w),
    ])
    .areas(area);

    components::render_button(
        frame,
        add_area,
        add_label,
        focused == Some(widgets::RELAY_OVERRIDE_ADD),
        registry,
        widgets::RELAY_OVERRIDE_ADD,
    );
    components::render_button_danger(
        frame,
        clear_area,
        clear_label,
        focused == Some(widgets::RELAY_OVERRIDE_CLEAR_ALL),
        registry,
        widgets::RELAY_OVERRIDE_CLEAR_ALL,
    );
    registry.end_row();
}
