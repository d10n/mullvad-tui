// SPDX-License-Identifier: GPL-3.0-or-later

//! `Settings > VPN > DNS content blockers` sub-page renderer.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Paragraph, Wrap},
};

use crate::{
    app::{App, DNS_BLOCKERS, DnsBlocker, FocusRegistry, WidgetId},
    tui::components,
};

use super::{checkbox_label, on_off_label, widgets};

/// Render the DNS content blockers sub-page: a short blurb at the top,
/// then six `<label>: On|Off ... [Enable]/[Disable]` rows - one per
/// [`DnsBlocker`] variant - driven by [`DNS_BLOCKERS`] for stable order.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let blockers = app.dns_blockers();

    let description = vec![
        Line::from(
            "When this feature is enabled it stops the device from contacting certain domains or websites known for distributing ads, malware, trackers and more.",
        ),
        Line::from(""),
        Line::from("This might cause issues on certain websites, services, and apps."),
        Line::from(""),
        Line::from(
            "Attention: this setting cannot be used in combination with \"Use custom DNS server\"",
        ),
    ];
    let description_height = Paragraph::new(description.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width) as u16;

    let mut constraints = vec![
        Constraint::Length(1),               // Title
        Constraint::Length(1),               // Spacer
        Constraint::Min(description_height), // Description
        Constraint::Min(1),                  // Spacer
    ];
    constraints.extend(DNS_BLOCKERS.iter().map(|_| Constraint::Length(1)));
    constraints.push(Constraint::Min(1));
    let chunks = Layout::vertical(constraints).split(area);
    let title = chunks[0];
    let description_area = chunks[2];
    let options = &chunks[4..];

    frame.render_widget(Paragraph::new(Line::from("DNS content blockers")), title);
    frame.render_widget(
        Paragraph::new(description).wrap(Wrap { trim: false }),
        description_area,
    );

    for (row, blocker) in options.iter().zip(DNS_BLOCKERS.iter()) {
        let enabled = blockers.is_some_and(|opts| blocker.read(opts));
        render_row(frame, *row, *blocker, enabled, focused, registry);
    }
}

fn render_row(
    frame: &mut Frame<'_>,
    area: Rect,
    blocker: DnsBlocker,
    enabled: bool,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    // `<label>: On    [Disable]` row - same shape as
    // `render_vpn_toggle_row` but without the decorative `[Info]`.
    let label = format!("{}: {}", blocker.label(), on_off_label(enabled));
    components::render_label_button_row(
        frame,
        area,
        label,
        checkbox_label(enabled),
        widget_id(blocker),
        focused,
        registry,
    );
}

/// Stable widget id for one of the six DNS-content-blocker toggles.
/// Used by the sub-page renderer to assign widget ids in declaration
/// order. Inverse of [`super::SettingsWidget::dns_blocker`].
pub(super) fn widget_id(blocker: DnsBlocker) -> WidgetId {
    match blocker {
        DnsBlocker::Ads => widgets::DNS_BLOCK_ADS,
        DnsBlocker::Trackers => widgets::DNS_BLOCK_TRACKERS,
        DnsBlocker::Malware => widgets::DNS_BLOCK_MALWARE,
        DnsBlocker::AdultContent => widgets::DNS_BLOCK_ADULT_CONTENT,
        DnsBlocker::Gambling => widgets::DNS_BLOCK_GAMBLING,
        DnsBlocker::SocialMedia => widgets::DNS_BLOCK_SOCIAL_MEDIA,
    }
}
