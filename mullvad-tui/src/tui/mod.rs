// SPDX-License-Identifier: GPL-3.0-or-later

use std::io;

use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use crate::{
    app::{ANTI_CENSORSHIP_PORT_RANGE, App, ConfirmAction, FocusRegistry, PageId, WidgetId},
    integration::{
        AppEvent, DeviceState, IntegrationError, MullvadService, RpcMullvadService,
        project_relay_list,
    },
    logging::LogEntry,
};

/// Background color stamped on the cell range under the mouse cursor
/// for whichever focusable widget covers it. `DarkGray` is the ANSI
/// "bright black" slot, which both light and dark terminal themes
/// render as a mid-gray that contrasts against the default surface.
const HOVER_BG: ratatui::style::Color = ratatui::style::Color::Indexed(237);

/// Stamp [`HOVER_BG`] on the cell range covered by whichever
/// focusable widget sits under `cursor`. Runs as the last paint pass
/// in every draw, so it bg-overlays on top of the page, any input
/// modal, and any overlay - while reading from the topmost layer's
/// registry, which correctly suppresses page-widget highlights when
/// a modal or overlay has reset the registry to its own widgets.
///
/// Sets only the bg, leaving each cell's existing fg untouched - so
/// the focused widget's yellow text and the danger button's red text
/// both stay readable on the gray. Cells whose bg is already yellow
/// are skipped: that bg is reserved for the inline text-input cursor
/// glyph (e.g. the MTU pill placeholder), and hovering over it must
/// not mask the cursor color. `None` cursor (no mouse event yet this
/// session) and a cursor that misses every registered rect both no-op.
fn paint_hover_highlight(
    buffer: &mut ratatui::buffer::Buffer,
    registry: &FocusRegistry,
    cursor: Option<(u16, u16)>,
) {
    let Some((col, row)) = cursor else { return };
    let Some(rect) = registry.rect_at(col, row) else {
        return;
    };
    for y in rect.y..rect.y.saturating_add(rect.height) {
        for x in rect.x..rect.x.saturating_add(rect.width) {
            let cell = &mut buffer[(x, y)];
            if cell.bg == ratatui::style::Color::Yellow {
                continue;
            }
            cell.bg = HOVER_BG;
        }
    }
}

