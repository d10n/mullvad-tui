// SPDX-License-Identifier: GPL-3.0-or-later

//! Settings page family: the root menu plus the sub-page renderers
//! (Multihop, DAITA, VPN settings, DNS blockers, custom DNS,
//! anti-censorship, split tunneling, API access).
//!
//! The root is a vertical list of buttons, each leading to a sub-page.
//! Per-button right-aligned values display the current setting state
//! for at-a-glance reference (e.g. `[Multihop]   Off`).

use indoc::indoc;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Paragraph, Wrap},
};

use crate::{
    app::{
        App, ConfirmAction, FocusKind, FocusRegistry, FocusableWidget, WidgetId,
        mode_has_configurable_port,
    },
    integration::{MullvadService, SelectedObfuscation, TunnelState},
    tui::{
        components,
        error::format_action_error,
        modals::{
            InputMode,
            custom_dns::CustomDnsInputState,
            port::PortInputState,
            split_tunnel::{SplitTunnelPathInputState, SplitTunnelPidInputState},
        },
        overlays::OverlayMode,
    },
};

mod anti_censorship;
mod api_access;
mod custom_dns;
mod dns_blockers;
mod relay_overrides;

pub use anti_censorship::render as render_anti_censorship;
pub use api_access::render as render_api_access;
pub use custom_dns::render as render_custom_dns;
pub use dns_blockers::render as render_dns_blockers;
pub use relay_overrides::render as render_relay_overrides;

crate::define_page_widgets! {
    /// Closed enum of every Settings-family widget across the root menu
    /// and the three sub-pages (Multihop / DAITA / VPN settings). They
    /// share a single activation handler in `tui::run_loop`, so a single
    /// enum keeps that match exhaustive across all four pages.
    ///
    /// Each cluster (root menu / Multihop / DAITA / VPN settings / DNS
    /// blockers / custom DNS / anti-censorship) anchors at its own base;
    /// the rest auto-increment via `#[repr(u32)]` semantics. The bases
    /// keep their original gap layout - adding a variant in the middle
    /// of a cluster shifts only the variants below it within that
    /// cluster, not the whole family.
    ///
    /// `LastSentinel` marks the end of the Settings slice. It's never
    /// rendered or matched against - its only role is to anchor the
    /// dynamic Custom-DNS `[Remove]` row range so the base shifts
    /// automatically when new variants are added before it.
    pub enum SettingsWidget {
        // Settings root menu
        Daita = 0x40,
        Multihop,
        VpnSettings,
        SplitTunneling,
        ApiAccess,
        DisconnectAndQuit,

        // Multihop sub-page
        MultihopToggle = 0x50,

        // DAITA sub-page
        DaitaToggle = 0x60,
        DaitaDirectOnlyToggle,
        /// Pager `[<]` / `[>]` buttons that step through the DAITA
        /// sub-page's two-page descriptive blurb. Page state lives on
        /// `pages::settings::PageState::daita_blurb_page`.
        DaitaBlurbPrev,
        DaitaBlurbNext,

        // VPN settings sub-page (cluster spans 0x70..=0x8F to fit the
        // 13 focusables: 7 toggles, 5 per-row [Info] companions, the
        // Kill-switch toggle, the inline `Device IP version` radio
        // group's three options, and the chevron / value-edit rows).
        VpnLanToggle = 0x70,
        VpnDnsBlockersInfo,
        VpnCustomDnsInfo,
        VpnIpv6Toggle,
        VpnKillSwitchInfo,
        VpnLockdownToggle,
        VpnAntiCensorshipEdit,
        VpnQuantumToggle,
        VpnDeviceIpInfo,
        VpnMtuEdit,
        VpnServerIpInfo,
        VpnAutoConnectToggle,
        /// Split-tunnel sub-page widgets. The cross-platform toggle and
        /// the two add buttons (one per platform group) are static; the
        /// per-row [Remove] buttons live in dynamic ranges (see
        /// `widgets::SPLIT_TUNNEL_REMOVE_*_BASE`). Appended at the end of
        /// the VPN-cluster slice.
        SplitTunnelToggle,
        SplitTunnelAddApp,
        SplitTunnelAddPid,
        /// Per-toggle `[Info]` companions for the VPN rows that pair an
        /// Info button with a checkbox.
        VpnLanInfo,
        VpnIpv6Info,
        /// Kill switch is a toggle row with an `[Info]` companion;
        /// [`Self::VpnKillSwitchInfo`] is the Info-button focusable,
        /// and this variant drives the toggle.
        VpnKillSwitchToggle,
        VpnLockdownInfo,
        VpnQuantumInfo,
        /// Inline Device-IP-version radio group below the `Device IP
        /// version` row: `( ) Automatic`, `(•) IPv4`, `( ) IPv6`.
        /// Activation routes through the relay constraints
        /// (`wireguard_constraints.ip_version`).
        VpnDeviceIpAuto,
        VpnDeviceIpV4,
        VpnDeviceIpV6,

        // DNS content blockers sub-page (six toggles)
        DnsBlockAds = 0x90,
        DnsBlockTrackers,
        DnsBlockMalware,
        DnsBlockAdultContent,
        DnsBlockGambling,
        DnsBlockSocialMedia,

        // Custom DNS sub-page. Per-address `[Remove]` rows live in a
        // dynamic range - `CUSTOM_DNS_REMOVE_BASE..+CUSTOM_DNS_REMOVE_MAX` -
        // outside the closed enum (same pattern as the `[Remove device]`
        // rows on Manage devices).
        CustomDnsToggle = 0x96,
        CustomDnsAddAddress,

        // Anti-censorship sub-page (one widget per obfuscation mode +
        // a port-edit row that's only focusable when the active mode
        // supports a configurable port).
        AntiCensorshipModeOff = 0x98,
        AntiCensorshipModeAuto,
        AntiCensorshipModeUdp2Tcp,
        AntiCensorshipModeShadowsocks,
        AntiCensorshipModeWireguardPort,
        AntiCensorshipModeQuic,
        AntiCensorshipModeLwo,
        AntiCensorshipPortEdit,

        // Server IP overrides sub-page. Per-row [Remove] buttons live in
        // a dynamic range (see `RELAY_OVERRIDE_REMOVE_*`); the two static
        // footer buttons are here.
        RelayOverrideAdd = 0xA0,
        RelayOverrideClearAll,
    }
    sentinel LastSentinel;
    extra widgets {
        /// Base widget id for per-address `[Remove]` buttons. Each row gets
        /// `CUSTOM_DNS_REMOVE_BASE + index`. Capped at
        /// [`CUSTOM_DNS_REMOVE_MAX`] to keep the range bounded. Derived
        /// from the enum's `LastSentinel` so adding a new settings widget
        /// shifts this range automatically - no hand-picked hex byte to
        /// keep in sync.
        pub const CUSTOM_DNS_REMOVE_BASE: WidgetId =
            WidgetId(SettingsWidget::LastSentinel as u32);
        /// Maximum number of `[Remove]` rows in the focus registry. The
        /// daemon allows more addresses than this; rows beyond the cap are
        /// rendered without `[Remove]` buttons (display-only) until lower
        /// rows are removed and the cap relaxes.
        pub const CUSTOM_DNS_REMOVE_MAX: u32 = 16;
        /// Per-row `[Edit]` button on the Custom DNS sub-page. Lives
        /// adjacent to the `[Remove]` range; together they bound the
        /// custom-DNS row's focusable surface.
        pub const CUSTOM_DNS_EDIT_BASE: WidgetId =
            WidgetId(CUSTOM_DNS_REMOVE_BASE.0 + CUSTOM_DNS_REMOVE_MAX);
        /// Cap on `[Edit]` rows in the focus registry. Shares the
        /// `CUSTOM_DNS_REMOVE_MAX` cap so an `Edit` is reachable for every
        /// `Remove`-able row.
        pub const CUSTOM_DNS_EDIT_MAX: u32 = CUSTOM_DNS_REMOVE_MAX;
        /// Per-row [Enable]/[Disable] toggle on the API access sub-page.
        /// Anchored after the custom-DNS dynamic range so the two don't
        /// alias.
        pub const API_ACCESS_TOGGLE_BASE: WidgetId =
            WidgetId(CUSTOM_DNS_EDIT_BASE.0 + CUSTOM_DNS_EDIT_MAX);
        /// Per-row [Use] (set as active) button on the API access sub-page.
        /// Adjacent to the toggle range; together they bound the modal's
        /// per-method-row focusable surface.
        pub const API_ACCESS_USE_BASE: WidgetId =
            WidgetId(API_ACCESS_TOGGLE_BASE.0 + API_ACCESS_MAX);
        /// Cap on per-method rows (built-ins + custom) the focus registry
        /// will host. Rows beyond this are display-only until lower rows
        /// are removed (custom methods only). 4 built-ins + plenty of
        /// custom slack.
        pub const API_ACCESS_MAX: u32 = 16;
        /// Per-row [Remove] button on the Split tunneling sub-page,
        /// app-list (Win/macOS) variant. Adjacent to the API access ranges.
        pub const SPLIT_TUNNEL_REMOVE_APP_BASE: WidgetId =
            WidgetId(API_ACCESS_USE_BASE.0 + API_ACCESS_MAX);
        /// Per-row [Remove] button on the Split tunneling sub-page, PID-list
        /// (Linux) variant. The two ranges don't overlap because only one
        /// is rendered per platform, but keeping them disjoint avoids
        /// surprises if both ever co-exist (e.g. macOS gains PID support).
        pub const SPLIT_TUNNEL_REMOVE_PID_BASE: WidgetId =
            WidgetId(SPLIT_TUNNEL_REMOVE_APP_BASE.0 + SPLIT_TUNNEL_REMOVE_MAX);
        /// Cap on per-row split-tunnel [Remove] rows in the focus registry.
        /// Rows beyond this render display-only.
        pub const SPLIT_TUNNEL_REMOVE_MAX: u32 = 32;
        /// Per-row [Remove] button on the Server IP overrides sub-page.
        /// Anchored after the split-tunnel PID range so the dynamic
        /// ranges don't alias.
        pub const RELAY_OVERRIDE_REMOVE_BASE: WidgetId =
            WidgetId(SPLIT_TUNNEL_REMOVE_PID_BASE.0 + SPLIT_TUNNEL_REMOVE_MAX);
        /// Cap on per-row [Remove] rows in the focus registry. The
        /// daemon allows more overrides than this; rows beyond the cap
        /// are rendered without `[Remove]` buttons (display-only) until
        /// lower rows are removed and the cap relaxes.
        pub const RELAY_OVERRIDE_REMOVE_MAX: u32 = 32;
    }
}

impl SettingsWidget {
    /// Map a mode-row widget id to the [`SelectedObfuscation`] it
    /// targets (or `None` if the widget isn't a mode-row toggle).
    /// Used by the activation handler so the dispatch is one match
    /// arm per *kind* of widget, not one per mode.
    pub fn anti_censorship_mode(self) -> Option<crate::integration::SelectedObfuscation> {
        use crate::integration::SelectedObfuscation;
        match self {
            Self::AntiCensorshipModeOff => Some(SelectedObfuscation::Off),
            Self::AntiCensorshipModeAuto => Some(SelectedObfuscation::Auto),
            Self::AntiCensorshipModeUdp2Tcp => Some(SelectedObfuscation::Udp2Tcp),
            Self::AntiCensorshipModeShadowsocks => Some(SelectedObfuscation::Shadowsocks),
            Self::AntiCensorshipModeWireguardPort => Some(SelectedObfuscation::WireguardPort),
            Self::AntiCensorshipModeQuic => Some(SelectedObfuscation::Quic),
            Self::AntiCensorshipModeLwo => Some(SelectedObfuscation::Lwo),
            _ => None,
        }
    }

