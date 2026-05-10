// SPDX-License-Identifier: GPL-3.0-or-later

//! Account page + Manage devices sub-page renderers.
//!
//! Logged-out state shows a login prompt instead of the account info -
//! the user reaches it via the `[Log in]` button which opens the
//! existing AccountInput overlay.
//!
//! Per-page state (account-number visibility, cached device list) lives
//! in `app::pages::account::PageState`. Enter-key dispatch is in
//! `tui::mod`'s run-loop handler.

use chrono::Local;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::Paragraph,
};

use crate::{
    app::{App, ConfirmAction, FocusRegistry, WidgetId},
    integration::{AccountData, Device, DeviceState, MullvadService},
    tui::{
        components,
        error::format_action_error,
        modals::{InputMode, account::AccountInputState, voucher::VoucherInputState},
        overlays::OverlayMode,
    },
};

crate::define_page_widgets! {
    /// Closed enum of static Account-page widgets (top-level page +
    /// logged-out login button). The Manage-devices sub-page's `[Remove]`
    /// rows live in a *dynamic* `WidgetId` range outside this enum -
    /// see [`widgets::REMOVE_DEVICE_BASE`] and [`remove_device_index`].
    ///
    /// `LastSentinel` marks the end of this page's slice. It's never
    /// rendered or matched against - its only role is to anchor the
    /// dynamic `[Remove]` row range so the base shifts automatically when
    /// new variants are added before it.
    pub enum AccountWidget {
        ManageDevices = 0x20,
        ShowAccount,
        BuyCredit,
        RedeemVoucher,
        LogOut,
        /// Logged-out path.
        LogIn,
        /// Manage devices sub-page: per-current-device action.
        RotateKey,
    }
    sentinel LastSentinel;
    extra widgets {
        // Manage devices sub-page: `REMOVE_DEVICE_BASE..REMOVE_DEVICE_BASE+N`
        // is a per-row range with one focus id per "other device". 16 rows
        // is comfortably more than the daemon's 5-device cap. Dynamic by
        // construction, so it can't live in the closed `AccountWidget` enum.
        // Base is derived from the enum's `LastSentinel` so adding a new
        // top-level account widget shifts this range automatically - no
        // hand-picked hex byte to keep in sync.
        pub const REMOVE_DEVICE_BASE: WidgetId = WidgetId(AccountWidget::LastSentinel as u32);
        pub const REMOVE_DEVICE_MAX: u32 = 16;
    }
}

/// True if `widget` is one of the Account top-level page's body widgets.
/// Used by the run-loop's Enter dispatch to recognize Account-page
/// activations vs. tab activations or other pages.
pub fn owns_top_widget(widget: WidgetId) -> bool {
    AccountWidget::from_widget_id(widget).is_some()
}

/// Decode a `[Remove]` widget id into a 0-based "other device" index.
/// Returns `None` when the id isn't in the remove-row range.
pub fn remove_device_index(widget: WidgetId) -> Option<usize> {
    let base = widgets::REMOVE_DEVICE_BASE.0;
    let id = widget.0;
    if id >= base && id < base + widgets::REMOVE_DEVICE_MAX {
        Some((id - base) as usize)
    } else {
        None
    }
}