/// Build the breadcrumb segment list for `page`. Top-level pages
/// return an empty vector (no breadcrumb shown); sub-pages return
/// `[(parent_label, false), ..., (page_label, true)]` so the active
/// segment is the deepest one.
fn breadcrumb_segments(page: PageId) -> Vec<(&'static str, bool)> {
    let root = page.top_level_root();
    if page == root {
        return Vec::new();
    }
    // Walk up the parent chain via `PageId::parent_sub_page` until we
    // reach the top-level root. Adding a new nested sub-page is then
    // "wire its parent in `parent_sub_page`" - no breadcrumb
    // bookkeeping here.
    let mut chain: Vec<PageId> = Vec::new();
    let mut cursor = page;
    loop {
        chain.push(cursor);
        match cursor.parent_sub_page() {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    chain.reverse();
    let mut segments = Vec::with_capacity(chain.len() + 1);
    segments.push((root.breadcrumb_label(), false));
    let last = chain.len() - 1;
    for (i, p) in chain.iter().enumerate() {
        segments.push((p.breadcrumb_label(), i == last));
    }
    segments
}

/// Per-page hint bar contents, keyed on `PageId`. Top-level pages get
/// the standard nav set; sub-pages get the back-and-move set; Logs has
/// page-specific scroll/filter hints.
fn page_hints(app: &App) -> &'static [(&'static str, &'static str)] {
    const TOP_LEVEL: &[(&str, &str)] = &[
        ("1-4", "Tabs"),
        ("↑↓←→", "Move"),
        ("Enter", "Activate"),
        ("q", "Quit"),
    ];
    const LOGS: &[(&str, &str)] = &[
        ("1-4", "Tabs"),
        ("↑↓", "Line"),
        ("PgUp/PgDn", "Page"),
        ("Home/End", "Top/Tail"),
        ("q", "Quit"),
    ];
    const SUB_PAGE: &[(&str, &str)] = &[
        ("Esc", "Back"),
        ("↑↓←→", "Move"),
        ("Enter", "Activate"),
        ("q", "Quit"),
    ];
    // When the MTU pill is focused with a pending edit, the first Esc
    // is field-local (revert) - the global `Back` semantics only kick in
    // on the *second* Esc once the buffer is clean. Surface that to the
    // user via the hint bar so they don't have to remember the
    // two-step.
    const SUB_PAGE_MTU_DIRTY: &[(&str, &str)] = &[
        ("Esc", "Abort"),
        ("↑↓←→", "Move"),
        ("Enter", "Activate"),
        ("q", "Quit"),
    ];
    // Same two-step pattern for the Select-location filter: Esc on a
    // non-empty query clears the filter (field-local) before the
    // global `Back` kicks in on the next press.
    const SUB_PAGE_FILTER_DIRTY: &[(&str, &str)] = &[
        ("Esc", "Clear"),
        ("↑↓←→", "Move"),
        ("Enter", "Activate"),
        ("q", "Quit"),
    ];

    let page = app.current_page();
    let root = page.top_level_root();
    if page == root {
        match page {
            PageId::Logs => LOGS,
            _ => TOP_LEVEL,
        }
    } else if mtu_field_dirty(app) {
        SUB_PAGE_MTU_DIRTY
    } else if select_location_filter_dirty(app) {
        SUB_PAGE_FILTER_DIRTY
    } else {
        SUB_PAGE
    }
}

/// True when the user is on the Select-location page with focus on the
/// search anchor *and* a non-empty filter. Drives the hint bar's Esc
/// label swap (`Back` -> `Clear`); the Esc keystroke dispatch already
/// uses an inline guard against the same condition.
fn select_location_filter_dirty(app: &App) -> bool {
    app.current_page() == crate::app::PageId::SelectLocation
        && app.page_focus().focused == Some(pages::select_location::widgets::SEARCH_ANCHOR)
        && !app.select_location_page_state().query().is_empty()
}

/// True when the user is actively editing the inline MTU pill and the
/// buffer's parsed value differs from the daemon's MTU. Drives both the
/// Esc-key dispatch (field-local revert vs. global Back) and the hint
/// bar's Esc label.
fn mtu_field_dirty(app: &App) -> bool {
    if !mtu_field_focused(app) {
        return false;
    }
    let buffer = app.settings_page_state().mtu_buffer();
    let daemon_mtu = app.settings().and_then(|s| s.tunnel_options.wireguard.mtu);
    match pages::settings::parse_mtu_input(&buffer) {
        Ok(parsed) => parsed != daemon_mtu,
        // An unparseable buffer can only happen via a pending user edit
        // (the daemon-sync path always produces a parseable string), so
        // treat it as dirty.
        Err(_) => true,
    }
}

mod components;
mod error;
mod keybindings;
mod modals;
mod overlays;
mod pages;

use error::format_action_error;
use keybindings::{Action, map_key_event};
use modals::{InputMode, InputOutcome};
use overlays::OverlayMode;

pub async fn run(
    app: &mut App,
    log_tx: mpsc::Sender<LogEntry>,
    log_rx: mpsc::Receiver<LogEntry>,
) -> Result<()> {
    tracing::info!(
        "mullvad-tui starting (version {})",
        mullvad_version::VERSION
    );

    let service = RpcMullvadService::new()
        .await
        .context("Cannot connect to mullvad-daemon. Is the Mullvad daemon running?")?;
    tracing::info!("Connected to mullvad-daemon");

    // Pull the initial state for every push-cached value before the event
    // loop starts. The daemon's `events_listen` only fires on *changes*, so
    // a TUI subscribed mid-session would otherwise see nothing until the
    // user did something - empty panels, "not yet loaded" everywhere. The
    // resync turns that into "everything visible on the first frame".
    if let Err(error) = app.resync(&service).await {
        tracing::warn!("Initial state resync failed: {error}. The UI may show stale data");
    }

    // Surface daemon-vs-client version mismatch as a startup warning. The
    // workspace's `mullvad-management-interface` is pinned to a specific
    // upstream commit; if the running daemon is from a different release
    // the protobuf schema can drift, leading to opaque "Failed to parse
    // gRPC response" errors when individual messages don't decode. After
    // resync, this is a cache read - no extra RPC.
    let client_version = mullvad_version::VERSION;
    match app.daemon_version() {
        Some(daemon_version) if daemon_version.trim() != client_version.trim() => {
            tracing::warn!(
                "Daemon/client version mismatch: client built against `{client_version}`, \
                daemon reports `{daemon_version}`. Some RPCs may fail with \
                'Failed to parse gRPC response' due to protobuf schema drift between \
                versions. Either rebuild the TUI against your daemon's version or \
                upgrade the daemon to match."
            );
        }
        Some(daemon_version) => tracing::info!("Daemon version matches client: {daemon_version}"),
        None => tracing::warn!("Daemon version unknown - initial resync may have failed."),
    }

    // Spawn the daemon-event listener on a separate connection. If the listener
    // can't start, fall back to poll-only mode rather than aborting the TUI.
    let app_events = match RpcMullvadService::spawn_event_listener().await {
        Ok(rx) => Some(rx),
        Err(error) => {
            tracing::warn!(
                "Daemon event listener failed to start: {error}. UI will fall back to manual refresh."
            );
            None
        }
    };

    // Spawn the daemon-log forwarder on a third dedicated connection.
    // Each line the daemon emits arrives as a `LogSource::Daemon`
    // entry in the same ring buffer the TUI's own tracing flows into,
    // so the Logs page renders both intermixed in arrival order. If
    // the listener can't start (typically because the daemon doesn't
    // expose `log_listen` over this socket), the TUI keeps running
    // with TUI-only logs.
    if let Err(error) = RpcMullvadService::spawn_log_listener(log_tx).await {
        tracing::warn!(
            "Daemon log listener failed to start: {error}. The Logs page will show TUI tracing only."
        );
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Mouse capture: enables clicking focusable widgets to focus +
    // activate them. Trade-off: terminal-native text selection
    // (drag-to-copy) is suppressed while the app is running. Most
    // terminals fall back to "hold Shift while dragging" for raw
    // selection.
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(app, &service, app_events, log_rx, &mut terminal).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
    )?;
    terminal.show_cursor()?;

    result
}

/// Detect and discard the byte stream of an SGR mouse event
/// (`ESC[<button;col;row M/m`) that crossterm's parser failed to
/// recognise as such and leaked into the event stream as a series of
/// `Char(...)` key events. The leak occurs when an `Esc` keystroke
/// lands immediately before the mouse sequence's leading `ESC`: the
/// parser consumes both `ESC` bytes resolving the standalone Esc and
/// the remaining `[<...M` bytes fall through as raw character events.
/// Without this filter the digits in the leaked column/row coordinates
/// hit the `1`-`4` tab shortcuts in [`keybindings::map_key_event`] and
/// randomly switch tabs while the user is just mashing Esc with the
/// cursor over the window. Same leak also corrupts inline-input fields
/// (MTU pill, account number) that consume `Char(digit)` directly.
///
/// State machine:
/// * `Idle` -> on `Esc`, advance to `AfterEsc` (and pass the Esc through; it is a real keystroke).
/// * `AfterEsc` -> on `Char('[')`, advance to `AfterEscBracket` and swallow the byte. Anything
///   else: reset and pass through.
/// * `AfterEscBracket` -> on `Char('<')`, advance to `Swallow` and swallow the byte. Anything else:
///   reset and pass through.
/// * `Swallow(n)` -> swallow chars; on `Char('M'|'m')` reset to `Idle`. `n` caps the swallow window
///   so a malformed sequence missing its terminator can't lock the filter on.
#[derive(Debug, Default, Clone, Copy)]
enum SgrLeakFilter {
    #[default]
    Idle,
    AfterEsc,
    AfterEscBracket,
    Swallow(u8),
}

/// Upper bound on the body of a leaked SGR mouse sequence we will
/// silently drop before giving up on the swallow state. A real SGR
/// body for sane terminal sizes is well under this (`~~;~~~;~~~` =
/// roughly 12 chars at most for a 999x999 terminal); the cap exists
/// to recover gracefully if the terminator byte itself was dropped.
const SGR_LEAK_MAX_SWALLOW: u8 = 16;

impl SgrLeakFilter {
    /// Returns `true` if `event` should be forwarded to the dispatcher,
    /// `false` if it is part of a leaked SGR mouse sequence and must
    /// be dropped.
    fn pass(&mut self, event: &Event) -> bool {
        let Event::Key(key) = event else {
            *self = Self::Idle;
            return true;
        };
        if key.kind != KeyEventKind::Press {
            return true;
        }
        match (*self, key.code) {
            (Self::Swallow(_), KeyCode::Char('M' | 'm')) => {
                *self = Self::Idle;
                false
            }
            (Self::Swallow(n), _) => {
                if n + 1 >= SGR_LEAK_MAX_SWALLOW {
                    *self = Self::Idle;
                } else {
                    *self = Self::Swallow(n + 1);
                }
                false
            }
            (Self::AfterEscBracket, KeyCode::Char('<')) => {
                *self = Self::Swallow(0);
                false
            }
            (Self::AfterEsc, KeyCode::Char('[')) => {
                *self = Self::AfterEscBracket;
                false
            }
            (_, KeyCode::Esc) => {
                *self = Self::AfterEsc;
                true
            }
            _ => {
                *self = Self::Idle;
                true
            }
        }
    }
}

async fn run_loop<S: MullvadService>(
    app: &mut App,
    service: &S,
    mut app_events: Option<mpsc::Receiver<AppEvent>>,
    mut log_rx: mpsc::Receiver<LogEntry>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    let mut input_mode = InputMode::Default;
    // Page-widget id focused immediately before the most recent input
    // modal opened. The modal-render block in this run loop wipes the
    // focus registry to its own buttons, so `set_focus_registry` snaps
    // page focus to a modal widget on the first frame after the modal
    // opens. Without restoring on close, the snap-to-first-body
    // fallback then lands focus on the page's top body widget (e.g.
    // the Status toggle on `Settings > VPN > Use custom DNS server`)
    // instead of the button that opened the modal. We track the slot
    // here rather than per-modal because every variant of `InputMode`
    // needs the same save/restore around the same render-cycle seam -
    // hanging it off each modal state struct would be 6x the surface
    // area for one behavior.
    let mut input_modal_return_focus: Option<crate::app::WidgetId> = None;
    let mut overlay = OverlayMode::None;
    let mut leak_filter = SgrLeakFilter::default();
    // One-shot pickup of the notification channel that App created on
    // construction. The run loop is the only consumer; subsequent
    // calls to `take_notification_receiver` return None.
    let mut notification_rx = app
        .take_notification_receiver()
        .expect("App::new creates the notification channel; only the run loop consumes it");
    let mut events = EventStream::new();

    // 30 fps animation ticker. Drives the Status-page globe lerp and
    // the Connect/Disconnect/Reconnect spinners (status-label and
    // action-button). Gated by `App::needs_animation_tick` so we stay
    // idle when nothing on screen is moving. The first tick fires
    // immediately; missed ticks are dropped (we don't need to
    // backfill - every frame just samples the current animation t).
    let mut animation_ticker = {
        let mut t = tokio::time::interval(std::time::Duration::from_millis(33));
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        t
    };

    while !app.should_quit() {
        // Push the current camera target into the animation tracker
        // *before* draw so the renderer samples a fresh interpolated
        // value. The closure captures the renderer-side mapping that
        // App can't reference directly (tui_globe lives outside `app`).
        app.advance_status_camera(
            |app| {
                pages::status::camera_target_for(
                    app.connection_status(),
                    app.current_relay_selection(),
                )
            },
            std::time::Instant::now(),
        );

        // Render-and-snap loop. The renderer reads `app.page_focus()` to
        // decide which widget gets the yellow highlight, so that read
        // happens *before* `set_focus_registry` runs. On the first
        // frame after a page change (top-level tab switch, sub-page
        // entry/leave, or a widget vanishing), the pre-render focus
        // points at a widget that isn't on the new page, so no body
        // widget gets highlighted. `set_focus_registry` then snaps
        // focus to the new page's first body widget - but the user
        // wouldn't see that highlight until the *next* event-driven
        // redraw, so the new page would appear with no focus indicator.
        // Closing the gap: if the snap moved focus, immediately redraw
        // so the highlight is on screen before we yield to the event
        // loop. One re-render suffices because the snapped focus is
        // already in the new registry, so the second snap is a no-op.
        let mut redraws_remaining = 1u8;
        loop {
            // Built up by the renderer below, then handed back to App after the
            // draw closure exits so the *next* keystroke can resolve arrow-key
            // navigation against the just-rendered widget layout.
            let mut registry = FocusRegistry::new();
            terminal.draw(|frame| {
                if components::is_small_terminal(frame.area()) {
                    components::render_small_terminal_warning(frame, frame.area());
                    return;
                }

                let current_page = app.current_page();
                let is_sub_page = app.is_on_sub_page();
                let regions = components::split_layout(
                    frame,
                    frame.area(),
                    is_sub_page,
                    app.title(),
                    app.page_focus(),
                    &mut registry,
                );

                components::render_tab_bar(
                    frame,
                    regions.tabs,
                    current_page,
                    app.page_focus(),
                    &mut registry,
                );

                // Sub-pages get a centered breadcrumb between the tab bar
                // and the body. Top-level pages have no breadcrumb slot
                // reserved (see `split_layout`), so this is a no-op there.
                if let Some(breadcrumb_area) = regions.breadcrumb {
                    let segments = breadcrumb_segments(current_page);
                    components::render_breadcrumb(
                        frame,
                        breadcrumb_area,
                        &segments,
                        app.page_focus().focused,
                        &mut registry,
                    );
                }

                // Hint bar at the bottom - content swaps based on whether
                // we're on a top-level page (tab shortcuts) or a sub-page
                // (Esc-back). Logs has its own scroll/filter hints.
                let hints = page_hints(app);
                components::render_hint_bar(frame, regions.hint_bar, hints);

                let body_area = regions.body;
                // Per-page dispatch: each `PageId` routes to its own
                // `pages::*::render`.
                match app.current_page() {
                    crate::app::PageId::Logs => {
                        // Logs intentionally ignore the body's 1ch L/R
                        // padding - long log lines are precious and the
                        // page already paints its own bordered viewport,
                        // so full-bleed maximises usable column count. For
                        // the same reason it also reclaims the 1-row margin
                        // between body and hint bar: the bordered viewport
                        // already separates the panel from the hint bar
                        // visually, so the breathing room is wasted on Logs.
                        let logs_area = ratatui::layout::Rect {
                            height: regions.body_full_width.height.saturating_add(1),
                            ..regions.body_full_width
                        };
                        pages::logs::render(frame, logs_area, app);
                    }
                    crate::app::PageId::Status => {
                        pages::status::render(
                            frame,
                            body_area,
                            regions.body_full_width,
                            app,
                            &mut registry,
                        );
                    }
                    crate::app::PageId::SelectLocation => {
                        pages::select_location::render(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::SelectLocationFilter => {
                        pages::select_location_filter::render(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::Account => {
                        pages::account::render(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::AccountDevices => {
                        pages::account::render_devices(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::Settings => {
                        pages::settings::render(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::SettingsMultihop => {
                        pages::settings::render_multihop(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::SettingsDaita => {
                        pages::settings::render_daita(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::SettingsVpn => {
                        pages::settings::render_vpn_settings(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::SettingsDnsBlockers => {
                        pages::settings::render_dns_blockers(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::SettingsCustomDns => {
                        pages::settings::render_custom_dns(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::SettingsAntiCensorship => {
                        pages::settings::render_anti_censorship(
                            frame,
                            body_area,
                            app,
                            &mut registry,
                        );
                    }
                    crate::app::PageId::SettingsApiAccess => {
                        pages::settings::render_api_access(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::SettingsSplitTunnel => {
                        pages::settings::render_split_tunnel(frame, body_area, app, &mut registry);
                    }
                    crate::app::PageId::SettingsRelayOverrides => {
                        pages::settings::render_relay_overrides(
                            frame,
                            body_area,
                            app,
                            &mut registry,
                        );
                    }
                }

                // Layered popup rendering: input modal first (over
                // the page), then the overlay (Confirm/Notification)
                // on top of *both*. Each layer resets the registry
                // before drawing so only the topmost layer's widgets
                // are interactable - clicks/arrows on lower layers
                // are inert until the layer above is dismissed.
                //
                // This stacking is what lets a validation
                // notification fired by an input-modal `submit()`
                // appear immediately on top of the modal instead of
                // queuing silently until the modal closes.
                if !matches!(input_mode, InputMode::Default) {
                    registry = crate::app::FocusRegistry::default();
                    match &mut input_mode {
                        InputMode::AccountInput(state) => components::render_input_prompt(
                            frame,
                            "Account login",
                            "Enter Mullvad account number (16 digits):",
                            &state.buffer,
                            "Log in",
                            state.focus,
                            &mut registry,
                        ),
                        InputMode::PortInput(state) => {
                            // The mode label is dynamic (driven by which
                            // obfuscation is currently selected) and the
                            // range bounds reference [`ANTI_CENSORSHIP_PORT_RANGE`]
                            // so the prompt text can't drift from the parser's
                            // accepted range. One runtime allocation per frame
                            // while the modal is open.
                            let prompt = format!(
                                "Enter port for anti-censorship mode '{}' ({}-{}, blank for any):",
                                state.mode,
                                ANTI_CENSORSHIP_PORT_RANGE.start(),
                                ANTI_CENSORSHIP_PORT_RANGE.end(),
                            );
                            components::render_input_prompt(
                                frame,
                                "Port",
                                &prompt,
                                &state.buffer,
                                "Submit",
                                state.focus,
                                &mut registry,
                            );
                        }
                        InputMode::VoucherInput(state) => components::render_input_prompt(
                            frame,
                            "Redeem voucher",
                            "Enter voucher code:",
                            &state.buffer,
                            "Redeem",
                            state.focus,
                            &mut registry,
                        ),
                        InputMode::CustomDnsInput(state) => components::render_input_prompt(
                            frame,
                            "Custom DNS server",
                            "Enter custom DNS server (IPv4 or IPv6):",
                            &state.buffer,
                            if state.edit_index.is_some() {
                                "Save"
                            } else {
                                "Add"
                            },
                            state.focus,
                            &mut registry,
                        ),
                        InputMode::SplitTunnelPathInput(state) => components::render_input_prompt(
                            frame,
                            "Add split-tunnel app",
                            "Enter app path (e.g. C:\\Users\\…\\app.exe or /Applications/Foo.app):",
                            &state.buffer,
                            "Add",
                            state.focus,
                            &mut registry,
                        ),
                        InputMode::SplitTunnelPidInput(state) => components::render_input_prompt(
                            frame,
                            "Add split-tunnel PID",
                            "Enter process ID (positive integer):",
                            &state.buffer,
                            "Add",
                            state.focus,
                            &mut registry,
                        ),
                        InputMode::RelayOverrideInput(state) => {
                            use crate::tui::modals::relay_override::FieldFocus;
                            let fields = [
                                components::InputField {
                                    label: "Hostname:",
                                    buffer: &state.hostname,
                                    focused: matches!(state.focus, FieldFocus::Hostname),
                                },
                                components::InputField {
                                    label: "IPv4 in-address (leave blank to skip):",
                                    buffer: &state.ipv4,
                                    focused: matches!(state.focus, FieldFocus::Ipv4),
                                },
                                components::InputField {
                                    label: "IPv6 in-address (leave blank to skip):",
                                    buffer: &state.ipv6,
                                    focused: matches!(state.focus, FieldFocus::Ipv6),
                                },
                            ];
                            components::render_multi_field_input_prompt(
                                frame,
                                "Set server IP override",
                                "Override the IPv4/IPv6 in-address for a specific relay.",
                                &fields,
                                "Save",
                                matches!(state.focus, FieldFocus::Cancel),
                                matches!(state.focus, FieldFocus::Save),
                                &mut registry,
                            );
                        }
                        InputMode::Default => unreachable!("guarded by outer match"),
                    }
                }
                match &overlay {
                    OverlayMode::None => {}
                    OverlayMode::Confirm { title, message, .. } => {
                        registry = crate::app::FocusRegistry::default();
                        components::render_confirm_overlay(
                            frame,
                            title,
                            message,
                            &mut registry,
                            app.page_focus().focused,
                        );
                    }
                    OverlayMode::Notification { message, .. } => {
                        registry = crate::app::FocusRegistry::default();
                        components::render_notification_overlay(
                            frame,
                            message,
                            &mut registry,
                            app.page_focus().focused,
                        );
                    }
                }

                paint_hover_highlight(frame.buffer_mut(), &registry, app.cursor());
            })?;

            // Hand the just-rendered registry back to App so arrow-key handling
            // on the *next* keystroke knows where each focusable widget is.
            // Snaps focus to the first widget if no widget was focused (or
            // if the previously-focused widget disappeared between frames).
            // The `active_tab` hint anchors the snap to the user's current
            // page when the body has no focusables (Logs) - otherwise the
            // fallback would land on `registry.first()` = Status tab,
            // visually reading as a random tab jump.
            let focus_before_snap = app.page_focus().focused;
            let active_tab = components::tab_widget_id_for_top_level(app.current_page());
            app.set_focus_registry(std::mem::take(&mut registry), Some(active_tab));
            if app.page_focus().focused == focus_before_snap || redraws_remaining == 0 {
                break;
            }
            redraws_remaining -= 1;
        }

        tokio::select! {
            biased;
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        if !leak_filter.pass(&event) {
                            continue;
                        }
                        match event {
                            Event::Key(key) => {
                                let snapshot = ModalLifecycleSnapshot::take(&input_mode);
                                handle_key_event(
                                    key,
                                    app,
                                    &mut input_mode,
                                    &mut overlay,
                                    service,
                                )
                                .await;
                                snapshot.apply(
                                    &input_mode,
                                    app,
                                    &mut input_modal_return_focus,
                                );
                            }
                            Event::Mouse(mouse) => {
                                let snapshot = ModalLifecycleSnapshot::take(&input_mode);
                                handle_mouse_event(
                                    mouse,
                                    app,
                                    &mut input_mode,
                                    &mut overlay,
                                    service,
                                )
                                .await;
                                snapshot.apply(
                                    &input_mode,
                                    app,
                                    &mut input_modal_return_focus,
                                );
                            }
                            _ => continue,
                        }
                    }
                    Some(Err(_)) => continue,
                    None => break,
                }
            }
            maybe_app_event = recv_app_event(&mut app_events) => {
                match maybe_app_event {
                    Some(event) => apply_app_event(app, event, service).await,
                    None => app_events = None,
                }
            }
            maybe_log = log_rx.recv() => {
                // `None` means every `tracing` Sender (the layer's `tx`) was
                // dropped - only happens at process shutdown when the global
                // subscriber goes away. Continue gracefully; the `recv` future
                // will then return `None` immediately on subsequent polls and
                // `select!` will fall through to other arms.
                if let Some(entry) = maybe_log {
                    app.append_log_entry(entry);
                }
            }
            // Drain notification requests from any code path that
            // called `App::show_notification`. Latest-wins semantics:
            // the most recent message overwrites whatever was on
            // screen. If a notification arrives while another overlay
            // is already open, inherit that overlay's `return_focus`
            // so the user lands back on their original page widget
            // when the notification is dismissed (rather than on a
            // now-stale overlay button).
            maybe_msg = notification_rx.recv() => {
                if let Some(message) = maybe_msg {
                    let return_focus = match &overlay {
                        OverlayMode::None => app.page_focus().focused,
                        OverlayMode::Confirm { return_focus, .. }
                        | OverlayMode::Notification { return_focus, .. } => *return_focus,
                    };
                    overlay = OverlayMode::Notification {
                        message,
                        return_focus,
                    };
                }
            }
            // Camera-animation tick - only active while the Status
            // page's globe is mid-transition. Empty body: we just
            // need to wake the loop so the next iteration's draw
            // samples a fresh animation value.
            _ = animation_ticker.tick(), if app.needs_animation_tick() => {}
        }
    }

    Ok(())
}

/// Await the next [`AppEvent`] from the daemon stream, or pend forever once the
/// stream has been retired (so the surrounding `select!` arm becomes inert
/// instead of tight-looping on `None`).
async fn recv_app_event(app_events: &mut Option<mpsc::Receiver<AppEvent>>) -> Option<AppEvent> {
    match app_events.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Apply a daemon-pushed [`AppEvent`] to the cached App state.
///
/// Async + service-aware so that event handlers can fire derived RPC follow-ups
/// inline (e.g. `DeviceChanged(LoggedIn)` triggers a `get_account_data` fetch
/// because the daemon's device push doesn't carry the account-data payload).
/// Inline `await` is acceptable while side effects stay short and infrequent;
/// if more event-driven RPCs accrete, switch to a `tokio::spawn` + fan-in
/// channel to keep the event loop unblocked.
async fn apply_app_event<S: MullvadService>(app: &mut App, event: AppEvent, service: &S) {
    match event {
        AppEvent::StatusChanged(status) => app.set_connection_status(status),
        AppEvent::SettingsChanged(settings) => app.set_settings(settings),
        AppEvent::AppVersionInfoChanged(info) => app.set_app_version_info(info),
        AppEvent::RelayListChanged(list) => app.set_relay_locations(project_relay_list(&list)),
        AppEvent::DeviceChanged(device) => {
            // Capture the account number before moving `device` into the App;
            // we need it to fire the `get_account_data` follow-up RPC since
            // the daemon's device push doesn't carry account data.
            let account_for_followup = match &device {
                DeviceState::LoggedIn(account_and_device) => {
                    Some(account_and_device.account_number.clone())
                }
                DeviceState::LoggedOut | DeviceState::Revoked => None,
            };
            app.set_device(device);

            match account_for_followup {
                Some(account) => match service.get_account_data(account).await {
                    Ok(data) => app.set_account_data(Some(data)),
                    Err(error) => {
                        // Don't interrupt the user - the device half is
                        // already updated. Log so the failure is visible
                        // in the Logs panel.
                        tracing::warn!("Account data follow-up failed: {error}");
                    }
                },
                None => app.set_account_data(None),
            }
        }
    }
}

/// True when the inline MTU input pill currently owns focus on the VPN
/// settings sub-page. Centralised so the keystroke intercept and the
/// commit-on-blur wrapper agree on what "focused" means.
fn mtu_field_focused(app: &App) -> bool {
    app.current_page() == crate::app::PageId::SettingsVpn
        && app.page_focus().focused == Some(pages::settings::widgets::VPN_MTU_EDIT)
}

/// Parse the inline MTU draft and push it to the daemon. No-op when the
/// parsed value already matches the daemon's current MTU (avoids a
/// redundant settings RPC every time the user defocuses an unchanged
/// field). Surfaces parse failures and validation errors as
/// notifications. Shared by the Enter handler and the commit-on-blur
/// path so both code paths apply the same value-and-error behavior.
async fn commit_pending_mtu<S: MullvadService>(app: &mut App, service: &S) {
    let buffer = app.settings_page_state().mtu_buffer();
    let parsed = match pages::settings::parse_mtu_input(&buffer) {
        Ok(v) => v,
        Err(message) => {
            app.show_notification(message);
            return;
        }
    };
    let current = app.settings().and_then(|s| s.tunnel_options.wireguard.mtu);
    if parsed == current {
        return;
    }
    if let Err(error) = app.set_mtu(service, parsed).await {
        app.show_notification(crate::tui::error::format_action_error("MTU update", &error));
    }
}

/// Top-level keystroke handler. Wraps [`handle_key_event_inner`] with a
/// commit-on-blur hook for the inline MTU input pill: when the body
/// moves focus off the field (Esc out of the sub-page, arrow off the
/// row, Tab away, top-level tab-jump, etc.) the buffer is auto-applied
/// the same way Enter would, with an error notification on invalid
/// input.
async fn handle_key_event<S: MullvadService>(
    key: KeyEvent,
    app: &mut App,
    input_mode: &mut InputMode,
    overlay: &mut OverlayMode,
    service: &S,
) {
    let was_mtu_focused = mtu_field_focused(app);
    handle_key_event_inner(key, app, input_mode, overlay, service).await;
    if was_mtu_focused && !mtu_field_focused(app) {
        commit_pending_mtu(app, service).await;
    }
}

async fn handle_key_event_inner<S: MullvadService>(
    key: KeyEvent,
    app: &mut App,
    input_mode: &mut InputMode,
    overlay: &mut OverlayMode,
    service: &S,
) {
    // When a modal is open, every keystroke is routed to it. The
    // modal's `handle_key` distinguishes typed/edited keys (Handled),
    // unrecognized keys (NotHandled - kept on the floor so terminal
    // shortcuts and arrows don't escape the modal), Esc (Cancel), and
    // Enter (Submit). On Submit, `submit` returns `true` to keep the
    // modal open for re-correction (empty buffer, parse error) and
    // `false` to close it.
    //
    // Skipped when an overlay (Confirm/Notification) is layered on
    // top of the modal - the overlay needs first crack at keys so
    // Esc dismisses *the overlay*, not the underlying modal. Once
    // the overlay clears, the next key falls through to this block
    // and the modal is interactive again.
    if matches!(*overlay, OverlayMode::None) && !matches!(input_mode, InputMode::Default) {
        match input_mode.handle_key(key) {
            InputOutcome::Handled | InputOutcome::NotHandled => return,
            InputOutcome::Cancel => {
                *input_mode = InputMode::Default;
                return;
            }
            InputOutcome::Submit => {
                let mode = std::mem::take(input_mode);
                if mode.submit(app, service).await {
                    *input_mode = mode;
                }
                return;
            }
        }
    }

    // Inline text input: when the SelectLocation page's search anchor
    // owns focus, printable chars and Backspace go straight into the
    // page's filter query rather than through `map_key_event` (which
    // would otherwise treat `q` as Quit, `1`-`4` as tab-jumps, etc.).
    // Arrows / Enter / Esc / Tab / Home / End / PgUp / PgDn still fall
    // through so the user can leave the search field with the focus
    // engine. Ctrl/Alt-modified keys also fall through to avoid
    // hijacking terminal shortcuts.
    if app.current_page() == crate::app::PageId::SelectLocation
        && app.page_focus().focused == Some(pages::select_location::widgets::SEARCH_ANCHOR)
    {
        let modifiers_ok = !key.modifiers.intersects(
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
        );
        if modifiers_ok && key.kind == crossterm::event::KeyEventKind::Press {
            match key.code {
                crossterm::event::KeyCode::Char(c) => {
                    app.select_location_page_state_mut().push_query_char(c);
                    return;
                }
                crossterm::event::KeyCode::Backspace => {
                    app.select_location_page_state_mut().pop_query_char();
                    return;
                }
                // Two-step Esc, mirroring the MTU pill: with text in
                // the filter, Esc clears it and stays on the page; with
                // an empty filter it falls through so the global Esc
                // leaves the sub-page.
                crossterm::event::KeyCode::Esc
                    if !app.select_location_page_state().query().is_empty() =>
                {
                    app.select_location_page_state_mut().clear_query();
                    return;
                }
                _ => {}
            }
        }
    }

    // Inline text input: when the VPN settings sub-page's MTU pill owns
    // focus, digits and Backspace edit the persistent draft buffer
    // directly (mirroring the search-anchor pattern above). Enter parses
    // the buffer and pushes the value to the daemon. Arrows / Esc /
    // Tab still fall through so the user can leave the field with the
    // focus engine. Ctrl/Alt-modified keys also fall through.
    if app.current_page() == crate::app::PageId::SettingsVpn
        && app.page_focus().focused == Some(pages::settings::widgets::VPN_MTU_EDIT)
    {
        let modifiers_ok = !key.modifiers.intersects(
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
        );
        if modifiers_ok && key.kind == crossterm::event::KeyEventKind::Press {
            match key.code {
                crossterm::event::KeyCode::Char(c) if c.is_ascii_digit() => {
                    app.settings_page_state().push_mtu_char(c);
                    return;
                }
                crossterm::event::KeyCode::Backspace => {
                    app.settings_page_state().pop_mtu_char();
                    return;
                }
                crossterm::event::KeyCode::Enter => {
                    commit_pending_mtu(app, service).await;
                    return;
                }
                // Two-step Esc:
                //   * dirty buffer (user has a pending edit) - revert to the daemon's value and
                //     stay on the field (return early; global Esc doesn't run).
                //   * clean buffer - no match here, falls through so Esc takes its global meaning
                //     (close overlay / leave sub-page). The commit-on-blur wrapper sees a buffer
                //     matching the daemon and short-circuits, so no RPC fires either.
                // The shared [`mtu_field_dirty`] helper also drives the
                // hint bar's `Esc -> Abort` / `Esc -> Back` swap, so the
                // visible label and the dispatch can't drift apart.
                crossterm::event::KeyCode::Esc if mtu_field_dirty(app) => {
                    let daemon_mtu = app.settings().and_then(|s| s.tunnel_options.wireguard.mtu);
                    app.settings_page_state()
                        .sync_mtu_buffer_from_daemon(daemon_mtu);
                    return;
                }
                _ => {}
            }
        }
    }

    // `/` shortcut: jump focus to the search anchor from anywhere on
    // the SelectLocation page. The block above already routed `/` to
    // the buffer when the anchor is already focused, so this only
    // fires when the user is elsewhere (tree, filter button, tabs).
    if app.current_page() == crate::app::PageId::SelectLocation
        && key.kind == crossterm::event::KeyEventKind::Press
        && key.code == crossterm::event::KeyCode::Char('/')
        && !key.modifiers.intersects(
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
        )
    {
        app.page_focus_mut().focused = Some(pages::select_location::widgets::SEARCH_ANCHOR);
        return;
    }

    let Some(action) = map_key_event(key) else {
        return;
    };

    match action {
        Action::Quit => app.quit(),
        Action::Arrow(dir) => {
            // SelectLocation page intercepts ←/→ on tree rows for
            // standard tree-view expand/collapse/parent/first-child
            // semantics. Returns true when the key was consumed; on
            // false (e.g. Right on a relay leaf) we fall through to
            // the generic focus-engine navigation below.
            if app.current_page() == crate::app::PageId::SelectLocation
                && matches!(
                    dir,
                    crate::app::ArrowDir::Left | crate::app::ArrowDir::Right
                )
                && let Some(focused) = app.page_focus().focused
                && pages::select_location::owns_tree_row(focused)
                && pages::select_location::handle_tree_arrow(app, focused, dir)
            {
                return;
            }
            // Logs page intercepts ↑/↓ to scroll the panel by 1 line.
            // The page has no body focusables, so the focus-engine
            // fallback would just walk into the tab bar - surprising
            // behavior for what visually reads as a scrollable
            // content area. ←/→ still fall through (no-op on Logs).
            if app.current_page() == crate::app::PageId::Logs
                && matches!(dir, crate::app::ArrowDir::Up | crate::app::ArrowDir::Down)
            {
                let (total, viewport) = app.logs_page_state().last_dimensions();
                match dir {
                    crate::app::ArrowDir::Up => {
                        app.logs_page_state().scroll_up_by(1, total, viewport);
                    }
                    crate::app::ArrowDir::Down => {
                        app.logs_page_state().scroll_down_by(1, total, viewport);
                    }
                    _ => unreachable!(),
                }
                return;
            }
            // Resolve against the registry from the *previous* render -
            // rebuilt fresh each frame, so positions stay current.
            if let Some(focused) = app.page_focus().focused
                && let Some(next) = app.last_focus_registry().navigate(focused, dir)
            {
                // When Up/Down lands anywhere on the tab bar, prefer
                // the *active* tab over whatever column-snap picked.
                // Without this, every body widget at col 0 would
                // land on `[Status]` when the user is actually on
                // (say) the Settings tab; they expect the tab bar
                // to "remember" where they are. Down matters for
                // moving from the `[x]` close button (rendered above
                // the tab row) into the tab bar.
                let resolved =
                    if matches!(dir, crate::app::ArrowDir::Up | crate::app::ArrowDir::Down) {
                        prefer_active_tab(app, next)
                    } else {
                        next
                    };
                app.page_focus_mut().focused = Some(resolved);
            }
        }
        Action::Home => {
            // Logs-scroll branch only fires on the Logs root with no
            // overlay open; with an overlay up the user is
            // interacting with the popup and the Logs panel
            // underneath shouldn't move.
            if app.current_page() == crate::app::PageId::Logs
                && matches!(*overlay, OverlayMode::None)
            {
                app.logs_page_state().scroll_to_top();
            } else {
                // When focus is inside a scrollable list, `Home`
                // should land on the list's first row - not jump
                // above to a sibling search anchor or filter
                // button. Fall back to `first_body_widget` when
                // the focused widget isn't in any scroll group
                // (or when there's no current focus at all).
                let registry = app.last_focus_registry();
                let target = app
                    .page_focus()
                    .focused
                    .and_then(|f| registry.first_in_scroll_group(f))
                    .or_else(|| registry.first_body_widget());
                if let Some(next) = target {
                    app.page_focus_mut().focused = Some(next);
                }
            }
        }
        Action::End => {
            if app.current_page() == crate::app::PageId::Logs
                && matches!(*overlay, OverlayMode::None)
            {
                app.logs_page_state().scroll_to_bottom();
            } else {
                let registry = app.last_focus_registry();
                let target = app
                    .page_focus()
                    .focused
                    .and_then(|f| registry.last_in_scroll_group(f))
                    .or_else(|| registry.last_widget());
                if let Some(next) = target {
                    app.page_focus_mut().focused = Some(next);
                }
            }
        }
        Action::PageUp => {
            if app.current_page() == crate::app::PageId::Logs
                && matches!(*overlay, OverlayMode::None)
            {
                let (total, viewport) = app.logs_page_state().last_dimensions();
                app.logs_page_state().page_up(total, viewport);
            } else if let Some(focused) = app.page_focus().focused {
                let delta = -(crate::tui::keybindings::PAGE_STEP_ROWS as isize);
                let registry = app.last_focus_registry();
                let target = registry
                    .move_rows_in_scroll_group(focused, delta)
                    .or_else(|| registry.move_rows(focused, delta));
                if let Some(next) = target {
                    app.page_focus_mut().focused = Some(next);
                }
            }
        }
        Action::PageDown => {
            if app.current_page() == crate::app::PageId::Logs
                && matches!(*overlay, OverlayMode::None)
            {
                let (total, viewport) = app.logs_page_state().last_dimensions();
                app.logs_page_state().page_down(total, viewport);
            } else if let Some(focused) = app.page_focus().focused {
                let delta = crate::tui::keybindings::PAGE_STEP_ROWS as isize;
                let registry = app.last_focus_registry();
                let target = registry
                    .move_rows_in_scroll_group(focused, delta)
                    .or_else(|| registry.move_rows(focused, delta));
                if let Some(next) = target {
                    app.page_focus_mut().focused = Some(next);
                }
            }
        }
        Action::CycleNextPane => {
            if let Some(focused) = app.page_focus().focused
                && let Some(next) = app.last_focus_registry().next_pane(focused)
            {
                app.page_focus_mut().focused = Some(prefer_active_tab(app, next));
            }
        }
        Action::CyclePrevPane => {
            if let Some(focused) = app.page_focus().focused
                && let Some(next) = app.last_focus_registry().prev_pane(focused)
            {
                app.page_focus_mut().focused = Some(prefer_active_tab(app, next));
            }
        }
        Action::Cancel => {
            // Esc. Priority order: dismiss an open overlay (confirm
            // prompt or notification) first, then leave the current
            // sub-page if any. On a top-level page with no overlay,
            // this is a no-op - quitting is `q`, not Esc.
            if !matches!(*overlay, OverlayMode::None) {
                close_overlay_restoring_focus(app, overlay);
            } else if app.is_on_sub_page() {
                app.leave_sub_page();
            }
        }
        Action::Activate => {
            if let Some(focused) = app.page_focus().focused {
                activate_focused(app, service, input_mode, overlay, focused).await;
            }
        }
        Action::NavigateTab(page) => {
            // While an overlay (confirm prompt or notification) is
            // open, the popup is the only thing the user should be
            // interacting with - `1`-`4` would otherwise switch the
            // page in the background, which is jarring and lets the
            // user lose track of what the overlay is asking. The
            // overlay's [Cancel]/[Dismiss] button is the way out.
            if !matches!(*overlay, OverlayMode::None) {
                return;
            }
            app.navigate_to(page);
            // Land focus on the newly-selected tab so the user's next
            // arrow press starts from the tab bar (and the yellow
            // focus highlight tracks the page they just jumped to).
            // Without this, focus stays wherever it was on the old
            // page; if that widget id isn't in the new page's
            // registry, the snap-to-first fallback drops focus into
            // the new page's body instead of the tab the user just
            // activated.
            if let Some(tab_id) = components::tab_widget_id(page) {
                app.page_focus_mut().focused = Some(tab_id);
            }
        }
    }
}

/// When focus lands on a tab-bar widget, swap it for the
/// currently-active tab's id. Used by `Up`-arrow nav and `Tab`/
/// `Shift+Tab` pane cycling: arriving at the tab bar should always
/// highlight where the user actually is, not whichever tab the
/// generic column-snap picked. Pass-through for non-tab-bar ids.
fn prefer_active_tab(app: &App, next: WidgetId) -> WidgetId {
    if components::tab_page_for_widget(next).is_some() {
        components::tab_widget_id_for_top_level(app.current_page())
    } else {
        next
    }
}

/// Activate the widget identified by `focused`. Shared by Enter
/// (`Action::Activate`) and mouse-click dispatch so a click and an
/// Enter on the same widget run identical logic. Routes overlay
/// buttons through the overlay-close path, tab-bar widgets through
/// `navigate_to`, and per-page widgets through their respective
/// `activate_*` helpers. Unknown ids are silently ignored.
async fn activate_focused<S: MullvadService>(
    app: &mut App,
    service: &S,
    input_mode: &mut InputMode,
    overlay: &mut OverlayMode,
    focused: WidgetId,
) {
    if focused == components::OVERLAY_CONFIRM_ACCEPT {
        // Snapshot the action before clearing the overlay so the
        // dispatch sees a stable target even if a re-render lands
        // between Activate and dispatch_confirm completing.
        let action = match &*overlay {
            OverlayMode::Confirm { action, .. } => Some(action.clone()),
            _ => None,
        };
        close_overlay_restoring_focus(app, overlay);
        if let Some(action) = action
            && let Err(error) = dispatch_confirm(app, service, action).await
        {
            app.show_notification(format_action_error("confirm action", &error));
        }
    } else if focused == components::OVERLAY_CONFIRM_REJECT
        || focused == components::OVERLAY_NOTIFICATION_DISMISS
    {
        close_overlay_restoring_focus(app, overlay);
    } else if focused == components::INPUT_MODAL_CANCEL {
        // Mouse click on `[Cancel]` inside an input popup. Mirrors
        // the keyboard `Esc` / `Enter on focused [Cancel]` path: drop
        // the modal back to `Default` and discard the buffer.
        *input_mode = InputMode::Default;
    } else if focused == components::INPUT_MODAL_SUBMIT {
        // Mouse click on `[Submit]` (or the modal's verb-specific
        // label like `[Redeem]`/`[Add]`). Mirrors the keyboard
        // submit path - `submit` returns `true` to keep the modal
        // open on validation failure (so the user can correct
        // without retyping), `false` to close it.
        let mode = std::mem::take(input_mode);
        if mode.submit(app, service).await {
            *input_mode = mode;
        }
    } else if focused == components::INPUT_MODAL_FIELD {
        // Mouse click on the buffer row of a single-field modal. Reset
        // the modal's internal focus to `Field` so the next keystroke
        // types into the buffer instead of activating a previously-
        // focused button. No buffer mutation here - the click is just
        // "put me in typing mode".
        input_mode.set_focus(crate::tui::modals::InputFocus::Field);
    } else if let Some(idx) = components::input_modal_field_index(focused) {
        // Mouse click on one of the buffer rows of a multi-field modal.
        // Route the per-field index to the modal's internal focus so
        // the click lands on the specific buffer that was clicked,
        // rather than always snapping to the first field.
        input_mode.set_field_index(idx);
    } else if focused == components::BREADCRUMB_BACK {
        // `[<]` button on a sub-page breadcrumb. Pops one frame off
        // the sub-page stack - same outcome as `Esc`. No-op when
        // somehow activated from a top-level page (the breadcrumb
        // row, and therefore this widget, isn't rendered there).
        if app.is_on_sub_page() {
            app.leave_sub_page();
        }
    } else if focused == components::WINDOW_CLOSE {
        // `[x]` close button on the top border. Same outcome as the
        // `q` keyboard shortcut.
        app.quit();
    } else if let Some(page) = components::tab_page_for_widget(focused) {
        app.navigate_to(page);
    } else if pages::status::owns_widget(focused) {
        pages::status::activate(app, service, overlay, focused).await;
    } else if pages::select_location::owns_widget(focused) {
        pages::select_location::activate(app, service, focused).await;
    } else if pages::select_location_filter::owns_widget(focused) {
        pages::select_location_filter::activate(app, focused);
    } else if pages::account::owns_top_widget(focused)
        || pages::account::remove_device_index(focused).is_some()
    {
        pages::account::activate(app, service, input_mode, overlay, focused).await;
    } else if pages::settings::owns_widget(focused) {
        pages::settings::activate(app, service, input_mode, overlay, focused).await;
    }
}

/// Translate a mouse event into focus + activation. Left-button-down
/// hit-tests against the previous frame's focus registry; on hit, the
/// widget receives focus and is activated through the same
/// [`activate_focused`] path Enter uses (so click and Enter behave
/// identically). All other mouse-event kinds (release, drag, move,
/// scroll, right/middle button) are ignored - TUI scrolling already
/// runs through PgUp/PgDn, and we don't want a stray drag to swallow
/// keyboard focus.
///
/// When a popup [`InputMode`] is open the registry doesn't include
/// the popup's text input, so a click that would otherwise hit a
/// page widget beneath would fight the popup. Treat those clicks as
/// no-ops; the user can dismiss with Esc.
async fn handle_mouse_event<S: MullvadService>(
    event: MouseEvent,
    app: &mut App,
    input_mode: &mut InputMode,
    overlay: &mut OverlayMode,
    service: &S,
) {
    let was_mtu_focused = mtu_field_focused(app);
    handle_mouse_event_inner(event, app, input_mode, overlay, service).await;
    if was_mtu_focused && !mtu_field_focused(app) {
        commit_pending_mtu(app, service).await;
    }
}

async fn handle_mouse_event_inner<S: MullvadService>(
    event: MouseEvent,
    app: &mut App,
    input_mode: &mut InputMode,
    overlay: &mut OverlayMode,
    service: &S,
) {
    // Capture the cursor position from every mouse event - moves,
    // clicks, drags, and scrolls. The next render's hover-highlight
    // pass hit-tests this against the just-built focus registry to
    // gray-bg the cell range under the cursor. Doing it here, ahead
    // of the kind-specific dispatch below, ensures the highlight
    // tracks the cursor even on events that early-return (scroll on
    // a page with no scrollable surface, clicks while a modal is up).
    app.set_cursor(event.column, event.row);

    // Mouse-wheel scroll. Routed per page:
    //   * Logs has a focus-independent scroll surface; adjust the log scroll offset directly.
    //   * Select location moves the tree viewport via the page state's `scroll_by`; the page
    //     renderer honors the wheel-driven offset instead of pulling focus back into view (focus
    //     stays where the user put it, even when it scrolls off-screen).
    //   * Other pages: no-op (no scrollable surface).
    // Gate on `Default` input mode + no overlay so wheel events don't
    // slip past popups.
    let scroll_dir = match event.kind {
        MouseEventKind::ScrollUp => Some(true),
        MouseEventKind::ScrollDown => Some(false),
        _ => None,
    };
    if let Some(up) = scroll_dir {
        if !matches!(input_mode, InputMode::Default) || !matches!(overlay, OverlayMode::None) {
            return;
        }
        // Match common terminal-emulator and editor conventions:
        // one wheel notch ~ 3 lines.
        const LINES_PER_NOTCH: u16 = 3;
        match app.current_page() {
            PageId::Logs => {
                let (total, viewport) = app.logs_page_state().last_dimensions();
                if up {
                    app.logs_page_state()
                        .scroll_up_by(LINES_PER_NOTCH, total, viewport);
                } else {
                    app.logs_page_state()
                        .scroll_down_by(LINES_PER_NOTCH, total, viewport);
                }
            }
            PageId::SelectLocation => {
                let delta = if up {
                    -(LINES_PER_NOTCH as isize)
                } else {
                    LINES_PER_NOTCH as isize
                };
                app.select_location_page_state().scroll_by(delta);
            }
            _ => {}
        }
        return;
    }

    if !matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
        return;
    }
    let Some(target) = app.last_focus_registry().hit_test(event.column, event.row) else {
        return;
    };
    // While a popup is open (input modal or overlay), the registry
    // only contains the popup's own buttons - clicks on the dimmed
    // background fall through `hit_test` as `None` and the user's
    // page focus stays put. We still let the activation flow run so
    // those popup buttons dispatch normally.
    app.page_focus_mut().focused = Some(target);
    activate_focused(app, service, input_mode, overlay, target).await;
}

/// Close the overlay and restore page focus to whatever was focused
/// when it opened. The captured `return_focus` survives chained
/// overlays (Confirm -> Notification, etc.) because each new variant
/// inherits the prior one's `return_focus` rather than re-capturing
/// the now-stale overlay-button id.
///
/// If the saved id is no longer in the post-overlay registry (page
/// state mutated while the overlay was open), `set_focus_registry`'s
/// snap-to-first fallback fires on the next render - so this helper
/// is best-effort, not a hard guarantee.
fn close_overlay_restoring_focus(app: &mut App, overlay: &mut OverlayMode) {
    let saved = overlay.return_focus();
    *overlay = OverlayMode::None;
    if let Some(id) = saved {
        app.page_focus_mut().focused = Some(id);
    }
}

/// Pre/post snapshot around an event handler that may toggle
/// [`InputMode`] open/closed, used by the run loop to save the page's
/// focus when a modal opens and restore it when the modal closes. See
/// the comment on `input_modal_return_focus` in [`run_app`] for the
/// motivation; this struct centralises the lifecycle so every input
/// modal gets the behavior for free without each state struct having
/// to carry its own `return_focus` field.
///
/// Only `was_open` is captured at `take` time. The page-widget focus
/// we want to remember is read inside [`Self::apply`] - on a mouse
/// click, `handle_mouse_event` sets `page_focus` to the clicked
/// target *during* the handler (the click on `[Add address]` lands
/// focus on that button before `activate_focused` opens the modal),
/// so capturing earlier would stash whatever was focused before the
/// click. Reading after the handler returns aligns the mouse and
/// keyboard paths: in both cases, the opener widget owns
/// `page_focus().focused` at apply time.
struct ModalLifecycleSnapshot {
    was_open: bool,
}

impl ModalLifecycleSnapshot {
    fn take(input_mode: &InputMode) -> Self {
        Self {
            was_open: input_mode.is_open(),
        }
    }

    /// Compare the post-event modal state to `self` and update the
    /// run-loop's `return_focus` slot:
    /// - **closed -> open**: stash the current page focus (the handler has just landed it on the
    ///   opener widget).
    /// - **open -> closed**: restore page focus from the stash.
    /// - no transition: leave both slots alone (a still-open modal keeps its original return focus
    ///   across validation retries, and a still-closed loop leaves stale focus untouched).
    fn apply(
        self,
        input_mode: &InputMode,
        app: &mut App,
        return_focus: &mut Option<crate::app::WidgetId>,
    ) {
        match (self.was_open, input_mode.is_open()) {
            (false, true) => *return_focus = app.page_focus().focused,
            (true, false) => {
                if let Some(id) = return_focus.take() {
                    app.page_focus_mut().focused = Some(id);
                }
            }
            _ => {}
        }
    }
}

/// Run the action behind a [`ConfirmAction`] tag. Caller is responsible
/// for clearing the overlay; this function only runs the action and
/// returns its result.
async fn dispatch_confirm<S: MullvadService>(
    app: &mut App,
    service: &S,
    action: ConfirmAction,
) -> Result<(), IntegrationError> {
    match action {
        ConfirmAction::Disconnect => {
            // `Ok(false)` from `disconnect` means "already
            // disconnected"; the confirm-disconnect button only fires
            // when there's something to disconnect, so map it through.
            app.disconnect(service).await.map(|_| ())
        }
        ConfirmAction::Logout => app.logout(service).await,
        ConfirmAction::ToggleLockdown => app.toggle_lockdown(service).await,
        ConfirmAction::RotateWireGuardKey => app.rotate_wireguard_key(service).await,
        ConfirmAction::ClearRelayOverrides => app.clear_relay_overrides(service).await,
        ConfirmAction::RemoveRelayOverride { hostname } => {
            app.remove_relay_override(service, hostname).await
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use mullvad_types::{
        device::{AccountAndDevice, Device, DeviceId, DeviceName},
        relay_list::RelayList,
    };

    use crate::{
        app::{App, ConfirmAction, Operation, OperationStatus, WidgetId},
        integration::{AccountData, AppEvent, DeviceState, RelayLocation, SelectedObfuscation},
        test_support::{StubService, stub_version_info},
    };

    use super::{
        InputMode, InputOutcome, ModalLifecycleSnapshot, OverlayMode, SgrLeakFilter,
        apply_app_event, breadcrumb_segments, components, dispatch_confirm, handle_key_event,
        handle_mouse_event, page_hints, prefer_active_tab,
    };
    use crate::{
        app::PageId,
        tui::{
            modals::{
                account::{AccountInputState, validate_account_number},
                port::{PortInputState, parse_port_input},
            },
            pages,
        },
    };

    fn app() -> App {
        App::new()
    }

    fn logged_in_device(account: &str) -> DeviceState {
        DeviceState::LoggedIn(AccountAndDevice {
            account_number: account.to_string(),
            device: Device {
                id: DeviceId::from("dev-id-1"),
                name: DeviceName::from("test-device"),
                pubkey: talpid_types::net::wireguard::PrivateKey::new_from_random().public_key(),
                hijack_dns: false,
                created: chrono::Utc::now(),
            },
        })
    }

    #[tokio::test]
    async fn device_changed_logged_in_triggers_account_data_followup() {
        let mut app = app();
        let service = StubService::default();
        // Seed the AccountData the follow-up RPC will return.
        *service.account_data.borrow_mut() = Some(AccountData {
            id: "stub-account-id".to_string(),
            expiry: chrono::Utc::now() + chrono::Duration::days(30),
        });

        apply_app_event(
            &mut app,
            AppEvent::DeviceChanged(logged_in_device("0000000000000000")),
            &service,
        )
        .await;

        // Device half stored, follow-up fetched, AccountData populated.
        assert_eq!(service.get_account_data_calls.borrow().len(), 1);
        let info = app
            .account_info()
            .expect("account info should be populated by DeviceChanged");
        assert!(matches!(info.device, DeviceState::LoggedIn(_)));
        assert!(info.data.is_some(), "AccountData follow-up should populate");
    }

    #[tokio::test]
    async fn login_stays_running_until_device_push_confirms() {
        use crate::app::{Operation, OperationStatus};
        let mut app = app();
        let service = StubService::default();

        // RPC returns Ok(()), but no device push has arrived yet -
        // status must stay Running, not flip to Success.
        app.login(&service, "0000000000000000".to_string())
            .await
            .expect("login RPC should succeed");
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Running(Operation::Login),
        );

        // Simulate the daemon's `DeviceChanged(LoggedIn)` push.
        // Resolution flips Running -> Success.
        app.set_device(logged_in_device("0000000000000000"));
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::Login),
        );
    }

    #[tokio::test]
    async fn logout_marks_failed_when_device_unexpectedly_logged_in() {
        use crate::app::{Operation, OperationStatus};
        let mut app = app();
        let service = StubService::default();

        // Pre-seed: user is currently logged in.
        app.set_device(logged_in_device("0000000000000000"));

        // Logout RPC returns Ok; pending entry waits for LoggedOut.
        app.logout(&service)
            .await
            .expect("logout RPC should succeed");
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Running(Operation::Logout),
        );

        // Daemon bug: device push arrives in LoggedIn state instead.
        // Resolution flips to Failed.
        app.set_device(logged_in_device("0000000000000000"));
        assert!(matches!(
            app.operation_status(),
            OperationStatus::Failed {
                operation: Operation::Logout,
                ..
            },
        ));
    }

    #[tokio::test]
    async fn device_changed_logged_out_clears_account_data() {
        let mut app = app();
        let service = StubService::default();
        // Pre-seed an AccountInfo with data, mimicking a prior LoggedIn state.
        let pre_data = AccountData {
            id: "stub-account-id".to_string(),
            expiry: chrono::Utc::now() + chrono::Duration::days(30),
        };
        app.set_device(logged_in_device("0000000000000000"));
        app.set_account_data(Some(pre_data));

        apply_app_event(
            &mut app,
            AppEvent::DeviceChanged(DeviceState::LoggedOut),
            &service,
        )
        .await;

        // No follow-up RPC; data cleared; device updated.
        assert!(service.get_account_data_calls.borrow().is_empty());
        let info = app
            .account_info()
            .expect("account info struct should still exist");
        assert!(matches!(info.device, DeviceState::LoggedOut));
        assert!(info.data.is_none());
    }

    #[tokio::test]
    async fn relay_list_changed_replaces_not_appends() {
        let mut app = app();
        let service = StubService::default();
        // Pre-seed a relay; an incoming push should *replace* not *merge* -
        // this catches the easy-to-make `extend()`-vs-`=` bug.
        app.set_relay_locations(vec![RelayLocation {
            hostname: "stale-relay".to_string(),
            ..RelayLocation::default()
        }]);
        assert_eq!(app.relay_locations().len(), 1);

        apply_app_event(
            &mut app,
            AppEvent::RelayListChanged(RelayList::empty()),
            &service,
        )
        .await;

        assert_eq!(
            app.relay_locations().len(),
            0,
            "empty push should replace the stale relay, not merge with it"
        );
    }

    /// Seed the `App` with two relays (Sweden / USA), navigate to
    /// the SelectLocation sub-page, and assert the open precondition.
    /// Shared by the `selecting_a_*_closes_*` regression tests.
    fn open_select_location_with_relays(app: &mut App) {
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
        ]);
        app.navigate_to(PageId::Status);
        app.enter_sub_page(PageId::SelectLocation);
        assert!(app.is_on_sub_page(), "test setup precondition");
    }

    #[tokio::test]
    async fn selecting_a_country_closes_the_select_location_sub_page() {
        let mut app = app();
        let service = StubService::default();
        open_select_location_with_relays(&mut app);

        // Country tree is alphabetical by code: se idx 0, us idx 1.
        let se_radio = crate::app::WidgetId(pages::select_location::COUNTRY_RADIO_BASE);
        pages::select_location::activate(&mut app, &service, se_radio).await;

        assert_eq!(
            service.set_relay_country_calls.borrow().as_slice(),
            &["se".to_string()],
            "daemon RPC was issued",
        );
        assert!(
            !app.is_on_sub_page(),
            "selecting a country must immediately close the sub-page",
        );
    }

    #[tokio::test]
    async fn selecting_a_city_closes_the_select_location_sub_page() {
        let mut app = app();
        let service = StubService::default();
        open_select_location_with_relays(&mut app);
        // Expand `se` so its city (Gothenburg) becomes addressable as
        // city_idx 0 - matches what the renderer would assign.
        app.select_location_page_state_mut().expand_country("se");

        let got_radio = crate::app::WidgetId(pages::select_location::CITY_RADIO_BASE);
        pages::select_location::activate(&mut app, &service, got_radio).await;

        assert_eq!(
            service.set_relay_city_calls.borrow().as_slice(),
            &[("se".to_string(), "got".to_string())],
        );
        assert!(
            !app.is_on_sub_page(),
            "selecting a city must immediately close the sub-page",
        );
    }

    #[tokio::test]
    async fn selecting_a_relay_closes_the_select_location_sub_page() {
        let mut app = app();
        let service = StubService::default();
        open_select_location_with_relays(&mut app);
        app.select_location_page_state_mut().expand_country("se");
        app.select_location_page_state_mut()
            .expand_city("se", "got");

        let relay_radio = crate::app::WidgetId(pages::select_location::RELAY_RADIO_BASE);
        pages::select_location::activate(&mut app, &service, relay_radio).await;

        assert_eq!(
            service.set_relay_calls.borrow().as_slice(),
            &["se-got-wg-001".to_string()],
        );
        assert!(
            !app.is_on_sub_page(),
            "selecting a relay must immediately close the sub-page",
        );
    }

    #[tokio::test]
    async fn app_version_info_changed_stores_pushed_value() {
        let mut app = app();
        let service = StubService::default();
        assert!(app.app_version_info().is_none());
        let pushed = stub_version_info();

        apply_app_event(
            &mut app,
            AppEvent::AppVersionInfoChanged(pushed.clone()),
            &service,
        )
        .await;

        let cached = app
            .app_version_info()
            .expect("push should have populated the cache");
        assert_eq!(
            cached.current_version_supported,
            pushed.current_version_supported
        );
    }

    #[test]
    fn account_input_accepts_digits_submit_and_cancel() {
        let mut state = AccountInputState::default();

        let digit = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE);
        assert!(matches!(state.handle_key(digit), InputOutcome::Handled));
        assert_eq!(state.buffer, "1");

        let submit = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(state.handle_key(submit), InputOutcome::Submit));

        let cancel = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(state.handle_key(cancel), InputOutcome::Cancel));
    }

    #[test]
    fn account_input_button_focus_routes_enter_to_the_focused_button() {
        // With internal focus on the [Cancel] button, Enter must
        // cancel - not submit. With focus on [Log in] (Submit), Enter
        // submits. This is what makes the buttons "focusable" from
        // the user's POV: navigating to them with Tab/arrows changes
        // what Enter does.
        use crate::tui::modals::InputFocus;

        let mut state = AccountInputState {
            buffer: "1234123412341234".to_string(),
            focus: InputFocus::Cancel,
        };
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(state.handle_key(enter), InputOutcome::Cancel));

        state.focus = InputFocus::Submit;
        assert!(matches!(state.handle_key(enter), InputOutcome::Submit));
    }

    #[test]
    fn account_input_buttons_dont_swallow_typed_chars() {
        // Once focus is on a button, the user is no longer typing
        // into the buffer - char keys should be `NotHandled` so the
        // run loop doesn't silently corrupt the field.
        use crate::tui::modals::InputFocus;

        let mut state = AccountInputState {
            buffer: "1".to_string(),
            focus: InputFocus::Cancel,
        };
        let digit = KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE);
        assert!(matches!(state.handle_key(digit), InputOutcome::NotHandled));
        assert_eq!(
            state.buffer, "1",
            "buffer must not change while a button is focused"
        );

        // After moving focus back to the field via ↑, typing resumes.
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        state.handle_key(up);
        assert_eq!(state.focus, InputFocus::Field);
        assert!(matches!(state.handle_key(digit), InputOutcome::Handled));
        assert_eq!(state.buffer, "12");
    }

    #[test]
    fn account_validation_covers_valid_invalid_and_empty() {
        assert!(validate_account_number("0000000000000000").is_ok());
        assert_eq!(
            validate_account_number("").expect_err("empty should fail"),
            "Account number cannot be empty"
        );
        assert_eq!(
            validate_account_number("12345abc").expect_err("non-digit should fail"),
            "Account number must contain only digits"
        );
        assert_eq!(
            validate_account_number("1234").expect_err("short length should fail"),
            "Account number must be exactly 16 digits"
        );
    }

    // --- Anti-censorship port input ---

    #[test]
    fn parse_port_input_handles_blank_valid_zero_and_oversize() {
        assert_eq!(parse_port_input("").unwrap(), None);
        assert_eq!(parse_port_input("   ").unwrap(), None);
        assert_eq!(parse_port_input("443").unwrap(), Some(443));
        assert_eq!(parse_port_input("65535").unwrap(), Some(65535));
        assert_eq!(parse_port_input("  80  ").unwrap(), Some(80));

        assert!(parse_port_input("0").is_err(), "0 must be rejected");
        assert!(parse_port_input("65536").is_err(), "u16 overflow rejected");
        assert!(parse_port_input("8000a").is_err(), "non-digits rejected");
        assert!(parse_port_input("-1").is_err(), "negative rejected");
    }

    #[test]
    fn handle_port_input_key_accepts_digits_submit_and_cancel() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = PortInputState {
            mode: SelectedObfuscation::Udp2Tcp,
            ..Default::default()
        };

        let digit = KeyEvent::new(KeyCode::Char('8'), KeyModifiers::NONE);
        assert!(matches!(state.handle_key(digit), InputOutcome::Handled));
        assert_eq!(state.buffer, "8");

        // Non-digits fall through (NotHandled) without mutating the buffer.
        let alpha = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(state.handle_key(alpha), InputOutcome::NotHandled));
        assert_eq!(state.buffer, "8");

        // Backspace edits
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        state.handle_key(backspace);
        assert_eq!(state.buffer, "");

        let submit = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(state.handle_key(submit), InputOutcome::Submit));

        let cancel = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(state.handle_key(cancel), InputOutcome::Cancel));
    }

    // --- Inline search-anchor text input ---

    #[tokio::test]
    async fn search_anchor_focused_typing_appends_to_query_and_skips_global_shortcuts() {
        // The SelectLocation search anchor is an inline text input.
        // While focused, printable chars (including `q` and the
        // tab-jump digits `1`-`4`) must land in the page's query
        // buffer instead of triggering Quit / NavigateTab. Backspace
        // pops a char.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::SelectLocation);
        app.page_focus_mut().focused = Some(pages::select_location::widgets::SEARCH_ANCHOR);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        for ch in ['q', '1', 'a'] {
            handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut app,
                &mut input_mode,
                &mut overlay,
                &service,
            )
            .await;
        }
        assert_eq!(app.select_location_page_state().query(), "q1a");
        // `q` did not Quit, `1` did not jump tabs.
        assert!(!app.should_quit());
        assert_eq!(app.current_page(), PageId::SelectLocation);

        handle_key_event(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.select_location_page_state().query(), "q1");
    }

    #[tokio::test]
    async fn notification_layers_on_top_of_open_input_modal() {
        // Regression: a notification raised by an input modal's
        // `submit()` (e.g. invalid Custom DNS address) used to be
        // queued but never displayed because the render dispatch
        // treated InputMode and OverlayMode as mutually exclusive.
        // Now both render - modal first, notification on top - and
        // the topmost layer's [Dismiss] button is the only thing in
        // the registry, so Esc / Enter / clicks all act on the
        // notification first. Once dismissed, the modal becomes
        // interactive again.
        use crate::tui::modals::{InputFocus, custom_dns::CustomDnsInputState};
        use ratatui::{Terminal, backend::TestBackend};

        let app = app();
        let mut input_mode = InputMode::CustomDnsInput(CustomDnsInputState {
            buffer: "abc".to_string(),
            edit_index: None,
            focus: InputFocus::Submit,
        });
        let overlay = OverlayMode::Notification {
            message: "'abc' is not a valid IPv4 or IPv6 address".to_string(),
            return_focus: None,
        };

        // Drive a single render frame the same way the run loop does.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut registry = crate::app::FocusRegistry::default();
        terminal
            .draw(|frame| {
                // Input modal first - populates the registry with its own buttons.
                if let InputMode::CustomDnsInput(state) = &mut input_mode {
                    registry = crate::app::FocusRegistry::default();
                    components::render_input_prompt(
                        frame,
                        "Custom DNS server",
                        "Enter custom DNS server (IPv4 or IPv6):",
                        &state.buffer,
                        "Add",
                        state.focus,
                        &mut registry,
                    );
                }
                // Overlay layered on top - the run loop's
                // registry-reset means only the [Dismiss] button is
                // hit-testable now.
                if let OverlayMode::Notification { message, .. } = &overlay {
                    registry = crate::app::FocusRegistry::default();
                    components::render_notification_overlay(
                        frame,
                        message,
                        &mut registry,
                        app.page_focus().focused,
                    );
                }
            })
            .unwrap();

        // Final registry must be the notification's own - modal
        // buttons get dropped by the overlay's reset.
        assert!(
            registry.contains(components::OVERLAY_NOTIFICATION_DISMISS),
            "notification's [Dismiss] button must be the active hit-test target",
        );
        assert!(
            !registry.contains(components::INPUT_MODAL_CANCEL),
            "modal buttons must be inert while a notification is on top",
        );
        assert!(
            !registry.contains(components::INPUT_MODAL_SUBMIT),
            "modal buttons must be inert while a notification is on top",
        );
    }

    #[tokio::test]
    async fn esc_with_notification_over_input_modal_dismisses_only_the_notification() {
        // Layered popup keystrokes target the topmost layer first.
        // With both an input modal and a notification open, Esc
        // should clear the notification but leave the modal intact -
        // letting the user fix their input after acknowledging the
        // validation error.
        use crate::tui::modals::{InputFocus, custom_dns::CustomDnsInputState};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        // Seed the registry with the [Dismiss] button (matches what
        // the run loop's overlay-reset render would produce).
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget};
        use ratatui::layout::Rect;
        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: components::OVERLAY_NOTIFICATION_DISMISS,
            rect: Rect::new(35, 12, 11, 1),
            kind: FocusKind::Button,
        });
        registry.end_row();
        app.set_focus_registry(registry, None);
        app.page_focus_mut().focused = Some(components::OVERLAY_NOTIFICATION_DISMISS);

        let mut input_mode = InputMode::CustomDnsInput(CustomDnsInputState {
            buffer: "abc".to_string(),
            edit_index: None,
            focus: InputFocus::Submit,
        });
        let mut overlay = OverlayMode::Notification {
            message: "'abc' is not a valid IPv4 or IPv6 address".to_string(),
            return_focus: None,
        };

        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        assert!(
            matches!(overlay, OverlayMode::None),
            "Esc must dismiss the notification on top",
        );
        match &input_mode {
            InputMode::CustomDnsInput(state) => assert_eq!(state.buffer, "abc"),
            _ => panic!("the modal underneath must remain open with its buffer intact"),
        }
    }

    #[tokio::test]
    async fn left_click_on_window_close_button_quits() {
        // `[x]` on the outer-frame border is a focusable button that
        // quits the app - same outcome as the `q` keyboard shortcut
        // or `App::quit`. Mouse-clickable.
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget};
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = app();
        let service = StubService::default();
        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: components::WINDOW_CLOSE,
            rect: Rect::new(76, 0, 3, 1),
            kind: FocusKind::WindowClose,
        });
        registry.end_row();
        app.set_focus_registry(registry, None);

        assert!(!app.should_quit(), "pre-condition: app not quitting");

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 77,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        assert!(
            app.should_quit(),
            "click on `[x]` close button should set the quit flag",
        );
    }

    #[tokio::test]
    async fn mouse_events_capture_cursor_for_hover_tracking() {
        // Every mouse event kind must update `App::cursor` so the
        // hover-highlight pass can hit-test it on the next frame.
        // Move and Drag are the obvious cases; clicks and scrolls
        // matter too (they imply the cursor is at that position),
        // and they each take a different early-return path through
        // `handle_mouse_event_inner`, so cover them all.
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mut app = app();
        let service = StubService::default();
        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;

        assert_eq!(app.cursor(), None, "no mouse activity yet");

        for (kind, col, row) in [
            (MouseEventKind::Moved, 5, 7),
            (MouseEventKind::Drag(MouseButton::Left), 12, 3),
            (MouseEventKind::Down(MouseButton::Right), 0, 0),
            (MouseEventKind::ScrollDown, 40, 20),
        ] {
            handle_mouse_event(
                MouseEvent {
                    kind,
                    column: col,
                    row,
                    modifiers: KeyModifiers::NONE,
                },
                &mut app,
                &mut input_mode,
                &mut overlay,
                &service,
            )
            .await;
            assert_eq!(
                app.cursor(),
                Some((col, row)),
                "cursor must follow {kind:?} events",
            );
        }
    }

    #[test]
    fn paint_hover_highlight_stamps_bg_only_inside_widget_rect() {
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget, WidgetId};
        use ratatui::{
            buffer::Buffer,
            layout::Rect,
            style::{Color, Style},
        };

        // 10x3 buffer with a 4-cell button at (2, 1)..(6, 1). Pre-fill
        // a non-default fg on the button cells so we can verify the
        // hover pass sets bg without clobbering the fg the renderer
        // wrote (the focus ring's yellow / danger button's red would
        // otherwise turn into gray on gray).
        let area = Rect::new(0, 0, 10, 3);
        let mut buffer = Buffer::empty(area);
        let button_rect = Rect::new(2, 1, 4, 1);
        for x in button_rect.x..button_rect.x + button_rect.width {
            buffer[(x, 1)].set_style(Style::new().fg(Color::Yellow));
        }

        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: WidgetId(1),
            rect: button_rect,
            kind: FocusKind::Button,
        });

        // Cursor inside the button -> every cell of `button_rect`
        // gets `HOVER_BG`, original fg preserved.
        super::paint_hover_highlight(&mut buffer, &registry, Some((3, 1)));
        for x in button_rect.x..button_rect.x + button_rect.width {
            assert_eq!(buffer[(x, 1)].bg, super::HOVER_BG);
            assert_eq!(buffer[(x, 1)].fg, Color::Yellow, "fg must survive bg paint");
        }
        // Cells outside the rect stay default.
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
        assert_eq!(buffer[(0, 1)].bg, Color::Reset);
        assert_eq!(buffer[(7, 1)].bg, Color::Reset);
    }

    #[test]
    fn paint_hover_highlight_preserves_yellow_bg_cursor_cell() {
        // Reproduces the MTU placeholder cursor: a single cell inside
        // the focusable rect carries an explicit yellow bg (the text
        // cursor). Hovering must leave that cell yellow - otherwise
        // the cursor disappears under the gray hover paint.
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget, WidgetId};
        use ratatui::{
            buffer::Buffer,
            layout::Rect,
            style::{Color, Style},
        };

        let area = Rect::new(0, 0, 10, 3);
        let mut buffer = Buffer::empty(area);
        let pill_rect = Rect::new(2, 1, 4, 1);
        let cursor_x = 3;
        buffer[(cursor_x, 1)].set_style(Style::new().fg(Color::Black).bg(Color::Yellow));

        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: WidgetId(1),
            rect: pill_rect,
            kind: FocusKind::Button,
        });

        super::paint_hover_highlight(&mut buffer, &registry, Some((cursor_x, 1)));

        assert_eq!(
            buffer[(cursor_x, 1)].bg,
            Color::Yellow,
            "yellow cursor cell must survive hover paint",
        );
        for x in pill_rect.x..pill_rect.x + pill_rect.width {
            if x == cursor_x {
                continue;
            }
            assert_eq!(buffer[(x, 1)].bg, super::HOVER_BG);
        }
    }

    #[test]
    fn paint_hover_highlight_no_ops_when_cursor_misses_or_unset() {
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget, WidgetId};
        use ratatui::{buffer::Buffer, layout::Rect, style::Color};

        let area = Rect::new(0, 0, 10, 3);
        let button_rect = Rect::new(2, 1, 4, 1);
        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: WidgetId(1),
            rect: button_rect,
            kind: FocusKind::Button,
        });

        // No cursor yet -> buffer untouched.
        let mut buffer = Buffer::empty(area);
        super::paint_hover_highlight(&mut buffer, &registry, None);
        for x in 0..area.width {
            for y in 0..area.height {
                assert_eq!(buffer[(x, y)].bg, Color::Reset);
            }
        }

        // Cursor outside every registered rect -> still untouched.
        super::paint_hover_highlight(&mut buffer, &registry, Some((9, 0)));
        for x in 0..area.width {
            for y in 0..area.height {
                assert_eq!(buffer[(x, y)].bg, Color::Reset);
            }
        }
    }

    /// Push the byte stream of a leaked SGR mouse sequence (as emitted
    /// by crossterm 0.29 when an `Esc` keystroke arrives just before
    /// the mouse seq's leading `ESC`) through [`SgrLeakFilter`] and
    /// collect the events that survive. Mirrors the real crossterm
    /// output: the user's `Esc` is preserved as a `KeyCode::Esc`
    /// event, then the body bytes leak as `Char(...)` events with the
    /// `M` terminator carrying `KeyModifiers::SHIFT` (since `M` is
    /// uppercase).
    fn run_through_leak_filter(events: &[crossterm::event::Event]) -> Vec<KeyCode> {
        let mut filter = SgrLeakFilter::default();
        events
            .iter()
            .filter(|e| filter.pass(e))
            .filter_map(|e| match e {
                crossterm::event::Event::Key(k) => Some(k.code),
                _ => None,
            })
            .collect()
    }

    fn key(code: KeyCode) -> crossterm::event::Event {
        crossterm::event::Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn key_with(code: KeyCode, mods: KeyModifiers) -> crossterm::event::Event {
        crossterm::event::Event::Key(KeyEvent::new(code, mods))
    }

    #[test]
    fn sgr_leak_filter_swallows_leaked_mouse_sequence_after_esc() {
        // `Esc` followed by a leaked SGR mouse sequence
        // `[<35;17;5M` (button 35 = motion, col 17, row 5). The body
        // digits include `1` and `7`, both of which would otherwise
        // trigger `NavigateTab` via `keybindings::map_key_event`.
        // Only the user's real `Esc` should reach the dispatcher.
        let stream = vec![
            key(KeyCode::Esc),
            key(KeyCode::Char('[')),
            key(KeyCode::Char('<')),
            key(KeyCode::Char('3')),
            key(KeyCode::Char('5')),
            key(KeyCode::Char(';')),
            key(KeyCode::Char('1')),
            key(KeyCode::Char('7')),
            key(KeyCode::Char(';')),
            key(KeyCode::Char('5')),
            key_with(KeyCode::Char('M'), KeyModifiers::SHIFT),
        ];
        assert_eq!(run_through_leak_filter(&stream), vec![KeyCode::Esc]);
    }

    #[test]
    fn sgr_leak_filter_handles_release_terminator_lowercase_m() {
        // Release events terminate with lowercase `m` (no SHIFT).
        let stream = vec![
            key(KeyCode::Esc),
            key(KeyCode::Char('[')),
            key(KeyCode::Char('<')),
            key(KeyCode::Char('0')),
            key(KeyCode::Char(';')),
            key(KeyCode::Char('2')),
            key(KeyCode::Char(';')),
            key(KeyCode::Char('3')),
            key(KeyCode::Char('m')),
        ];
        assert_eq!(run_through_leak_filter(&stream), vec![KeyCode::Esc]);
    }

    #[test]
    fn sgr_leak_filter_recovers_after_terminator() {
        // After the leaked sequence ends, the next `Esc` keystroke
        // and a real digit must pass through normally - the filter
        // resets to `Idle` on the `M`/`m` terminator.
        let stream = vec![
            key(KeyCode::Esc),
            key(KeyCode::Char('[')),
            key(KeyCode::Char('<')),
            key(KeyCode::Char('1')),
            key(KeyCode::Char(';')),
            key(KeyCode::Char('1')),
            key(KeyCode::Char(';')),
            key(KeyCode::Char('1')),
            key_with(KeyCode::Char('M'), KeyModifiers::SHIFT),
            key(KeyCode::Esc),
            key(KeyCode::Char('2')),
        ];
        assert_eq!(
            run_through_leak_filter(&stream),
            vec![KeyCode::Esc, KeyCode::Esc, KeyCode::Char('2')],
        );
    }

    #[test]
    fn sgr_leak_filter_passes_real_keystrokes_through() {
        // No leaked-mouse-seq prefix - every event must pass.
        let stream = vec![
            key(KeyCode::Char('q')),
            key(KeyCode::Char('1')),
            key(KeyCode::Char('2')),
            key(KeyCode::Esc),
            key(KeyCode::Char('q')),
        ];
        assert_eq!(
            run_through_leak_filter(&stream),
            vec![
                KeyCode::Char('q'),
                KeyCode::Char('1'),
                KeyCode::Char('2'),
                KeyCode::Esc,
                KeyCode::Char('q'),
            ],
        );
    }

    #[test]
    fn sgr_leak_filter_does_not_swallow_after_esc_without_full_prefix() {
        // `Esc` followed by something that is *not* `[<` must not
        // trigger swallow mode. Only the leading `[` is dropped (it
        // is not bound to anything in the keybinding table); the
        // following `1` must pass through so the user can still type
        // the `1` tab shortcut after a stray `Esc` `[` typo.
        let stream = vec![
            key(KeyCode::Esc),
            key(KeyCode::Char('[')),
            key(KeyCode::Char('1')),
        ];
        assert_eq!(
            run_through_leak_filter(&stream),
            vec![KeyCode::Esc, KeyCode::Char('1')],
        );
    }

    #[test]
    fn sgr_leak_filter_caps_swallow_window_when_terminator_missing() {
        // If the `M`/`m` terminator never arrives (parser glitch), the
        // filter must give up after [`SGR_LEAK_MAX_SWALLOW`] bytes so
        // the user's subsequent keystrokes are not silently dropped
        // forever.
        let mut stream: Vec<crossterm::event::Event> = vec![
            key(KeyCode::Esc),
            key(KeyCode::Char('[')),
            key(KeyCode::Char('<')),
        ];
        for _ in 0..super::SGR_LEAK_MAX_SWALLOW {
            stream.push(key(KeyCode::Char('1')));
        }
        // After the cap is exhausted the filter is back in `Idle`,
        // so a plain `Char('q')` here must pass through.
        stream.push(key(KeyCode::Char('q')));
        assert_eq!(
            run_through_leak_filter(&stream),
            vec![KeyCode::Esc, KeyCode::Char('q')],
        );
    }

    #[test]
    fn sgr_leak_filter_resets_on_non_key_events() {
        // A real `Mouse` event landing mid-prefix means the parser is
        // back in sync; reset the leak detector so we don't keep
        // dangling state.
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let stream = vec![
            key(KeyCode::Esc),
            key(KeyCode::Char('[')),
            crossterm::event::Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
            key(KeyCode::Char('<')),
            key(KeyCode::Char('1')),
        ];
        // `[` was swallowed (after Esc), then mouse event resets the
        // filter, so `<` and `1` pass through unfiltered.
        assert_eq!(
            run_through_leak_filter(&stream),
            vec![KeyCode::Esc, KeyCode::Char('<'), KeyCode::Char('1')],
        );
    }

    #[test]
    fn window_close_does_not_hijack_first_body_widget_snap() {
        // The `[x]` close button is chrome - `first_body_widget`
        // must skip its row (along with the tab bar and breadcrumb)
        // so the post-render snap-to-first lands on actual page
        // content rather than the close button.
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget, WidgetId};
        use ratatui::layout::Rect;

        let mut registry = FocusRegistry::new();
        // Row 0: window-close `[x]`.
        registry.register(FocusableWidget {
            id: components::WINDOW_CLOSE,
            rect: Rect::new(76, 0, 3, 1),
            kind: FocusKind::WindowClose,
        });
        registry.end_row();
        // Row 1: tab bar.
        registry.register(FocusableWidget {
            id: components::tab_widget_id_for_top_level(PageId::Settings),
            rect: Rect::new(20, 1, 10, 1),
            kind: FocusKind::TabButton,
        });
        registry.end_row();
        // Row 2: actual body content.
        let body_id = WidgetId(0xAA);
        registry.register(FocusableWidget {
            id: body_id,
            rect: Rect::new(0, 4, 12, 1),
            kind: FocusKind::Button,
        });
        registry.end_row();

        assert_eq!(
            registry.first_body_widget(),
            Some(body_id),
            "first_body_widget must skip the window-close, tab, and breadcrumb rows",
        );
    }

    #[tokio::test]
    async fn left_click_on_breadcrumb_back_button_pops_one_sub_page() {
        // The `[<]` button on the breadcrumb row of a sub-page pops
        // exactly one frame (matching `Esc`). Mouse-clickable via
        // the same `activate_focused` dispatch as overlay buttons.
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget};
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::Settings);
        app.enter_sub_page(PageId::SettingsVpn);
        app.enter_sub_page(PageId::SettingsDnsBlockers);
        assert_eq!(app.current_page(), PageId::SettingsDnsBlockers);

        // Seed the registry with the `[<]` button at a known rect.
        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: components::BREADCRUMB_BACK,
            rect: Rect::new(0, 1, 3, 1),
            kind: FocusKind::BreadcrumbBack,
        });
        registry.end_row();
        app.set_focus_registry(registry, None);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        assert_eq!(
            app.current_page(),
            PageId::SettingsVpn,
            "[<] click pops one frame, not the whole stack",
        );

        // A second click pops to Settings root.
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.current_page(), PageId::Settings);
        assert!(!app.is_on_sub_page());
    }

    #[test]
    fn breadcrumb_back_does_not_hijack_first_body_widget_snap() {
        // The breadcrumb `[<]` is chrome, not body - `first_body_widget`
        // must skip it so the post-render snap-to-first lands on
        // actual page content rather than the back button.
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget, WidgetId};
        use ratatui::layout::Rect;

        let mut registry = FocusRegistry::new();
        // Row 0: tab bar.
        registry.register(FocusableWidget {
            id: components::tab_widget_id_for_top_level(PageId::Settings),
            rect: Rect::new(20, 0, 10, 1),
            kind: FocusKind::TabButton,
        });
        registry.end_row();
        // Row 1: breadcrumb back button.
        registry.register(FocusableWidget {
            id: components::BREADCRUMB_BACK,
            rect: Rect::new(0, 1, 3, 1),
            kind: FocusKind::BreadcrumbBack,
        });
        registry.end_row();
        // Row 2: actual body content.
        let body_id = WidgetId(0xAA);
        registry.register(FocusableWidget {
            id: body_id,
            rect: Rect::new(0, 3, 12, 1),
            kind: FocusKind::Button,
        });
        registry.end_row();

        assert_eq!(
            registry.first_body_widget(),
            Some(body_id),
            "first_body_widget must skip both the tab row and the breadcrumb row",
        );
    }

    #[tokio::test]
    async fn left_click_on_input_modal_field_focuses_the_buffer() {
        // When the user has Tab'd over to a button and then changes
        // their mind, clicking the buffer row should drop focus back
        // on the field so the next keystroke types instead of
        // activating the button.
        use crate::{
            app::{FocusKind, FocusRegistry, FocusableWidget},
            tui::modals::{InputFocus, account::AccountInputState},
        };
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = app();
        let service = StubService::default();
        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: components::INPUT_MODAL_FIELD,
            rect: Rect::new(10, 5, 40, 1),
            kind: FocusKind::TextInput,
        });
        registry.end_row();
        app.set_focus_registry(registry, None);

        let mut input_mode = InputMode::AccountInput(AccountInputState {
            buffer: "1234".to_string(),
            focus: InputFocus::Submit,
        });
        let mut overlay = OverlayMode::None;
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        match &input_mode {
            InputMode::AccountInput(state) => {
                assert_eq!(
                    state.focus,
                    InputFocus::Field,
                    "buffer click must focus the field"
                );
                assert_eq!(state.buffer, "1234", "click must not mutate the buffer");
            }
            _ => panic!("modal must remain open"),
        }
    }

    #[tokio::test]
    async fn left_click_on_input_modal_cancel_button_dismisses_modal() {
        // Mouse click on `[Cancel]` inside an input popup must
        // dismiss the modal - same outcome as Esc / keyboard Enter on
        // the focused Cancel button. Locks the new mouse-clickable
        // popup behavior.
        use crate::{
            app::{FocusKind, FocusRegistry, FocusableWidget},
            tui::modals::{InputFocus, account::AccountInputState},
        };
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = app();
        let service = StubService::default();
        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: components::INPUT_MODAL_CANCEL,
            rect: Rect::new(10, 10, 8, 1),
            kind: FocusKind::Button,
        });
        registry.end_row();
        app.set_focus_registry(registry, None);

        let mut input_mode = InputMode::AccountInput(AccountInputState {
            buffer: "1234".to_string(),
            focus: InputFocus::Field,
        });
        let mut overlay = OverlayMode::None;
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 10,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        assert!(
            matches!(input_mode, InputMode::Default),
            "clicking [Cancel] must drop the modal back to Default",
        );
    }

    #[tokio::test]
    async fn left_click_on_input_modal_submit_runs_submit_path() {
        // Mouse click on `[Submit]` (or modal-specific verb) must
        // run the submit path. Account login validates the buffer
        // before calling the daemon; an invalid buffer (less than 16
        // digits) shows a notification but still drops the modal -
        // matching the existing keyboard-Enter behavior for that
        // failure mode.
        use crate::{
            app::{FocusKind, FocusRegistry, FocusableWidget},
            tui::modals::{InputFocus, account::AccountInputState},
        };
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = app();
        let service = StubService::default();
        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: components::INPUT_MODAL_SUBMIT,
            rect: Rect::new(20, 10, 10, 1),
            kind: FocusKind::Button,
        });
        registry.end_row();
        app.set_focus_registry(registry, None);

        let mut input_mode = InputMode::AccountInput(AccountInputState {
            // Short buffer -> validation error -> submit returns
            // `false` (close the modal) and queues a notification.
            buffer: "123".to_string(),
            focus: InputFocus::Submit,
        });
        let mut overlay = OverlayMode::None;
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 22,
                row: 10,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        assert!(
            matches!(input_mode, InputMode::Default),
            "clicking [Submit] must run the submit path and close the modal",
        );
    }

    #[tokio::test]
    async fn left_click_on_dimmed_background_is_inert_while_modal_open() {
        // When an input modal is open, the registry only contains the
        // modal's own widgets - a click on any other position is a
        // no-op (the page underneath isn't interactable).
        use crate::{
            app::{FocusKind, FocusRegistry, FocusableWidget},
            tui::modals::{InputFocus, account::AccountInputState},
        };
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = app();
        let service = StubService::default();
        // Modal-only registry: only Cancel/Submit are hit-testable.
        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: components::INPUT_MODAL_CANCEL,
            rect: Rect::new(10, 10, 8, 1),
            kind: FocusKind::Button,
        });
        registry.register(FocusableWidget {
            id: components::INPUT_MODAL_SUBMIT,
            rect: Rect::new(20, 10, 10, 1),
            kind: FocusKind::Button,
        });
        registry.end_row();
        app.set_focus_registry(registry, None);

        let mut input_mode = InputMode::AccountInput(AccountInputState {
            buffer: "1234".to_string(),
            focus: InputFocus::Field,
        });
        let mut overlay = OverlayMode::None;
        // Click far from any modal button.
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        match &input_mode {
            InputMode::AccountInput(state) => {
                assert_eq!(state.buffer, "1234", "buffer must not change");
            }
            _ => panic!("modal should still be open"),
        }
    }

    #[tokio::test]
    async fn left_click_focuses_and_activates_widget() {
        // Mouse click at a focusable widget's rect should both move
        // page focus to it and run the same activation path Enter
        // would. We click a tab button - activation calls
        // `app.navigate_to(page)`, so a successful click changes the
        // current page.
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget};
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = app();
        let service = StubService::default();
        // Seed the registry with a single Settings tab button at a
        // known cell rect - bypasses the full render so we don't have
        // to reconstruct the chrome.
        let mut registry = FocusRegistry::new();
        let settings_id = components::tab_widget_id_for_top_level(PageId::Settings);
        registry.register(FocusableWidget {
            id: settings_id,
            rect: Rect::new(20, 0, 10, 1),
            kind: FocusKind::TabButton,
        });
        registry.end_row();
        app.set_focus_registry(registry, None);

        // Pre-condition: starting page is Status, not Settings.
        assert_eq!(app.current_page(), PageId::Status);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 22,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        // Click both focused the tab and ran its activation
        // (`navigate_to(Settings)`).
        assert_eq!(app.current_page(), PageId::Settings);
    }

    #[tokio::test]
    async fn click_outside_any_widget_is_a_no_op() {
        // Empty area -> hit_test returns None -> focus untouched, no
        // activation runs.
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget};
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = app();
        let service = StubService::default();
        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: components::tab_widget_id_for_top_level(PageId::Settings),
            rect: Rect::new(20, 0, 10, 1),
            kind: FocusKind::TabButton,
        });
        registry.end_row();
        app.set_focus_registry(registry, None);
        let starting_page = app.current_page();

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 50,
                row: 10,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.current_page(), starting_page);
    }

    #[tokio::test]
    async fn non_left_mouse_events_are_ignored() {
        // Right-button down, drag, release - all ignored. Otherwise
        // they'd act like clicks and steal focus on a stray drag.
        // Scroll-up is included here on the Status page, where there's
        // no scrollable surface (so the wheel is a no-op too); the
        // `mouse_wheel_scrolls_logs_page` test below covers the wheel
        // doing the right thing on the Logs page.
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget};
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = app();
        let service = StubService::default();
        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id: components::tab_widget_id_for_top_level(PageId::Settings),
            rect: Rect::new(20, 0, 10, 1),
            kind: FocusKind::TabButton,
        });
        registry.end_row();
        app.set_focus_registry(registry, None);
        let starting_page = app.current_page();

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Moved,
            MouseEventKind::ScrollUp,
        ] {
            handle_mouse_event(
                MouseEvent {
                    kind,
                    column: 22,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                },
                &mut app,
                &mut input_mode,
                &mut overlay,
                &service,
            )
            .await;
        }
        assert_eq!(app.current_page(), starting_page);
    }

    #[tokio::test]
    async fn arrow_keys_scroll_logs_page_one_line_at_a_time() {
        // ↑/↓ on the Logs page bypass the focus engine and step the
        // scroll offset by one line each. The page has no body
        // focusables, so the focus-engine fallback would walk into the
        // tab bar - surprising behavior for what visually reads as a
        // scrollable content area.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        for i in 0..200 {
            app.append_log_entry(crate::logging::LogEntry {
                timestamp: chrono::Local::now(),
                source: crate::logging::LogSource::Tui {
                    level: tracing::Level::INFO,
                    target: "tests".to_string(),
                    message: format!("entry-{i:03}"),
                },
            });
        }
        app.navigate_to(PageId::Logs);
        // Seed last_dimensions so the scroll math has something to
        // clamp against (the renderer normally does this on each
        // draw).
        app.logs_page_state().record_dimensions(200, 20);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;

        // Default state is auto-tail - offset == max == 180.
        assert_eq!(app.logs_page_state().effective_scroll(200, 20), 180);

        handle_key_event(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(
            app.logs_page_state().effective_scroll(200, 20),
            179,
            "Up on Logs should bump the offset up by one line",
        );

        handle_key_event(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        // Down 1 from 179 lands at 180 = max_offset, which re-engages
        // auto-tail (offset becomes None - `effective_scroll` resolves
        // it to max_offset).
        assert_eq!(app.logs_page_state().effective_scroll(200, 20), 180);
    }

    #[tokio::test]
    async fn mouse_wheel_scrolls_logs_page_up_and_down() {
        // ScrollUp / ScrollDown on the Logs page route through to the
        // logs page state (3 lines per notch). Other pages should
        // treat the wheel as a no-op (verified separately by
        // `non_left_mouse_events_are_ignored`).
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};

        let mut app = app();
        let service = StubService::default();
        for i in 0..200 {
            app.append_log_entry(crate::logging::LogEntry {
                timestamp: chrono::Local::now(),
                source: crate::logging::LogSource::Tui {
                    level: tracing::Level::INFO,
                    target: "tests".to_string(),
                    message: format!("entry-{i:03}"),
                },
            });
        }
        app.navigate_to(PageId::Logs);
        // Seed last_dimensions so the page-state's scroll math has
        // something to clamp against (the renderer would normally do
        // this on each draw).
        app.logs_page_state().record_dimensions(200, 20);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;

        // Default state is auto-tail (offset == max == total - viewport).
        assert_eq!(app.logs_page_state().effective_scroll(200, 20), 180);

        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(
            app.logs_page_state().effective_scroll(200, 20),
            177,
            "ScrollUp should bump the offset up by 3 lines (1 notch)",
        );

        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        // ScrollDown 3 from offset 177 lands at 180 = max_offset, which
        // re-engages auto-tail (offset becomes None - `effective_scroll`
        // resolves it to max_offset).
        assert_eq!(app.logs_page_state().effective_scroll(200, 20), 180);
    }

    #[tokio::test]
    async fn mouse_wheel_on_select_location_scrolls_viewport_without_moving_focus() {
        // Routes through `select_location::PageState::scroll_by`. The
        // page state needs `record_dimensions` to clamp; in production
        // the renderer calls it each frame, but this test seeds it
        // directly so we don't have to render. ScrollDown shifts the
        // offset by 3, focus stays put. ScrollUp 3 returns to the top
        // and clamps. Verifies the wheel does NOT move focus.
        use crate::app::{PageId, WidgetId};
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};

        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::Status);
        app.enter_sub_page(PageId::SelectLocation);

        // 100 projected rows in a 20-row body.
        app.select_location_page_state().record_dimensions(100, 20);
        app.page_focus_mut().focused = Some(WidgetId(0xBEEF));

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;

        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.select_location_page_state().scroll_offset(), 3);
        assert!(app.select_location_page_state().is_user_scrolled());
        assert_eq!(
            app.page_focus_mut().focused,
            Some(WidgetId(0xBEEF)),
            "wheel must not change focus on Select location",
        );

        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(
            app.select_location_page_state().scroll_offset(),
            0,
            "ScrollUp 3 from offset 3 clamps at the top",
        );
    }

    #[tokio::test]
    async fn mouse_wheel_with_overlay_open_is_a_no_op() {
        // A wheel event arriving while a Notification or Confirm
        // overlay is open shouldn't sneak past it and reach the page
        // beneath. Same gate the click handler uses.
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};

        let mut app = app();
        let service = StubService::default();
        for i in 0..200 {
            app.append_log_entry(crate::logging::LogEntry {
                timestamp: chrono::Local::now(),
                source: crate::logging::LogSource::Tui {
                    level: tracing::Level::INFO,
                    target: "tests".to_string(),
                    message: format!("entry-{i:03}"),
                },
            });
        }
        app.navigate_to(PageId::Logs);
        app.logs_page_state().record_dimensions(200, 20);
        let starting_offset = app.logs_page_state().effective_scroll(200, 20);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::Notification {
            message: "test".to_string(),
            return_focus: None,
        };

        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(
            app.logs_page_state().effective_scroll(200, 20),
            starting_offset,
            "wheel event must not slip past an open overlay",
        );
    }

    #[tokio::test]
    async fn down_arrow_walks_filter_page_rows_end_to_end() {
        // End-to-end check: dispatch a real `Down` key event through
        // `handle_key_event` against the filter page's actual frame
        // registry. Catches any regression that splits the renderer
        // and the run-loop's Arrow handling from the focus engine.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};

        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::SelectLocation);
        app.enter_sub_page(PageId::SelectLocationFilter);
        app.page_focus_mut().focused = Some(pages::select_location_filter::widgets::OWNERSHIP_ANY);

        // Seed `last_focus_registry` with what the body actually
        // produces - the run loop does this between frames.
        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut registry = crate::app::FocusRegistry::default();
        terminal
            .draw(|frame| {
                pages::select_location_filter::render(
                    frame,
                    Rect::new(0, 0, 60, 30),
                    &app,
                    &mut registry,
                );
            })
            .unwrap();
        app.set_focus_registry(registry, None);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_key_event(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(
            app.page_focus().focused,
            Some(pages::select_location_filter::widgets::OWNERSHIP_MULLVAD_OWNED),
            "Down from OWNERSHIP_ANY should land on OWNERSHIP_MULLVAD_OWNED",
        );

        // Right is no-op (single-cell rows).
        handle_key_event(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(
            app.page_focus().focused,
            Some(pages::select_location_filter::widgets::OWNERSHIP_MULLVAD_OWNED),
            "Right is a no-op on a single-cell row",
        );
    }

    #[tokio::test]
    async fn slash_focuses_search_anchor_from_elsewhere_on_select_location() {
        // From any other widget on the SelectLocation page, `/` should
        // jump focus to the search anchor. When the anchor is already
        // focused, `/` instead lands in the buffer (covered by
        // `search_anchor_focused_typing_appends_to_query_and_skips_global_shortcuts`).
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::SelectLocation);
        app.page_focus_mut().focused = Some(pages::select_location::widgets::FILTER_BUTTON);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_key_event(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(
            app.page_focus().focused,
            Some(pages::select_location::widgets::SEARCH_ANCHOR),
        );
        // The `/` did not land in the buffer - it was a focus jump.
        assert_eq!(app.select_location_page_state().query(), "");

        // Once focused, a subsequent `/` does append to the buffer.
        handle_key_event(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.select_location_page_state().query(), "/");
    }

    #[tokio::test]
    async fn slash_is_inert_off_select_location() {
        // The `/` shortcut is page-scoped - it shouldn't reach into
        // other pages' focus state.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        // Stay on the default Status page.
        let original_focus = app.page_focus().focused;

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_key_event(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.page_focus().focused, original_focus);
    }

    #[tokio::test]
    async fn search_anchor_unfocused_keeps_global_shortcuts() {
        // Focus elsewhere on the page -> `q` quits as usual.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::SelectLocation);
        // Focus something other than the search anchor (filter button
        // is the natural sibling).
        app.page_focus_mut().focused = Some(pages::select_location::widgets::FILTER_BUTTON);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_key_event(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.select_location_page_state().query(), "");
        assert!(app.should_quit());
    }

    #[tokio::test]
    async fn esc_on_search_anchor_with_query_clears_it_and_stays_on_page() {
        // Two-step Esc, mirroring the MTU pill: the *first* Esc on a
        // non-empty filter is field-local - it drops the typed query
        // and keeps the user on the page, where they can either retype
        // or press Esc again to leave.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        // Enter via enter_sub_page so the sub-page stack is populated;
        // otherwise the empty-query case below has no parent to fall
        // back to.
        app.enter_sub_page(PageId::SelectLocation);
        app.page_focus_mut().focused = Some(pages::select_location::widgets::SEARCH_ANCHOR);
        for ch in "lax".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }
        assert_eq!(app.select_location_page_state().query(), "lax");

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(
            app.select_location_page_state().query(),
            "",
            "Esc with text in the filter must clear it",
        );
        assert_eq!(
            app.current_page(),
            PageId::SelectLocation,
            "Esc with a non-empty filter is field-local - it doesn't leave the sub-page",
        );
        assert_eq!(
            app.page_focus().focused,
            Some(pages::select_location::widgets::SEARCH_ANCHOR),
            "focus stays on the search anchor after the field-local clear",
        );
    }

    #[tokio::test]
    async fn esc_on_search_anchor_with_empty_query_leaves_sub_page() {
        // Second half of the two-step Esc: an already-empty filter
        // means there's nothing to cancel, so Esc takes its global
        // meaning and leaves the sub-page.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        app.enter_sub_page(PageId::SelectLocation);
        app.page_focus_mut().focused = Some(pages::select_location::widgets::SEARCH_ANCHOR);
        // Filter is "" by default.

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_ne!(
            app.current_page(),
            PageId::SelectLocation,
            "Esc on an empty filter must leave the sub-page",
        );
    }

    // --- Overlay confirmation dispatch ---

    #[tokio::test]
    async fn dispatch_confirm_disconnect_routes_to_app_disconnect() {
        let mut app = app();
        let service = StubService {
            disconnect_result: Ok(true),
            ..StubService::default()
        };

        dispatch_confirm(&mut app, &service, ConfirmAction::Disconnect)
            .await
            .expect("disconnect dispatch should succeed");

        // Two-stage: Disconnect Ok(true) leaves status `Running` until
        // the matching `TunnelState::Disconnected` push arrives.
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Running(Operation::Disconnect),
        );
    }

    #[tokio::test]
    async fn dispatch_confirm_rotate_key_fires_one_rpc() {
        let mut app = app();
        let service = StubService::default();

        dispatch_confirm(&mut app, &service, ConfirmAction::RotateWireGuardKey)
            .await
            .expect("rotate-key dispatch should succeed");

        assert_eq!(*service.rotate_wireguard_key_calls.borrow(), 1);
        // RotateKey has no push event, so it flips immediately to
        // Success on RPC ack via the run_operation path.
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::RotateWireGuardKey),
        );
    }

    #[tokio::test]
    async fn dispatch_confirm_clear_relay_overrides_routes_to_clear_all() {
        let mut app = app();
        let service = StubService::default();

        dispatch_confirm(&mut app, &service, ConfirmAction::ClearRelayOverrides)
            .await
            .expect("clear overrides dispatch ok");

        assert_eq!(*service.clear_all_relay_overrides_calls.borrow(), 1);
        // Two-stage via Settings push.
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Running(Operation::ClearRelayOverrides),
        );
    }

    #[tokio::test]
    async fn dispatch_confirm_remove_relay_override_sends_empty_for_hostname() {
        use crate::integration::RelayOverride;
        let mut app = app();
        let service = StubService::default();

        dispatch_confirm(
            &mut app,
            &service,
            ConfirmAction::RemoveRelayOverride {
                hostname: "se-got-wg-001".to_string(),
            },
        )
        .await
        .expect("remove override dispatch ok");

        let calls = service.set_relay_override_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], RelayOverride::empty("se-got-wg-001".to_string()),);
    }

    #[tokio::test]
    async fn vpn_server_ip_button_activates_into_relay_overrides_sub_page() {
        // The `VpnServerIpInfo` widget used to surface a CLI-hint
        // notification ("Server IP override is not yet configurable...").
        // It now navigates into the new sub-page; lock that mapping.
        let mut app = app();
        let service = StubService::default();
        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        app.navigate_to(PageId::Settings);
        app.enter_sub_page(PageId::SettingsVpn);

        pages::settings::activate(
            &mut app,
            &service,
            &mut input_mode,
            &mut overlay,
            pages::settings::widgets::VPN_SERVER_IP_INFO,
        )
        .await;

        assert_eq!(app.current_page(), PageId::SettingsRelayOverrides);
        assert!(
            matches!(overlay, OverlayMode::None),
            "the row navigates - no overlay should open",
        );
        assert!(
            matches!(input_mode, InputMode::Default),
            "the row navigates - no modal should open",
        );
    }

    #[tokio::test]
    async fn clear_all_button_opens_confirmation_overlay() {
        let mut app = app();
        let service = StubService::default();
        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        app.navigate_to(PageId::Settings);
        app.enter_sub_page(PageId::SettingsVpn);
        app.enter_sub_page(PageId::SettingsRelayOverrides);

        pages::settings::activate(
            &mut app,
            &service,
            &mut input_mode,
            &mut overlay,
            pages::settings::widgets::RELAY_OVERRIDE_CLEAR_ALL,
        )
        .await;

        match overlay {
            OverlayMode::Confirm { action, .. } => {
                assert_eq!(action, ConfirmAction::ClearRelayOverrides);
            }
            other => panic!("expected Confirm overlay, got {other:?}"),
        }
        // The RPC fires only when the user accepts the confirm; the
        // pure activation should not have called it yet.
        assert_eq!(*service.clear_all_relay_overrides_calls.borrow(), 0);
    }

    #[tokio::test]
    async fn add_override_button_opens_relay_override_input_modal() {
        let mut app = app();
        let service = StubService::default();
        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        app.navigate_to(PageId::Settings);
        app.enter_sub_page(PageId::SettingsVpn);
        app.enter_sub_page(PageId::SettingsRelayOverrides);

        pages::settings::activate(
            &mut app,
            &service,
            &mut input_mode,
            &mut overlay,
            pages::settings::widgets::RELAY_OVERRIDE_ADD,
        )
        .await;

        assert!(
            matches!(input_mode, InputMode::RelayOverrideInput(_)),
            "Add button should open the relay-override input modal",
        );
    }

    #[tokio::test]
    async fn left_click_on_ipv4_buffer_focuses_the_ipv4_field() {
        // The relay-override modal has three buffers. Clicking the v4
        // buffer should land focus on `FieldFocus::Ipv4`, not on
        // `Hostname` (the first field). Verifies the per-field click
        // routing via `INPUT_MODAL_FIELD_BASE + idx` plus
        // `InputMode::set_field_index`.
        use crate::{
            app::{FocusKind, FocusRegistry, FocusableWidget, WidgetId},
            tui::modals::relay_override::{FieldFocus, RelayOverrideInputState},
        };
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = app();
        let service = StubService::default();
        // Seed the registry with three buffer rects at known rows -
        // one per relay-override field. The modal renderer normally
        // produces these via render_multi_field_input_prompt; we
        // hand-roll them here so the test bypasses a full draw cycle.
        let mut registry = FocusRegistry::new();
        for i in 0..3u32 {
            registry.register(FocusableWidget {
                id: WidgetId(components::INPUT_MODAL_FIELD_BASE.0 + i),
                rect: Rect::new(10, 5 + i as u16 * 3, 40, 1),
                kind: FocusKind::TextInput,
            });
            registry.end_row();
        }
        app.set_focus_registry(registry, None);

        // Open the modal with focus on Hostname (the default landing).
        let mut input_mode = InputMode::RelayOverrideInput(RelayOverrideInputState::default());
        let mut overlay = OverlayMode::None;

        // Click on field index 1 (the IPv4 buffer, at row 5 + 1*3 = 8).
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 8,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        match &input_mode {
            InputMode::RelayOverrideInput(state) => assert_eq!(
                state.focus,
                FieldFocus::Ipv4,
                "click on the v4 buffer must focus the v4 field, not snap back to Hostname",
            ),
            _ => panic!("modal closed unexpectedly"),
        }

        // Now click the v6 buffer (index 2, row 11).
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 11,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        match &input_mode {
            InputMode::RelayOverrideInput(state) => assert_eq!(state.focus, FieldFocus::Ipv6),
            _ => panic!("modal closed unexpectedly"),
        }
    }

    #[tokio::test]
    async fn per_row_remove_opens_confirm_with_hostname() {
        use crate::{app::WidgetId, integration::RelayOverride};
        use std::net::Ipv4Addr;

        let mut app = app();
        let service = StubService::default();
        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        // Seed two overrides so we can target index 1 and confirm the
        // hostname lookup uses the right row.
        let mut seeded = mullvad_types::settings::Settings::default();
        seeded.relay_overrides.push(RelayOverride {
            hostname: "se-got-wg-001".to_string(),
            ipv4_addr_in: Some(Ipv4Addr::new(1, 1, 1, 1)),
            ipv6_addr_in: None,
        });
        seeded.relay_overrides.push(RelayOverride {
            hostname: "de-fra-wg-005".to_string(),
            ipv4_addr_in: None,
            ipv6_addr_in: None,
        });
        app.set_settings(seeded);
        app.navigate_to(PageId::Settings);
        app.enter_sub_page(PageId::SettingsVpn);
        app.enter_sub_page(PageId::SettingsRelayOverrides);

        let remove_idx1 = WidgetId(pages::settings::widgets::RELAY_OVERRIDE_REMOVE_BASE.0 + 1);
        pages::settings::activate(
            &mut app,
            &service,
            &mut input_mode,
            &mut overlay,
            remove_idx1,
        )
        .await;

        match overlay {
            OverlayMode::Confirm { action, .. } => match action {
                ConfirmAction::RemoveRelayOverride { hostname } => {
                    assert_eq!(hostname, "de-fra-wg-005");
                }
                other => panic!("expected RemoveRelayOverride, got {other:?}"),
            },
            other => panic!("expected Confirm overlay, got {other:?}"),
        }
        assert_eq!(
            service.set_relay_override_calls.borrow().len(),
            0,
            "activation only opens the overlay; the RPC fires on Confirm",
        );
    }

    #[test]
    fn overlay_confirm_constructs_with_dispatch_tag() {
        // Lock in the OverlayMode shape: Confirm carries the
        // user-facing copy alongside the ConfirmAction tag the run
        // loop's `dispatch_confirm` arm matches on, plus the
        // `return_focus` slot used by `close_overlay_restoring_focus`
        // to put the user back where they were.
        let overlay = OverlayMode::Confirm {
            title: "T".to_string(),
            message: "M".to_string(),
            action: ConfirmAction::Disconnect,
            return_focus: None,
        };
        match overlay {
            OverlayMode::Confirm { action, .. } => {
                assert_eq!(action, ConfirmAction::Disconnect);
            }
            _ => panic!("expected Confirm variant"),
        }
    }

    #[test]
    fn modal_lifecycle_saves_focus_on_open_and_restores_on_close() {
        // Regression for the Custom DNS focus-jump bug: the modal-
        // render block in `run_app` replaces the focus registry with
        // its own buttons, so `set_focus_registry`'s snap-to-first-
        // body fallback steals focus on close unless the run loop
        // restores the pre-open page-widget id. `ModalLifecycleSnapshot`
        // is the single seam responsible for that bookkeeping.
        use crate::tui::modals::custom_dns::CustomDnsInputState;

        let mut app = app();
        let opener = WidgetId(0x42);
        // Simulate the user having something else focused before the
        // click (e.g. the Status toggle). The opener widget only
        // becomes the focused one *during* `handle_mouse_event`, so
        // the snapshot must not capture this stale value at `take`.
        app.page_focus_mut().focused = Some(WidgetId(0x99));
        let mut input_mode = InputMode::Default;
        let mut return_focus: Option<WidgetId> = None;

        // Closed -> open transition. `take` runs *before* the event
        // handler; the handler then focuses the opener widget (mouse:
        // `page_focus = hit_test target`; keyboard: opener already
        // focused) and toggles input_mode. `apply` reads focus *after*
        // the handler so both paths land on the right widget.
        let snap = ModalLifecycleSnapshot::take(&input_mode);
        app.page_focus_mut().focused = Some(opener);
        input_mode = InputMode::CustomDnsInput(CustomDnsInputState::default());
        snap.apply(&input_mode, &mut app, &mut return_focus);
        assert_eq!(
            return_focus,
            Some(opener),
            "opening must stash the opener widget the handler just focused",
        );

        // First render after open snaps page focus onto a modal
        // widget; the saved `return_focus` is what survives until
        // close.
        app.page_focus_mut().focused = Some(components::INPUT_MODAL_FIELD);

        // Open -> closed transition: focus snaps back to the opener
        // and the stash empties so a later cycle starts clean.
        let snap = ModalLifecycleSnapshot::take(&input_mode);
        input_mode = InputMode::Default;
        snap.apply(&input_mode, &mut app, &mut return_focus);
        assert_eq!(
            app.page_focus().focused,
            Some(opener),
            "closing must restore the page focus saved on open",
        );
        assert!(
            return_focus.is_none(),
            "stash must clear after restore so the next cycle starts fresh",
        );
    }

    #[test]
    fn modal_lifecycle_no_op_when_modal_stays_open_or_stays_closed() {
        // Validation failures keep the modal open across a Submit;
        // the snapshot pair must not re-stash mid-modal (would clobber
        // the original opener) nor restore (would yank focus out of
        // the still-open modal).
        use crate::tui::modals::custom_dns::CustomDnsInputState;

        let mut app = app();
        let opener = WidgetId(0x42);
        app.page_focus_mut().focused = Some(components::INPUT_MODAL_FIELD);
        let mut input_mode = InputMode::CustomDnsInput(CustomDnsInputState::default());
        let mut return_focus: Option<WidgetId> = Some(opener);

        // Open -> still open: stash untouched, focus untouched.
        let snap = ModalLifecycleSnapshot::take(&input_mode);
        // Simulate a no-op handler (validation failed, modal stays).
        snap.apply(&input_mode, &mut app, &mut return_focus);
        assert_eq!(return_focus, Some(opener));
        assert_eq!(
            app.page_focus().focused,
            Some(components::INPUT_MODAL_FIELD)
        );

        // Closed -> closed: also a no-op.
        input_mode = InputMode::Default;
        app.page_focus_mut().focused = Some(opener);
        let snap = ModalLifecycleSnapshot::take(&input_mode);
        snap.apply(&input_mode, &mut app, &mut return_focus);
        assert_eq!(
            return_focus,
            Some(opener),
            "no transition must leave the stash untouched",
        );
        assert_eq!(app.page_focus().focused, Some(opener));
    }

    #[test]
    fn overlay_return_focus_is_preserved_across_chain() {
        // A Confirm overlay opens with return_focus = Some(button A).
        // A Notification then arrives while the Confirm is open; the
        // notification inherits the Confirm's return_focus rather
        // than re-capturing the now-stale Cancel/Confirm button. On
        // dismissal, focus returns to A - the place the user was
        // before the chain started.
        let original = WidgetId(0x42);
        let confirm = OverlayMode::Confirm {
            title: "T".to_string(),
            message: "M".to_string(),
            action: ConfirmAction::Disconnect,
            return_focus: Some(original),
        };
        assert_eq!(confirm.return_focus(), Some(original));
        // Simulate the chain: notification inherits the saved focus.
        let inherited = confirm.return_focus();
        let notification = OverlayMode::Notification {
            message: "Network blip".to_string(),
            return_focus: inherited,
        };
        assert_eq!(notification.return_focus(), Some(original));
    }

    // ---- Chrome helpers ----

    #[test]
    fn breadcrumb_segments_empty_for_top_level_pages() {
        for page in [
            PageId::Status,
            PageId::Account,
            PageId::Settings,
            PageId::Logs,
        ] {
            assert!(
                breadcrumb_segments(page).is_empty(),
                "{page:?} is top-level - no breadcrumb expected",
            );
        }
    }

    #[test]
    fn breadcrumb_segments_for_settings_sub_pages() {
        let segs = breadcrumb_segments(PageId::SettingsVpn);
        assert_eq!(
            segs,
            vec![("Settings", false), ("VPN", true)],
            "sub-page breadcrumb is [parent, current(active)]",
        );
        // First-level Settings children (Multihop / DAITA / Split
        // tunneling / API access) hang directly off Settings.
        let segs = breadcrumb_segments(PageId::SettingsMultihop);
        assert_eq!(segs, vec![("Settings", false), ("Multihop", true)]);
    }

    #[test]
    fn breadcrumb_segments_pushes_vpn_sub_pages_onto_the_chain() {
        // From `Settings > VPN > DNS content blockers`, the
        // breadcrumb chain has all three segments - drilling deeper
        // shouldn't replace VPN with the leaf.
        assert_eq!(
            breadcrumb_segments(PageId::SettingsDnsBlockers),
            vec![
                ("Settings", false),
                ("VPN", false),
                ("DNS content blockers", true),
            ],
        );
        assert_eq!(
            breadcrumb_segments(PageId::SettingsCustomDns),
            vec![("Settings", false), ("VPN", false), ("Custom DNS", true),],
        );
        assert_eq!(
            breadcrumb_segments(PageId::SettingsAntiCensorship),
            vec![
                ("Settings", false),
                ("VPN", false),
                ("Anti-censorship", true),
            ],
        );
    }

    #[test]
    fn breadcrumb_segments_for_select_location_filter_chain() {
        // The Status > Select location > Filter chain is also driven
        // by the generic walk-via-parent_sub_page logic.
        assert_eq!(
            breadcrumb_segments(PageId::SelectLocationFilter),
            vec![
                ("Status", false),
                ("Select location", false),
                ("Filter", true),
            ],
        );
    }

    #[test]
    fn breadcrumb_segments_for_account_sub_pages() {
        let segs = breadcrumb_segments(PageId::AccountDevices);
        assert_eq!(segs, vec![("Account", false), ("Devices", true)],);
    }

    #[test]
    fn esc_from_two_deep_sub_page_pops_to_intermediate_parent() {
        // Drill in via the actual user path: Status -> Select
        // location -> Filter. Esc should rewind one stack frame at a
        // time, mirroring the navigation history.
        let mut app = App::new();
        app.enter_sub_page(PageId::SelectLocation);
        app.enter_sub_page(PageId::SelectLocationFilter);
        assert_eq!(app.current_page(), PageId::SelectLocationFilter);
        app.leave_sub_page();
        assert_eq!(
            app.current_page(),
            PageId::SelectLocation,
            "Esc should pop one stack frame at a time, not jump to the top-level page",
        );
        assert!(app.is_on_sub_page());
        // A second Esc lands on the top-level Status page.
        app.leave_sub_page();
        assert_eq!(app.current_page(), PageId::Status);
        assert!(!app.is_on_sub_page());
    }

    #[test]
    fn esc_from_vpn_sub_page_pops_to_vpn_settings() {
        // Settings > VPN > DNS content blockers -> Settings > VPN.
        let mut app = App::new();
        app.navigate_to(PageId::Settings);
        app.enter_sub_page(PageId::SettingsVpn);
        app.enter_sub_page(PageId::SettingsDnsBlockers);
        assert_eq!(app.current_page(), PageId::SettingsDnsBlockers);
        app.leave_sub_page();
        assert_eq!(
            app.current_page(),
            PageId::SettingsVpn,
            "Esc from a VPN-nested sub-page should land on VPN, not Settings root",
        );
        // A second Esc lands on Settings root.
        app.leave_sub_page();
        assert_eq!(app.current_page(), PageId::Settings);
    }

    #[test]
    fn esc_restores_focus_to_the_widget_used_to_enter_the_sub_page() {
        // Settings root with focus on the [VPN settings] button ->
        // entering VPN should remember that focus -> Esc back to
        // Settings restores it. Set up a 2-deep chain too: VPN with
        // focus on [DNS content blockers] -> entering DNS -> Esc
        // brings focus back to [DNS content blockers].
        use crate::app::WidgetId;
        let vpn_settings_btn = WidgetId(0xAA);
        let dns_btn = WidgetId(0xBB);

        let mut app = App::new();
        app.navigate_to(PageId::Settings);
        app.page_focus_mut().focused = Some(vpn_settings_btn);
        app.enter_sub_page(PageId::SettingsVpn);
        // First sub-page entry captured the Settings-root focus.
        // Now drill into a VPN child after moving focus.
        app.page_focus_mut().focused = Some(dns_btn);
        app.enter_sub_page(PageId::SettingsDnsBlockers);

        // Pop the inner level: focus restores to [DNS content blockers].
        app.leave_sub_page();
        assert_eq!(app.page_focus().focused, Some(dns_btn));
        // Pop the outer level: focus restores to [VPN settings].
        app.leave_sub_page();
        assert_eq!(app.page_focus().focused, Some(vpn_settings_btn));
    }

    #[test]
    fn tab_into_the_tab_bar_focuses_the_currently_active_tab() {
        // The user is on the Settings tab with focus on a body
        // widget; cycling Tab into the tab-bar pane must land on
        // the Settings tab, not on Status (which is what the
        // generic column-snap would pick).
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget, TOP_LEVEL_PAGES};
        use ratatui::layout::Rect;

        let mut app = App::new();
        app.navigate_to(PageId::Settings);

        let mut registry = FocusRegistry::new();
        let dummy = Rect::new(0, 0, 1, 1);
        // Tab bar - register all four top-level tabs.
        for &page in &TOP_LEVEL_PAGES {
            registry.register(FocusableWidget {
                id: components::tab_widget_id_for_top_level(page),
                rect: dummy,
                kind: FocusKind::TabButton,
            });
        }
        registry.end_row();
        // Body row - a single button.
        let body = WidgetId(0x500);
        registry.register(FocusableWidget {
            id: body,
            rect: dummy,
            kind: FocusKind::Button,
        });
        registry.end_row();
        app.set_focus_registry(registry, None);
        app.page_focus_mut().focused = Some(body);

        // Tab from the body wraps back up to the tab bar pane. The
        // generic next_pane lands on the first cell (Status), but
        // prefer_active_tab should swap it for the active tab.
        let next = app
            .last_focus_registry()
            .next_pane(body)
            .expect("body widget has a successor pane");
        let resolved = prefer_active_tab(&app, next);
        assert_eq!(
            resolved,
            components::tab_widget_id_for_top_level(PageId::Settings),
            "Tab into the tab bar should land on the active tab",
        );
    }

    #[test]
    fn down_from_window_close_focuses_the_active_tab() {
        // The user is on the Settings tab with focus on `[x]`;
        // pressing Down should land on the Settings tab, not the
        // leftmost tab that column-snap would otherwise pick.
        use crate::app::{ArrowDir, FocusKind, FocusRegistry, FocusableWidget, TOP_LEVEL_PAGES};
        use ratatui::layout::Rect;

        let mut app = App::new();
        app.navigate_to(PageId::Settings);

        let mut registry = FocusRegistry::new();
        let dummy = Rect::new(0, 0, 1, 1);
        // Row 0: `[x]` close button.
        registry.register(FocusableWidget {
            id: components::WINDOW_CLOSE,
            rect: dummy,
            kind: FocusKind::WindowClose,
        });
        registry.end_row();
        // Row 1: tab bar.
        for &page in &TOP_LEVEL_PAGES {
            registry.register(FocusableWidget {
                id: components::tab_widget_id_for_top_level(page),
                rect: dummy,
                kind: FocusKind::TabButton,
            });
        }
        registry.end_row();
        app.set_focus_registry(registry, None);
        app.page_focus_mut().focused = Some(components::WINDOW_CLOSE);

        // Column-snap from `[x]` (col 0) into the tab row would land
        // on the first tab; prefer_active_tab should swap it for
        // the active tab.
        let next = app
            .last_focus_registry()
            .navigate(components::WINDOW_CLOSE, ArrowDir::Down)
            .expect("Down from [x] has a successor");
        let resolved = prefer_active_tab(&app, next);
        assert_eq!(
            resolved,
            components::tab_widget_id_for_top_level(PageId::Settings),
            "Down from [x] should land on the active tab",
        );
    }

    #[tokio::test]
    async fn digit_shortcut_focuses_the_newly_selected_tab() {
        // Pressing 1-4 to jump tabs should land focus on the newly-
        // selected tab itself, regardless of where focus was on the
        // previous page. Without this, focus stays on the prior
        // widget id and either hits the snap-to-first fallback
        // (dropping into the new page's body) or sticks to the old
        // page's tab - neither is what the user means by "switch tab".
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;

        // Land on Status (the default), then jump to Account via `2`.
        handle_key_event(
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.current_page(), PageId::Account);
        assert_eq!(
            app.page_focus().focused,
            Some(components::tab_widget_id_for_top_level(PageId::Account)),
            "digit shortcut should leave focus on the activated tab",
        );

        // Jump again with `3` from a different starting tab.
        handle_key_event(
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.current_page(), PageId::Settings);
        assert_eq!(
            app.page_focus().focused,
            Some(components::tab_widget_id_for_top_level(PageId::Settings)),
        );
    }

    #[test]
    fn navigating_to_a_top_level_tab_drops_the_sub_page_stack() {
        // Even when entered through a chain, a tab click clears the
        // whole stack - and the next Esc shouldn't pop anything.
        let mut app = App::new();
        app.navigate_to(PageId::Settings);
        app.enter_sub_page(PageId::SettingsVpn);
        app.enter_sub_page(PageId::SettingsDnsBlockers);
        app.navigate_to(PageId::Status);
        assert!(!app.is_on_sub_page());
        app.leave_sub_page(); // no-op
        assert_eq!(app.current_page(), PageId::Status);
    }

    #[test]
    fn esc_from_one_deep_sub_page_pops_to_top_level() {
        let mut app = App::new();
        app.navigate_to(PageId::Account);
        app.enter_sub_page(PageId::AccountDevices);
        app.leave_sub_page();
        assert_eq!(app.current_page(), PageId::Account);
        assert!(!app.is_on_sub_page());
    }

    #[tokio::test]
    async fn tab_shortcut_is_inert_while_overlay_is_open() {
        // Regression: pressing `1`-`4` while a confirm or
        // notification overlay was up used to navigate the
        // background page out from under the popup. Lock the new
        // behavior: NavigateTab is a no-op until the overlay is
        // dismissed.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::Settings);
        let mut overlay = OverlayMode::Notification {
            message: "anything".to_string(),
            return_focus: components::tab_widget_id(PageId::Settings),
        };
        let mut input_mode = InputMode::Default;

        for (key, label) in [
            (KeyCode::Char('1'), "1 -> Status"),
            (KeyCode::Char('2'), "2 -> Account"),
            (KeyCode::Char('3'), "3 -> Settings"),
            (KeyCode::Char('4'), "4 -> Logs"),
        ] {
            handle_key_event(
                KeyEvent::new(key, KeyModifiers::NONE),
                &mut app,
                &mut input_mode,
                &mut overlay,
                &service,
            )
            .await;
            assert_eq!(
                app.current_page(),
                PageId::Settings,
                "{label} must not switch pages while a notification overlay is open",
            );
            assert!(
                matches!(overlay, OverlayMode::Notification { .. }),
                "{label} must not dismiss the overlay either",
            );
        }
    }

    #[tokio::test]
    async fn esc_on_logs_after_notification_with_no_return_focus_does_not_jump_to_status_tab() {
        // Stronger repro: the notification was captured with
        // `return_focus = None` (e.g. focus was lost between events).
        // After the overlay's [Dismiss] button is registered and
        // focused, Esc clears the overlay but not the focus, leaving
        // the [Dismiss] id on `App.page_focus`. Next render's registry
        // doesn't contain [Dismiss], so the snap-to-first fires - and
        // on Logs (no body row 1) lands on `first()` = Status tab.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};

        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::Logs);
        // Simulate the run loop's overlay rendering: registry is
        // overlay-only and focus lands on the [Dismiss] button.
        app.page_focus_mut().focused = Some(components::OVERLAY_NOTIFICATION_DISMISS);

        let mut overlay = OverlayMode::Notification {
            message: "anything".to_string(),
            return_focus: None,
        };
        let mut input_mode = InputMode::Default;

        // Press Esc to dismiss.
        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert!(matches!(overlay, OverlayMode::None));

        // Now the run loop redraws the page (no overlay).
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut registry = crate::app::FocusRegistry::default();
        terminal
            .draw(|frame| {
                components::render_tab_bar(
                    frame,
                    Rect::new(0, 0, 80, 1),
                    PageId::Logs,
                    app.page_focus(),
                    &mut registry,
                );
                pages::logs::render(frame, Rect::new(0, 1, 80, 23), &app);
            })
            .unwrap();
        // Mirror the run loop's snap-fallback hint so the test
        // exercises the production code path.
        app.set_focus_registry(registry, components::tab_widget_id(PageId::Logs));

        assert_eq!(app.current_page(), PageId::Logs);
        assert_ne!(
            app.page_focus().focused,
            components::tab_widget_id(PageId::Status),
            "Esc on Logs root must not land focus on the Status tab",
        );
    }

    #[tokio::test]
    async fn esc_on_logs_after_notification_does_not_jump_to_status_tab() {
        // Reproduction for the user-reported bug: dismiss a
        // notification overlay on the Logs root page (Logs has no
        // body focusables) and the post-render snap-to-first picks
        // `first()` - which is the Status tab id, the very first
        // widget the run loop registers. Visually that reads as
        // "Esc jumped to a random tab".
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};

        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::Logs);
        // Land focus on the Logs tab - the realistic landing spot
        // after `4`-key navigation.
        app.page_focus_mut().focused = components::tab_widget_id(PageId::Logs);

        // Open a notification overlay whose `return_focus` is the
        // Logs tab (what the run loop captures).
        let mut overlay = OverlayMode::Notification {
            message: "anything".to_string(),
            return_focus: components::tab_widget_id(PageId::Logs),
        };
        let mut input_mode = InputMode::Default;

        // Pre-overlay registry: just the page (run loop swaps in
        // overlay-only registry while the overlay is up).
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut registry = crate::app::FocusRegistry::default();
        terminal
            .draw(|frame| {
                components::render_tab_bar(
                    frame,
                    Rect::new(0, 0, 80, 1),
                    PageId::Logs,
                    app.page_focus(),
                    &mut registry,
                );
                pages::logs::render(frame, Rect::new(0, 1, 80, 23), &app);
            })
            .unwrap();
        app.set_focus_registry(registry, components::tab_widget_id(PageId::Logs));

        // Press Esc to dismiss the notification.
        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert!(matches!(overlay, OverlayMode::None));

        // Re-render now that the overlay is gone - same page, same
        // registry - and let `set_focus_registry` run its snap-check.
        let mut registry = crate::app::FocusRegistry::default();
        terminal
            .draw(|frame| {
                components::render_tab_bar(
                    frame,
                    Rect::new(0, 0, 80, 1),
                    PageId::Logs,
                    app.page_focus(),
                    &mut registry,
                );
                pages::logs::render(frame, Rect::new(0, 1, 80, 23), &app);
            })
            .unwrap();
        app.set_focus_registry(registry, components::tab_widget_id(PageId::Logs));

        assert_eq!(app.current_page(), PageId::Logs);
        assert_eq!(
            app.page_focus().focused,
            components::tab_widget_id(PageId::Logs),
            "Esc on Logs root after dismissing a notification must keep focus on \
             the Logs tab - not snap back to the Status tab via `registry.first()`",
        );
    }

    #[tokio::test]
    async fn esc_on_logs_root_page_does_not_move_focus_to_status_tab() {
        // Logs has no body focusables - if Esc somehow invalidates
        // focus, the snap-to-first fallback in `set_focus_registry`
        // walks past row 1 (empty for Logs) and lands on `first()`,
        // which is the Status tab. That would visually read as Esc
        // "jumping to a random tab", which is what the user is
        // reporting. Lock against it.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};

        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::Logs);
        let logs_tab = components::tab_widget_id(PageId::Logs);
        app.page_focus_mut().focused = logs_tab;

        // Seed `last_focus_registry` from a real render so any
        // post-Esc snap has the actual page registry to walk.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut registry = crate::app::FocusRegistry::default();
        terminal
            .draw(|frame| {
                components::render_tab_bar(
                    frame,
                    Rect::new(0, 0, 80, 1),
                    PageId::Logs,
                    app.page_focus(),
                    &mut registry,
                );
                pages::logs::render(frame, Rect::new(0, 1, 80, 23), &app);
            })
            .unwrap();
        app.set_focus_registry(registry, components::tab_widget_id(PageId::Logs));
        assert_eq!(app.page_focus().focused, logs_tab, "pre-condition");

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(
            app.current_page(),
            PageId::Logs,
            "Esc on Logs root must not switch tabs",
        );
        assert_eq!(
            app.page_focus().focused,
            logs_tab,
            "Esc on Logs root must leave focus on the Logs tab",
        );
    }

    #[tokio::test]
    async fn esc_on_root_page_with_tab_focus_does_not_move_focus() {
        // Same regression check, but with focus on the active tab in
        // the tab bar - the most common "root page" landing spot
        // since pane-cycle / `1`-`4` accelerators leave focus there.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::Settings);
        let initial = components::tab_widget_id(PageId::Settings);
        app.page_focus_mut().focused = initial;

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.current_page(), PageId::Settings);
        assert_eq!(
            app.page_focus().focused,
            initial,
            "Esc on a root page must leave focus where it was, even when focus is on a tab",
        );
    }

    #[tokio::test]
    async fn esc_on_root_page_does_not_move_focus() {
        // Regression: pressing Esc on a top-level tab with no
        // overlay/sub-page used to leak focus to a different tab
        // because the handler treated "no focused widget" the same as
        // an explicit no-op. Lock that behavior: Esc on a root page
        // is inert - same page, same focus.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app();
        let service = StubService::default();
        app.navigate_to(PageId::Settings);
        let initial = Some(crate::tui::pages::settings::widgets::DAITA);
        app.page_focus_mut().focused = initial;

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.current_page(), PageId::Settings);
        assert_eq!(
            app.page_focus().focused,
            initial,
            "Esc on a root page must leave focus where it was",
        );
    }

    #[test]
    fn page_hints_top_level_uses_tab_shortcuts() {
        let app = app(); // default landing page is Status
        let hints = page_hints(&app);
        let keys: Vec<&str> = hints.iter().map(|(k, _)| *k).collect();
        assert!(
            keys.contains(&"1-4"),
            "top-level pages advertise tab shortcuts",
        );
        assert!(keys.contains(&"q"));
        // The `r` resync accelerator was retired; the daemon's push
        // events keep state fresh so a manual refresh hotkey isn't
        // useful enough to justify the surface area.
        assert!(!keys.contains(&"r"));
    }

    #[test]
    fn page_hints_logs_has_scroll_navigation() {
        let mut app = app();
        app.navigate_to(PageId::Logs);
        let hints = page_hints(&app);
        let keys: Vec<&str> = hints.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"↑↓"), "Logs uses ↑↓ for line scroll");
        assert!(keys.contains(&"PgUp/PgDn"));
        assert!(keys.contains(&"Home/End"));
        assert!(
            !keys.contains(&"↑↓←→"),
            "Logs has no focusable widgets - the standard ↑↓←→ Move hint doesn't apply",
        );
    }

    #[test]
    fn page_hints_sub_page_uses_esc_back() {
        let mut app = app();
        app.enter_sub_page(PageId::SettingsVpn);
        let hints = page_hints(&app);
        // Esc -> "Back" by default; switches to "Abort" only when the
        // MTU pill has a pending edit (covered separately below).
        assert_eq!(
            hints
                .iter()
                .find(|(k, _)| *k == "Esc")
                .map(|(_, label)| *label),
            Some("Back"),
        );
        let keys: Vec<&str> = hints.iter().map(|(k, _)| *k).collect();
        assert!(
            !keys.contains(&"1-4"),
            "sub-pages drop tab shortcuts in favor of Esc-back",
        );
    }

    #[test]
    fn page_hints_sub_page_relabels_esc_to_abort_on_dirty_mtu_buffer() {
        // While the user has a pending MTU edit, the first Esc is
        // field-local (revert) - surface that to the user via the hint
        // bar so they don't have to remember the two-step.
        let mut app = app();
        app.enter_sub_page(PageId::SettingsVpn);
        app.page_focus_mut().focused = Some(pages::settings::widgets::VPN_MTU_EDIT);
        app.settings_page_state().push_mtu_char('1');
        app.settings_page_state().push_mtu_char('4');
        app.settings_page_state().push_mtu_char('0');
        app.settings_page_state().push_mtu_char('6');

        let hints = page_hints(&app);
        assert_eq!(
            hints
                .iter()
                .find(|(k, _)| *k == "Esc")
                .map(|(_, label)| *label),
            Some("Abort"),
            "dirty MTU buffer must surface the field-local revert as `Abort`",
        );
    }

    #[test]
    fn page_hints_sub_page_keeps_esc_back_when_mtu_buffer_is_clean() {
        // MTU pill focused but buffer matches daemon -> no pending edit,
        // so Esc still means the global "Back". (Daemon MTU defaults to
        // None in `App::new`, and the buffer starts as "" - clean.)
        let mut app = app();
        app.enter_sub_page(PageId::SettingsVpn);
        app.page_focus_mut().focused = Some(pages::settings::widgets::VPN_MTU_EDIT);

        let hints = page_hints(&app);
        assert_eq!(
            hints
                .iter()
                .find(|(k, _)| *k == "Esc")
                .map(|(_, label)| *label),
            Some("Back"),
        );
    }

    #[test]
    fn page_hints_sub_page_relabels_esc_to_clear_on_non_empty_filter() {
        // While the user has a non-empty filter on the Select-location
        // search anchor, the first Esc clears it (field-local) - the
        // hint bar surfaces that as `Clear` so the two-step is
        // discoverable.
        let mut app = app();
        app.enter_sub_page(PageId::SelectLocation);
        app.page_focus_mut().focused = Some(pages::select_location::widgets::SEARCH_ANCHOR);
        for ch in "lax".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }

        let hints = page_hints(&app);
        assert_eq!(
            hints
                .iter()
                .find(|(k, _)| *k == "Esc")
                .map(|(_, label)| *label),
            Some("Clear"),
            "non-empty filter must surface the field-local clear as `Clear`",
        );
    }

    #[test]
    fn page_hints_sub_page_keeps_esc_back_when_filter_is_empty() {
        // Empty filter -> no field-local Esc to surface, so the hint
        // bar shows the global `Back`.
        let mut app = app();
        app.enter_sub_page(PageId::SelectLocation);
        app.page_focus_mut().focused = Some(pages::select_location::widgets::SEARCH_ANCHOR);

        let hints = page_hints(&app);
        assert_eq!(
            hints
                .iter()
                .find(|(k, _)| *k == "Esc")
                .map(|(_, label)| *label),
            Some("Back"),
        );
    }

    #[test]
    fn page_hints_sub_page_keeps_esc_back_when_search_anchor_not_focused() {
        // A non-empty filter doesn't relabel Esc when the user has
        // navigated focus elsewhere on the page - the field-local
        // semantics only apply while the search anchor is focused.
        let mut app = app();
        app.enter_sub_page(PageId::SelectLocation);
        for ch in "lax".chars() {
            app.select_location_page_state_mut().push_query_char(ch);
        }
        app.page_focus_mut().focused = Some(pages::select_location::widgets::FILTER_BUTTON);

        let hints = page_hints(&app);
        assert_eq!(
            hints
                .iter()
                .find(|(k, _)| *k == "Esc")
                .map(|(_, label)| *label),
            Some("Back"),
            "Clear label is gated on the search-anchor focus, not just query state",
        );
    }

    #[test]
    fn page_hints_sub_page_keeps_esc_back_when_mtu_pill_not_focused() {
        // Even with a (somehow) dirty buffer, if the user isn't on the
        // MTU pill the field-local Esc semantics don't apply - Esc
        // still does the global Back.
        let mut app = app();
        app.enter_sub_page(PageId::SettingsVpn);
        app.page_focus_mut().focused = Some(pages::settings::widgets::VPN_LAN_TOGGLE);
        app.settings_page_state().push_mtu_char('9'); // dirty by daemon (None) != Some(9)

        let hints = page_hints(&app);
        assert_eq!(
            hints
                .iter()
                .find(|(k, _)| *k == "Esc")
                .map(|(_, label)| *label),
            Some("Back"),
            "Abort label is gated on focus, not just buffer state",
        );
    }

    // ---- Device IP version writes ----

    #[tokio::test]
    async fn set_ip_version_preference_dispatches_to_service_with_value() {
        use talpid_types::net::IpVersion;
        let mut app = app();
        let service = StubService::default();
        // The settings cache starts empty; the App's setter still
        // queues the call through `start_push_op_unit` and dispatches
        // the trait method.
        app.set_ip_version_preference(&service, Some(IpVersion::V4))
            .await
            .expect("RPC stub returns Ok");
        let calls = service.set_ip_version_preference_calls.borrow().clone();
        assert_eq!(calls, vec![Some(IpVersion::V4)]);
    }

    #[tokio::test]
    async fn set_ip_version_preference_none_clears_constraint() {
        let mut app = app();
        let service = StubService::default();
        app.set_ip_version_preference(&service, None)
            .await
            .expect("RPC stub returns Ok");
        let calls = service.set_ip_version_preference_calls.borrow().clone();
        assert_eq!(
            calls,
            vec![None],
            "None signals 'Automatic' (Constraint::Any)",
        );
    }

    // ---- Inline MTU input pill ----

    #[tokio::test]
    async fn mtu_pill_focused_typing_appends_digits_and_skips_global_shortcuts() {
        // The VPN settings sub-page MTU pill is an inline text input
        // (mirroring the SelectLocation search anchor). While focused,
        // ASCII digits must land in the buffer instead of triggering
        // tab-jumps (`1`-`4`) or Quit (`q` would be caught at the
        // earlier digit check, but verifying digits don't navigate is
        // the load-bearing assertion here). Backspace pops a digit.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        app.enter_sub_page(PageId::SettingsVpn);
        app.page_focus_mut().focused = Some(pages::settings::widgets::VPN_MTU_EDIT);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        for ch in ['1', '4', '0', '6'] {
            handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut app,
                &mut input_mode,
                &mut overlay,
                &service,
            )
            .await;
        }
        assert_eq!(app.settings_page_state().mtu_buffer(), "1406");
        // `1` did not jump to the Status tab; we're still on the VPN
        // settings sub-page.
        assert_eq!(app.current_page(), PageId::SettingsVpn);

        handle_key_event(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(app.settings_page_state().mtu_buffer(), "140");
    }

    #[tokio::test]
    async fn mtu_pill_caps_buffer_at_four_digits() {
        // The valid MTU range tops out at 1420 (4 digits), so the
        // inline pill silently drops any 5th-digit keystroke. The cap
        // lives on `push_mtu_char` so the run-loop path inherits it.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        app.enter_sub_page(PageId::SettingsVpn);
        app.page_focus_mut().focused = Some(pages::settings::widgets::VPN_MTU_EDIT);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        for ch in ['1', '2', '3', '4', '5', '6'] {
            handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut app,
                &mut input_mode,
                &mut overlay,
                &service,
            )
            .await;
        }
        assert_eq!(app.settings_page_state().mtu_buffer(), "1234");
    }

    #[tokio::test]
    async fn mtu_pill_enter_submits_buffer_to_daemon() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        app.enter_sub_page(PageId::SettingsVpn);
        app.page_focus_mut().focused = Some(pages::settings::widgets::VPN_MTU_EDIT);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        for ch in ['1', '3', '8', '0'] {
            handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut app,
                &mut input_mode,
                &mut overlay,
                &service,
            )
            .await;
        }
        handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert_eq!(
            service.set_mtu_calls.borrow().as_slice(),
            &[Some(1380u16)],
            "Enter on a valid in-range buffer pushes the value through to the service",
        );
    }

    #[tokio::test]
    async fn mtu_pill_esc_with_dirty_buffer_reverts_and_stays_on_sub_page() {
        // Two-step Esc: the *first* Esc on a dirty buffer is field-
        // local - it discards the in-flight edit and keeps the user on
        // the field, where they can either retry or press Esc again to
        // leave the sub-page. The discarded edit is not pushed.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        app.enter_sub_page(PageId::SettingsVpn);
        app.page_focus_mut().focused = Some(pages::settings::widgets::VPN_MTU_EDIT);
        // Pre-condition: daemon's MTU is `None` (the `Settings::default`
        // shape `App::new()` fakes), buffer is "" - clean. The 4-char
        // [`MTU_BUFFER_MAX_LEN`] cap then leaves 4 free slots for the
        // user to dirty the buffer with "1406".
        assert_eq!(app.settings_page_state().mtu_buffer(), "");

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        for ch in ['1', '4', '0', '6'] {
            handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut app,
                &mut input_mode,
                &mut overlay,
                &service,
            )
            .await;
        }
        assert_eq!(app.settings_page_state().mtu_buffer(), "1406");

        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        assert_eq!(
            app.settings_page_state().mtu_buffer(),
            "",
            "Esc must revert the dirty buffer to the daemon's value (None -> \"\")",
        );
        assert!(
            service.set_mtu_calls.borrow().is_empty(),
            "the discarded edit must not be pushed to the daemon",
        );
        assert_eq!(
            app.current_page(),
            PageId::SettingsVpn,
            "the first Esc on a dirty buffer is field-local - it doesn't leave the sub-page",
        );
        assert_eq!(
            app.page_focus().focused,
            Some(pages::settings::widgets::VPN_MTU_EDIT),
            "focus stays on the MTU pill after the field-local revert",
        );
    }

    #[tokio::test]
    async fn mtu_pill_esc_with_clean_buffer_leaves_sub_page() {
        // Second half of the two-step Esc: when the buffer already
        // matches the daemon (clean), Esc takes its global meaning and
        // leaves the sub-page. After the dirty-Esc test reverts a
        // pending edit, this is what the *second* Esc would do.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        app.enter_sub_page(PageId::SettingsVpn);
        app.page_focus_mut().focused = Some(pages::settings::widgets::VPN_MTU_EDIT);
        // Buffer is "" (clean against the default `None` daemon MTU).

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        assert_ne!(
            app.current_page(),
            PageId::SettingsVpn,
            "Esc on a clean buffer must leave the sub-page",
        );
        assert!(
            service.set_mtu_calls.borrow().is_empty(),
            "leaving with a clean buffer must not fire an RPC",
        );
    }

    /// Build a focus registry holding a single tab-bar button at a
    /// known cell, suitable for steering a mouse click to a known blur
    /// target. Returns `(registry, click_x, click_y, target_page)`.
    fn registry_with_one_tab(target: PageId) -> (crate::app::FocusRegistry, u16, u16, PageId) {
        use crate::app::{FocusKind, FocusRegistry, FocusableWidget};
        use ratatui::layout::Rect;

        let id = components::tab_widget_id_for_top_level(target);
        let mut registry = FocusRegistry::new();
        registry.register(FocusableWidget {
            id,
            rect: Rect::new(20, 0, 10, 1),
            kind: FocusKind::TabButton,
        });
        registry.end_row();
        (registry, 25, 0, target)
    }

    #[tokio::test]
    async fn mtu_pill_blur_via_mouse_click_with_invalid_value_surfaces_a_failed_op_and_no_rpc() {
        // 999 is below the daemon-accepted range (1280..=1420). When
        // the user clicks away from the focused MTU pill, the wrapper
        // auto-applies the buffer; `App::set_mtu` validates before the
        // RPC, so the op flips to `Failed` and `set_mtu_calls` stays
        // empty. The renderer's overlay surfaces the matching
        // notification - verifying the op state is enough here.
        use crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };

        let mut app = app();
        let service = StubService::default();
        app.enter_sub_page(PageId::SettingsVpn);

        // Order matters: install the registry *first*, since
        // `set_focus_registry` snaps focus into the registry when the
        // prior focused id isn't present. We then override focus to MTU
        // (which is *not* in the registry but doesn't get re-snapped
        // until the next `set_focus_registry` call).
        let (registry, click_x, click_y, _) = registry_with_one_tab(PageId::Status);
        app.set_focus_registry(registry, None);
        app.page_focus_mut().focused = Some(pages::settings::widgets::VPN_MTU_EDIT);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        for ch in ['9', '9', '9'] {
            handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut app,
                &mut input_mode,
                &mut overlay,
                &service,
            )
            .await;
        }

        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: click_x,
                row: click_y,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        assert!(
            service.set_mtu_calls.borrow().is_empty(),
            "validation must reject the value before the daemon RPC",
        );
        match app.operation_status() {
            OperationStatus::Failed { operation, message } => {
                assert_eq!(*operation, Operation::SetMtu);
                assert!(
                    message.contains("999") && message.contains("valid range"),
                    "failure message should explain the range: {message}",
                );
            }
            other => panic!("expected SetMtu Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mtu_pill_blur_via_mouse_click_with_unchanged_value_skips_the_rpc() {
        // If the user focuses the pill, types nothing, and clicks away,
        // the buffer still equals the daemon's value - re-pushing it is
        // wasteful, so the commit-on-blur short-circuits. (Esc would
        // also produce no RPC here, but for a different reason - Esc
        // reverts. Mouse click is the path that actually exercises the
        // skip-if-unchanged guard.)
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use mullvad_types::settings::Settings;

        let mut app = app();
        let mut settings = Settings::default();
        settings.tunnel_options.wireguard.mtu = Some(1380);
        app.set_settings(settings);
        app.settings_page_state()
            .sync_mtu_buffer_from_daemon(Some(1380));

        let service = StubService::default();
        app.enter_sub_page(PageId::SettingsVpn);

        // Same ordering trick as the invalid-blur test: install the
        // registry first, then override focus to MTU (which isn't in
        // the registry but only gets re-snapped on the next
        // `set_focus_registry` call).
        let (registry, click_x, click_y, _) = registry_with_one_tab(PageId::Status);
        app.set_focus_registry(registry, None);
        app.page_focus_mut().focused = Some(pages::settings::widgets::VPN_MTU_EDIT);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: click_x,
                row: click_y,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;

        assert!(
            service.set_mtu_calls.borrow().is_empty(),
            "buffer matches daemon - commit-on-blur must skip the RPC",
        );
    }

    #[tokio::test]
    async fn mtu_pill_unfocused_keeps_global_shortcuts() {
        // When the MTU pill is *not* focused, the global shortcuts
        // (Quit on `q`, tab-jumps on digits) must work normally on the
        // VPN settings sub-page - the inline-input intercept block must
        // gate on focus, not just on page id.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let service = StubService::default();
        app.enter_sub_page(PageId::SettingsVpn);
        // Focus a non-MTU widget on the same page so the intercept
        // block doesn't fire.
        app.page_focus_mut().focused = Some(pages::settings::widgets::VPN_LAN_TOGGLE);

        let mut input_mode = InputMode::Default;
        let mut overlay = OverlayMode::None;
        handle_key_event(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut app,
            &mut input_mode,
            &mut overlay,
            &service,
        )
        .await;
        assert!(app.should_quit(), "`q` off the MTU pill must still Quit");
    }
}
