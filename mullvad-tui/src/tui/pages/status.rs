// SPDX-License-Identifier: GPL-3.0-or-later

//! Status page - the landing page of the TUI.
//!
//! Layout, status color, action-row shape, and button labels all
//! branch on [`StatusKind`] - see [`classify`] for the
//! `TunnelState` -> `StatusKind` mapping.

use std::{sync::OnceLock, time::Instant};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use tui_globe::{Camera, Globe, MapData};

use crate::{
    app::{
        App, ConfirmAction, CurrentRelaySelection, FocusRegistry, Operation, OperationStatus,
        WidgetId, pages::status::CameraState,
    },
    integration::{
        DeviceState, ErrorState, FeatureIndicators, MullvadService, ObfuscationInfo, TunnelState,
    },
    tui::{components, error::format_action_error, overlays::OverlayMode},
};

/// Classification of [`TunnelState`] for the Status page renderer.
/// Layout, status color, action-row shape, and button label all
/// branch on this - keeping the classification in one place stops the
/// per-state details from drifting across the helpers that consume them.
///
/// `Disconnecting` is rendered like `Connecting` (single action row, brief).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusKind {
    Connected,
    Connecting,
    Disconnecting,
    Disconnected,
    Blocked,
}

impl StatusKind {
    /// True when the inline `[Show detail]` / `[Hide detail]` toggle
    /// renders alongside the status label (Connected only - other
    /// states have no expandable connection details).
    fn shows_details_toggle(self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// Classify a daemon-reported tunnel state into the renderer's
/// state-machine. `None` (no daemon state yet) is treated as
/// `Disconnected`.
pub fn classify(state: Option<&TunnelState>) -> StatusKind {
    match state {
        Some(TunnelState::Connected { .. }) => StatusKind::Connected,
        Some(TunnelState::Connecting { .. }) => StatusKind::Connecting,
        Some(TunnelState::Disconnecting(_)) => StatusKind::Disconnecting,
        Some(TunnelState::Disconnected { .. }) | None => StatusKind::Disconnected,
        Some(TunnelState::Error(_)) => StatusKind::Blocked,
    }
}

/// Status-text color per kind.
fn status_color(kind: StatusKind) -> Color {
    match kind {
        StatusKind::Connected => Color::LightGreen,
        StatusKind::Connecting | StatusKind::Disconnecting => Color::Yellow,
        StatusKind::Disconnected => Color::LightRed,
        StatusKind::Blocked => Color::Indexed(202),
    }
}

/// Human-readable status label per kind. Connected/Disconnected/
/// Blocked are static; Connecting/Disconnecting suffix an ellipsis to
/// signal in-flight work.
fn status_label_text(kind: StatusKind) -> &'static str {
    match kind {
        StatusKind::Connected => "CONNECTED",
        StatusKind::Connecting => "CONNECTING…",
        StatusKind::Disconnecting => "DISCONNECTING…",
        StatusKind::Disconnected => "DISCONNECTED",
        StatusKind::Blocked => "BLOCKED",
    }
}

/// Label for the bottom-right action button (the [`StatusWidget::ConnectDisconnect`]
/// focusable):
/// - Connected/Blocked -> "Disconnect"
/// - Connecting -> "Cancel"
/// - Disconnecting -> "Connect" (placeholder while the tunnel tears down)
/// - Disconnected -> "Connect" (success-green)
fn connect_button_label(kind: StatusKind) -> &'static str {
    match kind {
        StatusKind::Connected | StatusKind::Blocked => "Disconnect",
        StatusKind::Connecting => "Cancel",
        StatusKind::Disconnecting | StatusKind::Disconnected => "Connect",
    }
}

/// Embedded globe geometry, parsed lazily on first render. The shipped
/// buffer is ~98k indices; parsing it fresh every frame would be
/// wasteful while the data never changes between frames.
static MAP_DATA: OnceLock<MapData> = OnceLock::new();

crate::define_page_widgets! {
    pub enum StatusWidget {
        DetailsToggle = 0x10,
        SwitchLocation,
        RefreshConnection,
        ConnectDisconnect,
    }
}