/// Run the action bound to a focused Account-page or Manage-devices
/// widget. Caller has already verified ownership via
/// [`owns_top_widget`] or [`remove_device_index`].
pub async fn activate<S: MullvadService>(
    app: &mut App,
    service: &S,
    input_mode: &mut InputMode,
    overlay: &mut OverlayMode,
    widget: WidgetId,
) {
    if let Some(index) = remove_device_index(widget) {
        // Resolve the device id at activation time; the cached list is
        // fresh as of the last fetch and the renderer keeps the row
        // ordering stable, so `index` lines up.
        let target = app.account_page_state().devices.as_ref().and_then(|all| {
            let current = app.current_device();
            all.iter()
                .filter(|d| match current {
                    Some(c) => d.id != c.id,
                    None => true,
                })
                .nth(index)
                .map(|d| d.id.clone())
        });
        match target {
            Some(device_id) => {
                if let Err(error) = app.remove_device(service, device_id).await {
                    app.show_notification(format_action_error("remove device", &error));
                }
            }
            None => app.show_notification("Device row no longer in cache; refresh and retry"),
        }
        return;
    }

    let Some(widget) = AccountWidget::from_widget_id(widget) else {
        return;
    };
    match widget {
        AccountWidget::ManageDevices => {
            app.enter_sub_page(crate::app::PageId::AccountDevices);
            // Lazy-fetch on entry. The cached list (if any) is preserved
            // for instant first-paint; the fetch refreshes it underneath.
            if let Err(error) = app.list_devices(service).await {
                app.show_notification(format_action_error("list devices", &error));
            }
        }
        AccountWidget::ShowAccount => {
            let state = app.account_page_state_mut();
            state.account_number_visible = !state.account_number_visible;
        }
        AccountWidget::BuyCredit => {
            // No daemon RPC for an auto-login URL is exposed today
            // (`get_www_auth_token` is commented out upstream), so we
            // surface the public account page URL and let the user copy
            // it. Future: spawn a `xdg-open` / `open` / `cmd /c start`
            // depending on platform once we add the auto-login token.
            app.show_notification(
                "Buy more credit at https://mullvad.net/account/ - sign in with your account number",
            );
        }
        AccountWidget::RedeemVoucher => {
            *input_mode = InputMode::VoucherInput(VoucherInputState::default());
        }
        AccountWidget::LogOut => {
            let return_focus = app.page_focus().focused;
            *overlay = OverlayMode::Confirm {
                title: "Confirm logout".to_string(),
                message: "Log out from the current Mullvad account session?".to_string(),
                action: ConfirmAction::Logout,
                return_focus,
            };
        }
        AccountWidget::LogIn => {
            *input_mode = InputMode::AccountInput(AccountInputState::default());
        }
        AccountWidget::RotateKey => {
            let return_focus = app.page_focus().focused;
            *overlay = OverlayMode::Confirm {
                title: "Rotate WireGuard key".to_string(),
                message: "The daemon will generate a new WireGuard key and register it \
                          with Mullvad's API. Connectivity drops briefly while the tunnel \
                          swaps onto the new key. Continue?"
                    .to_string(),
                action: ConfirmAction::RotateWireGuardKey,
                return_focus,
            };
        }
        // Sentinel - never registered as a focusable widget; only
        // exists to anchor `widgets::REMOVE_DEVICE_BASE`. Reaching
        // this arm would mean a synthetic id was activated; no-op.
        AccountWidget::LastSentinel => {}
    }
}

/// Render the Account top-level page. Branches on logged-in state: a
/// logged-in user sees account/device info; a logged-out user sees a
/// login prompt with a `[Log in]` button.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let info = app.account_info();
    let logged_in_device = info.is_some_and(|i| matches!(i.device, DeviceState::LoggedIn(_)));

    if logged_in_device {
        render_logged_in(frame, area, app, focused, registry);
    } else {
        render_logged_out(frame, area, app, focused, registry);
    }
}

