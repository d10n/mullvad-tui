// SPDX-License-Identifier: GPL-3.0-or-later

//! `Settings > VPN > Anti-censorship` sub-page renderer.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::Paragraph,
};

use crate::{
    app::{App, FocusRegistry, WidgetId, mode_has_configurable_port},
    integration::SelectedObfuscation,
    tui::components,
};

use super::{render_vpn_button_row, widgets};

/// Anti-censorship modes in their declared display order. Mirrors the
/// daemon's `SelectedObfuscation` discriminant order; `Off` and `Auto`
/// stay at the top so the safer/default options are visually first.
const MODES: [SelectedObfuscation; 7] = [
    SelectedObfuscation::Off,
    SelectedObfuscation::Auto,
    SelectedObfuscation::Udp2Tcp,
    SelectedObfuscation::Shadowsocks,
    SelectedObfuscation::WireguardPort,
    SelectedObfuscation::Quic,
    SelectedObfuscation::Lwo,
];

/// Render the Anti-censorship sub-page: brief description + one row
/// per [`SelectedObfuscation`] mode, plus an optional `[Edit port]`
/// row at the bottom that's only present when the active mode
/// supports a configurable port.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let settings = app.settings();
    let active_mode = settings
        .map(|s| s.obfuscation_settings.selected_obfuscation)
        .unwrap_or_default();
    let active_port = settings.and_then(|s| current_port_for_mode(s, active_mode));
    let port_editable = mode_has_configurable_port(active_mode);

    let header_lines: Vec<Line<'static>> = vec![
        Line::from("Anti-censorship"),
        Line::from(""),
        Line::from("Override how WireGuard tunnels look on the wire to evade"),
        Line::from("network-level VPN blocking. The active mode is marked *."),
    ];

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(header_lines.len() as u16),
        Constraint::Length(1), // blank
    ];
    for _ in MODES {
        constraints.push(Constraint::Length(1));
    }
    if port_editable {
        constraints.push(Constraint::Length(1)); // blank
        constraints.push(Constraint::Length(1)); // port row
    }
    constraints.push(Constraint::Min(0)); // spacer

    let chunks = Layout::vertical(constraints).split(area);

    frame.render_widget(Paragraph::new(header_lines), chunks[0]);
    for (i, mode) in MODES.iter().enumerate() {
        let row = chunks[2 + i];
        render_row(frame, row, *mode, *mode == active_mode, focused, registry);
    }
    if port_editable {
        // Layout indices: header, blank, 7 modes, blank, port-row.
        let port_row = chunks[2 + MODES.len() + 1];
        let value = active_port
            .map(|p| p.to_string())
            .unwrap_or_else(|| "Any".to_string());
        render_vpn_button_row(
            frame,
            port_row,
            &format!("Port: {value}"),
            "Edit",
            widgets::ANTI_CENSORSHIP_PORT_EDIT,
            focused,
            registry,
        );
    }
}

/// Render one mode-row: `* <Mode>     [Select]` (asterisk only when
/// active). The asterisk + label are static text; only the `[Select]`
/// button registers for focus.
fn render_row(
    frame: &mut Frame<'_>,
    area: Rect,
    mode: SelectedObfuscation,
    active: bool,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let prefix = if active { "*" } else { " " };
    components::render_label_button_row(
        frame,
        area,
        format!("{prefix} {mode}"),
        "Select",
        widget_id(mode),
        focused,
        registry,
    );
}

/// Stable widget id for one of the seven anti-censorship mode rows.
/// Inverse of [`super::SettingsWidget::anti_censorship_mode`]; used by
/// the renderer so each row's `[Select]` button gets a fixed focus id.
fn widget_id(mode: SelectedObfuscation) -> WidgetId {
    match mode {
        SelectedObfuscation::Off => widgets::ANTI_CENSORSHIP_MODE_OFF,
        SelectedObfuscation::Auto => widgets::ANTI_CENSORSHIP_MODE_AUTO,
        SelectedObfuscation::Udp2Tcp => widgets::ANTI_CENSORSHIP_MODE_UDP2_TCP,
        SelectedObfuscation::Shadowsocks => widgets::ANTI_CENSORSHIP_MODE_SHADOWSOCKS,
        SelectedObfuscation::WireguardPort => widgets::ANTI_CENSORSHIP_MODE_WIREGUARD_PORT,
        SelectedObfuscation::Quic => widgets::ANTI_CENSORSHIP_MODE_QUIC,
        SelectedObfuscation::Lwo => widgets::ANTI_CENSORSHIP_MODE_LWO,
    }
}

/// Read the current port for `mode` from cached `Settings`. `None`
/// means the daemon picks (Constraint::Any), `Some(0)` is impossible -
/// the port-input flow rejects 0 - so callers can format `None` as
/// "Any" and `Some(p)` as the literal port. Returns `None` for modes
/// without a configurable port.
fn current_port_for_mode(
    settings: &crate::integration::Settings,
    mode: SelectedObfuscation,
) -> Option<u16> {
    use crate::integration::Constraint;
    match mode {
        SelectedObfuscation::Udp2Tcp => match settings.obfuscation_settings.udp2tcp.port {
            Constraint::Any => None,
            Constraint::Only(p) => Some(p),
        },
        SelectedObfuscation::Shadowsocks => match settings.obfuscation_settings.shadowsocks.port {
            Constraint::Any => None,
            Constraint::Only(p) => Some(p),
        },
        SelectedObfuscation::WireguardPort => {
            match settings.obfuscation_settings.wireguard_port.get() {
                Constraint::Any => None,
                Constraint::Only(p) => Some(p),
            }
        }
        SelectedObfuscation::Off
        | SelectedObfuscation::Auto
        | SelectedObfuscation::Quic
        | SelectedObfuscation::Lwo => None,
    }
}