/// Render the Status page body and register its focusable widgets.
///
/// The action block always reserves two rows; what fills them is
/// state-driven:
/// - **Connected / Blocked**: `[Switch location] [Reconnect]` over a centered `[Disconnect]`.
/// - **Reconnecting** (Connecting + reconnect op in flight): `[Switch location] [⠋ Reconnect]` over
///   a centered `[Cancel]`.
/// - **Connecting** (initial): centered `[Switch location]` over a centered `[Cancel]` - no
///   Reconnect during a fresh connect.
/// - **Disconnecting**: centered `[Switch location]` over a centered `[Connect]` placeholder.
/// - **Disconnected**: centered `[Select location]` over a success-green centered `[Connect]`. When
///   lockdown mode is enabled it adds a `BLOCKING INTERNET` banner above the map.
///
/// `area` is the marginned body the rest of the page draws into;
/// `body_full_width` is the same vertical slice extended back out to
/// the unmargined width so the 3D map can full-bleed past the body's
/// 1ch L/R padding.
pub fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    body_full_width: Rect,
    app: &App,
    registry: &mut FocusRegistry,
) {
    let state = app.status_page_state();
    let focused = app.page_focus().focused;
    let connection_state = app.connection_status();
    let kind = classify(connection_state);

    // Above-map header: identity + remaining time. Trailing blank line
    // separates the header from the map.
    let header_lines: Vec<Line<'static>> = vec![
        Line::from(account_device_line(app)),
        Line::from(time_left_line(app)),
        Line::from(""),
    ];

    // Status block beneath the map. The first row gets special handling
    // (inline details toggle on Connected); the rest is per-kind.
    let block_lines = status_block_below_status_row(
        kind,
        connection_state,
        app,
        state.connection_details_expanded,
        area.width,
    );
    // Wrap long lines so they reflow instead of getting clipped at the right edge.
    // The features block is already pre-fitted to width by
    // `format_features_lines`, so the wrapper leaves it untouched.
    // `line_count(width)` reports the post-wrap row count, which we
    // reserve below.
    let below_status_para = Paragraph::new(block_lines).wrap(Wrap { trim: true });
    let below_status_height =
        u16::try_from(below_status_para.line_count(area.width)).unwrap_or(u16::MAX);
    // +1 for the leading blank, +1 for the status row itself, +N for
    // the per-kind rows below it (after wrapping).
    let status_block_height = 1 + 1 + below_status_height;

    // Lockdown banner sits above the map on Disconnected when lockdown
    // mode is on, explaining why there's no internet right now.
    let banner = lockdown_banner(kind, app);
    let banner_height = banner.as_ref().map_or(0, |para| {
        u16::try_from(para.line_count(area.width)).unwrap_or(u16::MAX)
    });

    // Outer split: header / variable middle / fixed-height action block.
    // The action block reserves two rows in every state - the top row
    // may collapse to a single centered button when Reconnect is hidden,
    // but the slot count stays constant so the map's vertical extent
    // doesn't jitter between transitions.
    let [header_area, middle_area, _pad, actions_area] = Layout::vertical([
        Constraint::Length(header_lines.len() as u16),
        Constraint::Min(1),
        Constraint::Length(1), // padding
        Constraint::Length(2), // action rows
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(header_lines), header_area);

    // Middle: optional banner above the map, then the status block at
    // the bottom. The map's `Min(1)` is what flexes vertically.
    let (map_area, status_block_area) = if let Some(para) = banner {
        let [banner_area, map_area, status_block_area] = Layout::vertical([
            Constraint::Length(banner_height),
            Constraint::Min(1),
            Constraint::Length(status_block_height),
        ])
        .areas(middle_area);
        frame.render_widget(para, banner_area);
        (map_area, status_block_area)
    } else {
        let [map_area, status_block_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(status_block_height)])
                .areas(middle_area);
        (map_area, status_block_area)
    };

    // The run loop calls `App::advance_status_camera` before drawing,
    // so the animation tracker is already pointed at the right target
    // and we just sample the current frame's interpolated value here.
    let camera_state = state.camera_anim.current(Instant::now());
    let camera = to_globe_camera(camera_state);
    // Full-bleed the map past the body's 1ch L/R padding: keep the
    // map's vertical slot intact but stretch its horizontal extent
    // back to the unmargined body width.
    let map_full_bleed = Rect::new(
        body_full_width.x,
        map_area.y,
        body_full_width.width,
        map_area.height,
    );
    render_map_placeholder(frame, map_full_bleed, camera, connection_state);

    // Spinner-on-button: when a Connect/Disconnect/Reconnect op is in
    // flight, render the action button with a leading spinner glyph.
    // The tick is system-time-derived (~10 fps) so we don't need a
    // per-frame counter on `App`. The status label on Connecting /
    // Disconnecting also reads this tick so its leading spinner glyph
    // stays in lockstep with the buttons.
    let connect_running = matches!(
        app.operation_status(),
        OperationStatus::Running(Operation::Connect)
            | OperationStatus::Running(Operation::Disconnect)
            | OperationStatus::Running(Operation::Reconnect),
    );
    let reconnect_running = matches!(
        app.operation_status(),
        OperationStatus::Running(Operation::Reconnect),
    );
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_millis() / 100) as u64)
        .unwrap_or(0);

    render_status_block(
        frame,
        status_block_area,
        kind,
        below_status_para,
        state.connection_details_expanded,
        focused,
        registry,
        tick,
    );

    let [top_area, bottom_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(actions_area);
    render_top_action_row(
        frame,
        top_area,
        kind,
        focused,
        registry,
        reconnect_running,
        tick,
    );
    render_bottom_action_row(
        frame,
        bottom_area,
        kind,
        focused,
        registry,
        connect_running,
        tick,
    );
}

/// Lockdown-blocking banner shown above the map on Disconnected when
/// lockdown mode is enabled. Returns `None` when no banner should
/// render so the caller can omit the constraint slot entirely.
fn lockdown_banner(kind: StatusKind, app: &App) -> Option<Paragraph<'static>> {
    if !matches!(kind, StatusKind::Disconnected) {
        return None;
    }
    if !app.settings().is_some_and(|s| s.lockdown_mode) {
        return None;
    }
    let lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::raw("●").yellow(),
            Span::raw(" BLOCKING INTERNET"),
        ]),
        Line::from(
            "Lockdown mode is enabled. Press \"Connect\" or disable lockdown \
             mode to unblock internet."
                .to_string(),
        ),
    ];
    Some(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: true }))
}

/// Render the status block (status row + per-kind rows below it). Splits
/// `area` vertically into a 1-row blank spacer, the status row, and a
/// `Length(N)` block for the rest of the lines so the inline
/// `[Show detail]` button on Connected can be horizontally placed
/// against the status label without breaking the block's vertical math.
#[expect(
    clippy::too_many_arguments,
    reason = "tick was added alongside the existing focused/registry/expanded args \
              for the inline status-label spinner; splitting into a struct would add \
              a layer for one call site"
)]
fn render_status_block(
    frame: &mut Frame<'_>,
    area: Rect,
    kind: StatusKind,
    below_status_para: Paragraph<'static>,
    expanded: bool,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
    tick: u64,
) {
    // Reserve the post-wrap row count for the below-status block.
    // `area.width` here matches the width used in `render` to size the
    // outer status block, so the row count agrees.
    let below_status_height =
        u16::try_from(below_status_para.line_count(area.width)).unwrap_or(u16::MAX);
    let [_blank, status_row, rest] = Layout::vertical([
        Constraint::Length(1),                   // blank spacer
        Constraint::Length(1),                   // status row
        Constraint::Length(below_status_height), // rest of block (post-wrap)
    ])
    .areas(area);

    // Status row: status label on the left (colored), optional
    // `[Show detail]` / `[Hide detail]` toggle on the right.
    let status_label = Span::raw(status_label_text(kind).to_string()).fg(status_color(kind));
    if kind.shows_details_toggle() {
        let toggle_label = if expanded {
            "Hide detail"
        } else {
            "Show detail"
        };
        let toggle_width = (toggle_label.len() as u16) + 2; // brackets
        let [label_area, toggle_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(toggle_width)])
                .areas(status_row);
        // Status label gets the spinner prefix on Connecting via the
        // builder; Connected just shows the label.
        frame.render_widget(Paragraph::new(Line::from(status_label)), label_area);
        components::render_button(
            frame,
            toggle_area,
            toggle_label,
            focused == Some(widgets::DETAILS_TOGGLE),
            registry,
            widgets::DETAILS_TOGGLE,
        );
        registry.end_row();
    } else {
        // Connecting/Disconnecting prepend a spinner glyph; the label
        // already encodes whether it's connecting or disconnecting.
        let spans: Vec<Span<'static>> =
            if matches!(kind, StatusKind::Connecting | StatusKind::Disconnecting) {
                vec![
                    Span::raw(components::spinner_frame(tick).to_string()).fg(status_color(kind)),
                    Span::raw(" "),
                    status_label,
                ]
            } else {
                vec![status_label]
            };
        frame.render_widget(Paragraph::new(Line::from(spans)), status_row);
    }

    // Below-status rows. The paragraph is built with `Wrap { trim:
    // true }` in `render`, so long lines reflow into the reserved
    // height instead of being clipped at the right edge.
    frame.render_widget(below_status_para, rest);
}