    /// Map a DNS-blocker widget id to its [`DnsBlocker`] enum value
    /// (or `None` if the widget isn't a DNS-blocker toggle). Used by
    /// the activation handler to dispatch to `App::set_dns_blocker`
    /// without re-pattern-matching the widget id.
    pub fn dns_blocker(self) -> Option<crate::app::DnsBlocker> {
        use crate::app::DnsBlocker;
        match self {
            Self::DnsBlockAds => Some(DnsBlocker::Ads),
            Self::DnsBlockTrackers => Some(DnsBlocker::Trackers),
            Self::DnsBlockMalware => Some(DnsBlocker::Malware),
            Self::DnsBlockAdultContent => Some(DnsBlocker::AdultContent),
            Self::DnsBlockGambling => Some(DnsBlocker::Gambling),
            Self::DnsBlockSocialMedia => Some(DnsBlocker::SocialMedia),
            _ => None,
        }
    }
}

/// Decode a `[Remove]` widget id into a 0-based custom-DNS row index.
/// Returns `None` when the id isn't in the remove-row range.
pub fn custom_dns_remove_index(widget: WidgetId) -> Option<usize> {
    let base = widgets::CUSTOM_DNS_REMOVE_BASE.0;
    let id = widget.0;
    if id >= base && id < base + widgets::CUSTOM_DNS_REMOVE_MAX {
        Some((id - base) as usize)
    } else {
        None
    }
}

/// Decode an `[Edit]` widget id into a 0-based custom-DNS row index.
/// Returns `None` when the id isn't in the edit-row range.
pub fn custom_dns_edit_index(widget: WidgetId) -> Option<usize> {
    let base = widgets::CUSTOM_DNS_EDIT_BASE.0;
    let id = widget.0;
    if id >= base && id < base + widgets::CUSTOM_DNS_EDIT_MAX {
        Some((id - base) as usize)
    } else {
        None
    }
}

/// Decode an [Enable]/[Disable] widget id on the API access sub-page
/// into a 0-based access-method row index. `None` when out of range.
pub fn api_access_toggle_index(widget: WidgetId) -> Option<usize> {
    let base = widgets::API_ACCESS_TOGGLE_BASE.0;
    let id = widget.0;
    if id >= base && id < base + widgets::API_ACCESS_MAX {
        Some((id - base) as usize)
    } else {
        None
    }
}

/// Decode a [Use] widget id on the API access sub-page into a 0-based
/// access-method row index. `None` when out of range.
pub fn api_access_use_index(widget: WidgetId) -> Option<usize> {
    let base = widgets::API_ACCESS_USE_BASE.0;
    let id = widget.0;
    if id >= base && id < base + widgets::API_ACCESS_MAX {
        Some((id - base) as usize)
    } else {
        None
    }
}

/// Decode a [Remove] widget id from the split-tunnel app list. `None`
/// outside the reserved range.
pub fn split_tunnel_remove_app_index(widget: WidgetId) -> Option<usize> {
    let base = widgets::SPLIT_TUNNEL_REMOVE_APP_BASE.0;
    let id = widget.0;
    if id >= base && id < base + widgets::SPLIT_TUNNEL_REMOVE_MAX {
        Some((id - base) as usize)
    } else {
        None
    }
}

/// Decode a [Remove] widget id from the split-tunnel PID list.
pub fn split_tunnel_remove_pid_index(widget: WidgetId) -> Option<usize> {
    let base = widgets::SPLIT_TUNNEL_REMOVE_PID_BASE.0;
    let id = widget.0;
    if id >= base && id < base + widgets::SPLIT_TUNNEL_REMOVE_MAX {
        Some((id - base) as usize)
    } else {
        None
    }
}

/// Decode a [Remove] widget id from the Server IP overrides list.
pub fn relay_override_remove_index(widget: WidgetId) -> Option<usize> {
    let base = widgets::RELAY_OVERRIDE_REMOVE_BASE.0;
    let id = widget.0;
    if id >= base && id < base + widgets::RELAY_OVERRIDE_REMOVE_MAX {
        Some((id - base) as usize)
    } else {
        None
    }
}

/// True if `widget` belongs to the Settings family (root menu or any
/// sub-page). The activation handler dispatches every Settings page
/// from one match, so an `is_some` check is all the outer run-loop
/// cascade needs.
pub fn owns_widget(widget: WidgetId) -> bool {
    SettingsWidget::from_widget_id(widget).is_some()
        || custom_dns_remove_index(widget).is_some()
        || custom_dns_edit_index(widget).is_some()
        || api_access_toggle_index(widget).is_some()
        || api_access_use_index(widget).is_some()
        || split_tunnel_remove_app_index(widget).is_some()
        || split_tunnel_remove_pid_index(widget).is_some()
        || relay_override_remove_index(widget).is_some()
}