fn render_logged_in(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let state = app.account_page_state();
    let info = app.account_info();
    let device = app.current_device();
    let account_number = current_account_number(app);
    let expiry_text = info
        .and_then(|i| i.data.as_ref())
        .map(format_account_expiry)
        .unwrap_or_else(|| "(loading…)".to_string());

    // Vertical layout: section heading, "Device name" row + value, blank,
    // "Account number" row + masked/unmasked value with [Show] toggle,
    // blank, "Paid until" + value, blank, [Buy more credit] button row,
    // blank, [Redeem voucher] button row, expand, [Log out] anchored at
    // the bottom.
    let [
        device_row,
        _b1,
        device_name_row,
        _b2,
        account_number_label,
        _b3,
        account_number_row,
        _b4,
        paid_until_label,
        _b5,
        expiry_row,
        _b6,
        buy_credit_row,
        _b7,
        redeem_row,
        _spacer,
        log_out_row,
    ] = Layout::vertical([
        Constraint::Length(1), // "Device name"        [Manage devices]
        Constraint::Length(1), // blank
        Constraint::Length(1), // device-name value
        Constraint::Length(1), // blank
        Constraint::Length(1), // "Account number"
        Constraint::Length(1), // blank
        Constraint::Length(1), // **** **** ...   [Show]
        Constraint::Length(1), // blank
        Constraint::Length(1), // "Paid until"
        Constraint::Length(1), // blank
        Constraint::Length(1), // expiry value
        Constraint::Length(1), // blank
        Constraint::Length(1), // [Buy more credit]
        Constraint::Length(1), // blank
        Constraint::Length(1), // [Redeem voucher]
        Constraint::Min(1),    // spacer
        Constraint::Length(1), // [Log out]
    ])
    .areas(area);

    render_device_row(frame, device_row, focused, registry);

    let device_name = device
        .map(|d| d.pretty_name())
        .unwrap_or_else(|| "(unknown)".to_string());
    frame.render_widget(Paragraph::new(format!("  {device_name}")), device_name_row);

    frame.render_widget(Paragraph::new("Account number"), account_number_label);
    render_account_number_row(
        frame,
        account_number_row,
        account_number.as_deref(),
        state.account_number_visible,
        focused,
        registry,
    );

    frame.render_widget(Paragraph::new("Paid until"), paid_until_label);
    frame.render_widget(Paragraph::new(format!("  {expiry_text}")), expiry_row);

    components::render_centered_button(
        frame,
        buy_credit_row,
        "Buy more credit...",
        widgets::BUY_CREDIT,
        focused,
        registry,
    );
    components::render_centered_button(
        frame,
        redeem_row,
        "Redeem voucher",
        widgets::REDEEM_VOUCHER,
        focused,
        registry,
    );
    components::render_centered_button_danger(
        frame,
        log_out_row,
        "Log out",
        widgets::LOG_OUT,
        focused,
        registry,
    );
}

fn render_logged_out(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let [hint_row, _b2, login_row, _spacer] = Layout::vertical([
        Constraint::Length(1), // status hint
        Constraint::Length(1), // blank
        Constraint::Length(1), // [Log in]
        Constraint::Min(1),    // spacer
    ])
    .areas(area);

    let hint = match app.account_info() {
        Some(info) => match info.device {
            DeviceState::Revoked => {
                "Device was revoked from this account. Log in to register a new one."
            }
            DeviceState::LoggedOut | DeviceState::LoggedIn(_) => {
                "Not logged in. Press the button below to enter your account number."
            }
        },
        None => "Account state not yet loaded - waiting for the daemon.",
    };
    frame.render_widget(Paragraph::new(hint), hint_row);
    components::render_centered_button(
        frame,
        login_row,
        "Log in",
        widgets::LOG_IN,
        focused,
        registry,
    );
}

fn render_device_row(
    frame: &mut Frame<'_>,
    area: Rect,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    components::render_label_button_row(
        frame,
        area,
        "Device name".to_string(),
        "Manage devices",
        widgets::MANAGE_DEVICES,
        focused,
        registry,
    );
}

fn render_account_number_row(
    frame: &mut Frame<'_>,
    area: Rect,
    account_number: Option<&str>,
    visible: bool,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let button_label = if visible { "Hide" } else { "Show" };
    let display = match (account_number, visible) {
        (Some(number), true) => format_account_number(number),
        (Some(_), false) => "**** **** **** ****".to_string(),
        (None, _) => "(unknown)".to_string(),
    };
    components::render_label_button_row(
        frame,
        area,
        format!("  {display}"),
        button_label,
        widgets::SHOW_ACCOUNT,
        focused,
        registry,
    );
}