/// Build the lines that go below the status row, per kind:
/// - Connected: exit-location, exit-relay, features (with "first 2 + N more…" rule unless
///   `expanded`), then the WireGuard endpoint detail block when expanded.
/// - Connecting: target exit-location, muted exit-relay.
/// - Disconnecting: muted "Tearing down…" line.
/// - Disconnected: empty.
/// - Blocked: error cause line (orange) + explanation (muted).
fn status_block_below_status_row(
    kind: StatusKind,
    state: Option<&TunnelState>,
    app: &App,
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    match kind {
        StatusKind::Connected => {
            let mut lines: Vec<Line<'static>> = vec![
                Line::from(location_line(state)),
                Line::from(relay_line(app, state)),
            ];
            lines.extend(features_lines(app, expanded, width));
            if expanded {
                lines.push(Line::from(""));
                lines.extend(connection_details(state).into_iter().map(Line::from));
            }
            lines
        }
        StatusKind::Connecting => vec![
            Line::from(location_line(state)),
            Line::from(Span::raw(relay_line(app, state)).dark_gray()),
        ],
        StatusKind::Disconnecting => {
            vec![Line::from(
                Span::raw("Tearing down…".to_string()).dark_gray(),
            )]
        }
        StatusKind::Disconnected => Vec::new(),
        StatusKind::Blocked => {
            let mut lines: Vec<Line<'static>> = Vec::new();
            if let Some(TunnelState::Error(err)) = state {
                lines.push(Line::from(
                    Span::raw(error_state_summary(err)).fg(status_color(StatusKind::Blocked)),
                ));
            }
            // Don't claim lockdown is the cause when we can't tell.
            let lockdown = app.settings().is_some_and(|s| s.lockdown_mode);
            let explanation = if lockdown {
                "Lockdown mode is keeping all traffic blocked."
            } else {
                "All tunnel traffic is currently blocked."
            };
            lines.push(Line::from(Span::raw(explanation.to_string()).dark_gray()));
            lines
        }
    }
}

/// Format the relay slot as a one-line summary for under the status
/// row. Always prefers concrete daemon-reported hostnames over the
/// user's selection text, so even a country-level pick like
/// "Germany" renders as `"de-ber-wg-001"` once the daemon reports
/// which relay it actually picked. When the daemon also reports a
/// multihop entry hostname the line becomes `"<exit> via <entry>"`.
///
/// Falls back to the user's selection label (`"any in <city>"` etc.)
/// only when there's no daemon-reported hostname - typically while
/// disconnected.
fn relay_line(app: &App, state: Option<&TunnelState>) -> String {
    let location = state.and_then(TunnelState::get_location);
    format_relay_line(
        location.and_then(|loc| loc.hostname.as_deref()),
        location.and_then(|loc| loc.entry_hostname.as_deref()),
        || relay_selection_label(app),
    )
}

/// Pure formatter for the relay line. Prefers the daemon-reported
/// exit hostname over `selection_fallback` so a country-level pick
/// still renders as a concrete relay name once the daemon picks
/// one. Multihop appends ` via <entry>`.
fn format_relay_line(
    exit_hostname: Option<&str>,
    entry_hostname: Option<&str>,
    selection_fallback: impl FnOnce() -> String,
) -> String {
    let exit = exit_hostname
        .map(str::to_owned)
        .unwrap_or_else(selection_fallback);
    match entry_hostname {
        Some(entry) => format!("{exit} via {entry}"),
        None => exit,
    }
}

fn relay_selection_label(app: &App) -> String {
    match app.current_relay_selection() {
        CurrentRelaySelection::Hostname(name) => name.to_string(),
        CurrentRelaySelection::City { country, city } => format!("any in {city}, {country}"),
        CurrentRelaySelection::Country(code) => format!("any in {code}"),
        CurrentRelaySelection::Any => "any (daemon picks)".to_string(),
        CurrentRelaySelection::Unknown => "unknown".to_string(),
        CurrentRelaySelection::CustomList => "custom list".to_string(),
        CurrentRelaySelection::CustomTunnel => "custom tunnel".to_string(),
    }
}

/// Build the active-features block, packing labels into one or more
/// lines that fit `width`. When `expanded`, every feature is shown;
/// otherwise apply the "first 2 + N more…" rule.
fn features_lines(app: &App, expanded: bool, width: u16) -> Vec<Line<'static>> {
    format_features_lines(&collect_features(app), expanded, width)
}