/// Run the action bound to a focused Settings widget - root menu and
/// all sub-pages (Multihop, DAITA, VPN, DNS blockers, custom DNS,
/// split tunneling, API access, anti-censorship). Caller has already
/// verified ownership via [`owns_widget`].
pub async fn activate<S: MullvadService>(
    app: &mut App,
    service: &S,
    input_mode: &mut InputMode,
    overlay: &mut OverlayMode,
    widget: WidgetId,
) {
    // Custom-DNS `[Remove]` rows live in a dynamic widget-id range
    // outside the closed `SettingsWidget` enum (one id per row, growing
    // with the address list). Handle them first so the activation
    // doesn't fall through to the `from_widget_id` `None` path below.
    if let Some(idx) = custom_dns_remove_index(widget) {
        if let Err(error) = app.remove_custom_dns(service, idx).await {
            app.show_notification(format_action_error("remove custom DNS server", &error));
        }
        return;
    }
    if let Some(idx) = custom_dns_edit_index(widget) {
        // Pre-fill the input with the row's existing address so the
        // user is editing in place rather than retyping. `edit_index`
        // routes the submit through `App::replace_custom_dns`.
        let buffer = app
            .custom_dns_addresses()
            .get(idx)
            .map(|a| a.to_string())
            .unwrap_or_default();
        *input_mode = InputMode::CustomDnsInput(CustomDnsInputState {
            buffer,
            edit_index: Some(idx),
            ..Default::default()
        });
        return;
    }
    if let Some(idx) = api_access_toggle_index(widget) {
        let Some(id) = api_access::method_id_at(app, idx) else {
            return;
        };
        if let Err(error) = app.toggle_access_method(service, id).await {
            app.show_notification(format_action_error("toggle API access method", &error));
        }
        return;
    }
    if let Some(idx) = api_access_use_index(widget) {
        let Some(id) = api_access::method_id_at(app, idx) else {
            return;
        };
        if let Err(error) = app.set_active_access_method(service, id).await {
            app.show_notification(format_action_error("set active API access method", &error));
        }
        return;
    }
    if let Some(idx) = split_tunnel_remove_app_index(widget) {
        let Some(path) = split_tunnel_app_at(app, idx) else {
            return;
        };
        if let Err(error) = app.remove_split_tunnel_app(service, path).await {
            app.show_notification(format_action_error("remove split-tunnel app", &error));
        }
        return;
    }
    if let Some(idx) = split_tunnel_remove_pid_index(widget) {
        let Some(pid) = split_tunnel_pid_at(app, idx) else {
            return;
        };
        if let Err(error) = app.remove_split_tunnel_process(service, pid).await {
            app.show_notification(format_action_error("remove split-tunnel PID", &error));
        }
        return;
    }
    if let Some(idx) = relay_override_remove_index(widget) {
        // Per-row `[Remove]` on the Server IP overrides sub-page.
        // Looks up the hostname at activation time so the cached
        // ordering at render time and dispatch time agree.
        let Some(hostname) = app.relay_overrides().get(idx).map(|ov| ov.hostname.clone()) else {
            return;
        };
        let return_focus = app.page_focus().focused;
        *overlay = OverlayMode::Confirm {
            title: "Remove server IP override".to_string(),
            message: format!(
                "Remove the IPv4/IPv6 in-address override for `{hostname}`? The daemon will fall back to the upstream-published address."
            ),
            action: ConfirmAction::RemoveRelayOverride { hostname },
            return_focus,
        };
        return;
    }
    let Some(widget) = SettingsWidget::from_widget_id(widget) else {
        return;
    };
    match widget {
        // ---- Root menu ----
        SettingsWidget::Daita => app.enter_sub_page(crate::app::PageId::SettingsDaita),
        SettingsWidget::Multihop => app.enter_sub_page(crate::app::PageId::SettingsMultihop),
        SettingsWidget::VpnSettings => app.enter_sub_page(crate::app::PageId::SettingsVpn),
        SettingsWidget::ApiAccess => {
            app.enter_sub_page(crate::app::PageId::SettingsApiAccess);
            // The daemon doesn't push changes for "current api access
            // method"; pull it once on entry so the `*` marker is
            // accurate on the very first frame.
            if let Err(error) = app.refresh_current_access_method(service).await {
                tracing::warn!(
                    "Failed to fetch current API access method: {error}. The active-method `*` marker may be wrong."
                );
            }
        }
        SettingsWidget::SplitTunneling => {
            app.enter_sub_page(crate::app::PageId::SettingsSplitTunnel);
            // PIDs aren't push-driven on Linux, so fetch once on entry.
            // Non-Linux daemons return an error; the renderer hides the
            // PID list anyway and the desktop app-list comes from the
            // already-cached `Settings.split_tunnel.apps`.
            if cfg!(target_os = "linux")
                && let Err(error) = app.refresh_split_tunnel_pids(service).await
            {
                tracing::warn!(
                    "Failed to fetch split-tunnel PID list: {error}. The Linux PID list will be empty."
                );
            }
        }
        SettingsWidget::DisconnectAndQuit => {
            // Only fire the disconnect RPC when there's actually a
            // tunnel (or firewall) to tear down - otherwise the button
            // is the plain `[Quit]` rendered by `disconnect_and_quit_label`
            // and the RPC would be a no-op round trip.
            if !matches!(app.connection_status(), Some(TunnelState::Disconnected { .. }) | None)
                && let Err(error) = app.disconnect(service).await
            {
                tracing::warn!("Disconnect on quit failed: {error}");
            }
            app.quit();
        }

        // ---- Multihop sub-page ----
        SettingsWidget::MultihopToggle => {
            if let Err(error) = app.toggle_multihop(service).await {
                app.show_notification(format_action_error("multihop toggle", &error));
            }
        }

        // ---- DAITA sub-page ----
        SettingsWidget::DaitaToggle => {
            if let Err(error) = app.toggle_daita_enabled(service).await {
                app.show_notification(format_action_error("DAITA toggle", &error));
            }
        }
        SettingsWidget::DaitaDirectOnlyToggle => {
            if let Err(error) = app.toggle_daita_direct_only(service).await {
                app.show_notification(format_action_error("DAITA Direct only toggle", &error));
            }
        }
        SettingsWidget::DaitaBlurbPrev => {
            app.settings_page_state_mut().step_daita_blurb_page(-1);
        }
        SettingsWidget::DaitaBlurbNext => {
            app.settings_page_state_mut().step_daita_blurb_page(1);
        }

        // ---- VPN settings sub-page ----
        SettingsWidget::VpnAutoConnectToggle => {
            if let Err(error) = app.toggle_auto_connect(service).await {
                app.show_notification(format_action_error("auto-connect toggle", &error));
            }
        }
        SettingsWidget::VpnLanToggle => {
            if let Err(error) = app.toggle_lan(service).await {
                app.show_notification(format_action_error("LAN toggle", &error));
            }
        }
        SettingsWidget::VpnIpv6Toggle => {
            if let Err(error) = app.toggle_ipv6(service).await {
                app.show_notification(format_action_error("IPv6 toggle", &error));
            }
        }
        SettingsWidget::VpnLockdownToggle => {
            // Lockdown toggling routes through a confirmation overlay
            // (irreversible-firewall-block warning) rather than
            // surprising the user with a one-click flip.
            let return_focus = app.page_focus().focused;
            *overlay = OverlayMode::Confirm {
                title: "Confirm lockdown mode update".to_string(),
                message: "Attention: enabling this will always require a Mullvad VPN connection in order to reach the internet. The app's built-in kill switch is always on. This setting will additionally block the internet if clicking Disconnect or Quit."
                    .to_string(),
                action: ConfirmAction::ToggleLockdown,
                return_focus,
            };
        }
        SettingsWidget::VpnQuantumToggle => {
            if let Err(error) = app.toggle_quantum_resistant(service).await {
                app.show_notification(format_action_error("quantum-resistant toggle", &error));
            }
        }
        SettingsWidget::VpnAntiCensorshipEdit => {
            app.enter_sub_page(crate::app::PageId::SettingsAntiCensorship);
        }
        SettingsWidget::VpnMtuEdit => {
            // Inline text input - the run loop's MTU keystroke block in
            // `tui::handle_key_event` handles digits / Backspace / Enter
            // directly. Mouse clicks just set focus (handled upstream);
            // `Activate` arriving here means a focus-engine activation
            // (e.g. mouse) on a field that's already focused - nothing
            // to do beyond leaving focus where it is.
        }
        SettingsWidget::VpnDnsBlockersInfo => {
            // Naming kept as `VpnDnsBlockersInfo` (the discriminant is on
            // the wire as a stable widget id) but this is now a real
            // navigation row, not the old `[Info]` placeholder.
            app.enter_sub_page(crate::app::PageId::SettingsDnsBlockers);
        }
        SettingsWidget::VpnCustomDnsInfo => {
            // Discriminant name preserved for wire stability; this is
            // now a real navigation row (see `[Manage]` in VPN settings),
            // not the old `[Info]` placeholder.
            app.enter_sub_page(crate::app::PageId::SettingsCustomDns);
        }
        SettingsWidget::VpnKillSwitchInfo => app.show_notification(
            indoc! {"
                This built-in feature prevents your traffic from leaking outside of the VPN tunnel if your network suddenly stops working or if the tunnel fails, it does this by blocking your traffic until your connection is reestablished.

                The difference between the Kill Switch and Lockdown Mode is that the Kill Switch will prevent any leaks from happening during automatic tunnel reconnects, software crashes and similar accidents. With Lockdown Mode enabled, you must be connected to a Mullvad VPN server to be able to reach the internet. Manually disconnecting or quitting the app will block your connection.
            "},
        ),
        SettingsWidget::VpnDeviceIpInfo => app.show_notification(
            indoc! {"
                This feature allows you to choose whether to use only IPv4, only IPv6, or allow the app to automatically decide the best option when connecting to a server.

                It can be useful when you are aware of problems caused by a certain IP version.
            "},
        ),
        SettingsWidget::VpnServerIpInfo => {
            // Discriminant name preserved for wire stability (it was an
            // [Info] placeholder when first added); the row now navigates
            // into the Server IP overrides sub-page.
            app.enter_sub_page(crate::app::PageId::SettingsRelayOverrides);
        }

        // Per-toggle [Info] companions. Each surfaces a short
        // explanation of the paired setting until a help-overlay
        // system lands. Auto-connect is intentionally absent.
        SettingsWidget::VpnLanInfo => app.show_notification(
            indoc! {"
                This feature allows access to other devices on the local network, such as for sharing, printing, streaming, etc.

                It does this by allowing network communication outside the tunnel to local multicast and broadcast ranges as well as to and from these private IP ranges:

                - 10.0.0.0/8
                - 172.16.0.0/12
                - 192.168.0.0/16
                - 169.254.0.0/16
                - fe80::/10
                - fc00::/7
            "},
        ),
        SettingsWidget::VpnIpv6Info => app.show_notification(
            indoc! {"
                When this feature is enabled, IPv6 can be used alongside IPv4 in the VPN tunnel to communicate with internet services.

                IPv4 is always enabled and the majority of websites and applications use this protocol. We do not recommend enabling IPv6 unless you know you need it.
            "},
        ),
        SettingsWidget::VpnLockdownInfo => app.show_notification(
            indoc! {"
                The difference between the Kill Switch and Lockdown Mode is that the Kill Switch will prevent any leaks from happening during automatic tunnel reconnects, software crashes and similar accidents.

                With Lockdown Mode enabled, you must be connected to a Mullvad VPN server to be able to reach the internet. Manually disconnecting or quitting the app will block your connection.
            "},
        ),
        SettingsWidget::VpnQuantumInfo => app.show_notification(
            indoc! {"
                This feature makes the WireGuard tunnel resistant to potential attacks from quantum computers.

                It does this by performing an extra key exchange using a quantum safe algorithm and mixing the result into WireGuard's regular encryption.
            "},
        ),
        SettingsWidget::VpnKillSwitchToggle => app.show_notification(
            indoc! {"
                This built-in feature prevents your traffic from leaking outside of the VPN tunnel if your network suddenly stops working or if the tunnel fails, it does this by blocking your traffic until your connection is reestablished.

                The difference between the Kill Switch and Lockdown Mode is that the Kill Switch will prevent any leaks from happening during automatic tunnel reconnects, software crashes and similar accidents. With Lockdown Mode enabled, you must be connected to a Mullvad VPN server to be able to reach the internet. Manually disconnecting or quitting the app will block your connection.
            "},
        ),

        // Device IP version radio group. Dispatches to
        // `App::set_ip_version_preference`; the rendered radio
        // reflects `wireguard_constraints.ip_version`.
        SettingsWidget::VpnDeviceIpAuto => {
            if let Err(error) = app.set_ip_version_preference(service, None).await {
                app.show_notification(format_action_error("device IP version", &error));
            }
        }
        SettingsWidget::VpnDeviceIpV4 => {
            if let Err(error) = app
                .set_ip_version_preference(service, Some(talpid_types::net::IpVersion::V4))
                .await
            {
                app.show_notification(format_action_error("device IP version", &error));
            }
        }
        SettingsWidget::VpnDeviceIpV6 => {
            if let Err(error) = app
                .set_ip_version_preference(service, Some(talpid_types::net::IpVersion::V6))
                .await
            {
                app.show_notification(format_action_error("device IP version", &error));
            }
        }

        // ---- DNS content blockers sub-page ----
        SettingsWidget::DnsBlockAds
        | SettingsWidget::DnsBlockTrackers
        | SettingsWidget::DnsBlockMalware
        | SettingsWidget::DnsBlockAdultContent
        | SettingsWidget::DnsBlockGambling
        | SettingsWidget::DnsBlockSocialMedia => {
            // `widget.dns_blocker()` returns `Some` for exactly these
            // six discriminants, so the unwrap is structurally safe.
            let Some(blocker) = widget.dns_blocker() else {
                return;
            };
            let next = !app.dns_blocker_enabled(blocker);
            if let Err(error) = app.set_dns_blocker(service, blocker, next).await {
                app.show_notification(format_action_error("DNS content blocker toggle", &error));
            }
        }

        // ---- Custom DNS sub-page ----
        SettingsWidget::CustomDnsToggle => {
            if let Err(error) = app.toggle_custom_dns(service).await {
                app.show_notification(format_action_error("custom DNS toggle", &error));
            }
        }
        SettingsWidget::CustomDnsAddAddress => {
            *input_mode = InputMode::CustomDnsInput(CustomDnsInputState::default());
        }

        // ---- Split tunneling sub-page ----
        SettingsWidget::SplitTunnelToggle => {
            if let Err(error) = app.toggle_split_tunnel(service).await {
                app.show_notification(format_action_error("split-tunnel toggle", &error));
            }
        }
        SettingsWidget::SplitTunnelAddApp => {
            *input_mode = InputMode::SplitTunnelPathInput(SplitTunnelPathInputState::default());
        }
        SettingsWidget::SplitTunnelAddPid => {
            *input_mode = InputMode::SplitTunnelPidInput(SplitTunnelPidInputState::default());
        }

        // ---- Anti-censorship sub-page ----
        SettingsWidget::AntiCensorshipModeOff
        | SettingsWidget::AntiCensorshipModeAuto
        | SettingsWidget::AntiCensorshipModeUdp2Tcp
        | SettingsWidget::AntiCensorshipModeShadowsocks
        | SettingsWidget::AntiCensorshipModeWireguardPort
        | SettingsWidget::AntiCensorshipModeQuic
        | SettingsWidget::AntiCensorshipModeLwo => {
            // `widget.anti_censorship_mode()` returns `Some` for
            // exactly these seven discriminants, so the unwrap is
            // structurally safe.
            let Some(mode) = widget.anti_censorship_mode() else {
                return;
            };
            if let Err(error) = app.set_anti_censorship_mode(service, mode).await {
                app.show_notification(format_action_error("anti-censorship mode", &error));
            }
        }
        SettingsWidget::AntiCensorshipPortEdit => {
            // Open the port input modal targeting the currently-
            // active mode. Renderer only registers this widget when
            // the active mode supports a port, so a stale activation
            // (mode flipped to Off between render and key) defaults
            // to Auto and the modal validates on submit.
            let mode = app
                .settings()
                .map(|s| s.obfuscation_settings.selected_obfuscation)
                .unwrap_or(SelectedObfuscation::Auto);
            if mode_has_configurable_port(mode) {
                *input_mode = InputMode::PortInput(PortInputState {
                    mode,
                    buffer: String::new(),
                    ..Default::default()
                });
            }
        }
        // ---- Server IP overrides sub-page ----
        SettingsWidget::RelayOverrideAdd => {
            *input_mode = InputMode::RelayOverrideInput(
                crate::tui::modals::relay_override::RelayOverrideInputState::default(),
            );
        }
        SettingsWidget::RelayOverrideClearAll => {
            let return_focus = app.page_focus().focused;
            *overlay = OverlayMode::Confirm {
                title: "Clear all server IP overrides".to_string(),
                message: "Wipe every per-relay IPv4/IPv6 override? The daemon will fall back to the upstream-published addresses for every relay."
                    .to_string(),
                action: ConfirmAction::ClearRelayOverrides,
                return_focus,
            };
        }
        // Sentinel - never registered as a focusable widget; only
        // exists to anchor `widgets::CUSTOM_DNS_REMOVE_BASE`.
        SettingsWidget::LastSentinel => {}
    }
}

/// Render the Settings root menu. Five sub-page links (DAITA and
/// Multihop carry an `Off`/`On` summary; the rest are plain
/// `[Label] >`) sit above a non-focusable App info block (repo URL
/// plus TUI / daemon versions) and a centered danger
/// `[Disconnect & quit]` anchored at the bottom of the body.
///
/// Language / Support / App info are intentionally omitted as menu
/// entries; their dispatchers in `tui/mod.rs` show a "not yet
/// implemented" notification if activated via any leftover keyboard
/// path. The info that would have come from `[App info]` is rendered
/// inline at the bottom instead.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;

    let multihop_state = on_off_label(app.is_multihop_enabled());
    let daita_state = on_off_label(
        app.settings()
            .is_some_and(|s| s.tunnel_options.wireguard.daita.enabled),
    );

    let [
        daita_row,
        multihop_row,
        vpn_row,
        split_row,
        api_row,
        _spacer,
        app_info_area,
        _info_spacer,
        disconnect_row,
    ] = Layout::vertical([
        Constraint::Length(1),               // [DAITA]      Off >
        Constraint::Length(1),               // [Multihop]   Off >
        Constraint::Length(1),               // [VPN settings] >
        Constraint::Length(1),               // [Split tunneling] >
        Constraint::Length(1),               // [API access] >
        Constraint::Min(1),                  // spacer
        Constraint::Length(APP_INFO_HEIGHT), // App info block
        Constraint::Length(1),               // separator above disconnect
        Constraint::Length(1),               // centered danger [Disconnect & quit]
    ])
    .areas(area);

    render_menu_button(
        frame,
        daita_row,
        "DAITA",
        Some(daita_state),
        widgets::DAITA,
        focused,
        registry,
    );
    render_menu_button(
        frame,
        multihop_row,
        "Multihop",
        Some(multihop_state),
        widgets::MULTIHOP,
        focused,
        registry,
    );
    render_menu_button(
        frame,
        vpn_row,
        "VPN settings",
        None,
        widgets::VPN_SETTINGS,
        focused,
        registry,
    );
    render_menu_button(
        frame,
        split_row,
        "Split tunneling",
        None,
        widgets::SPLIT_TUNNELING,
        focused,
        registry,
    );
    render_menu_button(
        frame,
        api_row,
        "API access",
        None,
        widgets::API_ACCESS,
        focused,
        registry,
    );

    render_app_info(frame, app_info_area, app);

    components::render_centered_button_danger(
        frame,
        disconnect_row,
        disconnect_and_quit_label(app.connection_status()),
        widgets::DISCONNECT_AND_QUIT,
        focused,
        registry,
    );
}

/// Label for the bottom-of-Settings danger button. When there's no
/// active tunnel to tear down (Disconnected, or no daemon state yet)
/// the label collapses to plain `Quit` so the button stops advertising
/// a side effect that wouldn't actually fire. Every other state -
/// Connected, Connecting, Disconnecting, Error - keeps the full
/// `Disconnect & quit` so the user knows quitting will also drop the
/// in-flight tunnel.
fn disconnect_and_quit_label(state: Option<&TunnelState>) -> &'static str {
    match state {
        Some(TunnelState::Disconnected { .. }) | None => "Quit",
        _ => "Disconnect & quit",
    }
}

/// Number of rows the App info block occupies: one header, one URL row,
/// and one row each for the TUI version, the targeted daemon version, and
/// the running daemon version.
const APP_INFO_HEIGHT: u16 = 5;

/// Repository URL surfaced in the App info block. Static; no need to
/// pull from `Cargo.toml` metadata.
const REPO_URL: &str = "https://github.com/d10n/mullvad-tui";

/// Render the non-focusable App info block at the bottom of the
/// Settings root: a header line, the GitHub repo URL, and three
/// right-aligned version rows. `mullvad-tui` is this binary's own
/// version; `Targeted daemon` is the upstream daemon release this build
/// was compiled against (`mullvad_version::VERSION`, the pinned
/// `mullvadvpn-app` submodule); `mullvad-daemon` is the version the
/// connected daemon reports and shows `-` until the first `resync`
/// populates it. When the last two disagree the daemon is a different
/// release than the one the TUI targets, which is the same mismatch the
/// startup warning in `tui::run` flags.
fn render_app_info(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let tui_version = env!("CARGO_PKG_VERSION");
    let targeted_daemon_version = mullvad_version::VERSION;
    let daemon_version = app.daemon_version().unwrap_or("-");

    let [header_row, url_row, tui_row, targeted_row, daemon_row] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new("App info"), header_row);
    frame.render_widget(Paragraph::new(REPO_URL), url_row);
    render_app_info_value_row(frame, tui_row, "mullvad-tui", tui_version);
    render_app_info_value_row(
        frame,
        targeted_row,
        "Targeted daemon",
        targeted_daemon_version,
    );
    render_app_info_value_row(frame, daemon_row, "mullvad-daemon", daemon_version);
}

/// One label/value row in the App info block. Label hugs the left and
/// `value` is right-aligned in `area`. Mirrors the layout of
/// `render_menu_button`'s right-value slot but without the focusable
/// `[label]` button - these rows are display-only.
fn render_app_info_value_row(frame: &mut Frame<'_>, area: Rect, label: &str, value: &str) {
    let label_width = label.len() as u16;
    let value_width = value.len() as u16;

    let label_area = Rect::new(area.x, area.y, label_width.min(area.width), 1);
    frame.render_widget(Paragraph::new(label.to_string()), label_area);

    if area.width > label_width + value_width + 1 {
        let value_area = Rect::new(area.x + area.width - value_width, area.y, value_width, 1);
        frame.render_widget(Paragraph::new(value.to_string()), value_area);
    }
}

/// Render the Multihop sub-page: descriptive blurb + a
/// `Status: <on|off>  [x]/[ ]` checkbox-style toggle. Wires
/// `App::toggle_multihop`.
pub fn render_multihop(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let enabled = app.is_multihop_enabled();
    let toggle_label = checkbox_label(enabled);

    let description = vec![Line::from(
        "Multihop routes your traffic into one WireGuard server and out another, making it harder to trace. This results in increased latency but increases anonymity online.",
    )];
    let description_height = Paragraph::new(description.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width) as u16;

    let [
        title,
        _spacer1,
        description_area,
        _spacer2,
        toggle_row,
        _spacer3,
    ] = Layout::vertical([
        Constraint::Length(1), // Title
        Constraint::Length(1), // Spacer
        Constraint::Length(description_height),
        Constraint::Min(1),    // Spacer
        Constraint::Length(1), // Toggle row
        Constraint::Min(1),    // Spacer
    ])
    .areas(area);

    frame.render_widget(Paragraph::new("Multihop"), title);
    frame.render_widget(
        Paragraph::new(description).wrap(Wrap { trim: false }),
        description_area,
    );
    render_status_toggle_row(
        frame,
        toggle_row,
        enabled,
        toggle_label,
        widgets::MULTIHOP_TOGGLE,
        focused,
        registry,
    );
}

/// Render the DAITA sub-page: descriptive blurb at the top, two
/// status-toggle rows anchored at the bottom (Status, Direct only).
pub fn render_daita(frame: &mut Frame<'_>, area: Rect, app: &App, registry: &mut FocusRegistry) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let settings = app.settings();
    let enabled = settings.is_some_and(|s| s.tunnel_options.wireguard.daita.enabled);
    // "Direct only" is the inverse of `use_multihop_if_necessary`.
    let direct_only =
        settings.is_some_and(|s| !s.tunnel_options.wireguard.daita.use_multihop_if_necessary);
    let toggle_label = |on: bool| checkbox_label(on);

    let blurbs: [Vec<Line<'static>>; crate::app::pages::settings::DAITA_BLURB_PAGES] = [
        vec![
            Line::from(
                "Attention: This increases network traffic and will also negatively affect speed, latency, and battery usage. Use with caution on limited plans.",
            ),
            Line::from(""),
            Line::from(
                "DAITA (Defense against AI-guided Traffic Analysis) hides patterns in your encrypted VPN traffic.",
            ),
            Line::from(""),
            Line::from(
                "By using sophisticated AI it's possible to analyze the traffic of data packets going in and out of your device (even if the traffic is encrypted).",
            ),
        ],
        vec![
            Line::from(
                "If an observer monitors these data packets, DAITA makes it significantly harder for them to identify which websites you are visiting or with whom you are communicating.",
            ),
            Line::from(""),
            Line::from(
                "DAITA does this by carefully adding network noise and making all network packets the same size.",
            ),
            Line::from(""),
            Line::from(
                "Not all our servers are DAITA-enabled. Therefore, we use multihop automatically to enable DAITA with any server.",
            ),
        ],
    ];

    // Reserve a single fixed slot tall enough for the longest page so
    // the toggle rows below don't shift when the user steps between
    // pages.
    let blurb_height = blurbs
        .iter()
        .map(|lines| {
            Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .line_count(area.width) as u16
        })
        .max()
        .unwrap_or(1);

    let page_idx = app
        .settings_page_state()
        .daita_blurb_page()
        .min(blurbs.len() - 1);

    let [
        title,
        _spacer1,
        blurb_area,
        _spacer2,
        pager_row,
        _spacer3,
        status_row,
        direct_row,
        _spacer4,
    ] = Layout::vertical([
        Constraint::Length(1),            // Title
        Constraint::Length(1),            // Spacer
        Constraint::Length(blurb_height), // Blurb body
        Constraint::Length(1),            // Spacer
        Constraint::Length(1),            // Pager row: [<]  Page X/N  [>]
        Constraint::Min(1),               // Flex spacer pushing toggles to bottom
        Constraint::Length(1),            // Status row
        Constraint::Length(1),            // Direct only row
        Constraint::Min(1),               // Flex spacer
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(Line::from("DAITA")), title);
    frame.render_widget(
        Paragraph::new(blurbs[page_idx].clone()).wrap(Wrap { trim: false }),
        blurb_area,
    );
    render_blurb_pager(frame, pager_row, page_idx, blurbs.len(), focused, registry);
    render_status_toggle_row(
        frame,
        status_row,
        enabled,
        toggle_label(enabled),
        widgets::DAITA_TOGGLE,
        focused,
        registry,
    );
    render_direct_only_row(
        frame,
        direct_row,
        direct_only,
        toggle_label(direct_only),
        focused,
        registry,
    );
}

/// `[<]  Page X/N  [>]` pager row centered under the DAITA blurb.
/// Buttons sit at the column edges, indicator is centered. The `[<]`
/// button is registered as focusable only when `page > 0`, and `[>]`
/// only when `page < total - 1`, so the focus engine can't land on a
/// button that does nothing.
fn render_blurb_pager(
    frame: &mut Frame<'_>,
    area: Rect,
    page: usize,
    total: usize,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    const BTN_WIDTH: u16 = 3; // "[<]"
    let [_pad1, prev_area, indicator_area, next_area, _pad2] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(BTN_WIDTH),
        Constraint::Min(1),
        Constraint::Length(BTN_WIDTH),
        Constraint::Min(1),
    ])
    .areas(area);

    if page > 0 {
        components::render_button(
            frame,
            prev_area,
            "<",
            focused == Some(widgets::DAITA_BLURB_PREV),
            registry,
            widgets::DAITA_BLURB_PREV,
        );
    }
    let indicator = format!("Page {}/{}", page + 1, total);
    let indicator_x =
        indicator_area.x + indicator_area.width.saturating_sub(indicator.len() as u16) / 2;
    let indicator_rect = Rect::new(indicator_x, indicator_area.y, indicator.len() as u16, 1);
    frame.render_widget(Paragraph::new(indicator), indicator_rect);
    if page + 1 < total {
        components::render_button(
            frame,
            next_area,
            ">",
            focused == Some(widgets::DAITA_BLURB_NEXT),
            registry,
            widgets::DAITA_BLURB_NEXT,
        );
    }
    // Close the pager row so the Status / Direct-only toggle rows below
    // each get their own focus row instead of being merged with the
    // pager (which would let Down skip past Status).
    registry.end_row();
}

/// `Direct only: Off         [Info]   [Enable]` row. Two side-by-side
/// trailing buttons; only the toggle is focusable (the [Info] button
/// is decorative until the help-text overlay system lands).
fn render_direct_only_row(
    frame: &mut Frame<'_>,
    area: Rect,
    direct_only: bool,
    toggle_label: &str,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    components::render_label_button_row(
        frame,
        area,
        format!("Direct only: {}", on_off_label(direct_only)),
        toggle_label,
        widgets::DAITA_DIRECT_ONLY_TOGGLE,
        focused,
        registry,
    );
}

/// Render the VPN settings sub-page. One row per configurable item;
/// each row has a label, optional `[Info]` button (placeholder until
/// help-text overlays land), and a trailing action button (toggle or
/// `[Edit]`). Wired toggles: LAN, IPv6, Lockdown, Quantum-resistant.
/// Wired editors: Anti-censorship (cycles + port input), MTU (text
/// input). Placeholders surface a notification: DNS content blockers,
/// custom DNS, kill switch (always on), device IP version, server IP
/// override.
pub fn render_vpn_settings(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    registry: &mut FocusRegistry,
) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;

    // A flat sequence of single-row entries. Each row's shape is
    // captured by a `VpnRow` variant; adding a new VPN setting is
    // "extend [`vpn_rows`] and pick a variant" - no constraint-index
    // bookkeeping.
    let rows = vpn_rows(app);
    let mut constraints: Vec<Constraint> = rows.iter().map(|_| Constraint::Length(1)).collect();
    constraints.push(Constraint::Min(0)); // bottom spacer
    let chunks = Layout::vertical(constraints).split(area);

    for (slot, row) in chunks.iter().zip(&rows) {
        render_vpn_row(frame, *slot, row, focused, registry);
    }
}

/// Per-row description for [`render_vpn_settings`]. The renderer
/// dispatches on the variant rather than indexing into a constraint
/// table.
enum VpnRow {
    /// `<label>     [Info] [x]/[ ]` - focusable Info link (optional;
    /// `Auto-connect` has none) plus a focusable checkbox glyph.
    Toggle {
        label: &'static str,
        enabled: bool,
        info_id: Option<WidgetId>,
        toggle_id: WidgetId,
    },
    /// `<label>  <button_text>` where `button_text` is rendered as a
    /// focusable bracketed button (`>`, `<value> >`, `Info`, etc.).
    /// Covers DNS blockers, custom DNS, anti-censorship, the
    /// Device-IP-version Info pill, and Server IP override.
    Button {
        label: &'static str,
        button_text: String,
        button_id: WidgetId,
    },
    /// Indented radio row: `  ( ) <label>` / `  (•) <label>` -
    /// whole-row focusable. Used for the Device-IP-version group.
    Radio {
        label: &'static str,
        selected: bool,
        id: WidgetId,
    },
    /// `<label>  <input>` where `<input>` is a focusable inline text
    /// pill (dark bg, placeholder when empty, cursor block when focused).
    /// Mirrors the search-anchor pattern on `Status > Select location`.
    /// Today only the WireGuard MTU row uses this; if other settings
    /// adopt the inline-edit pattern they can share the same variant.
    Input {
        label: &'static str,
        buffer: String,
        placeholder: &'static str,
        id: WidgetId,
    },
    /// Display-only dimmed text (the MTU `Valid range: 1280-1420.` hint).
    Hint(String),
}

fn vpn_rows(app: &App) -> Vec<VpnRow> {
    let settings = app.settings();
    let auto_connect = settings.is_some_and(|s| s.auto_connect);
    let lan = settings.is_some_and(|s| s.allow_lan);
    let ipv6 = settings.is_some_and(|s| s.tunnel_options.generic.enable_ipv6);
    let lockdown = settings.is_some_and(|s| s.lockdown_mode);
    let quantum = settings.is_some_and(|s| s.tunnel_options.wireguard.quantum_resistant.enabled());
    let mtu = settings.and_then(|s| s.tunnel_options.wireguard.mtu);
    // Inline MTU pill is a text input with persistent draft. Sync the
    // buffer from the daemon every render *unless* the field is the
    // current focus - once focused, the user's draft owns the field
    // until they defocus and the next sync picks up the daemon value
    // again. Mirrors the search-anchor pattern on Select location, but
    // with an explicit sync because (unlike search) there's a daemon
    // value to display when the field is idle.
    let focused = app.page_focus().focused;
    if focused != Some(widgets::VPN_MTU_EDIT) {
        app.settings_page_state().sync_mtu_buffer_from_daemon(mtu);
    }
    let mtu_buffer = app.settings_page_state().mtu_buffer();
    let obfuscation_mode = settings
        .map(|s| format!("{}", s.obfuscation_settings.selected_obfuscation))
        .unwrap_or_else(|| "unknown".to_string());
    // Right-aligned value for the custom-DNS row: `Off` when DNS is set
    // to default (regardless of staged addresses), `<n> server(s)` when
    // custom is on, and a hyphen until settings load. The address-count
    // phrasing matches the sub-page's "Servers:" label so the two views
    // read consistently.
    let custom_dns_summary = if app.settings().is_none() {
        "-".to_string()
    } else if !app.custom_dns_enabled() {
        "Off".to_string()
    } else {
        match app.custom_dns_addresses().len() {
            0 => "On (no servers)".to_string(),
            1 => "1 server".to_string(),
            n => format!("{n} servers"),
        }
    };
    let device_ip_pref = app.current_ip_version_preference();

    vec![
        VpnRow::Toggle {
            label: "Auto-connect",
            enabled: auto_connect,
            info_id: None,
            toggle_id: widgets::VPN_AUTO_CONNECT_TOGGLE,
        },
        VpnRow::Toggle {
            label: "Local network sharing",
            enabled: lan,
            info_id: Some(widgets::VPN_LAN_INFO),
            toggle_id: widgets::VPN_LAN_TOGGLE,
        },
        VpnRow::Button {
            label: "DNS content blockers",
            button_text: ">".to_string(),
            button_id: widgets::VPN_DNS_BLOCKERS_INFO,
        },
        VpnRow::Button {
            label: "Use custom DNS server",
            button_text: format!("{custom_dns_summary} >"),
            button_id: widgets::VPN_CUSTOM_DNS_INFO,
        },
        VpnRow::Toggle {
            label: "In-tunnel IPv6",
            enabled: ipv6,
            info_id: Some(widgets::VPN_IPV6_INFO),
            toggle_id: widgets::VPN_IPV6_TOGGLE,
        },
        // Kill switch is daemon-managed and always on; rendering it as
        // a toggle keeps the visual shape consistent. The dispatch
        // surfaces a notification explaining the glyph can't be flipped.
        VpnRow::Toggle {
            label: "Kill switch",
            enabled: true,
            info_id: Some(widgets::VPN_KILL_SWITCH_INFO),
            toggle_id: widgets::VPN_KILL_SWITCH_TOGGLE,
        },
        VpnRow::Toggle {
            label: "Lockdown mode",
            enabled: lockdown,
            info_id: Some(widgets::VPN_LOCKDOWN_INFO),
            toggle_id: widgets::VPN_LOCKDOWN_TOGGLE,
        },
        VpnRow::Button {
            label: "Anti-censorship",
            button_text: format!("{obfuscation_mode} >"),
            button_id: widgets::VPN_ANTI_CENSORSHIP_EDIT,
        },
        VpnRow::Toggle {
            label: "Quantum-resistant tunnel",
            enabled: quantum,
            info_id: Some(widgets::VPN_QUANTUM_INFO),
            toggle_id: widgets::VPN_QUANTUM_TOGGLE,
        },
        VpnRow::Button {
            label: "Device IP version",
            button_text: "Info".to_string(),
            button_id: widgets::VPN_DEVICE_IP_INFO,
        },
        VpnRow::Radio {
            label: "Automatic",
            selected: device_ip_pref.is_none(),
            id: widgets::VPN_DEVICE_IP_AUTO,
        },
        VpnRow::Radio {
            label: "IPv4",
            selected: device_ip_pref == Some(talpid_types::net::IpVersion::V4),
            id: widgets::VPN_DEVICE_IP_V4,
        },
        VpnRow::Radio {
            label: "IPv6",
            selected: device_ip_pref == Some(talpid_types::net::IpVersion::V6),
            id: widgets::VPN_DEVICE_IP_V6,
        },
        VpnRow::Input {
            label: "MTU",
            buffer: mtu_buffer,
            placeholder: "Default",
            id: widgets::VPN_MTU_EDIT,
        },
        VpnRow::Hint(format!(
            "Valid range: {}-{}.",
            crate::app::WIREGUARD_MTU_RANGE.start(),
            crate::app::WIREGUARD_MTU_RANGE.end()
        )),
        VpnRow::Button {
            label: "Server IP override",
            button_text: ">".to_string(),
            button_id: widgets::VPN_SERVER_IP_INFO,
        },
    ]
}

fn render_vpn_row(
    frame: &mut Frame<'_>,
    area: Rect,
    row: &VpnRow,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    match row {
        VpnRow::Toggle {
            label,
            enabled,
            info_id,
            toggle_id,
        } => render_vpn_toggle_row(
            frame, area, label, *enabled, *info_id, *toggle_id, focused, registry,
        ),
        VpnRow::Button {
            label,
            button_text,
            button_id,
        } => render_vpn_button_row(
            frame,
            area,
            label,
            button_text,
            *button_id,
            focused,
            registry,
        ),
        VpnRow::Radio {
            label,
            selected,
            id,
        } => render_radio_subrow(frame, area, label, *selected, *id, focused, registry),
        VpnRow::Input {
            label,
            buffer,
            placeholder,
            id,
        } => render_vpn_input_row(
            frame,
            area,
            label,
            buffer,
            placeholder,
            *id,
            focused,
            registry,
        ),
        VpnRow::Hint(text) => frame.render_widget(
            Paragraph::new(text.clone()).style(ratatui::style::Style::new().dark_gray()),
            area,
        ),
    }
}

/// VPN toggle row: `<label>     [Info] <glyph>`. `<glyph>` is the
/// raw `[x]` / `[ ]` checkbox text (a focusable on its own); the
/// optional `[Info]` companion is a sibling focusable. `Auto-connect`
/// has no Info button - pass `None` for `info_id` there.
#[expect(
    clippy::too_many_arguments,
    reason = "row helper bundles label/glyph state + 2 focusable ids + frame/registry; \
              the 8 args avoid an ad-hoc struct used in 7 call sites"
)]
fn render_vpn_toggle_row(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    enabled: bool,
    info_id: Option<WidgetId>,
    toggle_id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    use ratatui::style::Style;

    let glyph = components::checkbox_glyph(enabled);
    let glyph_width = glyph.chars().count() as u16; // 3 - "[x]" / "[ ]"
    let info_text_width: u16 = 6; // "[Info]"

    let trailing = if info_id.is_some() {
        info_text_width + 1 + glyph_width
    } else {
        glyph_width
    };
    let label_area = Rect::new(area.x, area.y, area.width.saturating_sub(trailing), 1);
    let glyph_area = Rect::new(
        area.x + area.width.saturating_sub(glyph_width),
        area.y,
        glyph_width,
        1,
    );

    frame.render_widget(Paragraph::new(label.to_string()), label_area);

    // Register the toggle first so it owns column 0 within this row.
    // The focus engine's column-snap (used by Up/Down arrow keys when
    // the destination row has fewer cells) lands on whichever widget
    // sits at column 0; placing the toggle there means moving up or
    // down through the VPN-settings rows lands on the right-most
    // (primary) control instead of on the decorative `[Info]`
    // companion. We still draw `[Info]` to the *left* of the toggle
    // glyph, so visually the row reads `<label>     [Info] [x]` -
    // only the registry's column ordering changes.
    //
    // The checkbox glyph already has its own brackets, so we render
    // it raw rather than via `render_button` (which would double them).
    let glyph_style = if focused == Some(toggle_id) {
        Style::new().yellow()
    } else {
        Style::new()
    };
    frame.render_widget(
        Paragraph::new(glyph.to_string()).style(glyph_style),
        glyph_area,
    );
    registry.register(FocusableWidget {
        id: toggle_id,
        rect: glyph_area,
        kind: FocusKind::Toggle,
    });

    if let Some(info_id) = info_id {
        let info_area = Rect::new(
            area.x + area.width.saturating_sub(trailing),
            area.y,
            info_text_width,
            1,
        );
        components::render_button(
            frame,
            info_area,
            "Info",
            focused == Some(info_id),
            registry,
            info_id,
        );
    }
    registry.end_row();
}

