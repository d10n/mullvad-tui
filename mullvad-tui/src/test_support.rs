// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared test fixtures for `mullvad-tui` unit tests.
//!
//! Lives in its own module (rather than a per-file `#[cfg(test)]` block) so
//! both `app::tests` and `tui::tests` can drive the same `StubService`. Adding
//! a new shared fixture here is preferable to duplicating one across files.

use std::cell::RefCell;

use crate::integration::{
    AccessMethodId, AccessMethodSetting, AccountData, AccountInfo, AccountNumber, AppVersionInfo,
    Device, DeviceId, DeviceState, DnsOptions, IntegrationError, MullvadService,
    ObfuscationSettings, QuantumResistantState, RelayLocation, RelayOverride, Settings,
    TunnelState, VoucherSubmission,
};

pub(crate) struct StubService {
    pub status: Option<Result<TunnelState, IntegrationError>>,
    pub relays: Option<Result<Vec<RelayLocation>, IntegrationError>>,
    pub full_settings: RefCell<Settings>,
    pub account_data: RefCell<Option<AccountData>>,
    pub daemon_version: Result<String, IntegrationError>,
    pub app_version_info: Result<AppVersionInfo, IntegrationError>,
    pub connect_result: Result<bool, IntegrationError>,
    pub disconnect_result: Result<bool, IntegrationError>,
    pub reconnect_result: Result<bool, IntegrationError>,
    pub logout_result: Result<(), IntegrationError>,
    pub set_relay_calls: RefCell<Vec<String>>,
    pub set_relay_country_calls: RefCell<Vec<String>>,
    pub set_relay_city_calls: RefCell<Vec<(String, String)>>,
    pub set_multihop_calls: RefCell<Vec<bool>>,
    pub set_ip_version_preference_calls: RefCell<Vec<Option<talpid_types::net::IpVersion>>>,
    pub set_lan_calls: RefCell<Vec<bool>>,
    pub set_lockdown_calls: RefCell<Vec<bool>>,
    pub set_obfuscation_settings_calls: RefCell<Vec<ObfuscationSettings>>,
    pub get_account_data_calls: RefCell<Vec<AccountNumber>>,
    pub list_devices_result: RefCell<Result<Vec<Device>, IntegrationError>>,
    pub list_devices_calls: RefCell<Vec<AccountNumber>>,
    pub remove_device_calls: RefCell<Vec<(AccountNumber, DeviceId)>>,
    pub remove_device_result: Result<(), IntegrationError>,
    pub submit_voucher_result: RefCell<Result<VoucherSubmission, IntegrationError>>,
    pub submit_voucher_calls: RefCell<Vec<String>>,
    pub set_quantum_resistant_calls: RefCell<Vec<QuantumResistantState>>,
    pub set_ipv6_calls: RefCell<Vec<bool>>,
    pub set_mtu_calls: RefCell<Vec<Option<u16>>>,
    pub set_daita_enabled_calls: RefCell<Vec<bool>>,
    pub set_daita_direct_only_calls: RefCell<Vec<bool>>,
    pub set_dns_options_calls: RefCell<Vec<DnsOptions>>,
    pub set_auto_connect_calls: RefCell<Vec<bool>>,
    pub rotate_wireguard_key_calls: RefCell<u32>,
    pub current_api_access_method: RefCell<Option<Result<AccessMethodSetting, IntegrationError>>>,
    pub update_access_method_calls: RefCell<Vec<AccessMethodSetting>>,
    pub set_access_method_calls: RefCell<Vec<AccessMethodId>>,
    pub set_split_tunnel_state_calls: RefCell<Vec<bool>>,
    pub add_split_tunnel_app_calls: RefCell<Vec<String>>,
    pub remove_split_tunnel_app_calls: RefCell<Vec<String>>,
    pub split_tunnel_processes: RefCell<Vec<i32>>,
    pub add_split_tunnel_process_calls: RefCell<Vec<i32>>,
    pub remove_split_tunnel_process_calls: RefCell<Vec<i32>>,
    pub set_relay_override_calls: RefCell<Vec<RelayOverride>>,
    pub clear_all_relay_overrides_calls: RefCell<u32>,
}

