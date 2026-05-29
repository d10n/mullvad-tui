// SPDX-License-Identifier: GPL-3.0-or-later

//! Relay-selection state and methods on `App`. Holds the
//! [`CurrentRelaySelection`] projection, the obfuscation cycle helper,
//! and every method that writes the relay constraint, multihop flag, or
//! anti-censorship knobs through a [`MullvadService`].

use std::ops::RangeInclusive;

use super::{App, Operation};
use crate::integration::{
    Constraint, GeographicLocationConstraint, IntegrationError, LocationConstraint, MullvadService,
    ObfuscationSettings, RelayOverride, RelaySettings, SelectedObfuscation,
};

/// Valid TCP/UDP port range for obfuscation overrides. Single source
/// of truth for the bounds - port 0 is reserved by the IANA so `1` is
/// the lower bound; `u16::MAX` is `65535`. Used by the port-input
/// prompt + parser in `tui/mod.rs` so the prompt text and parser
/// validation can't drift apart.
pub const ANTI_CENSORSHIP_PORT_RANGE: RangeInclusive<u16> = 1..=u16::MAX;

/// True if this obfuscation mode has a configurable port. Modes without one
/// (`Off`, `Auto`, `Quic`, `Lwo`) reject `App::set_anti_censorship_port`
/// with a validation error; the TUI uses this predicate to gate the
/// port-input modal and surface a helpful notification instead.
pub(crate) fn mode_has_configurable_port(mode: SelectedObfuscation) -> bool {
    matches!(
        mode,
        SelectedObfuscation::Udp2Tcp
            | SelectedObfuscation::Shadowsocks
            | SelectedObfuscation::WireguardPort
    )
}

/// What the daemon currently has configured as its relay constraint, projected
/// from cached `Settings.relay_settings`. Push-driven via
/// `DaemonEvent::Settings`, so this is the truth - not the user's last
/// in-session click. Borrows from the cached settings; the `'a` lifetime
/// matches `App::settings()`.
///
/// `Unknown` covers the boot window when settings haven't been fetched yet
/// (or after a daemon disconnect that wiped the cache). Renderer treats it
/// as "not yet loaded" rather than equating it to `Any`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CurrentRelaySelection<'a> {
    Unknown,
    Any,
    Country(&'a str),
    City { country: &'a str, city: &'a str },
    Hostname(&'a str),
    CustomList,
    CustomTunnel,
}

/// Project a `Constraint<LocationConstraint>` into the renderer-facing
/// [`CurrentRelaySelection`].
fn location_to_selection(location: &Constraint<LocationConstraint>) -> CurrentRelaySelection<'_> {
    match location {
        Constraint::Any => CurrentRelaySelection::Any,
        Constraint::Only(LocationConstraint::CustomList { .. }) => {
            CurrentRelaySelection::CustomList
        }
        Constraint::Only(LocationConstraint::Location(geo)) => match geo {
            GeographicLocationConstraint::Country(code) => {
                CurrentRelaySelection::Country(code.as_str())
            }
            GeographicLocationConstraint::City(country, city) => CurrentRelaySelection::City {
                country: country.as_str(),
                city: city.as_str(),
            },
            GeographicLocationConstraint::Hostname(_, _, hostname) => {
                CurrentRelaySelection::Hostname(hostname.as_str())
            }
        },
    }
}