/// `<label>      [<button_label>]` row - one trailing focusable.
/// Used by every VPN row that isn't a toggle row: chevron links
/// (`[>]`), value-chevron links (`[Off >]`), and info-only headers
/// (`[Info]`).
pub(super) fn render_vpn_button_row(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    button_label: &str,
    id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    components::render_label_button_row(
        frame,
        area,
        label.to_string(),
        button_label,
        id,
        focused,
        registry,
    );
}

/// Parse what the user typed into the inline MTU input pill into a
/// daemon-call argument. Empty buffer means "clear the override"
/// (`None` -> daemon picks its default). Anything else must parse as a
/// `u16`; the daemon-side range check (`WIREGUARD_MTU_RANGE`) lives in
/// [`crate::app::App::set_mtu`].
pub fn parse_mtu_input(buffer: &str) -> Result<Option<u16>, String> {
    if buffer.is_empty() {
        return Ok(None);
    }
    buffer
        .parse::<u16>()
        .map(Some)
        .map_err(|_| "MTU must be a number between 0 and 65535".to_string())
}

/// `<label>      <inline text input>` row. The trailing focusable is
/// a dark-bg pill (matching the search anchor on `Status > Select
/// location`): placeholder when empty, current buffer otherwise, plus
/// a block-cursor glyph when focused. Sized to the longer of the
/// buffer and the placeholder so the field doesn't shrink below the
/// placeholder width as the user backspaces.
#[expect(
    clippy::too_many_arguments,
    reason = "row helper bundles label + buffer + placeholder + id + frame/registry/focus; \
              splitting into a struct adds a layer for one caller (today: MTU)"
)]
fn render_vpn_input_row(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    buffer: &str,
    placeholder: &str,
    id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    use ratatui::{
        style::{Color, Style},
        text::{Line, Span},
    };

    let is_focused = focused == Some(id);
    // Pill width: enough to fit either the placeholder, or the buffer
    // plus its trailing cursor block when focused-and-non-empty, plus
    // 2 cells for the surrounding `[` `]` brackets that mark the
    // widget as focusable (matching the convention used by every
    // other button on the page). Taking the *max* of (buffer + cursor)
    // and placeholder - instead of `max(buffer, placeholder) +
    // cursor_extra` - keeps the pill's right edge anchored at a fixed
    // column as the user types. Otherwise the pill would widen by 1
    // on the first keystroke, shifting the right-aligned pill left so
    // the typed digit lands one column to the left of where the cursor
    // had been sitting on the placeholder. (Assumes the placeholder is
    // wider than the max buffer; holds for the only caller - 7-char
    // `"Default"` vs the 4-char [`MTU_BUFFER_MAX_LEN`] buffer - and
    // would just clip the trailing cursor if violated.)
    let buffer_width = buffer.chars().count() as u16;
    let placeholder_width = placeholder.chars().count() as u16;
    let cursor_extra = if is_focused && !buffer.is_empty() {
        1
    } else {
        0
    };
    const BRACKETS_W: u16 = 2;
    let inner_width = buffer_width
        .saturating_add(cursor_extra)
        .max(placeholder_width);
    let pill_width = inner_width + BRACKETS_W;
    let pill_area = Rect::new(
        area.x + area.width.saturating_sub(pill_width),
        area.y,
        pill_width,
        1,
    );
    let label_area = Rect::new(area.x, area.y, area.width.saturating_sub(pill_width + 1), 1);
    frame.render_widget(Paragraph::new(label.to_string()), label_area);

    let bg = Color::Indexed(232);
    let cursor = || components::cursor_glyph_span(bg);
    // Bracket color follows the focus state so the focusable affordance
    // matches the convention used by `render_button` (yellow when focused).
    let bracket_style = if is_focused {
        Style::new().yellow().bg(bg)
    } else {
        Style::new().white().bg(bg)
    };
    let open_bracket = || Span::styled("[".to_string(), bracket_style);
    let close_bracket = || Span::styled("]".to_string(), bracket_style);
    // Pad between the inner content and `]` so the close bracket
    // stays anchored at the pill's right edge as the user types
    // (otherwise it would track the cursor leftward through the
    // pill's blank tail).
    let pad = |inner_content_w: u16| {
        let pad_w = inner_width.saturating_sub(inner_content_w);
        Span::styled(" ".repeat(pad_w as usize), Style::new().bg(bg))
    };
    let line = match (is_focused, buffer.is_empty()) {
        (true, true) => {
            // Focused with no draft yet: show the placeholder so the
            // daemon-default hint is visible while editing, with the
            // first character rendered in cursor colors (yellow bg) so
            // the user sees where typing will land. The first typed
            // digit replaces the placeholder with the live buffer; the
            // text-entry column matches the placeholder's first column.
            let mut chars = placeholder.chars();
            match chars.next() {
                Some(first) => {
                    let rest: String = chars.collect();
                    Line::from(vec![
                        open_bracket(),
                        Span::styled(first.to_string(), Style::new().fg(bg).on_yellow()),
                        Span::styled(rest, Style::new().dark_gray().bg(bg)),
                        pad(placeholder_width),
                        close_bracket(),
                    ])
                }
                // Empty placeholder is a configuration error - degrade
                // gracefully to a bare cursor.
                None => Line::from(vec![open_bracket(), cursor(), pad(1), close_bracket()]),
            }
        }
        (true, false) => Line::from(vec![
            open_bracket(),
            Span::styled(buffer.to_string(), Style::new().yellow().bg(bg)),
            cursor(),
            pad(buffer_width + 1),
            close_bracket(),
        ]),
        (false, true) => Line::from(vec![
            open_bracket(),
            Span::styled(placeholder.to_string(), Style::new().dark_gray().bg(bg)),
            pad(placeholder_width),
            close_bracket(),
        ]),
        (false, false) => Line::from(vec![
            open_bracket(),
            Span::styled(buffer.to_string(), Style::new().white().bg(bg)),
            pad(buffer_width),
            close_bracket(),
        ]),
    };
    frame.render_widget(Paragraph::new(line).style(Style::new().bg(bg)), pill_area);
    registry.register(FocusableWidget {
        id,
        rect: pill_area,
        kind: FocusKind::TextInput,
    });
    registry.end_row();
}