impl Default for StubService {
    fn default() -> Self {
        Self {
            status: None,
            relays: None,
            full_settings: RefCell::default(),
            account_data: RefCell::default(),
            daemon_version: Ok("mullvad 0.0.0".to_string()),
            app_version_info: Ok(stub_version_info()),
            connect_result: Ok(true),
            disconnect_result: Ok(true),
            reconnect_result: Ok(true),
            logout_result: Ok(()),
            set_relay_calls: RefCell::default(),
            set_relay_country_calls: RefCell::default(),
            set_relay_city_calls: RefCell::default(),
            set_multihop_calls: RefCell::default(),
            set_ip_version_preference_calls: RefCell::default(),
            set_lan_calls: RefCell::default(),
            set_lockdown_calls: RefCell::default(),
            set_obfuscation_settings_calls: RefCell::default(),
            get_account_data_calls: RefCell::default(),
            list_devices_result: RefCell::new(Ok(Vec::new())),
            list_devices_calls: RefCell::default(),
            remove_device_calls: RefCell::default(),
            remove_device_result: Ok(()),
            submit_voucher_result: RefCell::new(Err(IntegrationError::Validation(
                "no voucher result seeded in stub".to_string(),
            ))),
            submit_voucher_calls: RefCell::default(),
            set_quantum_resistant_calls: RefCell::default(),
            set_ipv6_calls: RefCell::default(),
            set_mtu_calls: RefCell::default(),
            set_daita_enabled_calls: RefCell::default(),
            set_daita_direct_only_calls: RefCell::default(),
            set_dns_options_calls: RefCell::default(),
            set_auto_connect_calls: RefCell::default(),
            rotate_wireguard_key_calls: RefCell::default(),
            current_api_access_method: RefCell::default(),
            update_access_method_calls: RefCell::default(),
            set_access_method_calls: RefCell::default(),
            set_split_tunnel_state_calls: RefCell::default(),
            add_split_tunnel_app_calls: RefCell::default(),
            remove_split_tunnel_app_calls: RefCell::default(),
            split_tunnel_processes: RefCell::default(),
            add_split_tunnel_process_calls: RefCell::default(),
            remove_split_tunnel_process_calls: RefCell::default(),
            set_relay_override_calls: RefCell::default(),
            clear_all_relay_overrides_calls: RefCell::default(),
        }
    }
}

impl MullvadService for StubService {
    async fn get_status(&self) -> Result<TunnelState, IntegrationError> {
        self.status
            .clone()
            .unwrap_or_else(|| Ok(disconnected_state()))
    }

    async fn connect(&self) -> Result<bool, IntegrationError> {
        self.connect_result.clone()
    }

    async fn disconnect(&self) -> Result<bool, IntegrationError> {
        self.disconnect_result.clone()
    }

    async fn reconnect(&self) -> Result<bool, IntegrationError> {
        self.reconnect_result.clone()
    }

    async fn get_account(&self) -> Result<AccountInfo, IntegrationError> {
        Ok(AccountInfo {
            device: DeviceState::LoggedOut,
            data: None,
        })
    }

    async fn get_account_data(
        &self,
        account: AccountNumber,
    ) -> Result<AccountData, IntegrationError> {
        self.get_account_data_calls.borrow_mut().push(account);
        self.account_data.borrow().clone().ok_or_else(|| {
            IntegrationError::Validation("no account data seeded in stub".to_string())
        })
    }

    async fn login(&self, _account: AccountNumber) -> Result<(), IntegrationError> {
        Ok(())
    }

    async fn logout(&self) -> Result<(), IntegrationError> {
        self.logout_result.clone()
    }

    async fn list_devices(&self, account: AccountNumber) -> Result<Vec<Device>, IntegrationError> {
        self.list_devices_calls.borrow_mut().push(account);
        match &*self.list_devices_result.borrow() {
            Ok(list) => Ok(list.clone()),
            Err(error) => Err(error.clone()),
        }
    }

    async fn remove_device(
        &self,
        account: AccountNumber,
        device_id: DeviceId,
    ) -> Result<(), IntegrationError> {
        self.remove_device_calls
            .borrow_mut()
            .push((account, device_id));
        self.remove_device_result.clone()
    }

    async fn submit_voucher(&self, voucher: String) -> Result<VoucherSubmission, IntegrationError> {
        self.submit_voucher_calls.borrow_mut().push(voucher);
        match &*self.submit_voucher_result.borrow() {
            Ok(submission) => Ok(VoucherSubmission {
                time_added: submission.time_added,
                new_expiry: submission.new_expiry,
            }),
            Err(error) => Err(error.clone()),
        }
    }

