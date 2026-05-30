// SPDX-License-Identifier: GPL-3.0-or-later

//! Settings cache accessors and the toggle/set methods that write
//! daemon-wide knobs (LAN access, lockdown, quantum-resistant tunnel,
//! IPv6, MTU, DAITA). Each toggle reads the cached `Settings`, writes
//! the inverse via the [`MullvadService`], and re-fetches the cache so
//! the next render reflects the change without waiting for the
//! `DaemonEvent::Settings` push.

use std::{net::IpAddr, ops::RangeInclusive};

use super::{App, Operation, settings_toggle};
use crate::integration::{
    AccessMethodId, AccessMethodSetting, DefaultDnsOptions, DnsOptions, DnsState, IntegrationError,
    MullvadService, Settings,
};

/// Daemon-accepted WireGuard MTU range. Used by the App-side validator
/// in [`App::set_mtu`] and by the inline MTU input pill on the VPN
/// settings sub-page (`tui::pages::settings`). Single source of truth
/// for the lower/upper bounds so the prompt text and the validator
/// can't drift apart.
pub const WIREGUARD_MTU_RANGE: RangeInclusive<u16> = 1280..=1420;

/// Names the six [`DefaultDnsOptions`] booleans without leaking strings
/// through the App layer. A field-pointer-style enum: `kind.read(opts)`
/// borrows the relevant `bool`, `kind.write(&mut opts, value)` flips it.
/// Used by [`App::toggle_dns_blocker`] so the per-blocker UI buttons
/// dispatch through one method rather than six.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DnsBlocker {
    Ads,
    Trackers,
    Malware,
    AdultContent,
    Gambling,
    SocialMedia,
}

impl DnsBlocker {
    /// Read this blocker's bool out of [`DefaultDnsOptions`].
    pub fn read(self, opts: &DefaultDnsOptions) -> bool {
        match self {
            Self::Ads => opts.block_ads,
            Self::Trackers => opts.block_trackers,
            Self::Malware => opts.block_malware,
            Self::AdultContent => opts.block_adult_content,
            Self::Gambling => opts.block_gambling,
            Self::SocialMedia => opts.block_social_media,
        }
    }

    /// Mutating projection: set this blocker's bool on [`DefaultDnsOptions`].
    pub fn write(self, opts: &mut DefaultDnsOptions, value: bool) {
        match self {
            Self::Ads => opts.block_ads = value,
            Self::Trackers => opts.block_trackers = value,
            Self::Malware => opts.block_malware = value,
            Self::AdultContent => opts.block_adult_content = value,
            Self::Gambling => opts.block_gambling = value,
            Self::SocialMedia => opts.block_social_media = value,
        }
    }

    /// User-facing label, shared by the toggle row and by the parent
    /// VPN-settings row's "Active" summary.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ads => "Ads",
            Self::Trackers => "Trackers",
            Self::Malware => "Malware",
            Self::AdultContent => "Adult content",
            Self::Gambling => "Gambling",
            Self::SocialMedia => "Social media",
        }
    }
}

/// Iteration order for the DNS-blockers sub-page rows. Stable so
/// the visual layout doesn't shift across releases.
pub const DNS_BLOCKERS: [DnsBlocker; 6] = [
    DnsBlocker::Ads,
    DnsBlocker::Trackers,
    DnsBlocker::Malware,
    DnsBlocker::AdultContent,
    DnsBlocker::Gambling,
    DnsBlocker::SocialMedia,
];

impl App {
    /// Cached daemon `Settings`. Populated at startup and refreshed by push.
    pub fn settings(&self) -> Option<&Settings> {
        self.settings.as_ref()
    }

    /// Apply a settings update (push from `DaemonEvent::Settings` or initial
    /// fetch). All projection getters that read settings see the new value
    /// immediately on the next render frame. Resolves any pending
    /// [`PendingPushMatch::Settings`] op so the toggle/setter that
    /// triggered this push flips from `Running` to `Success` after the
    /// cache is updated (renderer therefore sees the new value before
    /// status flips).
    pub fn set_settings(&mut self, settings: Settings) {
        self.settings = Some(settings);
        self.resolve_pending_for_settings();
    }

