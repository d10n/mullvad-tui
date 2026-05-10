// SPDX-License-Identifier: GPL-3.0-or-later

/// Identifies a page in the UI hierarchy. Top-level pages appear as
/// tabs at the top of the screen; sub-pages are reached by activating
/// a button on their parent and add a `[Back]` button to the tab bar.
/// Drives:
///
/// - The tab-bar's "active" indicator (via `top_level_root`).
/// - The body renderer dispatch in `tui::run_loop`.
/// - The sub-page push/pop machinery on `App`.
///
/// Sub-pages exit via `Esc` / `[Back]`; tabs don't accumulate history.
/// `App::current_sub_page` covers the only navigation depth.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PageId {
    Status,
    /// Inline relay-picker sub-page reached from Status's
    /// `[Switch location]` button.
    SelectLocation,
    /// Filter sub-page reached from `Select location > [Filter]`.
    /// Lets the user narrow the tree by ownership and provider.
    SelectLocationFilter,
    Account,
    AccountDevices,
    Settings,
    SettingsDaita,
    SettingsMultihop,
    SettingsVpn,
    SettingsDnsBlockers,
    SettingsCustomDns,
    SettingsAntiCensorship,
    SettingsApiAccess,
    SettingsSplitTunnel,
    SettingsRelayOverrides,
    Logs,
}

impl PageId {
    /// Display label for the tab bar. Sub-pages return their parent's
    /// label so the active tab follows them deeper into the hierarchy.
    pub fn tab_label(self) -> &'static str {
        match self.top_level_root() {
            Self::Status => "Status",
            Self::Account => "Account",
            Self::Settings => "Settings",
            Self::Logs => "Logs",
            // unreachable: top_level_root always returns a top-level page
            _ => "",
        }
    }

    /// Display label for the breadcrumb row that shows on sub-pages.
    /// Returns the page's own label (not its parent's, unlike
    /// [`Self::tab_label`]). Top-level pages use the same label as
    /// their tab; sub-pages get a shorter, page-specific name.
    pub fn breadcrumb_label(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::SelectLocation => "Select location",
            Self::SelectLocationFilter => "Filter",
            Self::Account => "Account",
            Self::AccountDevices => "Devices",
            Self::Settings => "Settings",
            Self::SettingsDaita => "DAITA",
            Self::SettingsMultihop => "Multihop",
            Self::SettingsVpn => "VPN",
            Self::SettingsDnsBlockers => "DNS content blockers",
            Self::SettingsCustomDns => "Custom DNS",
            Self::SettingsAntiCensorship => "Anti-censorship",
            Self::SettingsApiAccess => "API access",
            Self::SettingsSplitTunnel => "Split tunneling",
            Self::SettingsRelayOverrides => "Server IP overrides",
            Self::Logs => "Logs",
        }
    }

    /// Walk up to the top-level ancestor. For top-level pages, returns
    /// `self`. For sub-pages, returns the parent (and so on transitively
    /// in case we ever nest deeper).
    pub fn top_level_root(self) -> PageId {
        match self {
            Self::SelectLocation | Self::SelectLocationFilter => Self::Status,
            Self::AccountDevices => Self::Account,
            Self::SettingsDaita
            | Self::SettingsMultihop
            | Self::SettingsVpn
            | Self::SettingsDnsBlockers
            | Self::SettingsCustomDns
            | Self::SettingsAntiCensorship
            | Self::SettingsApiAccess
            | Self::SettingsSplitTunnel
            | Self::SettingsRelayOverrides => Self::Settings,
            other => other,
        }
    }

    /// Sub-page directly above this one in the breadcrumb chain, or
    /// `None` when the parent is the top-level root (in which case
    /// the caller should clear `current_sub_page` to land on the
    /// top-level page itself). Drives both `Esc`'s "go up one
    /// breadcrumb level" behavior and the breadcrumb renderer's
    /// chain construction - every page in the chain except the
    /// top-level root resolves to its immediate predecessor here.
    pub fn parent_sub_page(self) -> Option<PageId> {
        match self {
            // `Status > Select location > Filter`.
            Self::SelectLocationFilter => Some(Self::SelectLocation),
            // VPN sub-pages reached from `Settings > VPN`. The other
            // Settings sub-pages (DAITA, Multihop, Split tunneling,
            // API access) are first-level children of Settings root,
            // not VPN, so they keep the default `None`.
            Self::SettingsDnsBlockers
            | Self::SettingsCustomDns
            | Self::SettingsAntiCensorship
            | Self::SettingsRelayOverrides => Some(Self::SettingsVpn),
            _ => None,
        }
    }
}

/// The four top-level pages, in tab-bar display order.
pub const TOP_LEVEL_PAGES: [PageId; 4] = [
    PageId::Status,
    PageId::Account,
    PageId::Settings,
    PageId::Logs,
];

/// Tag identifying which destructive action a confirmation overlay
/// targets. The run loop's `dispatch_confirm` (in `tui/mod.rs`)
/// matches on this when the user activates `[Confirm]`. Lives here
/// alongside [`PageId`] because it's logically navigation-adjacent
/// (the overlay is the visible-target of a UI navigation), but the
/// overlay state itself is run-loop-owned in
/// [`crate::tui::OverlayMode`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfirmAction {
    Disconnect,
    Logout,
    ToggleLockdown,
    RotateWireGuardKey,
    /// Wipe every per-relay IP override. Backs the `[Clear all]`
    /// button on `Settings > VPN > Server IP overrides`.
    ClearRelayOverrides,
    /// Remove the IP override for a specific hostname. Backs the
    /// per-row `[Remove]` button on the Server IP overrides sub-page.
    RemoveRelayOverride {
        hostname: String,
    },
}

/// Navigation state owned by [`App`]: just the current top-level
/// page. Sub-pages live in `App.current_sub_page`, and overlay state
/// (confirmations / notifications) lives in `tui::OverlayMode` on
/// the run loop.
#[derive(Debug)]
pub struct NavigationState {
    current_page: PageId,
}

impl Default for NavigationState {
    fn default() -> Self {
        Self {
            // Status is the landing page - first tab in the bar, and
            // the page the daemon's connection state is most useful on.
            current_page: PageId::Status,
        }
    }
}

impl NavigationState {
    pub fn current_page(&self) -> PageId {
        self.current_page
    }

    pub fn navigate_to(&mut self, page: PageId) {
        self.current_page = page;
    }
}