/// Render the Manage devices sub-page. Pulls the cached device list off
/// `account_page_state.devices`; the run-loop ensures a fetch has run
/// (or a fetch is in-flight / errored) before this is called.
pub fn render_devices(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let state = app.account_page_state();
    let current_device = app.current_device();

    let mut lines: Vec<Line<'static>> = vec![
        Line::from("Manage devices"),
        Line::from(""),
        Line::from("You can have up to 5 devices on one account."),
        Line::from(""),
    ];

    if let Some(error) = state.devices_error.as_ref() {
        lines.push(Line::from(format!("Could not load devices: {error}")));
        lines.push(Line::from(""));
    } else if state.devices_loading {
        lines.push(Line::from("(loading device list…)"));
        lines.push(Line::from(""));
    }

    lines.push(Line::from("Current device:"));
    lines.push(Line::from(""));
    if let Some(device) = current_device {
        lines.push(Line::from(format!("  - {}", device.pretty_name())));
        lines.push(Line::from(format!(
            "    Created: {}",
            format_created_date(device.created)
        )));
    } else {
        lines.push(Line::from("    (unknown)"));
    }

    let other_devices = collect_other_devices(state.devices.as_deref(), current_device);
    let static_lines = lines.len();

    // Build the layout: static header lines first, then a per-current-
    // device `[Rotate WireGuard key]` button row, then per-other-device
    // 2-row blocks (`<name>   [Remove]` + `Created: …`), then a spacer.
    let mut constraints: Vec<Constraint> = vec![Constraint::Length(static_lines as u16)];
    constraints.push(Constraint::Length(1)); // [Rotate WireGuard key]
    constraints.push(Constraint::Length(1)); // blank
    constraints.push(Constraint::Length(1)); // "Other devices:" label
    constraints.push(Constraint::Length(1)); // blank
    for _ in &other_devices {
        constraints.push(Constraint::Length(1)); // name + [Remove]
        constraints.push(Constraint::Length(1)); // created date
    }
    constraints.push(Constraint::Min(0));

    let chunks = Layout::vertical(constraints).split(area);

    frame.render_widget(Paragraph::new(lines), chunks[0]);
    if current_device.is_some() {
        // Only register the button when there's a current device to
        // rotate the key for. With no device the row stays blank;
        // rotation has no meaning.
        components::render_centered_button(
            frame,
            chunks[1],
            "Rotate WireGuard key",
            widgets::ROTATE_KEY,
            focused,
            registry,
        );
    }
    // chunks[2] is intentionally blank.
    frame.render_widget(Paragraph::new("Other devices:"), chunks[3]);
    // chunks[4] is intentionally blank.

    for (i, device) in other_devices.iter().enumerate() {
        if (i as u32) >= widgets::REMOVE_DEVICE_MAX {
            break;
        }
        let name_chunk = chunks[5 + i * 2];
        let created_chunk = chunks[5 + i * 2 + 1];
        render_other_device_row(frame, name_chunk, device, i, focused, registry);
        frame.render_widget(
            Paragraph::new(format!(
                "    Created: {}",
                format_created_date(device.created)
            )),
            created_chunk,
        );
    }
}

fn render_other_device_row(
    frame: &mut Frame<'_>,
    area: Rect,
    device: &Device,
    index: usize,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let id = WidgetId(widgets::REMOVE_DEVICE_BASE.0 + index as u32);
    components::render_label_button_row(
        frame,
        area,
        format!("  - {}", device.pretty_name()),
        "Remove",
        id,
        focused,
        registry,
    );
}

fn collect_other_devices<'a>(
    cached: Option<&'a [Device]>,
    current: Option<&Device>,
) -> Vec<&'a Device> {
    let Some(cached) = cached else {
        return Vec::new();
    };
    cached
        .iter()
        .filter(|d| match current {
            Some(c) => d.id != c.id,
            None => true,
        })
        .collect()
}

fn current_account_number(app: &App) -> Option<String> {
    let info = app.account_info()?;
    match &info.device {
        DeviceState::LoggedIn(account_and_device) => {
            Some(account_and_device.account_number.clone())
        }
        _ => None,
    }
}

fn format_account_number(raw: &str) -> String {
    // Group the digits into 4-char blocks separated by spaces.
    // Non-digit characters (defensive) are passed through verbatim;
    // the daemon only stores digits.
    let mut out = String::with_capacity(raw.len() + raw.len() / 4);
    for (i, ch) in raw.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

fn format_account_expiry(data: &AccountData) -> String {
    // `Apr 1, 2027 12:34 PM`. Local timezone - this is user-facing UI
    // text, not a wire format, so it matches the reader's clock.
    data.expiry
        .with_timezone(&Local)
        .format("%b %-d, %Y %-I:%M %p")
        .to_string()
}

fn format_created_date(date: chrono::DateTime<chrono::Utc>) -> String {
    // `2026-04-01`. Date-only is enough for the device list; the API
    // returns timestamps but the user only cares about the day.
    date.with_timezone(&Local).format("%Y-%m-%d").to_string()
}