/// Whole-row-focusable indented radio sub-row (`( ) Automatic`,
/// `(•) IPv4`, `( ) IPv6` - used by the Device-IP-version group).
/// Indented 2ch from the row's left edge; the entire row is
/// registered as one focusable.
fn render_radio_subrow(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    selected: bool,
    id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    use ratatui::style::Style;

    let style = if focused == Some(id) {
        Style::new().yellow()
    } else {
        Style::new()
    };
    // 2ch indent, then `(•) Label` or `( ) Label`. The whole `area`
    // registers as the focusable rect so the row reads as one
    // selectable unit.
    let text = format!("  {} {label}", components::radio_glyph(selected));
    frame.render_widget(Paragraph::new(text).style(style), area);
    registry.register(FocusableWidget {
        id,
        rect: area,
        kind: FocusKind::SelectOption,
    });
    registry.end_row();
}

// ---- Layout helpers ----

fn render_menu_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    right_value: Option<&str>,
    id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    // The button hugs the left; an optional right-aligned value sits
    // in the trailing space (e.g. `[Multihop]   Off`). The value
    // isn't focusable - only the button label is.
    let button_text_width = (label.len() as u16).saturating_add(2); // "[label]"
    let button_area = Rect::new(area.x, area.y, button_text_width, 1);

    components::render_button(frame, button_area, label, focused == Some(id), registry, id);

    if let Some(value) = right_value {
        let value_width = value.len() as u16;
        if area.width > button_text_width + value_width + 1 {
            let value_area = Rect::new(area.x + area.width - value_width, area.y, value_width, 1);
            frame.render_widget(Paragraph::new(value.to_string()), value_area);
        }
    }
    registry.end_row();
}

