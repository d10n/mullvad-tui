// SPDX-License-Identifier: GPL-3.0-or-later

//! `Settings > VPN > Use custom DNS server` sub-page renderer.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Paragraph, Wrap},
};

use crate::{
    app::{App, FocusRegistry, WidgetId},
    tui::components,
};

use super::{checkbox_label, render_status_toggle_row, widgets};

/// Render the Custom DNS sub-page. Layout:
///
/// 1. Heading + descriptive blurb.
/// 2. `Status: On|Off    [Enable]/[Disable]` row driving [`App::toggle_custom_dns`].
/// 3. One `<addr>    [Remove]` row per cached address (capped at [`widgets::CUSTOM_DNS_REMOVE_MAX`]
///    focusable rows; addresses beyond that render display-only until lower rows free up space).
/// 4. Bottom-anchored centered `[Add address]` button opening the IP text-entry overlay.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let enabled = app.custom_dns_enabled();
    let addresses = app.custom_dns_addresses();
    let toggle_label = checkbox_label(enabled);

    let description = Line::from(
        "Use one or more DNS servers of your choice instead of Mullvad's default. Disable to fall back to Mullvad's resolvers.",
    );
    let description_height = Paragraph::new(description.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width) as u16;

    // Layout: heading block + blank + status row + blank +
    // optional "Servers:" label + per-address rows + spacer +
    // [Add address] button at the bottom.
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1),                  // Title
        Constraint::Length(1),                  // Blank
        Constraint::Length(description_height), // Description
        Constraint::Length(1),                  // Blank
        Constraint::Length(1),                  // Status row
        Constraint::Length(1),                  // Blank
        Constraint::Length(1),                  // "Servers:" label
    ];
    if addresses.is_empty() {
        constraints.push(Constraint::Length(1)); // "(no servers configured)"
    } else {
        for _ in addresses {
            constraints.push(Constraint::Length(1));
        }
    }
    constraints.push(Constraint::Min(1)); // spacer
    constraints.push(Constraint::Length(1)); // [Add address]
    constraints.push(Constraint::Min(1)); // spacer

    let chunks = Layout::vertical(constraints).split(area);

    frame.render_widget(Paragraph::new("Custom DNS server"), chunks[0]);
    frame.render_widget(
        Paragraph::new(description).wrap(Wrap { trim: false }),
        chunks[2],
    );
    render_status_toggle_row(
        frame,
        chunks[4],
        enabled,
        toggle_label,
        widgets::CUSTOM_DNS_TOGGLE,
        focused,
        registry,
    );
    frame.render_widget(Paragraph::new("Servers:"), chunks[6]);

    if addresses.is_empty() {
        frame.render_widget(Paragraph::new("    (no servers configured)"), chunks[7]);
    } else {
        for (i, addr) in addresses.iter().enumerate() {
            let row = chunks[7 + i];
            render_row(frame, row, *addr, i, focused, registry);
        }
    }

    // [Add address] anchored at the bottom. `chunks.len() - 1` is the
    // button row; the spacer right above it absorbs any surplus height.
    components::render_centered_button(
        frame,
        chunks[chunks.len() - 1],
        "Add address",
        widgets::CUSTOM_DNS_ADD_ADDRESS,
        focused,
        registry,
    );
}

/// One `<addr>    [Edit] [Remove]` row. Addresses beyond
/// [`widgets::CUSTOM_DNS_REMOVE_MAX`] are display-only - we run out
/// of reserved widget ids and silently drop the buttons rather than
/// risk id collisions with neighboring widget ranges.
fn render_row(
    frame: &mut Frame<'_>,
    area: Rect,
    addr: std::net::IpAddr,
    index: usize,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let label = format!("    {addr}");

    if (index as u32) >= widgets::CUSTOM_DNS_REMOVE_MAX {
        // Out of reserved ids - render display-only.
        frame.render_widget(Paragraph::new(label), area);
        return;
    }

    let edit_label = "[Edit]";
    let remove_label = "[Remove]";
    let edit_width = edit_label.len() as u16;
    let remove_width = remove_label.len() as u16;
    let trailing = edit_width + 1 + remove_width;
    let label_width = area.width.saturating_sub(trailing + 1);

    let label_area = Rect::new(area.x, area.y, label_width, 1);
    let edit_area = Rect::new(area.x + label_width + 1, area.y, edit_width, 1);
    let remove_area = Rect::new(
        area.x + label_width + 1 + edit_width + 1,
        area.y,
        remove_width,
        1,
    );

    frame.render_widget(Paragraph::new(label), label_area);

    let edit_id = WidgetId(widgets::CUSTOM_DNS_EDIT_BASE.0 + index as u32);
    components::render_button(
        frame,
        edit_area,
        "Edit",
        focused == Some(edit_id),
        registry,
        edit_id,
    );

    let remove_id = WidgetId(widgets::CUSTOM_DNS_REMOVE_BASE.0 + index as u32);
    components::render_button(
        frame,
        remove_area,
        "Remove",
        focused == Some(remove_id),
        registry,
        remove_id,
    );
    registry.end_row();
}