    async fn list_relays(&self) -> Result<Vec<RelayLocation>, IntegrationError> {
        self.relays.clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn set_relay_location(&self, location: &str) -> Result<(), IntegrationError> {
        self.set_relay_calls.borrow_mut().push(location.to_string());
        Ok(())
    }

    async fn set_relay_country(&self, country_code: &str) -> Result<(), IntegrationError> {
        self.set_relay_country_calls
            .borrow_mut()
            .push(country_code.to_string());
        Ok(())
    }

    async fn set_relay_city(
        &self,
        country_code: &str,
        city_code: &str,
    ) -> Result<(), IntegrationError> {
        self.set_relay_city_calls
            .borrow_mut()
            .push((country_code.to_string(), city_code.to_string()));
        Ok(())
    }

    async fn set_multihop_enabled(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.set_multihop_calls.borrow_mut().push(enabled);
        Ok(())
    }

    async fn set_ip_version_preference(
        &self,
        version: Option<talpid_types::net::IpVersion>,
    ) -> Result<(), IntegrationError> {
        self.set_ip_version_preference_calls
            .borrow_mut()
            .push(version);
        Ok(())
    }

    async fn get_full_settings(&self) -> Result<Settings, IntegrationError> {
        Ok(self.full_settings.borrow().clone())
    }

    async fn set_lan(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.set_lan_calls.borrow_mut().push(enabled);
        Ok(())
    }

    async fn set_auto_connect(&self, enabled: bool) -> Result<(), IntegrationError> {
        // Mirror the daemon: writing the value updates the cached
        // settings so a follow-up `get_full_settings` reads back the
        // new value (matches the `set_dns_options` stub pattern).
        self.full_settings.borrow_mut().auto_connect = enabled;
        self.set_auto_connect_calls.borrow_mut().push(enabled);
        Ok(())
    }

    async fn set_dns_options(&self, options: DnsOptions) -> Result<(), IntegrationError> {
        // Mirror the daemon: writing options updates the cached settings
        // so a follow-up `get_full_settings` reads back the new value.
        self.full_settings.borrow_mut().tunnel_options.dns_options = options.clone();
        self.set_dns_options_calls.borrow_mut().push(options);
        Ok(())
    }

    async fn get_daemon_version(&self) -> Result<String, IntegrationError> {
        self.daemon_version.clone()
    }

    async fn get_app_version_info(&self) -> Result<AppVersionInfo, IntegrationError> {
        self.app_version_info.clone()
    }

    async fn set_lockdown_mode(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.set_lockdown_calls.borrow_mut().push(enabled);
        Ok(())
    }

    async fn set_obfuscation_settings(
        &self,
        settings: ObfuscationSettings,
    ) -> Result<(), IntegrationError> {
        self.set_obfuscation_settings_calls
            .borrow_mut()
            .push(settings);
        Ok(())
    }

    async fn set_quantum_resistant_tunnel(
        &self,
        state: QuantumResistantState,
    ) -> Result<(), IntegrationError> {
        self.set_quantum_resistant_calls.borrow_mut().push(state);
        Ok(())
    }

    async fn set_enable_ipv6(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.set_ipv6_calls.borrow_mut().push(enabled);
        Ok(())
    }

    async fn set_wireguard_mtu(&self, mtu: Option<u16>) -> Result<(), IntegrationError> {
        self.set_mtu_calls.borrow_mut().push(mtu);
        Ok(())
    }

    async fn set_enable_daita(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.set_daita_enabled_calls.borrow_mut().push(enabled);
        Ok(())
    }

    async fn set_daita_direct_only(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.set_daita_direct_only_calls.borrow_mut().push(enabled);
        Ok(())
    }

    async fn rotate_wireguard_key(&self) -> Result<(), IntegrationError> {
        *self.rotate_wireguard_key_calls.borrow_mut() += 1;
        Ok(())
    }

    async fn get_current_api_access_method(&self) -> Result<AccessMethodSetting, IntegrationError> {
        match self.current_api_access_method.borrow().as_ref() {
            Some(Ok(setting)) => Ok(setting.clone()),
            Some(Err(error)) => Err(error.clone()),
            None => Err(IntegrationError::Validation(
                "no current api access method seeded in stub".to_string(),
            )),
        }
    }

    async fn update_access_method(
        &self,
        setting: AccessMethodSetting,
    ) -> Result<(), IntegrationError> {
        self.update_access_method_calls.borrow_mut().push(setting);
        Ok(())
    }

    async fn set_access_method(&self, id: AccessMethodId) -> Result<(), IntegrationError> {
        self.set_access_method_calls.borrow_mut().push(id);
        Ok(())
    }

    async fn set_split_tunnel_state(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.set_split_tunnel_state_calls.borrow_mut().push(enabled);
        // Mirror in the settings cache so the next read sees it. The
        // `split_tunnel` field only exists on Win/macOS/Androidl on
        // Linux the daemon stores this state outside `Settings`, so the
        // stub just records the call.
        #[cfg(any(target_os = "windows", target_os = "macos", target_os = "android"))]
        {
            self.full_settings
                .borrow_mut()
                .split_tunnel
                .enable_exclusions = enabled;
        }
        Ok(())
    }

    async fn add_split_tunnel_app(&self, path: &str) -> Result<(), IntegrationError> {
        self.add_split_tunnel_app_calls
            .borrow_mut()
            .push(path.to_string());
        // Cache-update is desktop-only; on Linux the field doesn't
        // exist. The daemon would reject the call there anyway.
        #[cfg(any(target_os = "windows", target_os = "macos", target_os = "android"))]
        {
            self.full_settings
                .borrow_mut()
                .split_tunnel
                .apps
                .insert(path.to_string().into());
        }
        Ok(())
    }

    async fn remove_split_tunnel_app(&self, path: &str) -> Result<(), IntegrationError> {
        self.remove_split_tunnel_app_calls
            .borrow_mut()
            .push(path.to_string());
        #[cfg(any(target_os = "windows", target_os = "macos", target_os = "android"))]
        {
            let target: mullvad_types::settings::SplitApp = path.to_string().into();
            self.full_settings
                .borrow_mut()
                .split_tunnel
                .apps
                .remove(&target);
        }
        Ok(())
    }

    async fn get_split_tunnel_processes(&self) -> Result<Vec<i32>, IntegrationError> {
        Ok(self.split_tunnel_processes.borrow().clone())
    }

    async fn add_split_tunnel_process(&self, pid: i32) -> Result<(), IntegrationError> {
        self.add_split_tunnel_process_calls.borrow_mut().push(pid);
        self.split_tunnel_processes.borrow_mut().push(pid);
        Ok(())
    }

    async fn remove_split_tunnel_process(&self, pid: i32) -> Result<(), IntegrationError> {
        self.remove_split_tunnel_process_calls
            .borrow_mut()
            .push(pid);
        self.split_tunnel_processes
            .borrow_mut()
            .retain(|p| *p != pid);
        Ok(())
    }

    async fn set_relay_override(
        &self,
        relay_override: RelayOverride,
    ) -> Result<(), IntegrationError> {
        self.set_relay_override_calls
            .borrow_mut()
            .push(relay_override.clone());
        // Mirror the daemon: an empty override removes the existing
        // entry; otherwise it adds or replaces by hostname.
        let mut settings = self.full_settings.borrow_mut();
        let existing = settings
            .relay_overrides
            .iter()
            .position(|elem| elem.hostname == relay_override.hostname);
        match existing {
            None => {
                if !relay_override.is_empty() {
                    settings.relay_overrides.push(relay_override);
                }
            }
            Some(index) => {
                if relay_override.is_empty() {
                    settings.relay_overrides.swap_remove(index);
                } else {
                    settings.relay_overrides[index] = relay_override;
                }
            }
        }
        Ok(())
    }

    async fn clear_all_relay_overrides(&self) -> Result<(), IntegrationError> {
        *self.clear_all_relay_overrides_calls.borrow_mut() += 1;
        self.full_settings.borrow_mut().relay_overrides.clear();
        Ok(())
    }
}

pub(crate) fn stub_version_info() -> AppVersionInfo {
    AppVersionInfo {
        current_version_supported: true,
        #[cfg(not(target_os = "android"))]
        suggested_upgrade: None,
    }
}

/// Build a minimal `TunnelState::Disconnected` for stubs and assertions.
/// `Disconnected` is the easiest variant to construct (only `Option`s and
/// a bool), so prefer it whenever a test only needs *some* variant.
pub(crate) fn disconnected_state() -> TunnelState {
    TunnelState::Disconnected {
        location: None,
        #[cfg(not(target_os = "android"))]
        locked_down: false,
    }
}

/// Build a minimal `TunnelState::Connected` for tests asserting against
/// `is_connected()` predicates. Endpoint fields are filler; assertions
/// should only ever care about the variant tag, not the inner payload.
pub(crate) fn connected_state() -> TunnelState {
    use mullvad_types::features::FeatureIndicators;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use talpid_types::net::{Endpoint, TransportProtocol, TunnelEndpoint};

    TunnelState::Connected {
        endpoint: TunnelEndpoint {
            endpoint: Endpoint {
                address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 51820),
                protocol: TransportProtocol::Udp,
            },
            quantum_resistant: false,
            obfuscation: None,
            entry_endpoint: None,
            tunnel_interface: None,
            daita: false,
        },
        location: None,
        feature_indicators: FeatureIndicators::default(),
    }
}

/// Build a `TunnelState::Error`. Cause is filler; assertions should rely on
/// `is_in_error_state()`, not the inner contents.
pub(crate) fn error_state() -> TunnelState {
    use talpid_types::tunnel::{ErrorState, ErrorStateCause};
    TunnelState::Error(ErrorState::new(ErrorStateCause::IsOffline, None))
}