pub(super) fn render_status_toggle_row(
    frame: &mut Frame<'_>,
    area: Rect,
    enabled: bool,
    toggle_label: &str,
    id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    // `Status: Off                       [Enable]`
    let toggle_text_width = (toggle_label.len() as u16).saturating_add(2);
    let label_text = format!("Status: {}", on_off_label(enabled));
    let label_width = area.width.saturating_sub(toggle_text_width + 1);

    let label_area = Rect::new(area.x, area.y, label_width, 1);
    let toggle_area = Rect::new(area.x + label_width + 1, area.y, toggle_text_width, 1);

    frame.render_widget(Paragraph::new(label_text), label_area);
    components::render_button(
        frame,
        toggle_area,
        toggle_label,
        focused == Some(id),
        registry,
        id,
    );
    registry.end_row();
}

pub(super) fn on_off_label(value: bool) -> &'static str {
    if value { "On" } else { "Off" }
}

/// Bracketed-button payload for a binary toggle. Pairs with
/// [`components::render_button`] which wraps its `label` in `[..]`,
/// so the rendered button reads `[x]` when on and `[ ]` when off -
/// matching the checkbox convention used by the VPN settings page.
pub(super) fn checkbox_label(checked: bool) -> &'static str {
    if checked { "x" } else { " " }
}