/// Pure formatter for the active-features block. With
/// `expanded = false` and 3+ flags, only the first two render and an
/// `"*N more…"` suffix counts the rest. With `expanded = true` all
/// flags render.
///
/// Labels are greedily packed into lines of at most `width` cells,
/// breaking only at feature boundaries; a multi-word label is never
/// split across lines. A single label wider than `width` still gets
/// its own line and overruns the right edge - in practice no shipped
/// label is wide enough for this to bite at realistic terminal widths.
fn format_features_lines(flags: &[String], expanded: bool, width: u16) -> Vec<Line<'static>> {
    if flags.is_empty() {
        return vec![Line::from("")];
    }
    let mut tokens: Vec<String> = if expanded || flags.len() <= 2 {
        flags.to_vec()
    } else {
        flags[..2].to_vec()
    };
    if !expanded && flags.len() > 2 {
        tokens.push(format!("*{} more…", flags.len() - 2));
    }

    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_w: u32 = 0;
    for token in tokens {
        let token_w = token.chars().count() as u32;
        if current.is_empty() {
            current.push(Span::raw(token));
            current_w = token_w;
        } else if current_w + 1 + token_w <= u32::from(width) {
            current.push(Span::raw(" "));
            current.push(Span::raw(token));
            current_w += 1 + token_w;
        } else {
            lines.push(std::mem::take(&mut current));
            current.push(Span::raw(token));
            current_w = token_w;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines.into_iter().map(Line::from).collect()
}

/// Top action row: `[Select/Switch location]` paired with `[Reconnect]`
/// when the tunnel is settled (Connected/Blocked) or a reconnect op is
/// in flight. Otherwise (initial Connect, Disconnecting, Disconnected)
/// the row collapses to a single centered location button so the
/// Reconnect affordance doesn't appear during transitions where it
/// would be a no-op.
fn render_top_action_row(
    frame: &mut Frame<'_>,
    area: Rect,
    kind: StatusKind,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
    reconnect_running: bool,
    tick: u64,
) {
    let show_reconnect =
        matches!(kind, StatusKind::Connected | StatusKind::Blocked) || reconnect_running;
    if !show_reconnect {
        render_centered_select_location(frame, area, kind, focused, registry);
        registry.end_row();
        return;
    }

    // Centered row up to 40 cells wide with the two buttons pinned to
    // its left and right edges. Falls back to buttons-plus-gap width
    // when the body is narrower than 40 cells.
    let select_label = "Switch location";
    let select_w = (select_label.len() as u16) + 2;
    let reconnect_label = "Reconnect";
    // Reserve room for the spinner prefix (`⠋ `) when the reconnect op
    // is in flight so the button doesn't shift width between idle and
    // running.
    let reconnect_w = (reconnect_label.len() as u16) + 2 + if reconnect_running { 2 } else { 0 };
    const MIN_GAP: u16 = 1;
    const TARGET_ROW_W: u16 = 40;
    let pair_min_w = select_w + MIN_GAP + reconnect_w;
    let row_w = area.width.min(TARGET_ROW_W).max(pair_min_w).min(area.width);
    let row = components::centered_horizontal(area, row_w);

    let [left, _gap, right] = Layout::horizontal([
        Constraint::Length(select_w),
        Constraint::Min(MIN_GAP),
        Constraint::Length(reconnect_w),
    ])
    .areas(row);
    components::render_button(
        frame,
        left,
        select_label,
        focused == Some(widgets::SWITCH_LOCATION),
        registry,
        widgets::SWITCH_LOCATION,
    );
    // Reconnect stays plain (no green/red bg) even when running - it
    // isn't a destructive or success action, so the spinner alone
    // signals progress. Yellow on focus matches the rest of the page.
    let reconnect_style = if focused == Some(widgets::REFRESH_CONNECTION) {
        Style::new().yellow()
    } else {
        Style::new()
    };
    components::render_button_running(
        frame,
        right,
        reconnect_label,
        reconnect_style,
        reconnect_running,
        tick,
        registry,
        widgets::REFRESH_CONNECTION,
    );
    registry.end_row();
}

/// Centered `[Select location]` / `[Switch location]` button - used
/// when the top row collapses to one button (Disconnected, or any
/// transition state where Reconnect is hidden). The label phrasing
/// follows the kind: Disconnected says "Select" because no tunnel is
/// configured-and-running yet, every other state says "Switch" because
/// a selection is already in play.
fn render_centered_select_location(
    frame: &mut Frame<'_>,
    area: Rect,
    kind: StatusKind,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let label = match kind {
        StatusKind::Disconnected => "Select location",
        _ => "Switch location",
    };
    let button_width = (label.len() as u16) + 2; // brackets
    let button_area = components::centered_horizontal(area, button_width);
    components::render_button(
        frame,
        button_area,
        label,
        focused == Some(widgets::SWITCH_LOCATION),
        registry,
        widgets::SWITCH_LOCATION,
    );
}

/// Centered bottom-row button. Label and palette branch on `kind`:
/// `[Disconnect]` (red) for Connected/Blocked, `[Cancel]` (red) for
/// Connecting, `[Connect]` (green) for Disconnected, and `[Connect]`
/// (red placeholder) for Disconnecting while the tunnel tears down.
fn render_bottom_action_row(
    frame: &mut Frame<'_>,
    area: Rect,
    kind: StatusKind,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
    connect_running: bool,
    tick: u64,
) {
    let label = connect_button_label(kind);
    // Spinner adds 2 cells (`⠋ `) when running; reserve room either way
    // so the button doesn't shift between idle and running.
    let label_width = if connect_running {
        label.len() as u16 + 2
    } else {
        label.len() as u16
    };
    let button_width = label_width + 2; // brackets
    let button_area = components::centered_horizontal(area, button_width);

    // Disconnected -> green `[Connect]`; Connected/Blocked/transition
    // states -> red `[Disconnect]` (matching the danger palette used by
    // Settings' `[Disconnect & quit]` and Account's `[Log out]`).
    // Focus highlight overrides either with yellow.
    let style = if focused == Some(widgets::CONNECT_DISCONNECT) {
        Style::new().yellow()
    } else if matches!(kind, StatusKind::Disconnected) {
        Style::new().green()
    } else {
        Style::new().red()
    };
    components::render_button_running(
        frame,
        button_area,
        label,
        style,
        connect_running,
        tick,
        registry,
        widgets::CONNECT_DISCONNECT,
    );
    registry.end_row();
}

/// Full-bleed 3D globe panel. Geometry comes from [`tui_globe`]; the
/// camera is aimed at the daemon-reported connection location so the
/// user's relay sits centered on the front of the globe. A "you are
/// here" cell is then bg-painted on top of the rasterized braille:
/// - **Connected**: green at the exit relay's lat/lon.
/// - **Disconnected, lockdown off, daemon reports a non-tunnel IP**: red at the user's home
///   lat/lon.
/// - **Other states** (Connecting, Disconnecting, Error, or stale tunnel-IP location): no marker.
///   The daemon-reported location during transitions can be aspirational or stale, and a colored
///   dot would mislead the user about where their traffic exits.
fn render_map_placeholder(
    frame: &mut Frame<'_>,
    area: Rect,
    camera: Camera,
    state: Option<&TunnelState>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let map = MAP_DATA.get_or_init(MapData::embedded);
    frame.render_widget(Globe::new(map, camera), area);
    if let Some((color, lat, lon)) = location_marker(state)
        && let Some((cx, cy)) = tui_globe::project_point(lat, lon, camera, area)
        && let Some(cell) = frame.buffer_mut().cell_mut((cx, cy))
    {
        cell.set_bg(color);
    }
}

/// Pick the marker color and lat/lon to overlay on the globe, or
/// `None` when no marker is appropriate for the current tunnel state.
/// See [`render_map_placeholder`] for the per-state policy this
/// encodes; pulled into its own pure function so the policy is
/// unit-testable without standing up a `Frame`.
fn location_marker(state: Option<&TunnelState>) -> Option<(Color, f32, f32)> {
    match state? {
        TunnelState::Connected {
            location: Some(loc),
            ..
        } => Some((Color::Green, loc.latitude as f32, loc.longitude as f32)),
        TunnelState::Disconnected {
            location: Some(loc),
            #[cfg(not(target_os = "android"))]
                locked_down: false,
        } if !loc.mullvad_exit_ip => {
            Some((Color::LightRed, loc.latitude as f32, loc.longitude as f32))
        }
        _ => None,
    }
}

/// Compute the globe-camera target for the current daemon state and
/// user selection - the value the animation tracker will lerp toward.
/// Called by the run loop each frame via `App::advance_status_camera`.
///
/// Orientation convention:
/// - Geometry positions follow `x = cos(φ)·sin(λ)`, `y = sin(φ)`, `z = cos(φ)·cos(λ)` with φ =
///   latitude, λ = longitude.
/// - To rotate point (φ, λ) to the front (0, 0, 1), apply `yaw = -λ` (around Y) followed by `pitch
///   = φ` (around X).
///
/// Zoom comes from the selection precision (see
/// [`zoom_for_selection`]). When no location is available -
/// disconnected, or daemon hasn't reported one yet - fall back to a
/// centered, wide Greenwich/equator view (the geometry's natural
/// orientation).
pub fn camera_target_for(
    state: Option<&TunnelState>,
    selection: CurrentRelaySelection<'_>,
) -> CameraState {
    match state.and_then(TunnelState::get_location) {
        Some(loc) => CameraState {
            yaw: -(loc.longitude as f32).to_radians(),
            pitch: (loc.latitude as f32).to_radians(),
            zoom: zoom_for_selection(selection),
        },
        None => CameraState::default(),
    }
}

/// Cross-the-streams converter from the renderer-agnostic
/// `app::pages::status::CameraState` to `tui_globe::Camera`. Lives
/// here so `app::pages::*` stays free of `tui_globe` imports.
fn to_globe_camera(s: CameraState) -> Camera {
    Camera {
        yaw: s.yaw,
        pitch: s.pitch,
        zoom: s.zoom,
    }
}

/// Map a [`CurrentRelaySelection`] precision to a zoom factor: the
/// more specific the user's pick, the closer the camera gets. `Any`
/// and the un-resolvable variants render the wide default view.
fn zoom_for_selection(selection: CurrentRelaySelection<'_>) -> f32 {
    match selection {
        CurrentRelaySelection::Hostname(_) => 35.0,
        CurrentRelaySelection::City { .. } => 30.0,
        CurrentRelaySelection::Country(_) => 25.0,
        CurrentRelaySelection::Any
        | CurrentRelaySelection::Unknown
        | CurrentRelaySelection::CustomList
        | CurrentRelaySelection::CustomTunnel => 25.0,
    }
}

fn account_device_line(app: &App) -> String {
    let name = app.account_info().and_then(|info| match &info.device {
        DeviceState::LoggedIn(account_and_device) => Some(account_and_device.device.pretty_name()),
        _ => None,
    });
    match name {
        Some(name) => format!("Device name: {name}"),
        None => "Device name: not logged in".to_string(),
    }
}

fn time_left_line(app: &App) -> String {
    let Some(info) = app.account_info() else {
        return "Time left: unknown".to_string();
    };
    let Some(data) = info.data.as_ref() else {
        return "Time left: unknown".to_string();
    };
    let now = chrono::Utc::now();
    if data.expiry <= now {
        return "Time left: expired".to_string();
    }
    let days = (data.expiry - now).num_days();
    format!("Time left: {days} days")
}

/// One-line summary of an [`ErrorState`] for the status row. Leans on
/// `ErrorStateCause::Display` for the readable cause text, then
/// annotates `(traffic blocked)` / `(NOT BLOCKING)` from the
/// `is_blocking()` predicate so the user knows whether the firewall
/// safety net is up.
fn error_state_summary(err: &ErrorState) -> String {
    let blocking = if err.is_blocking() {
        " (traffic blocked)"
    } else {
        " (NOT BLOCKING)"
    };
    format!("{}{}", err.cause(), blocking)
}

fn location_line(state: Option<&TunnelState>) -> String {
    match state.and_then(TunnelState::get_location) {
        Some(loc) => match loc.city.as_deref() {
            Some(city) => format!("{city}, {country}", country = loc.country),
            None => loc.country.clone(),
        },
        None => "(location unknown)".to_string(),
    }
}

/// Active-features list shown on the status block, formatted as
/// asterisk-prefixed strings.
///
/// Two sources, picked by tunnel state:
/// - **Connected / Connecting**: the daemon-reported [`FeatureIndicators`] on the tunnel state.
///   These are the *active* features for the running connection (e.g. multihop is only listed if
///   the entry endpoint actually came up). Authoritative once we have a live tunnel.
/// - **Anything else** (Disconnected / Disconnecting / Error / no daemon state yet): a small slice
///   of `Settings`. We surface only flags whose on-state is meaningful while disconnected -
///   kill-switch-adjacent config the user wants visible at a glance - rather than every future
///   feature. Configured-but-not-running is shown so the user can tell at a glance what *will* turn
///   on after `Connect`.
fn collect_features(app: &App) -> Vec<String> {
    if let Some(indicators) = active_feature_indicators(app.connection_status()) {
        return format_indicators(indicators);
    }
    let Some(settings) = app.settings() else {
        return Vec::new();
    };
    [
        (
            "*Quantum resistance",
            settings
                .tunnel_options
                .wireguard
                .quantum_resistant
                .enabled(),
        ),
        ("*Lockdown mode", settings.lockdown_mode),
        ("*Local network sharing", settings.allow_lan),
    ]
    .into_iter()
    .filter(|&(_, on)| on)
    .map(|(label, _)| label.to_string())
    .collect()
}

/// Pull `feature_indicators` out of an active tunnel state. `Some` for
/// the two variants that carry indicators (Connected / Connecting),
/// `None` for everything else - including states where the daemon
/// reports something is wrong (Error) and the indicator list would be
/// stale or absent.
fn active_feature_indicators(state: Option<&TunnelState>) -> Option<&FeatureIndicators> {
    match state? {
        TunnelState::Connected {
            feature_indicators, ..
        }
        | TunnelState::Connecting {
            feature_indicators, ..
        } => Some(feature_indicators),
        _ => None,
    }
}

/// Format a non-sorted [`FeatureIndicators`] as a list of asterisk-prefixed
/// rows in alphabetical order - matches upstream's own `Display` order, so
/// the rows stay stable across frames (the underlying set is a `HashSet`).
fn format_indicators(indicators: &FeatureIndicators) -> Vec<String> {
    let mut labels: Vec<String> = indicators
        .active_features()
        .map(|feature| format!("*{feature}"))
        .collect();
    labels.sort();
    labels
}

/// Expanded "Connection details" block. Mirrors what
/// `mullvad status --verbose` surfaces: tunnel WireGuard endpoint +
/// protocol, optional multihop entry endpoint, optional obfuscation
/// endpoint and type, the daemon-allocated tunnel interface name,
/// and the daemon-reported IPv4/IPv6. The hostname is intentionally
/// omitted - it's already shown in the relay row above.
///
/// Returns a one-line placeholder when there's no active endpoint.
fn connection_details(state: Option<&TunnelState>) -> Vec<String> {
    let Some(endpoint) = state.and_then(TunnelState::endpoint) else {
        return vec!["    (no active endpoint)".to_string()];
    };
    let location = state.and_then(TunnelState::get_location);
    let mut out = Vec::new();

    // ---- Tunnel interface (Linux: `wgN`; macOS: `utunN`) ----
    if let Some(interface) = endpoint.tunnel_interface.as_deref() {
        out.push(format!("    WireGuard interface: {interface}"));
    }

    if let Some(ref entry) = endpoint.entry_endpoint {
        // Annotate with the daemon's reported entry hostname when
        // available (multihop).
        out.push(format!("In        {}", entry.address));
    } else {
        // ---- Tunnel endpoint (always present when we have an endpoint) ----
        out.push(format!(
            "In        {} {}",
            endpoint.endpoint.address, endpoint.endpoint.protocol
        ));
    }

    // ---- Obfuscation (when active) ----
    if let Some(ref info) = endpoint.obfuscation {
        for line in obfuscation_lines(info) {
            out.push(line);
        }
    }

    // ---- Daemon-reported visible IPs (verbose-only on CLI). The
    // hostname is intentionally omitted here because it's already
    // shown in the relay row above the details block.
    if let Some(loc) = location {
        if let Some(ipv4) = loc.ipv4 {
            out.push(format!("Out IPv4  {ipv4}"));
        }
        if let Some(ipv6) = loc.ipv6 {
            out.push(format!("Out IPv6  {ipv6}"));
        }
    }

    out
}

/// Format the obfuscation block from an [`ObfuscationInfo`]. Single
/// obfuscators get one line; multiplexer obfuscators expand into one
/// line per child endpoint with the obfuscation type called out per
/// row. Only called when `endpoint.obfuscation.is_some()`, so the
/// result always contributes at least one line.
fn obfuscation_lines(info: &ObfuscationInfo) -> Vec<String> {
    match info {
        ObfuscationInfo::Single(ep) => vec![format!(
            "    Obfuscation: {} via {}",
            ep.obfuscation_type, ep.endpoint.address
        )],
        ObfuscationInfo::Multiplexer {
            direct,
            obfuscators,
        } => {
            let mut lines = Vec::new();
            lines.push("    Obfuscation: multiplexer".to_string());
            if let Some(direct) = direct {
                lines.push(format!("      direct  {}", direct.address));
            }
            for ep in obfuscators {
                lines.push(format!(
                    "      {}  {}",
                    ep.obfuscation_type, ep.endpoint.address
                ));
            }
            lines
        }
    }
}

/// True if `widget` is one of this page's body widgets - used by the
/// run-loop's Enter dispatch to recognize Status-page activations.
pub fn owns_widget(widget: WidgetId) -> bool {
    StatusWidget::from_widget_id(widget).is_some()
}

/// Decide what the bottom Connect/Disconnect button should do given the
/// current tunnel state. Pulled out so the run-loop's Enter dispatch can
/// share the same predicate as the renderer's button label.
///
/// Connected, Connecting, and Blocked all dispatch through the
/// disconnect path - Connecting "Cancel" is implemented as a
/// disconnect at the daemon level, and Blocked needs a disconnect to
/// clear the lockdown firewall.
pub fn connect_button_is_disconnect(connection: Option<&TunnelState>) -> bool {
    matches!(
        classify(connection),
        StatusKind::Connected | StatusKind::Connecting | StatusKind::Blocked
    )
}

/// Run the action bound to a focused Status-page body widget. The
/// caller has already verified the widget belongs to this page via
/// [`owns_widget`], so any non-matching id falls into the `None` arm
/// of `from_widget_id` and the dispatch is a no-op.
pub async fn activate<S: MullvadService>(
    app: &mut App,
    service: &S,
    overlay: &mut OverlayMode,
    widget: WidgetId,
) {
    let Some(widget) = StatusWidget::from_widget_id(widget) else {
        return;
    };
    match widget {
        StatusWidget::DetailsToggle => {
            let state = app.status_page_state_mut();
            state.connection_details_expanded = !state.connection_details_expanded;
        }
        StatusWidget::SwitchLocation => {
            // Open the page expanded onto (and focused on) the
            // daemon's current relay selection so the user lands
            // directly on their configured country / city / relay
            // rather than on the search anchor.
            super::select_location::enter_with_current_selection_focused(app);
        }
        StatusWidget::RefreshConnection => match app.reconnect(service).await {
            Ok(true) => {}
            Ok(false) => {
                app.show_notification("Reconnect not initiated - already disconnected");
            }
            Err(error) => app.show_notification(format_action_error("reconnect", &error)),
        },
        StatusWidget::ConnectDisconnect => {
            if connect_button_is_disconnect(app.connection_status()) {
                let return_focus = app.page_focus().focused;
                *overlay = OverlayMode::Confirm {
                    title: "Confirm disconnect".to_string(),
                    message: "Disconnect the current tunnel session?".to_string(),
                    action: ConfirmAction::Disconnect,
                    return_focus,
                };
            } else {
                match app.connect(service).await {
                    Ok(true) => {}
                    Ok(false) => app.show_notification("Already connected - no action taken"),
                    Err(error) => app.show_notification(format_action_error("connect", &error)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_handles_no_daemon_state() {
        assert_eq!(classify(None), StatusKind::Disconnected);
    }

    #[test]
    fn status_kind_layout_predicates() {
        assert!(StatusKind::Connected.shows_details_toggle());
        assert!(!StatusKind::Connecting.shows_details_toggle());
        assert!(!StatusKind::Disconnecting.shows_details_toggle());
        assert!(!StatusKind::Disconnected.shows_details_toggle());
        assert!(!StatusKind::Blocked.shows_details_toggle());
    }

    #[test]
    fn status_label_text_per_kind() {
        assert_eq!(status_label_text(StatusKind::Connected), "CONNECTED");
        assert_eq!(status_label_text(StatusKind::Connecting), "CONNECTING…");
        assert_eq!(
            status_label_text(StatusKind::Disconnecting),
            "DISCONNECTING…"
        );
        assert_eq!(status_label_text(StatusKind::Disconnected), "DISCONNECTED");
        assert_eq!(status_label_text(StatusKind::Blocked), "BLOCKED");
    }

    #[test]
    fn connect_button_label_per_kind() {
        assert_eq!(connect_button_label(StatusKind::Connected), "Disconnect");
        assert_eq!(connect_button_label(StatusKind::Connecting), "Cancel");
        assert_eq!(connect_button_label(StatusKind::Disconnecting), "Connect");
        assert_eq!(connect_button_label(StatusKind::Disconnected), "Connect");
        assert_eq!(connect_button_label(StatusKind::Blocked), "Disconnect");
    }

    #[test]
    fn status_colors_per_kind() {
        assert_eq!(status_color(StatusKind::Connected), Color::LightGreen);
        assert_eq!(status_color(StatusKind::Connecting), Color::Yellow);
        assert_eq!(status_color(StatusKind::Disconnecting), Color::Yellow);
        assert_eq!(status_color(StatusKind::Disconnected), Color::LightRed);
        assert_eq!(status_color(StatusKind::Blocked), Color::Indexed(202));
    }

    fn render_features_lines(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<&str>>()
                    .concat()
            })
            .collect()
    }

    #[test]
    fn features_lines_show_first_two_plus_n_more_when_collapsed() {
        let flags = vec![
            "*Quantum resistance".to_string(),
            "*Lockdown mode".to_string(),
            "*Local network sharing".to_string(),
        ];
        let lines = format_features_lines(&flags, false, 80);
        let combined = render_features_lines(&lines).join("\n");
        assert!(combined.contains("*Quantum resistance"));
        assert!(combined.contains("*Lockdown mode"));
        assert!(
            combined.contains("*1 more…"),
            "collapsed line should advertise overflow ({combined:?})",
        );
        assert!(
            !combined.contains("Local"),
            "the third flag is hidden behind 'N more…' when collapsed ({combined:?})",
        );
    }

    #[test]
    fn features_lines_expanded_shows_all_flags() {
        let flags = vec![
            "*Quantum resistance".to_string(),
            "*Lockdown mode".to_string(),
            "*Local network sharing".to_string(),
        ];
        let lines = format_features_lines(&flags, true, 80);
        let combined = render_features_lines(&lines).join("\n");
        assert!(combined.contains("*Quantum resistance"));
        assert!(combined.contains("*Lockdown mode"));
        assert!(combined.contains("*Local network sharing"));
        assert!(
            !combined.contains("more"),
            "expanded shows everything, no overflow suffix ({combined:?})",
        );
    }

    #[test]
    fn features_lines_two_or_fewer_flags_render_on_one_line_when_wide_enough() {
        let flags = vec![
            "*Quantum resistance".to_string(),
            "*Lockdown mode".to_string(),
        ];
        let lines = format_features_lines(&flags, false, 80);
        assert_eq!(lines.len(), 1);
        let rendered = render_features_lines(&lines);
        assert_eq!(rendered[0], "*Quantum resistance *Lockdown mode");
    }

    #[test]
    fn features_lines_wrap_at_feature_boundary_when_too_narrow_for_one_line() {
        // "*Quantum resistance" = 19, "*Lockdown mode" = 14;
        // together with a separator = 34. Width 25 fits the first
        // alone, forcing the second onto its own line.
        let flags = vec![
            "*Quantum resistance".to_string(),
            "*Lockdown mode".to_string(),
        ];
        let lines = format_features_lines(&flags, false, 25);
        let rendered = render_features_lines(&lines);
        assert_eq!(
            rendered,
            vec![
                "*Quantum resistance".to_string(),
                "*Lockdown mode".to_string(),
            ],
        );
    }

    #[test]
    fn features_lines_never_break_within_a_label() {
        // Across a range of widths, every rendered line must be a
        // sequence of complete feature labels separated by single
        // spaces - never a label fragment.
        let flags = vec![
            "*Quantum resistance".to_string(),
            "*Lockdown mode".to_string(),
            "*Local network sharing".to_string(),
        ];
        for width in [10u16, 15, 20, 25, 30, 40, 80] {
            let lines = format_features_lines(&flags, true, width);
            for line in &lines {
                let rendered: String = line
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<&str>>()
                    .concat();
                // Each label begins with '*' and labels are joined by
                // " "; splitting on '*' yields per-label payloads
                // (with a trailing separator space where another label
                // follows). Reconstruct each label and require it match
                // a real input flag.
                let parts: Vec<&str> = rendered.split('*').filter(|s| !s.is_empty()).collect();
                for part in parts {
                    let label = format!("*{}", part.trim_end());
                    assert!(
                        flags.contains(&label),
                        "line {rendered:?} contains label fragment {label:?} \
                         that isn't a complete feature label (width={width})",
                    );
                }
            }
        }
    }

    #[test]
    fn relay_line_prefers_daemon_hostname_over_country_selection() {
        // User selected just "Germany"; daemon picked de-ber-wg-001.
        // The line should show the concrete relay, not "any in DE".
        let line = format_relay_line(Some("de-ber-wg-001"), None, || "any in DE".to_string());
        assert_eq!(line, "de-ber-wg-001");
    }

    #[test]
    fn relay_line_prefers_daemon_hostname_over_city_selection() {
        // User selected just "Barcelona"; daemon picked es-bcn-wg-001.
        let line = format_relay_line(Some("es-bcn-wg-001"), None, || {
            "any in Barcelona, ES".to_string()
        });
        assert_eq!(line, "es-bcn-wg-001");
    }

    #[test]
    fn relay_line_multihop_uses_both_hostnames() {
        let line = format_relay_line(Some("de-ber-wg-001"), Some("es-bcn-wg-001"), || {
            "any in DE".to_string()
        });
        assert_eq!(line, "de-ber-wg-001 via es-bcn-wg-001");
    }

    #[test]
    fn relay_line_falls_back_to_selection_when_disconnected() {
        // No daemon-reported hostname (typical of disconnected state)
        // -> fall back to the user's selection text.
        let line = format_relay_line(None, None, || "any in DE".to_string());
        assert_eq!(line, "any in DE");
    }

    #[test]
    fn relay_line_multihop_falls_back_when_exit_hostname_missing() {
        // Defensive: if the daemon reports an entry hostname but no
        // exit hostname, the exit half falls back to the user's
        // selection rather than dropping the relay context.
        let line = format_relay_line(None, Some("es-bcn-wg-001"), || "any in DE".to_string());
        assert_eq!(line, "any in DE via es-bcn-wg-001");
    }

    /// Build a `GeoIpLocation` for marker-policy tests. Only
    /// `latitude`, `longitude`, and `mullvad_exit_ip` matter to
    /// [`location_marker`]; the rest are filler.
    fn geo_loc(
        lat: f64,
        lon: f64,
        mullvad_exit_ip: bool,
    ) -> mullvad_types::location::GeoIpLocation {
        mullvad_types::location::GeoIpLocation {
            ipv4: None,
            ipv6: None,
            country: String::new(),
            city: None,
            latitude: lat,
            longitude: lon,
            mullvad_exit_ip,
            hostname: None,
            entry_hostname: None,
            obfuscator_hostname: None,
        }
    }

    #[test]
    fn marker_is_green_at_relay_when_connected() {
        use crate::test_support::connected_state;
        let TunnelState::Connected {
            endpoint,
            feature_indicators,
            ..
        } = connected_state()
        else {
            unreachable!()
        };
        let state = TunnelState::Connected {
            endpoint,
            location: Some(geo_loc(59.0, 18.0, true)),
            feature_indicators,
        };
        let marker = location_marker(Some(&state)).expect("marker on Connected with location");
        assert_eq!(marker.0, Color::Green);
        assert!((marker.1 - 59.0).abs() < f32::EPSILON);
        assert!((marker.2 - 18.0).abs() < f32::EPSILON);
    }

    #[test]
    fn marker_is_red_at_home_when_disconnected_lockdown_off_and_home_ip() {
        let state = TunnelState::Disconnected {
            location: Some(geo_loc(40.7, -74.0, false)),
            #[cfg(not(target_os = "android"))]
            locked_down: false,
        };
        let marker = location_marker(Some(&state)).expect("home marker on Disconnected");
        assert_eq!(marker.0, Color::LightRed);
        assert!((marker.1 - 40.7).abs() < f32::EPSILON);
        assert!((marker.2 - -74.0).abs() < f32::EPSILON);
    }

    #[test]
    fn no_marker_when_disconnected_but_location_is_stale_tunnel_ip() {
        // Just-disconnected: daemon hasn't refreshed the IP yet, so
        // `mullvad_exit_ip` still reports the prior tunnel exit. A red
        // dot at the relay would mislead the user about where they
        // are, so suppress it.
        let state = TunnelState::Disconnected {
            location: Some(geo_loc(59.0, 18.0, true)),
            #[cfg(not(target_os = "android"))]
            locked_down: false,
        };
        assert!(location_marker(Some(&state)).is_none());
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn no_marker_when_disconnected_with_lockdown_on() {
        // Lockdown blocks all traffic, so even though the daemon may
        // have a cached home location, the user isn't actually
        // reaching the internet from there right now - showing a red
        // "you are here" would clash with the BLOCKING INTERNET banner.
        let state = TunnelState::Disconnected {
            location: Some(geo_loc(40.7, -74.0, false)),
            locked_down: true,
        };
        assert!(location_marker(Some(&state)).is_none());
    }

    #[test]
    fn no_marker_when_no_location_or_no_state() {
        let state = TunnelState::Disconnected {
            location: None,
            #[cfg(not(target_os = "android"))]
            locked_down: false,
        };
        assert!(location_marker(Some(&state)).is_none());
        assert!(location_marker(None).is_none());
    }

    #[test]
    fn features_lines_empty_renders_blank() {
        let lines = format_features_lines(&[], false, 80);
        assert_eq!(lines.len(), 1);
        let rendered = render_features_lines(&lines);
        assert!(
            rendered[0].is_empty(),
            "no flags -> blank line ({rendered:?})",
        );
    }
}
