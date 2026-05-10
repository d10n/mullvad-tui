// SPDX-License-Identifier: GPL-3.0-or-later

use std::{fmt, sync::Arc};

use chrono::Local;
use thiserror::Error;

mod rpc;
mod tolerant;

pub use mullvad_types::{
    access_method::{AccessMethodSetting, Id as AccessMethodId},
    account::{AccountData, AccountNumber, VoucherSubmission},
    constraints::Constraint,
    device::{Device, DeviceId, DeviceState},
    features::FeatureIndicators,
    relay_constraints::{
        GeographicLocationConstraint, LocationConstraint, ObfuscationSettings, RelayOverride,
        RelaySettings, SelectedObfuscation,
    },
    relay_list::RelayList,
    settings::{DefaultDnsOptions, DnsOptions, DnsState, Settings},
};
// `CustomDnsOptions` is referenced only in tests (`set_custom_dns_addresses`
// + the remove-test seeding), so re-exporting it would trip the
// `unused_imports` lint on production builds. Test sites import it
// directly from `mullvad_types::settings::CustomDnsOptions`.
pub use mullvad_types::{
    states::{TargetState, TunnelState},
    version::AppVersionInfo,
    wireguard::QuantumResistantState,
};
pub use rpc::RpcMullvadService;
pub use talpid_types::net::ObfuscationInfo;
// `ObfuscationType` is used only via its `Display` impl when
// formatting obfuscation rows in the verbose Connection details
// block - never named as a type - so re-exporting it would trip
// `unused_imports` on the production build.
pub use talpid_types::tunnel::ErrorState;

/// Project the daemon's nested [`RelayList`] (countries -> cities -> relays) to
/// the flat [`RelayLocation`] list the TUI consumes. Filters out inactive
/// relays, since the user can't actually connect to them. Used by both the
/// `list_relays` RPC fetch and the `DaemonEvent::RelayList` push handler so
/// they can't drift.
pub fn project_relay_list(relay_list: &RelayList) -> Vec<RelayLocation> {
    relay_list
        .countries
        .iter()
        .flat_map(|country| {
            country.cities.iter().flat_map(move |city| {
                city.relays
                    .iter()
                    .filter(|relay| relay.active)
                    .map(move |relay| RelayLocation {
                        hostname: relay.hostname.clone(),
                        country_name: country.name.clone(),
                        country_code: country.code.clone(),
                        city_name: city.name.clone(),
                        city_code: city.code.clone(),
                    })
            })
        })
        .collect()
}

/// Internal events surfaced to the TUI loop (typically pushed by the daemon
/// event stream). The `*Changed` suffix is intentional - these are change
/// notifications, not commands - and the clippy `enum_variant_names` lint is
/// allowed here because that semantic is more valuable than the lint's
/// brevity preference.
#[derive(Debug, Clone)]
#[expect(
    clippy::enum_variant_names,
    reason = "every variant is a *Changed event-stream notification; the suffix carries semantic weight"
)]
pub enum AppEvent {
    StatusChanged(TunnelState),
    SettingsChanged(Settings),
    DeviceChanged(DeviceState),
    AppVersionInfoChanged(AppVersionInfo),
    RelayListChanged(RelayList),
}

/// Snapshot of the current account session, projected from the daemon's typed
/// [`DeviceState`] plus the optional [`AccountData`] (which requires a separate
/// RPC and is `None` until that call succeeds).
#[derive(Debug, Clone)]
pub struct AccountInfo {
    pub device: DeviceState,
    pub data: Option<AccountData>,
}

impl fmt::Display for AccountInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.device {
            DeviceState::LoggedIn(account_and_device) => {
                write!(
                    f,
                    "{} | device: {}",
                    account_and_device.account_number,
                    account_and_device.device.pretty_name(),
                )?;
                if let Some(data) = &self.data {
                    write!(
                        f,
                        " | expires: {}",
                        data.expiry
                            .with_timezone(&Local)
                            .format("%Y-%m-%d %H:%M %Z")
                    )?;
                }
                Ok(())
            }
            DeviceState::LoggedOut => f.write_str("Not logged in"),
            DeviceState::Revoked => f.write_str("Device revoked"),
        }
    }
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct RelayLocation {
    pub hostname: String,
    pub country_name: String,
    pub country_code: String,
    pub city_name: String,
    pub city_code: String,
}