/// Render the Split tunneling sub-page. Layout (platform-conditional):
///
/// 1. Heading + descriptive blurb (cross-platform).
/// 2. `Status: On|Off    [Enable]/[Disable]` master toggle row.
/// 3. **Win/macOS**: app-path list (`<path>  [Remove]`) + bottom `[Add path]` button opening a
///    text-input modal.
/// 4. **Linux**: PID list (`<pid>  [Remove]`) + bottom `[Add PID]` button opening a numeric-input
///    modal. PIDs aren't push-driven; the run loop refreshes them on sub-page entry and after each
///    add/remove.
///
/// Add/edit by app picker, "clear all", and the macOS TCC-permission
/// pre-flight check are deferred to follow-ups; on macOS, missing TCC
/// surfaces as a daemon-side error through the standard notification
/// path.
pub fn render_split_tunnel(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    registry: &mut FocusRegistry,
) {
    let area = components::centered_column(area, components::PAGE_COLUMN_WIDTH);
    let focused = app.page_focus().focused;
    let enabled = app.split_tunnel_enabled();
    let toggle_label = checkbox_label(enabled);

    let header_lines: Vec<Line<'static>> = vec![
        Line::from("Split tunneling"),
        Line::from(""),
        Line::from("Route specific apps (or processes) outside the VPN tunnel."),
        Line::from(if cfg!(target_os = "linux") {
            "On Linux: by PID. Use [Add PID] to exclude a running process."
        } else {
            "On Windows/macOS: by app path. Use [Add path] to exclude a binary."
        }),
    ];

    let header_lines_height = Paragraph::new(header_lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width) as u16;

    // Pull the platform-relevant list. Empty / not-yet-loaded both
    // render the same "no entries" line; the difference shows up in the
    // [Add ...] button label.
    let app_paths = app.split_tunnel_apps();
    let pids: Vec<i32> = app
        .split_tunnel_pids()
        .map(|p| p.to_vec())
        .unwrap_or_default();
    let row_count = if cfg!(target_os = "linux") {
        pids.len()
    } else {
        app_paths.len()
    };

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(header_lines_height),
        Constraint::Length(1), // blank
        Constraint::Length(1), // status toggle row
        Constraint::Length(1), // blank
        Constraint::Length(1), // "Excluded:" label
    ];
    if row_count == 0 {
        constraints.push(Constraint::Length(1)); // "(none)" placeholder
    } else {
        for _ in 0..row_count {
            constraints.push(Constraint::Length(1));
        }
    }
    constraints.push(Constraint::Min(1)); // spacer
    constraints.push(Constraint::Length(1)); // bottom add button

    let chunks = Layout::vertical(constraints).split(area);

    frame.render_widget(
        Paragraph::new(header_lines).wrap(Wrap { trim: false }),
        chunks[0],
    );
    render_status_toggle_row(
        frame,
        chunks[2],
        enabled,
        toggle_label,
        widgets::SPLIT_TUNNEL_TOGGLE,
        focused,
        registry,
    );
    let list_label = if cfg!(target_os = "linux") {
        "Excluded PIDs:"
    } else {
        "Excluded apps:"
    };
    frame.render_widget(Paragraph::new(list_label), chunks[4]);

    if row_count == 0 {
        frame.render_widget(Paragraph::new("    (none)"), chunks[5]);
    } else if cfg!(target_os = "linux") {
        for (i, pid) in pids.iter().enumerate() {
            render_split_tunnel_remove_row(
                frame,
                chunks[5 + i],
                format!("    {pid}"),
                widgets::SPLIT_TUNNEL_REMOVE_PID_BASE,
                i,
                focused,
                registry,
            );
        }
    } else {
        for (i, path) in app_paths.iter().enumerate() {
            render_split_tunnel_remove_row(
                frame,
                chunks[5 + i],
                format!("    {path}"),
                widgets::SPLIT_TUNNEL_REMOVE_APP_BASE,
                i,
                focused,
                registry,
            );
        }
    }

    let (add_label, add_id) = if cfg!(target_os = "linux") {
        ("Add PID", widgets::SPLIT_TUNNEL_ADD_PID)
    } else {
        ("Add path", widgets::SPLIT_TUNNEL_ADD_APP)
    };
    components::render_centered_button(
        frame,
        chunks[chunks.len() - 1],
        add_label,
        add_id,
        focused,
        registry,
    );
}

/// `<label>  [Remove]` row used by both the Win/macOS app-path list
/// and the Linux PID list. Caller picks the base widget id and the
/// formatted label; rows past [`widgets::SPLIT_TUNNEL_REMOVE_MAX`]
/// render display-only.
fn render_split_tunnel_remove_row(
    frame: &mut Frame<'_>,
    area: Rect,
    label_text: String,
    base_id: WidgetId,
    index: usize,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    if (index as u32) >= widgets::SPLIT_TUNNEL_REMOVE_MAX {
        frame.render_widget(Paragraph::new(label_text), area);
        return;
    }
    let button_label = "[Remove]";
    let button_width = button_label.len() as u16;
    let label_width = area.width.saturating_sub(button_width + 1);

    let label_area = Rect::new(area.x, area.y, label_width, 1);
    let button_area = Rect::new(area.x + label_width + 1, area.y, button_width, 1);

    frame.render_widget(Paragraph::new(label_text), label_area);
    let id = WidgetId(base_id.0 + index as u32);
    components::render_button(
        frame,
        button_area,
        "Remove",
        focused == Some(id),
        registry,
        id,
    );
    registry.end_row();
}

/// Look up the path for a [Remove] row at `index`. Returns `None` when
/// the index is out of range or when the settings cache hasn't been
/// primed yet.
pub fn split_tunnel_app_at(app: &App, index: usize) -> Option<String> {
    app.split_tunnel_apps().into_iter().nth(index)
}