impl App {
    pub fn current_relay_selection(&self) -> CurrentRelaySelection<'_> {
        let Some(settings) = self.settings.as_ref() else {
            return CurrentRelaySelection::Unknown;
        };
        let constraints = match &settings.relay_settings {
            RelaySettings::CustomTunnelEndpoint(_) => return CurrentRelaySelection::CustomTunnel,
            RelaySettings::Normal(constraints) => constraints,
        };
        location_to_selection(&constraints.location)
    }

    /// What the daemon currently has configured as its multihop entry node
    /// constraint, projected from cached
    /// `wireguard_constraints.entry_location`. Mirrors
    /// [`Self::current_relay_selection`] (which reads the exit `location`).
    /// `Unknown` until settings load; `CustomTunnel` for a custom tunnel
    /// endpoint (which carries no entry constraint).
    pub fn current_entry_relay_selection(&self) -> CurrentRelaySelection<'_> {
        let Some(settings) = self.settings.as_ref() else {
            return CurrentRelaySelection::Unknown;
        };
        let constraints = match &settings.relay_settings {
            RelaySettings::CustomTunnelEndpoint(_) => return CurrentRelaySelection::CustomTunnel,
            RelaySettings::Normal(constraints) => constraints,
        };
        location_to_selection(&constraints.wireguard_constraints.entry_location)
    }

    /// True if the daemon has multihop enabled in cached `Settings`. False
    /// when settings are unloaded or the relay config is `CustomTunnelEndpoint`
    /// (multihop only applies to `Normal` constraints).
    pub fn is_multihop_enabled(&self) -> bool {
        match self.settings.as_ref().map(|s| &s.relay_settings) {
            Some(RelaySettings::Normal(c)) => c.wireguard_constraints.use_multihop,
            _ => false,
        }
    }

    /// True when DAITA is forcing the multihop entry node, making a
    /// user-chosen `entry_location` a no-op. Happens when multihop is on,
    /// DAITA is enabled, and DAITA's `use_multihop_if_necessary` is set (i.e.
    /// "Direct only" is off): in that mode the daemon may insert its own
    /// entry hop, overriding the configured entry. The relay-selector page
    /// reads this to show an explanatory message instead of an entry list
    /// (mirrors the desktop GUI's `showDisabledEntrySelection`).
    pub fn daita_overrides_entry(&self) -> bool {
        self.is_multihop_enabled()
            && self.settings.as_ref().is_some_and(|s| {
                let daita = &s.tunnel_options.wireguard.daita;
                daita.enabled && daita.use_multihop_if_necessary
            })
    }

    /// Set the port for a specific obfuscation mode (without changing the
    /// currently selected mode). `port = None` means `Constraint::Any` -
    /// daemon picks. Returns `Validation` for modes that have no port (Off,
    /// Auto, Quic); use [`mode_has_configurable_port`] to gate before calling.
    pub async fn set_anti_censorship_port<S: MullvadService>(
        &mut self,
        service: &S,
        mode: SelectedObfuscation,
        port: Option<u16>,
    ) -> Result<(), IntegrationError> {
        use mullvad_types::constraints::Constraint;

        if !mode_has_configurable_port(mode) {
            let error = IntegrationError::Validation(format!(
                "anti-censorship mode '{mode}' has no configurable port"
            ));
            self.set_operation_failed(Operation::SetAntiCensorshipPort, &error);
            return Err(error);
        }

        let mut next = self
            .settings
            .as_ref()
            .map(|s| s.obfuscation_settings.clone())
            .unwrap_or_default();
        let constraint = Constraint::from(port);
        match mode {
            SelectedObfuscation::Udp2Tcp => next.udp2tcp.port = constraint,
            SelectedObfuscation::Shadowsocks => next.shadowsocks.port = constraint,
            SelectedObfuscation::WireguardPort => next.wireguard_port = port.into(),
            // Filtered above; these arms are unreachable but the compiler
            // requires exhaustiveness over the upstream enum.
            SelectedObfuscation::Off
            | SelectedObfuscation::Auto
            | SelectedObfuscation::Quic
            | SelectedObfuscation::Lwo => {
                unreachable!("gated by mode_has_configurable_port")
            }
        }
        self.start_settings_push_op(Operation::SetAntiCensorshipPort, async move || {
            service.set_obfuscation_settings(next).await
        })
        .await
    }

    /// Set the anti-censorship mode directly. Per-mode port settings
    /// inside `ObfuscationSettings` are preserved.
    pub async fn set_anti_censorship_mode<S: MullvadService>(
        &mut self,
        service: &S,
        mode: SelectedObfuscation,
    ) -> Result<(), IntegrationError> {
        let current = self
            .settings
            .as_ref()
            .map(|s| s.obfuscation_settings.clone())
            .unwrap_or_default();
        let next = ObfuscationSettings {
            selected_obfuscation: mode,
            ..current
        };
        self.start_settings_push_op(Operation::SetAntiCensorshipMode, async move || {
            service.set_obfuscation_settings(next).await
        })
        .await
    }

    pub async fn select_relay<S: MullvadService>(
        &mut self,
        service: &S,
        relay_label: &str,
    ) -> Result<(), IntegrationError> {
        // Two-stage via Settings push: the daemon emits a settings
        // change after accepting the new relay constraint, which both
        // updates the cached `RelaySettings` (so `current_relay_selection()`
        // reflects the new constraint) and resolves this op to Success.
        let label = relay_label.to_string();
        self.start_settings_push_op(Operation::SelectRelay, async move || {
            service.set_relay_location(&label).await
        })
        .await
    }

    /// Coarser-grained sibling of [`Self::select_relay`]: writes a
    /// country-level location constraint so the daemon picks any active
    /// relay in `country_code`. Two-stage via Settings push.
    pub async fn select_relay_country<S: MullvadService>(
        &mut self,
        service: &S,
        country_code: &str,
    ) -> Result<(), IntegrationError> {
        let code = country_code.to_string();
        self.start_settings_push_op(Operation::SelectRelayCountry, async move || {
            service.set_relay_country(&code).await
        })
        .await
    }

    /// City-level sibling of [`Self::select_relay_country`].
    pub async fn select_relay_city<S: MullvadService>(
        &mut self,
        service: &S,
        country_code: &str,
        city_code: &str,
    ) -> Result<(), IntegrationError> {
        let cc = country_code.to_string();
        let cit = city_code.to_string();
        self.start_settings_push_op(Operation::SelectRelayCity, async move || {
            service.set_relay_city(&cc, &cit).await
        })
        .await
    }

    /// Multihop entry node sibling of [`Self::select_relay`]: writes
    /// `wireguard_constraints.entry_location` by hostname. Only meaningful
    /// when multihop is enabled. Two-stage via Settings push.
    pub async fn select_entry_relay<S: MullvadService>(
        &mut self,
        service: &S,
        relay_label: &str,
    ) -> Result<(), IntegrationError> {
        let label = relay_label.to_string();
        self.start_settings_push_op(Operation::SelectEntryRelay, async move || {
            service.set_entry_location(&label).await
        })
        .await
    }

    /// Country-level sibling of [`Self::select_entry_relay`].
    pub async fn select_entry_relay_country<S: MullvadService>(
        &mut self,
        service: &S,
        country_code: &str,
    ) -> Result<(), IntegrationError> {
        let code = country_code.to_string();
        self.start_settings_push_op(Operation::SelectEntryRelayCountry, async move || {
            service.set_entry_country(&code).await
        })
        .await
    }

    /// City-level sibling of [`Self::select_entry_relay`].
    pub async fn select_entry_relay_city<S: MullvadService>(
        &mut self,
        service: &S,
        country_code: &str,
        city_code: &str,
    ) -> Result<(), IntegrationError> {
        let cc = country_code.to_string();
        let cit = city_code.to_string();
        self.start_settings_push_op(Operation::SelectEntryRelayCity, async move || {
            service.set_entry_city(&cc, &cit).await
        })
        .await
    }

    /// Flip the daemon's multihop flag. Reads current state from the
    /// cached `Settings` (defaulting to `false` when the cache is empty),
    /// writes the negation, waits for the matching settings push.
    pub async fn toggle_multihop<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        let next = !self.is_multihop_enabled();
        self.start_settings_push_op(Operation::ToggleMultihop, async || {
            service.set_multihop_enabled(next).await
        })
        .await
    }

    /// Read the currently-selected device IP version from the cached
    /// `Settings` (`wireguard_constraints.ip_version`). `None` means
    /// the constraint is `Any` - daemon picks at connect time.
    pub fn current_ip_version_preference(&self) -> Option<talpid_types::net::IpVersion> {
        use mullvad_types::relay_constraints::RelaySettings;
        let RelaySettings::Normal(c) = &self.settings()?.relay_settings else {
            return None;
        };
        c.wireguard_constraints.ip_version.option()
    }

    /// Set the daemon's `wireguard_constraints.ip_version` constraint.
    /// `None` clears the constraint (Automatic - daemon picks); `Some`
    /// pins to a specific IP version. Backs the `Device IP version`
    /// radio group on the VPN settings page.
    pub async fn set_ip_version_preference<S: MullvadService>(
        &mut self,
        service: &S,
        version: Option<talpid_types::net::IpVersion>,
    ) -> Result<(), IntegrationError> {
        self.start_settings_push_op(Operation::SetIpVersionPreference, async || {
            service.set_ip_version_preference(version).await
        })
        .await
    }

    /// Borrow the daemon's currently-configured relay overrides from the
    /// cached `Settings`. Returns an empty slice when settings haven't
    /// loaded yet so the renderer can treat "loading" and "no overrides
    /// configured" identically.
    pub fn relay_overrides(&self) -> &[RelayOverride] {
        match self.settings.as_ref() {
            Some(s) => &s.relay_overrides,
            None => &[],
        }
    }

    /// Submit a per-hostname IPv4/IPv6 in-address override. Passing a
    /// [`RelayOverride`] with both addresses `None` removes the existing
    /// override for that hostname (the daemon's `set_relay_override`
    /// swap-removes empties); otherwise it adds or replaces. Two-stage
    /// via Settings push.
    pub async fn set_relay_override<S: MullvadService>(
        &mut self,
        service: &S,
        relay_override: RelayOverride,
    ) -> Result<(), IntegrationError> {
        self.start_settings_push_op(Operation::SetRelayOverride, async move || {
            service.set_relay_override(relay_override.clone()).await
        })
        .await
    }

    /// Remove the override for `hostname`, if any. Built atop
    /// [`Self::set_relay_override`] with an empty override - the
    /// daemon's swap-remove-on-empty rule does the rest.
    pub async fn remove_relay_override<S: MullvadService>(
        &mut self,
        service: &S,
        hostname: String,
    ) -> Result<(), IntegrationError> {
        self.set_relay_override(service, RelayOverride::empty(hostname))
            .await
    }

    /// Wipe every relay override. Backs the `[Clear all]` button on
    /// the Server IP overrides sub-page. Two-stage via Settings push.
    pub async fn clear_relay_overrides<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        self.start_settings_push_op(Operation::ClearRelayOverrides, async || {
            service.clear_all_relay_overrides().await
        })
        .await
    }
}