#[derive(Debug, Clone, Error)]
pub enum IntegrationError {
    /// TUI-side argument validation failure (bad input, no matching relay,
    /// unsupported mode, etc.). Reported to the user without retry.
    #[error("validation error: {0}")]
    Validation(String),
    /// Wraps the daemon's typed error so callers can match on specific variants
    /// (e.g. [`mullvad_management_interface::Error::AlreadyLoggedIn`]) for
    /// targeted UX. The source error is held in an [`Arc`] because it isn't
    /// `Clone` (it wraps `tonic::Status`, `io::Error`, etc.) but
    /// [`IntegrationError`] needs to be cheaply cloneable.
    #[error("daemon RPC error: {0}")]
    Rpc(Arc<mullvad_management_interface::Error>),
}

impl From<mullvad_management_interface::Error> for IntegrationError {
    fn from(error: mullvad_management_interface::Error) -> Self {
        IntegrationError::Rpc(Arc::new(error))
    }
}

pub trait MullvadService {
    async fn get_status(&self) -> Result<TunnelState, IntegrationError>;
    /// Returns `true` if the daemon transitioned out of the disconnected/error
    /// state, `false` if it was already connecting or connected (no-op).
    async fn connect(&self) -> Result<bool, IntegrationError>;
    /// Returns `true` if the daemon transitioned away from a connected/connecting
    /// state, `false` if it was already disconnected (no-op).
    async fn disconnect(&self) -> Result<bool, IntegrationError>;
    /// Returns `true` if a reconnect was initiated, `false` otherwise.
    async fn reconnect(&self) -> Result<bool, IntegrationError>;
    async fn get_account(&self) -> Result<AccountInfo, IntegrationError>;
    /// Fetch only the [`AccountData`] (expiry, paid_until) for a known account.
    /// Used by `apply_app_event` after a `DeviceChanged(LoggedIn)` push to
    /// populate the half of `AccountInfo` that the device push doesn't carry.
    async fn get_account_data(
        &self,
        account: AccountNumber,
    ) -> Result<AccountData, IntegrationError>;
    async fn login(&self, account: AccountNumber) -> Result<(), IntegrationError>;
    async fn logout(&self) -> Result<(), IntegrationError>;
    /// List the devices currently registered to `account`. Used by the
    /// Account -> Manage devices sub-page; called on demand when that
    /// sub-page is entered (no push event drives this list, so it's a
    /// pull every time).
    async fn list_devices(&self, account: AccountNumber) -> Result<Vec<Device>, IntegrationError>;
    /// Remove a single device by id from `account`. Used by the Manage
    /// devices `[Remove]` button. Removing the *current* device is
    /// equivalent to logging out - daemon will emit a DeviceChanged
    /// event that the app picks up.
    async fn remove_device(
        &self,
        account: AccountNumber,
        device_id: DeviceId,
    ) -> Result<(), IntegrationError>;
    /// Redeem a voucher code to extend the current account's expiry.
    /// Returns the new expiry + the amount of time the voucher added,
    /// for the success notification copy.
    async fn submit_voucher(&self, voucher: String) -> Result<VoucherSubmission, IntegrationError>;
    async fn list_relays(&self) -> Result<Vec<RelayLocation>, IntegrationError>;
    async fn set_relay_location(&self, location: &str) -> Result<(), IntegrationError>;
    /// Coarser-grained sibling of [`Self::set_relay_location`]: writes a
    /// country-level `LocationConstraint::Location(GeographicLocationConstraint::Country(_))`
    /// so the daemon picks any active relay in that country. Used by the
    /// relay-selector modal when the user picks a country row.
    async fn set_relay_country(&self, country_code: &str) -> Result<(), IntegrationError>;
    /// City-level sibling of [`Self::set_relay_country`]. Daemon picks any
    /// active relay in `country_code`/`city_code`.
    async fn set_relay_city(
        &self,
        country_code: &str,
        city_code: &str,
    ) -> Result<(), IntegrationError>;
    /// Toggle the daemon's `wireguard_constraints.use_multihop` flag.
    async fn set_multihop_enabled(&self, enabled: bool) -> Result<(), IntegrationError>;
    /// Set the daemon's `wireguard_constraints.ip_version` constraint.
    /// `None` clears the constraint (Automatic - daemon picks); `Some`
    /// pins it to v4 or v6. Backs the
    /// `Device IP version: ( ) Automatic / (•) IPv4 / ( ) IPv6` radio
    /// group on the VPN settings page.
    async fn set_ip_version_preference(
        &self,
        version: Option<talpid_types::net::IpVersion>,
    ) -> Result<(), IntegrationError>;
    /// Fetch the daemon's full typed [`Settings`]. The TUI caches the result
    /// in `App` and updates it via `DaemonEvent::Settings` push, so this is
    /// called once at startup and again on demand if the cache needs reseating.
    async fn get_full_settings(&self) -> Result<Settings, IntegrationError>;
    async fn set_lan(&self, enabled: bool) -> Result<(), IntegrationError>;
    /// Toggle the daemon's `auto_connect` setting - whether the daemon
    /// auto-establishes the tunnel on its own start. Off by default;
    /// users opt in via the VPN settings page.
    async fn set_auto_connect(&self, enabled: bool) -> Result<(), IntegrationError>;
    /// Replace the daemon's full DNS options. The daemon's `set_dns_options`
    /// RPC takes the whole [`DnsOptions`] struct (state + default_options +
    /// custom_options), not a delta - callers compute the new value from
    /// the cached [`Settings`] so per-field toggles don't silently zero
    /// other fields.
    async fn set_dns_options(&self, options: DnsOptions) -> Result<(), IntegrationError>;
    /// Daemon's running version string (e.g. `2026.4`). Doesn't change without
    /// a daemon restart, which would drop our gRPC connection - so fetched
    /// once at startup and on reconnect, never polled.
    async fn get_daemon_version(&self) -> Result<String, IntegrationError>;
    /// Typed [`AppVersionInfo`] (current-version-supported flag + suggested
    /// upgrade). Push-driven by `DaemonEvent::AppVersionInfo`; this method
    /// is the startup primer and the manual-refresh fallback.
    async fn get_app_version_info(&self) -> Result<AppVersionInfo, IntegrationError>;
    async fn set_lockdown_mode(&self, enabled: bool) -> Result<(), IntegrationError>;
    /// Replace the daemon's full obfuscation settings. Callers compute the new
    /// value from the cached [`Settings`] and pass the whole sub-struct so the
    /// service stays a thin RPC wrapper (no read-modify-set inside).
    async fn set_obfuscation_settings(
        &self,
        settings: ObfuscationSettings,
    ) -> Result<(), IntegrationError>;
    /// Set the WireGuard quantum-resistant tunnel mode. Maps to the
    /// `[Quantum-resistant tunnel]` toggle on the VPN settings page.
    async fn set_quantum_resistant_tunnel(
        &self,
        state: QuantumResistantState,
    ) -> Result<(), IntegrationError>;
    /// Toggle in-tunnel IPv6 traffic. Off by default on Linux/Windows
    /// (per the daemon's defaults), on by default on macOS/Android.
    async fn set_enable_ipv6(&self, enabled: bool) -> Result<(), IntegrationError>;
    /// Override the WireGuard MTU. `None` clears the override (daemon
    /// computes the default). Daemon validates the range itself; the
    /// TUI also pre-validates 1280..=1420 for a faster error path.
    async fn set_wireguard_mtu(&self, mtu: Option<u16>) -> Result<(), IntegrationError>;
    /// Toggle the master DAITA enable flag. When off, the per-relay
    /// DAITA opt-in has no effect; when on, the daemon enables DAITA
    /// for any relay that supports it.
    async fn set_enable_daita(&self, enabled: bool) -> Result<(), IntegrationError>;
    /// Toggle DAITA's "Direct only" mode. The user-visible wording is
    /// the inverse of the daemon's flag (`use_multihop_if_necessary`);
    /// the App-side wrapper does the inversion.
    async fn set_daita_direct_only(&self, enabled: bool) -> Result<(), IntegrationError>;
    /// Rotate the current device's WireGuard key. The daemon
    /// generates a new key, registers it with Mullvad's API, and
    /// transitions the tunnel onto the new key - connectivity drops
    /// briefly during the swap, so callers should gate this behind a
    /// confirmation overlay.
    async fn rotate_wireguard_key(&self) -> Result<(), IntegrationError>;
    /// Fetch the access method the daemon is currently using to reach the
    /// Mullvad API. Read once when entering the API access sub-page;
    /// the daemon doesn't push changes (no `DaemonEvent` variant), so
    /// this is the only signal for which method is "currently active".
    async fn get_current_api_access_method(&self) -> Result<AccessMethodSetting, IntegrationError>;
    /// Replace one access method's record (typically to flip its
    /// `enabled` flag). The TUI uses this for the per-row
    /// [Enable]/[Disable] toggle.
    async fn update_access_method(
        &self,
        setting: AccessMethodSetting,
    ) -> Result<(), IntegrationError>;
    /// Set (or update, or remove) the IPv4/IPv6 in-address override for
    /// one relay by hostname. Submitting a [`RelayOverride`] with both
    /// addresses `None` removes the existing override for that hostname;
    /// otherwise the daemon adds or replaces it. Backs the
    /// `Settings > VPN > Server IP overrides` sub-page.
    async fn set_relay_override(
        &self,
        relay_override: RelayOverride,
    ) -> Result<(), IntegrationError>;
    /// Wipe every relay override. Backs the `[Clear all]` button on
    /// the Server IP overrides sub-page.
    async fn clear_all_relay_overrides(&self) -> Result<(), IntegrationError>;
    /// Tell the daemon to use `id` as the active access method. The
    /// TUI uses this for the per-row [Use] button.
    async fn set_access_method(&self, id: AccessMethodId) -> Result<(), IntegrationError>;
    /// Enable or disable split tunneling globally. Cross-platform: on
    /// Linux this controls the cgroup setup, on Windows/macOS it
    /// controls the app-path exclusions. The per-OS PID/path lists are
    /// managed via the dedicated methods below; this is just the
    /// master switch.
    async fn set_split_tunnel_state(&self, enabled: bool) -> Result<(), IntegrationError>;
    /// Add an app to the Windows/macOS split-tunnel exclusion list. The
    /// daemon stores this in `Settings.split_tunnel.apps` and surfaces
    /// updates via `DaemonEvent::Settings`. Returns the daemon's error
    /// on Linux (which uses PIDs, not app paths).
    async fn add_split_tunnel_app(&self, path: &str) -> Result<(), IntegrationError>;
    /// Remove an app from the Windows/macOS split-tunnel exclusion list.
    async fn remove_split_tunnel_app(&self, path: &str) -> Result<(), IntegrationError>;
    /// Fetch the Linux split-tunnel PID list. PIDs aren't push-driven
    /// (no `DaemonEvent` variant), so callers refresh manually on
    /// sub-page entry and after each add/remove.
    async fn get_split_tunnel_processes(&self) -> Result<Vec<i32>, IntegrationError>;
    /// Add a Linux PID to the split-tunnel exclusion list.
    async fn add_split_tunnel_process(&self, pid: i32) -> Result<(), IntegrationError>;
    /// Remove a Linux PID from the split-tunnel exclusion list.
    async fn remove_split_tunnel_process(&self, pid: i32) -> Result<(), IntegrationError>;
}