/// Look up the PID for a [Remove] row at `index`. Returns `None` when
/// the index is out of range or when the PID cache is empty.
pub fn split_tunnel_pid_at(app: &App, index: usize) -> Option<i32> {
    app.split_tunnel_pids().and_then(|p| p.get(index).copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ArrowDir;

    #[test]
    fn custom_dns_edit_index_decodes_in_range() {
        let base = widgets::CUSTOM_DNS_EDIT_BASE.0;
        assert_eq!(custom_dns_edit_index(WidgetId(base)), Some(0));
        assert_eq!(custom_dns_edit_index(WidgetId(base + 5)), Some(5));
        assert_eq!(
            custom_dns_edit_index(WidgetId(base + widgets::CUSTOM_DNS_EDIT_MAX - 1)),
            Some((widgets::CUSTOM_DNS_EDIT_MAX - 1) as usize),
        );
    }

    #[test]
    fn custom_dns_edit_index_returns_none_out_of_range() {
        let base = widgets::CUSTOM_DNS_EDIT_BASE.0;
        // Just past the cap.
        assert_eq!(
            custom_dns_edit_index(WidgetId(base + widgets::CUSTOM_DNS_EDIT_MAX)),
            None,
        );
        // Inside the [Remove] range - must not collide.
        let remove_base = widgets::CUSTOM_DNS_REMOVE_BASE.0;
        assert_eq!(custom_dns_edit_index(WidgetId(remove_base)), None);
    }

    #[test]
    fn custom_dns_edit_and_remove_ranges_are_disjoint() {
        // Adjacency check: every Remove id decodes as Some via the
        // remove decoder and None via the edit decoder, and vice
        // versa. Catches accidental overlap when either MAX changes.
        for i in 0..widgets::CUSTOM_DNS_REMOVE_MAX {
            let remove_id = WidgetId(widgets::CUSTOM_DNS_REMOVE_BASE.0 + i);
            assert_eq!(custom_dns_remove_index(remove_id), Some(i as usize));
            assert_eq!(custom_dns_edit_index(remove_id), None);
        }
        for i in 0..widgets::CUSTOM_DNS_EDIT_MAX {
            let edit_id = WidgetId(widgets::CUSTOM_DNS_EDIT_BASE.0 + i);
            assert_eq!(custom_dns_edit_index(edit_id), Some(i as usize));
            assert_eq!(custom_dns_remove_index(edit_id), None);
        }
    }

    #[tokio::test]
    async fn settings_root_renders_app_info_block_at_the_bottom() {
        use ratatui::{Terminal, backend::TestBackend};

        use crate::test_support::StubService;

        let mut app = App::new();
        let stub = StubService {
            daemon_version: Ok("2026.4".to_string()),
            ..StubService::default()
        };
        app.resync(&stub).await.unwrap();

        let screen = render_to_screen(&app);
        assert!(
            screen.contains("App info"),
            "expected App info header, got:\n{screen}",
        );
        assert!(
            screen.contains(REPO_URL),
            "expected repo URL, got:\n{screen}",
        );
        assert!(
            screen.contains(env!("CARGO_PKG_VERSION")),
            "expected mullvad-tui version, got:\n{screen}",
        );
        assert!(
            screen.contains("Targeted daemon"),
            "expected targeted daemon row, got:\n{screen}",
        );
        assert!(
            screen.contains(mullvad_version::VERSION),
            "expected targeted daemon version, got:\n{screen}",
        );
        assert!(
            screen.contains("2026.4"),
            "expected daemon version, got:\n{screen}",
        );

        fn render_to_screen(app: &App) -> String {
            let width = 50;
            let height = 24;
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let buf = terminal
                .draw(|frame| {
                    let area = Rect::new(0, 0, width, height);
                    let mut registry = FocusRegistry::new();
                    render(frame, area, app, &mut registry);
                })
                .unwrap();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf.buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    #[test]
    fn settings_root_app_info_falls_back_to_dash_when_daemon_version_unknown() {
        use ratatui::{Terminal, backend::TestBackend};

        let app = App::new();
        let width = 50;
        let height = 24;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let buf = terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                let mut registry = FocusRegistry::new();
                render(frame, area, &app, &mut registry);
            })
            .unwrap();
        let screen: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("mullvad-daemon"),
            "expected mullvad-daemon row, got:\n{screen}",
        );
        assert!(
            screen.contains("-"),
            "expected dash placeholder for unknown daemon version, got:\n{screen}",
        );
    }

    #[test]
    fn vpn_toggle_row_registers_toggle_before_info() {
        // The focus engine snaps Up/Down by column index. Putting the
        // toggle (right-most control) at column 0 and `[Info]` at
        // column 1 means moving up or down through the VPN-settings
        // rows lands on the toggle - the primary control - instead
        // of on the decorative info companion. Left/Right, by
        // contrast, follow the visual `rect.x` of each widget so
        // arrow-key navigation still reads the row left-to-right
        // regardless of registration order.
        use ratatui::{Terminal, backend::TestBackend};

        let mut registry = FocusRegistry::default();
        let backend = TestBackend::new(50, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_vpn_toggle_row(
                    frame,
                    Rect::new(0, 0, 50, 1),
                    "Local network sharing",
                    false,
                    Some(widgets::VPN_LAN_INFO),
                    widgets::VPN_LAN_TOGGLE,
                    None,
                    &mut registry,
                );
            })
            .unwrap();

        // Toggle is column 0 (snap target); info is column 1.
        assert_eq!(
            registry.first_in_row(0),
            Some(widgets::VPN_LAN_TOGGLE),
            "toggle should be the first focus index on the row",
        );
        // Visually the row reads `<label>     [Info] [x]`, so the
        // toggle is the right-most cell - Right from the toggle
        // hits the row's edge and Left walks back to `[Info]`.
        assert_eq!(
            registry.navigate(widgets::VPN_LAN_TOGGLE, ArrowDir::Right),
            None,
            "toggle is the right-most cell visually; Right should not move",
        );
        assert_eq!(
            registry.navigate(widgets::VPN_LAN_TOGGLE, ArrowDir::Left),
            Some(widgets::VPN_LAN_INFO),
        );
        assert_eq!(
            registry.navigate(widgets::VPN_LAN_INFO, ArrowDir::Right),
            Some(widgets::VPN_LAN_TOGGLE),
        );
    }

    #[test]
    fn vpn_input_row_close_bracket_stays_at_right_edge_as_buffer_grows() {
        // Regression: the `]` used to render immediately after the
        // cursor, so as the user typed the cursor advanced and
        // dragged the `]` rightward through the pill. The pill is
        // right-aligned with constant width, so the `]` must stay
        // pinned to the rightmost cell - padding fills the gap
        // between the cursor and the close bracket.
        use ratatui::{Terminal, backend::TestBackend};

        fn close_bracket_x(buffer: &str) -> u16 {
            let mut registry = FocusRegistry::default();
            let backend = TestBackend::new(50, 1);
            let mut terminal = Terminal::new(backend).unwrap();
            let buf = terminal
                .draw(|frame| {
                    render_vpn_input_row(
                        frame,
                        Rect::new(0, 0, 50, 1),
                        "MTU",
                        buffer,
                        "Default",
                        widgets::VPN_MTU_EDIT,
                        Some(widgets::VPN_MTU_EDIT),
                        &mut registry,
                    );
                })
                .unwrap();
            // `]` is the last `]` on the row (first one is `[`'s pair
            // - the pill - there's no other `]` since the label is
            // plain "MTU").
            (0..buf.area.width)
                .rev()
                .find(|&x| buf.buffer[(x, 0)].symbol() == "]")
                .expect("close bracket should render")
        }

        let empty = close_bracket_x("");
        for buffer in ["1", "13", "138", "1380"] {
            assert_eq!(
                close_bracket_x(buffer),
                empty,
                "`]` must stay at the same column for buffer={buffer:?}",
            );
        }
    }

    #[test]
    fn vpn_input_row_wraps_pill_with_focusable_brackets() {
        // The pill is wrapped with `[` and `]` to mark it as a
        // focusable widget - same convention as `[Cancel]` / `[Add]`
        // buttons. Brackets render yellow when focused (matches the
        // page's button focus style), default otherwise.
        use ratatui::{Terminal, backend::TestBackend, style::Color};

        for focused_now in [true, false] {
            let mut registry = FocusRegistry::default();
            let backend = TestBackend::new(50, 1);
            let mut terminal = Terminal::new(backend).unwrap();
            let buf = terminal
                .draw(|frame| {
                    render_vpn_input_row(
                        frame,
                        Rect::new(0, 0, 50, 1),
                        "MTU",
                        "",
                        "Default",
                        widgets::VPN_MTU_EDIT,
                        focused_now.then_some(widgets::VPN_MTU_EDIT),
                        &mut registry,
                    );
                })
                .unwrap();

            let row: String = (0..buf.area.width)
                .map(|x| buf.buffer[(x, 0)].symbol())
                .collect();
            let open = row
                .find('[')
                .unwrap_or_else(|| panic!("open bracket missing (focused={focused_now}): {row:?}"))
                as u16;
            let close = row
                .rfind(']')
                .unwrap_or_else(|| panic!("close bracket missing (focused={focused_now}): {row:?}"))
                as u16;
            assert!(close > open, "brackets out of order: {row:?}");

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

    #[test]
    fn vpn_input_row_renders_placeholder_at_text_entry_column_when_focused_and_empty() {
        // Focused-and-empty: the placeholder starts at the same column
        // where text entry will land (no leading cursor block pushing
        // it right). The cursor is rendered as inverse styling on the
        // first placeholder character - yellow background - so the
        // user sees where typing will go.
        use ratatui::{Terminal, backend::TestBackend, style::Color};

        let mut registry = FocusRegistry::default();
        let backend = TestBackend::new(50, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let buf = terminal
            .draw(|frame| {
                render_vpn_input_row(
                    frame,
                    Rect::new(0, 0, 50, 1),
                    "MTU",
                    "",
                    "Default",
                    widgets::VPN_MTU_EDIT,
                    Some(widgets::VPN_MTU_EDIT),
                    &mut registry,
                );
            })
            .unwrap();

        let row: String = (0..buf.area.width)
            .map(|x| buf.buffer[(x, 0)].symbol())
            .collect();
        // Placeholder fully visible - no leading cursor block bumped it
        // off column 0 of the pill.
        assert!(
            row.contains("Default"),
            "focused empty pill should show the placeholder, got {row:?}",
        );
        assert!(
            !row.contains("█Default"),
            "no leading cursor block - placeholder should start at the text-entry column, got {row:?}",
        );

        // The cursor is the first placeholder character rendered with
        // a yellow background. Locate the placeholder's first column
        // and check the cell's bg.
        let first_col = row.find('D').expect("placeholder rendered") as u16;
        let cell = &buf.buffer[(first_col, 0)];
        assert_eq!(
            cell.bg,
            Color::Yellow,
            "first placeholder character should carry cursor styling (yellow bg)",
        );
    }

    #[test]
    fn vpn_input_row_pill_position_does_not_shift_when_first_digit_is_typed() {
        // Regression: the pill is right-aligned and was previously sized
        // as `max(buffer, placeholder) + cursor_extra`, which made it
        // widen by 1 the moment the buffer became non-empty - and a
        // right-aligned pill that gets wider shifts *left*, so the
        // newly-typed digit landed one column to the left of where the
        // cursor had been sitting on the placeholder. The pill's left
        // edge must instead stay put across the empty/non-empty
        // transition; the cursor advances within a fixed-width pill.
        use ratatui::{Terminal, backend::TestBackend};

        fn render(buffer: &str) -> (String, u16) {
            let mut registry = FocusRegistry::default();
            let backend = TestBackend::new(50, 1);
            let mut terminal = Terminal::new(backend).unwrap();
            let buf = terminal
                .draw(|frame| {
                    render_vpn_input_row(
                        frame,
                        Rect::new(0, 0, 50, 1),
                        "MTU",
                        buffer,
                        "Default",
                        widgets::VPN_MTU_EDIT,
                        Some(widgets::VPN_MTU_EDIT),
                        &mut registry,
                    );
                })
                .unwrap();
            let row: String = (0..buf.area.width)
                .map(|x| buf.buffer[(x, 0)].symbol())
                .collect();
            // Pill left edge is the leftmost cell whose bg is the dark
            // pill bg / the first non-default cell.
            let dark = ratatui::style::Color::Indexed(232);
            let yellow = ratatui::style::Color::Yellow;
            let pill_x = (0..buf.area.width)
                .find(|x| {
                    let bg = buf.buffer[(*x, 0)].bg;
                    bg == dark || bg == yellow
                })
                .expect("pill must render at least one styled cell");
            (row, pill_x)
        }

        let (empty_row, empty_x) = render("");
        let (typed_row, typed_x) = render("1");

        assert_eq!(
            empty_x, typed_x,
            "pill left edge must stay at the same column across the empty/typed transition; \
             empty row {empty_row:?} typed row {typed_row:?}",
        );
        // Pill is wrapped with `[` `]` brackets to mark it focusable;
        // the cursor sits in the first *content* column, one cell right
        // of the `[`. The first typed digit should land in that column.
        assert_eq!(
            empty_row.as_bytes()[empty_x as usize],
            b'[',
            "leftmost styled cell should be the open bracket; got {empty_row:?}",
        );
        let cursor_col = empty_x + 1;
        assert_eq!(
            typed_row.as_bytes()[cursor_col as usize],
            b'1',
            "the typed digit must land at the column the cursor was on",
        );
    }

    #[test]
    fn vpn_input_row_renders_buffer_then_cursor_when_focused_with_text() {
        // Focused-with-text: live buffer in yellow, cursor at end. The
        // placeholder is hidden in this state (the buffer occupies the
        // visible cells).
        use ratatui::{Terminal, backend::TestBackend};

        let mut registry = FocusRegistry::default();
        let backend = TestBackend::new(50, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let buf = terminal
            .draw(|frame| {
                render_vpn_input_row(
                    frame,
                    Rect::new(0, 0, 50, 1),
                    "MTU",
                    "1380",
                    "Default",
                    widgets::VPN_MTU_EDIT,
                    Some(widgets::VPN_MTU_EDIT),
                    &mut registry,
                );
            })
            .unwrap();

        let row: String = (0..buf.area.width)
            .map(|x| buf.buffer[(x, 0)].symbol())
            .collect();
        assert!(
            row.contains("1380█"),
            "focused non-empty pill should render buffer + trailing cursor, got {row:?}",
        );
        assert!(
            !row.contains("Default"),
            "placeholder should be hidden once the buffer has text, got {row:?}",
        );
    }

    #[test]
    fn disconnect_and_quit_label_collapses_when_no_tunnel_to_drop() {
        use crate::test_support::{connected_state, disconnected_state, error_state};

        // Disconnected (and the `None` daemon-state-not-yet-arrived
        // case) have nothing to tear down -> plain "Quit" so the
        // button stops advertising a no-op side effect.
        assert_eq!(disconnect_and_quit_label(None), "Quit");
        assert_eq!(
            disconnect_and_quit_label(Some(&disconnected_state())),
            "Quit"
        );

        // Anything with an active or in-flight tunnel keeps the full
        // label so the user knows quitting will also drop it.
        assert_eq!(
            disconnect_and_quit_label(Some(&connected_state())),
            "Disconnect & quit",
        );
        // Error state keeps the full label too: the firewall is still
        // up, and quitting would tear it down.
        assert_eq!(
            disconnect_and_quit_label(Some(&error_state())),
            "Disconnect & quit",
        );
    }

    #[test]
    fn settings_root_widget_ids_resolve() {
        assert_eq!(
            SettingsWidget::from_widget_id(widgets::DAITA),
            Some(SettingsWidget::Daita),
        );
        assert_eq!(
            SettingsWidget::from_widget_id(widgets::DISCONNECT_AND_QUIT),
            Some(SettingsWidget::DisconnectAndQuit),
        );
        assert_eq!(
            SettingsWidget::from_widget_id(widgets::VPN_DEVICE_IP_AUTO),
            Some(SettingsWidget::VpnDeviceIpAuto),
        );
        assert_eq!(
            SettingsWidget::from_widget_id(widgets::VPN_KILL_SWITCH_TOGGLE),
            Some(SettingsWidget::VpnKillSwitchToggle),
        );
    }
}