    /// Populate the [`Settings`] cache from the daemon. Tests use this
    /// to seed `App.settings` before exercising toggles; production
    /// startup primes the cache via `App::resync` directly and keeps it
    /// fresh through the [`PendingPushMatch::Settings`] two-stage driver
    /// in `connection.rs`.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "test-only fixture seeder; production startup uses resync directly"
        )
    )]
    pub async fn refresh_full_settings<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        self.run_operation(Operation::RefreshSettings, async |app| {
            app.settings = Some(service.get_full_settings().await?);
            Ok(())
        })
        .await
    }

    // Each `toggle_*` / `set_*` method below uses the two-stage
    // `start_push_op_unit(PendingPushMatch::Settings)` driver. The
    // RPC tells the daemon to update settings; `OperationStatus`
    // stays `Running` until the matching `DaemonEvent::Settings`
    // push arrives, at which point `App::set_settings` updates the
    // cache and resolves the pending entry to `Success`. No trailing
    // `refresh_full_settings` pull - the push path is the source of
    // truth.
    pub async fn toggle_lan<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        settings_toggle!(self, service, ToggleLan, |s| s.allow_lan, set_lan)
    }

    /// Flip the daemon's `auto_connect` flag - whether the daemon
    /// auto-establishes the tunnel on its own start. Reads cached
    /// state (defaulting to `false` when settings haven't loaded),
    /// sends the inverse, waits for the matching settings push.
    pub async fn toggle_auto_connect<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        settings_toggle!(
            self,
            service,
            ToggleAutoConnect,
            |s| s.auto_connect,
            set_auto_connect
        )
    }

    pub async fn toggle_lockdown<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        settings_toggle!(
            self,
            service,
            ToggleLockdown,
            |s| s.lockdown_mode,
            set_lockdown_mode
        )
    }

    /// Flip the WireGuard quantum-resistant tunnel mode. Reads the
    /// current cached value, sends the inverse to the daemon, waits
    /// for the matching settings push.
    pub async fn toggle_quantum_resistant<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        use crate::integration::QuantumResistantState;
        let enabled = self.settings.as_ref().is_some_and(|settings| {
            settings
                .tunnel_options
                .wireguard
                .quantum_resistant
                .enabled()
        });
        let next = if enabled {
            QuantumResistantState::Off
        } else {
            QuantumResistantState::On
        };
        self.start_settings_push_op(Operation::ToggleQuantumResistant, async || {
            service.set_quantum_resistant_tunnel(next).await
        })
        .await
    }

    /// Flip in-tunnel IPv6. Reads the current cached value, inverts,
    /// sends to daemon, waits for the matching settings push.
    pub async fn toggle_ipv6<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        settings_toggle!(
            self,
            service,
            ToggleIpv6,
            |s| s.tunnel_options.generic.enable_ipv6,
            set_enable_ipv6
        )
    }

    /// Set (or clear) the WireGuard MTU override. Validates against
    /// the daemon's accepted range upfront so the user gets a fast
    /// "out of range" error instead of a generic gRPC failure.
    pub async fn set_mtu<S: MullvadService>(
        &mut self,
        service: &S,
        mtu: Option<u16>,
    ) -> Result<(), IntegrationError> {
        if let Some(value) = mtu
            && !WIREGUARD_MTU_RANGE.contains(&value)
        {
            let error = IntegrationError::Validation(format!(
                "MTU {value} is outside the valid range {}..={}",
                WIREGUARD_MTU_RANGE.start(),
                WIREGUARD_MTU_RANGE.end(),
            ));
            self.set_operation_failed(Operation::SetMtu, &error);
            return Err(error);
        }
        self.start_settings_push_op(Operation::SetMtu, async || {
            service.set_wireguard_mtu(mtu).await
        })
        .await
    }

    /// Flip the master DAITA enable flag. Setting the daemon's
    /// `enable_daita` directly via the dedicated RPC (not via
    /// `set_daita_settings`) keeps the call atomic.
    pub async fn toggle_daita_enabled<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        settings_toggle!(
            self,
            service,
            ToggleDaita,
            |s| s.tunnel_options.wireguard.daita.enabled,
            set_enable_daita
        )
    }

    /// Flip DAITA's "Direct only" mode. The user-visible wording is
    /// the inverse of the daemon flag (`use_multihop_if_necessary`):
    /// "Direct only" = on means `use_multihop_if_necessary` = off.
    /// We invert here so callers can pass the user-visible boolean.
    pub async fn toggle_daita_direct_only<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        // "Direct only" = !use_multihop_if_necessary
        let direct_only = self.settings.as_ref().is_some_and(|settings| {
            !settings
                .tunnel_options
                .wireguard
                .daita
                .use_multihop_if_necessary
        });
        self.start_settings_push_op(Operation::ToggleDaitaDirectOnly, async || {
            service.set_daita_direct_only(!direct_only).await
        })
        .await
    }

    /// Read this blocker's current value from cached [`DefaultDnsOptions`].
    /// Returns `false` when settings haven't been fetched yet - the
    /// renderer treats that as "off" so the user sees a sensible default
    /// before the cache primes.
    pub fn dns_blocker_enabled(&self, blocker: DnsBlocker) -> bool {
        self.settings
            .as_ref()
            .is_some_and(|s| blocker.read(&s.tunnel_options.dns_options.default_options))
    }

    /// Read the full cached `DefaultDnsOptions`. The DNS-blockers
    /// sub-page uses this to decide which rows show "On" vs "Off"
    /// without making six separate borrows.
    pub fn dns_blockers(&self) -> Option<&DefaultDnsOptions> {
        self.settings
            .as_ref()
            .map(|s| &s.tunnel_options.dns_options.default_options)
    }

    /// Clone the cached `DnsOptions` for read-modify-write callers.
    /// Returns the default when settings haven't loaded yet so writers
    /// can always proceed with a fresh struct.
    fn cached_dns_options(&self) -> DnsOptions {
        self.settings
            .as_ref()
            .map(|s| s.tunnel_options.dns_options.clone())
            .unwrap_or_default()
    }

    /// Set a single DNS content blocker to `enabled`. Reads the cached
    /// [`DnsOptions`] (full struct - daemon's `set_dns_options` RPC
    /// requires the whole payload), flips the chosen field, sends the
    /// modified struct back, and refreshes the cache. State and
    /// `custom_options` are preserved verbatim.
    pub async fn set_dns_blocker<S: MullvadService>(
        &mut self,
        service: &S,
        blocker: DnsBlocker,
        enabled: bool,
    ) -> Result<(), IntegrationError> {
        let mut next = self.cached_dns_options();
        blocker.write(&mut next.default_options, enabled);
        self.start_settings_push_op(Operation::ToggleDnsBlocker, async move || {
            service.set_dns_options(next).await
        })
        .await
    }

    /// True when the daemon's DNS state is `Custom` (i.e. custom DNS
    /// servers are in effect). False when it's `Default` or settings
    /// haven't loaded yet.
    pub fn custom_dns_enabled(&self) -> bool {
        self.settings
            .as_ref()
            .is_some_and(|s| matches!(s.tunnel_options.dns_options.state, DnsState::Custom))
    }

    /// Borrow the cached custom DNS server list. Empty slice when
    /// settings haven't loaded - same shape the renderer wants for
    /// "no rows yet".
    pub fn custom_dns_addresses(&self) -> &[IpAddr] {
        match self.settings.as_ref() {
            Some(s) => &s.tunnel_options.dns_options.custom_options.addresses,
            None => &[],
        }
    }

    /// Flip the daemon's DNS state between `Default` and `Custom`.
    /// Preserves `default_options` and `custom_options.addresses` -
    /// only the `state` discriminant changes - so toggling off and back
    /// on restores the user's existing list.
    pub async fn toggle_custom_dns<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        let mut next = self.cached_dns_options();
        next.state = match next.state {
            DnsState::Default => DnsState::Custom,
            DnsState::Custom => DnsState::Default,
        };
        self.start_settings_push_op(Operation::ToggleCustomDns, async move || {
            service.set_dns_options(next).await
        })
        .await
    }

    /// Append `addr` to the cached custom-DNS address list and send the
    /// modified [`DnsOptions`] to the daemon. Does *not* automatically
    /// switch state to `Custom` - the user toggles that explicitly so
    /// adding addresses while disabled stages them for later activation.
    /// Duplicate-address suppression is left to the daemon (or a future
    /// validation pass); for now we send what the user typed.
    pub async fn add_custom_dns<S: MullvadService>(
        &mut self,
        service: &S,
        addr: IpAddr,
    ) -> Result<(), IntegrationError> {
        let mut next = self.cached_dns_options();
        next.custom_options.addresses.push(addr);
        self.start_settings_push_op(Operation::AddCustomDns, async move || {
            service.set_dns_options(next).await
        })
        .await
    }

    /// Remove the address at `idx` from the cached custom-DNS list and
    /// send the modified [`DnsOptions`] to the daemon. Out-of-range
    /// indices return a `Validation` error - the renderer rebuilds the
    /// row -> index map every frame, so a stale activation can only
    /// happen if the list shrank between render and key handling
    /// (extremely unlikely, but handled defensively).
    pub async fn remove_custom_dns<S: MullvadService>(
        &mut self,
        service: &S,
        idx: usize,
    ) -> Result<(), IntegrationError> {
        let len = self.custom_dns_addresses().len();
        if idx >= len {
            let error = IntegrationError::Validation(format!(
                "custom DNS address index {idx} is out of range (have {len})"
            ));
            self.set_operation_failed(Operation::RemoveCustomDns, &error);
            return Err(error);
        }
        let mut next = self.cached_dns_options();
        // Defensive re-check: settings may have mutated between the
        // outer guard and here (push event).
        if idx < next.custom_options.addresses.len() {
            next.custom_options.addresses.remove(idx);
        }
        self.start_settings_push_op(Operation::RemoveCustomDns, async move || {
            service.set_dns_options(next).await
        })
        .await
    }

    /// Replace one address in the custom-DNS list at `idx`. Used by
    /// the per-row `[Edit]` button on the Custom DNS sub-page.
    /// Out-of-range `idx` returns a `Validation` error - same
    /// defensive pattern as [`Self::remove_custom_dns`].
    pub async fn replace_custom_dns<S: MullvadService>(
        &mut self,
        service: &S,
        idx: usize,
        addr: IpAddr,
    ) -> Result<(), IntegrationError> {
        let len = self.custom_dns_addresses().len();
        if idx >= len {
            let error = IntegrationError::Validation(format!(
                "custom DNS address index {idx} is out of range (have {len})"
            ));
            self.set_operation_failed(Operation::ReplaceCustomDns, &error);
            return Err(error);
        }
        let mut next = self.cached_dns_options();
        if idx < next.custom_options.addresses.len() {
            next.custom_options.addresses[idx] = addr;
        }
        self.start_settings_push_op(Operation::ReplaceCustomDns, async move || {
            service.set_dns_options(next).await
        })
        .await
    }

    /// Replace the entire custom-DNS address list. Currently used only
    /// by tests; if a "clear all" UI ever lands it can call this
    /// directly. Kept module-private until then.
    #[cfg(test)]
    pub(crate) async fn set_custom_dns_addresses<S: MullvadService>(
        &mut self,
        service: &S,
        addresses: Vec<IpAddr>,
    ) -> Result<(), IntegrationError> {
        self.run_operation(Operation::ReplaceCustomDns, async |app| {
            let mut next = app.cached_dns_options();
            next.custom_options = mullvad_types::settings::CustomDnsOptions { addresses };
            service.set_dns_options(next).await?;
            app.refresh_full_settings(service).await
        })
        .await
    }

    /// Refresh `App.current_api_access_id` from the daemon. The daemon
    /// has no `DaemonEvent` for "the active access method changed", so
    /// this is the only signal - called on entry to the API access
    /// sub-page and after each [`Self::set_active_access_method`].
    pub async fn refresh_current_access_method<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        self.run_operation(Operation::RefreshCurrentAccessMethod, async |app| {
            let setting = service.get_current_api_access_method().await?;
            app.current_api_access_id = Some(setting.get_id());
            Ok(())
        })
        .await
    }

    /// Toggle the `enabled` flag on the API access method with the given
    /// id. Looks up the cached `AccessMethodSetting` from
    /// `Settings.api_access_methods` (built-ins + custom), flips its
    /// `enabled`, and pushes the whole setting via
    /// [`MullvadService::update_access_method`]. Two-stage via a
    /// `Settings` push.
    pub async fn toggle_access_method<S: MullvadService>(
        &mut self,
        service: &S,
        id: AccessMethodId,
    ) -> Result<(), IntegrationError> {
        let mut setting: AccessMethodSetting = self
            .settings
            .as_ref()
            .and_then(|s| {
                s.api_access_methods
                    .iter()
                    .find(|m| m.get_id() == id)
                    .cloned()
            })
            .ok_or_else(|| {
                IntegrationError::Validation(format!("api access method not found: {id}"))
            })?;
        setting.enabled = !setting.enabled;
        self.start_settings_push_op(Operation::ToggleAccessMethod, async move || {
            service.update_access_method(setting.clone()).await
        })
        .await
    }

    /// Tell the daemon to switch to access method `id`. Unlike
    /// [`Self::toggle_access_method`], this writes a runtime selector
    /// (no `Settings` push), so we manually refetch the current method
    /// to keep the cached `*` marker honest.
    pub async fn set_active_access_method<S: MullvadService>(
        &mut self,
        service: &S,
        id: AccessMethodId,
    ) -> Result<(), IntegrationError> {
        self.run_operation(Operation::SetActiveAccessMethod, async |app| {
            // `run_operation` is `AsyncFnOnce` and `id` is not used again,
            // so move it in rather than clone. `AccessMethodId` is `Copy`
            // on tip-of-`main` (where a clone here would trip
            // `clippy::clone_on_copy`) and `Clone`-not-`Copy` on stable; a
            // move compiles cleanly on both.
            service.set_access_method(id).await?;
            let setting = service.get_current_api_access_method().await?;
            app.current_api_access_id = Some(setting.get_id());
            Ok(())
        })
        .await
    }

    /// True if split tunneling is currently enabled. Read from cached
    /// `Settings.split_tunnel.enable_exclusions` on Windows/macOS; on
    /// Linux the daemon stores this outside `Settings`, so the read
    /// returns `false` (the user can flip via the toggle regardless;
    /// the daemon won't push state back so we don't try to mirror it).
    pub fn split_tunnel_enabled(&self) -> bool {
        #[cfg(any(target_os = "windows", target_os = "macos", target_os = "android"))]
        {
            self.settings
                .as_ref()
                .is_some_and(|s| s.split_tunnel.enable_exclusions)
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "android")))]
        {
            false
        }
    }

    /// Cached Win/macOS split-tunnel app paths, projected to UTF-8
    /// strings (per-app `to_string` is fallible on non-UTF-8 paths;
    /// those are silently dropped from the rendered list - they can't
    /// be entered through the TUI's text-input modal anyway). Returns
    /// an empty list on Linux, where the field doesn't exist.
    pub fn split_tunnel_apps(&self) -> Vec<String> {
        #[cfg(any(target_os = "windows", target_os = "macos", target_os = "android"))]
        {
            self.settings
                .as_ref()
                .map(|s| {
                    s.split_tunnel
                        .apps
                        .iter()
                        .filter_map(|app| app.clone().to_string())
                        .collect()
                })
                .unwrap_or_default()
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "android")))]
        {
            Vec::new()
        }
    }

    /// Flip the split-tunnel master switch. Linux: cgroup setup
    /// on/off; Windows/macOS: app-path exclusion engine on/off. Uses
    /// `run_operation` (no push wait) since the Linux daemon doesn't
    /// emit a `SettingsChanged` event for this state - on Win/macOS
    /// the push lands a moment later and the cached `enable_exclusions`
    /// field flips on its own.
    pub async fn toggle_split_tunnel<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        let next = !self.split_tunnel_enabled();
        self.run_operation(Operation::ToggleSplitTunnel, async |_app| {
            service.set_split_tunnel_state(next).await
        })
        .await
    }

    /// Add a Win/macOS app path to the split-tunnel exclusion list.
    /// Two-stage via `Settings` push (the daemon's `Settings.split_tunnel.apps`
    /// is part of the cached settings tree).
    pub async fn add_split_tunnel_app<S: MullvadService>(
        &mut self,
        service: &S,
        path: String,
    ) -> Result<(), IntegrationError> {
        self.start_settings_push_op(Operation::AddSplitTunnelApp, async move || {
            service.add_split_tunnel_app(&path).await
        })
        .await
    }

    /// Remove a Win/macOS app path from the split-tunnel exclusion list.
    pub async fn remove_split_tunnel_app<S: MullvadService>(
        &mut self,
        service: &S,
        path: String,
    ) -> Result<(), IntegrationError> {
        self.start_settings_push_op(Operation::RemoveSplitTunnelApp, async move || {
            service.remove_split_tunnel_app(&path).await
        })
        .await
    }

    /// Refresh the cached Linux split-tunnel PID list. The daemon
    /// doesn't push changes for this, so callers fire it on sub-page
    /// entry and after each add/remove.
    pub async fn refresh_split_tunnel_pids<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        self.run_operation(Operation::RefreshSplitTunnelProcesses, async |app| {
            let pids = service.get_split_tunnel_processes().await?;
            app.split_tunnel_pids = Some(pids);
            Ok(())
        })
        .await
    }

    /// Add a Linux PID to the split-tunnel exclusion list, then
    /// re-fetch the list since there's no `DaemonEvent` for it.
    pub async fn add_split_tunnel_process<S: MullvadService>(
        &mut self,
        service: &S,
        pid: i32,
    ) -> Result<(), IntegrationError> {
        self.run_operation(Operation::AddSplitTunnelProcess, async |app| {
            service.add_split_tunnel_process(pid).await?;
            let pids = service.get_split_tunnel_processes().await?;
            app.split_tunnel_pids = Some(pids);
            Ok(())
        })
        .await
    }

    /// Remove a Linux PID from the split-tunnel exclusion list.
    pub async fn remove_split_tunnel_process<S: MullvadService>(
        &mut self,
        service: &S,
        pid: i32,
    ) -> Result<(), IntegrationError> {
        self.run_operation(Operation::RemoveSplitTunnelProcess, async |app| {
            service.remove_split_tunnel_process(pid).await?;
            let pids = service.get_split_tunnel_processes().await?;
            app.split_tunnel_pids = Some(pids);
            Ok(())
        })
        .await
    }
}
