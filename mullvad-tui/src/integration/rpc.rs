// SPDX-License-Identifier: GPL-3.0-or-later

use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use mullvad_management_interface::{Code, client::DaemonEvent};
use mullvad_types::{
    constraints::Constraint,
    device::DeviceState,
    relay_constraints::{
        GeographicLocationConstraint, LocationConstraint, ObfuscationSettings, RelaySettings,
    },
};
use tokio::sync::{Mutex, mpsc};

use super::{
    AccessMethodId, AccessMethodSetting, AccountData, AccountInfo, AccountNumber, AppEvent,
    AppVersionInfo, Device, DeviceId, DnsOptions, IntegrationError, MullvadService,
    QuantumResistantState, RelayLocation, RelayOverride, Settings, TunnelState, VoucherSubmission,
    project_relay_list, tolerant::TolerantClient,
};
use crate::logging::{LogEntry, LogSource};

const RPC_CLIENT_NAME: &str = "mullvad-tui";

const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

pub struct RpcMullvadService {
    client: Arc<Mutex<TolerantClient>>,
}

impl RpcMullvadService {
    pub async fn new() -> Result<Self, IntegrationError> {
        let client = TolerantClient::new().await?;
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
        })
    }

    /// Run a daemon RPC sequence under the shared client. If the operation
    /// returns a transport-level [`IntegrationError::Rpc`] (channel dropped,
    /// daemon restarted), re-establish the connection once and retry the
    /// whole closure. Validation errors (`IntegrationError::Validation`) and
    /// non-transport RPC errors propagate without retry. Concurrent callers
    /// serialize on the client mutex, so only the first observer of a
    /// dropped channel pays the reconnect cost.
    async fn with_client<F, T>(&self, mut op: F) -> Result<T, IntegrationError>
    where
        F: AsyncFnMut(&mut TolerantClient) -> Result<T, IntegrationError>,
    {
        let mut client = self.client.lock().await;
        match op(&mut client).await {
            Ok(value) => Ok(value),
            Err(IntegrationError::Rpc(ref error)) if is_transport_error(error) => {
                *client = TolerantClient::new().await?;
                op(&mut client).await
            }
            Err(error) => Err(error),
        }
    }

    /// Open a second daemon connection dedicated to streaming `events_listen`,
    /// spawn a task that converts daemon events into [`AppEvent`]s, and return
    /// the receiving end of the channel. The dedicated connection avoids
    /// holding the shared client mutex for the lifetime of the stream.
    ///
    /// The spawned task re-establishes the connection with exponential backoff
    /// if the stream errors or the daemon restarts; it only exits when the
    /// receiver is dropped.
    pub async fn spawn_event_listener() -> Result<mpsc::Receiver<AppEvent>, IntegrationError> {
        // Initial connection must succeed so that the caller knows whether
        // push events are wired before entering the TUI loop.
        let initial_client = TolerantClient::new().await?;
        let (tx, rx) = mpsc::channel(64);

        tokio::spawn(event_listener_loop(initial_client, tx));

        Ok(rx)
    }

    /// Open a third daemon connection dedicated to `log_listen` and
    /// spawn a task that forwards each line into the shared
    /// [`crate::logging::LogEntry`] channel as a
    /// [`crate::logging::LogSource::Daemon`] entry. The TUI's own
    /// tracing entries flow through the same channel from
    /// [`crate::logging::RingBufferLayer`], so the two streams
    /// intermix in the App's ring buffer in arrival order.
    ///
    /// Same dedicated-connection + reconnect-with-backoff pattern as
    /// [`Self::spawn_event_listener`]. Initial connection must
    /// succeed; subsequent disconnects retry transparently. Exits
    /// only when the receiver side of `log_tx` is dropped.
    pub async fn spawn_log_listener(
        log_tx: mpsc::Sender<LogEntry>,
    ) -> Result<(), IntegrationError> {
        let initial_client = TolerantClient::new().await?;
        tokio::spawn(log_listener_loop(initial_client, log_tx));
        Ok(())
    }
}

enum EventLoopOutcome {
    /// Receiver side of the channel was dropped - the TUI is shutting down.
    ReceiverDropped,
    /// Stream ended or errored - try to reconnect.
    Disconnected,
}

async fn event_listener_loop(mut client: TolerantClient, tx: mpsc::Sender<AppEvent>) {
    loop {
        match drain_events(&mut client, &tx).await {
            EventLoopOutcome::ReceiverDropped => return,
            EventLoopOutcome::Disconnected => client = reconnect_with_backoff().await,
        }
    }
}

/// Re-establish a [`TolerantClient`] connection. Sleeps between failed
/// attempts, doubling the wait each time (capped at
/// [`RECONNECT_BACKOFF_MAX`]). Loops until a connection succeeds; the
/// only other way out is the surrounding task being dropped.
async fn reconnect_with_backoff() -> TolerantClient {
    let mut backoff = RECONNECT_BACKOFF_INITIAL;
    loop {
        tokio::time::sleep(backoff).await;
        match TolerantClient::new().await {
            Ok(client) => return client,
            Err(_) => backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX),
        }
    }
}

async fn drain_events(
    client: &mut TolerantClient,
    tx: &mpsc::Sender<AppEvent>,
) -> EventLoopOutcome {
    let stream = match client.events_listen().await {
        Ok(stream) => stream,
        Err(_) => return EventLoopOutcome::Disconnected,
    };
    tokio::pin!(stream);
    while let Some(event) = stream.next().await {
        let Ok(event) = event else {
            return EventLoopOutcome::Disconnected;
        };
        if let Some(app_event) = translate_daemon_event(event)
            && tx.send(app_event).await.is_err()
        {
            return EventLoopOutcome::ReceiverDropped;
        }
    }
    EventLoopOutcome::Disconnected
}

async fn log_listener_loop(mut client: TolerantClient, tx: mpsc::Sender<LogEntry>) {
    loop {
        match drain_log_lines(&mut client, &tx).await {
            EventLoopOutcome::ReceiverDropped => return,
            EventLoopOutcome::Disconnected => client = reconnect_with_backoff().await,
        }
    }
}

async fn drain_log_lines(
    client: &mut TolerantClient,
    tx: &mpsc::Sender<LogEntry>,
) -> EventLoopOutcome {
    let stream = match client.log_listen().await {
        Ok(stream) => stream,
        Err(_) => return EventLoopOutcome::Disconnected,
    };
    tokio::pin!(stream);
    while let Some(item) = stream.next().await {
        let Ok(line) = item else {
            return EventLoopOutcome::Disconnected;
        };
        let entry = LogEntry {
            timestamp: chrono::Local::now(),
            source: LogSource::Daemon { line },
        };
        if tx.send(entry).await.is_err() {
            return EventLoopOutcome::ReceiverDropped;
        }
    }
    EventLoopOutcome::Disconnected
}

/// True when the daemon RPC error indicates the channel has been torn down
/// (e.g. daemon restarted, socket disappeared). These are recoverable by
/// re-establishing the [`MullvadProxyClient`].
fn is_transport_error(error: &mullvad_management_interface::Error) -> bool {
    use mullvad_management_interface::Error;
    match error {
        Error::GrpcTransportError(_) => true,
        Error::Rpc(status) => status.code() == Code::Unavailable,
        _ => false,
    }
}

fn translate_daemon_event(event: DaemonEvent) -> Option<AppEvent> {
    match event {
        DaemonEvent::TunnelState(state) => Some(AppEvent::StatusChanged(state)),
        DaemonEvent::Settings(settings) => Some(AppEvent::SettingsChanged(settings)),
        DaemonEvent::Device(device_event) => {
            // `device_event.cause` (Login / Logout / Revoked / RotatedKey) is
            // intentionally dropped - the TUI currently reflects only the
            // resulting state. If we add transient notifications ("Key
            // rotated") later, extend `AppEvent::DeviceChanged` to carry the
            // cause through.
            Some(AppEvent::DeviceChanged(device_event.new_state))
        }
        DaemonEvent::AppVersionInfo(info) => Some(AppEvent::AppVersionInfoChanged(info)),
        DaemonEvent::RelayList(list) => Some(AppEvent::RelayListChanged(list)),
        _ => None,
    }
}

impl MullvadService for RpcMullvadService {
    async fn get_status(&self) -> Result<TunnelState, IntegrationError> {
        self.with_client(async |client| Ok(client.get_tunnel_state().await?))
            .await
    }

    async fn connect(&self) -> Result<bool, IntegrationError> {
        self.with_client(async |client| Ok(client.connect_tunnel().await?))
            .await
    }

    async fn disconnect(&self) -> Result<bool, IntegrationError> {
        self.with_client(async |client| Ok(client.disconnect_tunnel(RPC_CLIENT_NAME).await?))
            .await
    }

    async fn reconnect(&self) -> Result<bool, IntegrationError> {
        self.with_client(async |client| Ok(client.reconnect_tunnel().await?))
            .await
    }

    async fn get_account(&self) -> Result<AccountInfo, IntegrationError> {
        // Two RPCs against two short-lived locks rather than one long lock.
        // Splitting also lets the data half route through the canonical
        // `get_account_data` trait method, avoiding a duplicated body.
        let device = self
            .with_client(async |client| {
                let _ = client.update_device().await;
                Ok(client.get_device().await?)
            })
            .await?;
        let data = match &device {
            DeviceState::LoggedIn(account_and_device) => Some(
                self.get_account_data(account_and_device.account_number.clone())
                    .await?,
            ),
            DeviceState::LoggedOut | DeviceState::Revoked => None,
        };
        Ok(AccountInfo { device, data })
    }

    async fn get_account_data(
        &self,
        account: AccountNumber,
    ) -> Result<AccountData, IntegrationError> {
        self.with_client(async |client| Ok(client.get_account_data(account.clone()).await?))
            .await
    }

    async fn login(&self, account: AccountNumber) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.login_account(account.clone()).await?;
            Ok(())
        })
        .await
    }

    async fn logout(&self) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.logout_account(RPC_CLIENT_NAME).await?;
            Ok(())
        })
        .await
    }

    async fn list_devices(&self, account: AccountNumber) -> Result<Vec<Device>, IntegrationError> {
        self.with_client(async |client| Ok(client.list_devices(account.clone()).await?))
            .await
    }

    async fn remove_device(
        &self,
        account: AccountNumber,
        device_id: DeviceId,
    ) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client
                .remove_device(account.clone(), device_id.clone())
                .await?;
            Ok(())
        })
        .await
    }

    async fn submit_voucher(&self, voucher: String) -> Result<VoucherSubmission, IntegrationError> {
        self.with_client(async |client| Ok(client.submit_voucher(voucher.clone()).await?))
            .await
    }

    async fn list_relays(&self) -> Result<Vec<RelayLocation>, IntegrationError> {
        self.with_client(async |client| {
            Ok(project_relay_list(&client.get_relay_locations().await?))
        })
        .await
    }

    async fn set_relay_location(&self, location: &str) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            let geo = resolve_active_hostname(client, location).await?;
            write_location(client, geo).await
        })
        .await
    }

    async fn set_relay_country(&self, country_code: &str) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            write_location(client, GeographicLocationConstraint::country(country_code)).await
        })
        .await
    }

    async fn set_relay_city(
        &self,
        country_code: &str,
        city_code: &str,
    ) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            write_location(
                client,
                GeographicLocationConstraint::city(country_code, city_code),
            )
            .await
        })
        .await
    }

    async fn set_multihop_enabled(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            let mut constraints = current_normal_constraints(client).await?;
            constraints.wireguard_constraints.use_multihop = enabled;
            client
                .set_relay_settings(RelaySettings::Normal(constraints))
                .await?;
            Ok(())
        })
        .await
    }

    async fn set_ip_version_preference(
        &self,
        version: Option<talpid_types::net::IpVersion>,
    ) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            let mut constraints = current_normal_constraints(client).await?;
            constraints.wireguard_constraints.ip_version = version.into();
            client
                .set_relay_settings(RelaySettings::Normal(constraints))
                .await?;
            Ok(())
        })
        .await
    }

    async fn get_full_settings(&self) -> Result<Settings, IntegrationError> {
        self.with_client(async |client| Ok(client.get_settings().await?))
            .await
    }

    async fn set_lan(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_allow_lan(enabled).await?;
            Ok(())
        })
        .await
    }

    async fn set_auto_connect(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_auto_connect(enabled).await?;
            Ok(())
        })
        .await
    }

    async fn set_dns_options(&self, options: DnsOptions) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_dns_options(options.clone()).await?;
            Ok(())
        })
        .await
    }

    async fn get_daemon_version(&self) -> Result<String, IntegrationError> {
        self.with_client(async |client| Ok(client.get_current_version().await?))
            .await
    }

    async fn get_app_version_info(&self) -> Result<AppVersionInfo, IntegrationError> {
        self.with_client(async |client| Ok(client.get_version_info().await?))
            .await
    }

    async fn set_lockdown_mode(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_lockdown_mode(enabled).await?;
            Ok(())
        })
        .await
    }

    async fn set_obfuscation_settings(
        &self,
        settings: ObfuscationSettings,
    ) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_obfuscation_settings(settings.clone()).await?;
            Ok(())
        })
        .await
    }

    async fn set_quantum_resistant_tunnel(
        &self,
        state: QuantumResistantState,
    ) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_quantum_resistant_tunnel(state).await?;
            Ok(())
        })
        .await
    }

    async fn set_enable_ipv6(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_enable_ipv6(enabled).await?;
            Ok(())
        })
        .await
    }

    async fn set_wireguard_mtu(&self, mtu: Option<u16>) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_wireguard_mtu(mtu).await?;
            Ok(())
        })
        .await
    }

    async fn set_enable_daita(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_enable_daita(enabled).await?;
            Ok(())
        })
        .await
    }

    async fn set_daita_direct_only(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_daita_direct_only(enabled).await?;
            Ok(())
        })
        .await
    }

    async fn rotate_wireguard_key(&self) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.rotate_wireguard_key().await?;
            Ok(())
        })
        .await
    }

    async fn get_current_api_access_method(&self) -> Result<AccessMethodSetting, IntegrationError> {
        self.with_client(async |client| Ok(client.get_current_api_access_method().await?))
            .await
    }

    async fn update_access_method(
        &self,
        setting: AccessMethodSetting,
    ) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.update_access_method(setting.clone()).await?;
            Ok(())
        })
        .await
    }

    async fn set_relay_override(
        &self,
        relay_override: RelayOverride,
    ) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_relay_override(relay_override.clone()).await?;
            Ok(())
        })
        .await
    }

    async fn clear_all_relay_overrides(&self) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.clear_all_relay_overrides().await?;
            Ok(())
        })
        .await
    }

    async fn set_access_method(&self, id: AccessMethodId) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_access_method(id.clone()).await?;
            Ok(())
        })
        .await
    }

    async fn set_split_tunnel_state(&self, enabled: bool) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.set_split_tunnel_state(enabled).await?;
            Ok(())
        })
        .await
    }

    async fn add_split_tunnel_app(&self, path: &str) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.add_split_tunnel_app(path).await?;
            Ok(())
        })
        .await
    }

    async fn remove_split_tunnel_app(&self, path: &str) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.remove_split_tunnel_app(path).await?;
            Ok(())
        })
        .await
    }

    async fn get_split_tunnel_processes(&self) -> Result<Vec<i32>, IntegrationError> {
        self.with_client(async |client| Ok(client.get_split_tunnel_processes().await?))
            .await
    }

    async fn add_split_tunnel_process(&self, pid: i32) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.add_split_tunnel_process(pid).await?;
            Ok(())
        })
        .await
    }

    async fn remove_split_tunnel_process(&self, pid: i32) -> Result<(), IntegrationError> {
        self.with_client(async |client| {
            client.remove_split_tunnel_process(pid).await?;
            Ok(())
        })
        .await
    }
}

/// Look up an active relay by hostname (case-insensitive) and return the
/// fully-qualified `GeographicLocationConstraint::Hostname(country, city, host)`
/// that points at it. Used by `set_relay_location` and
/// `set_relay_entry_location` so the two share a single resolution path.
async fn resolve_active_hostname(
    client: &mut TolerantClient,
    hostname: &str,
) -> Result<GeographicLocationConstraint, IntegrationError> {
    let target = hostname.to_lowercase();
    let relay_list = client.get_relay_locations().await?;
    let matched = relay_list.countries.iter().find_map(|country| {
        country.cities.iter().find_map(|city| {
            city.relays.iter().find_map(|relay| {
                (relay.active && relay.hostname.to_lowercase() == target).then(|| {
                    GeographicLocationConstraint::Hostname(
                        country.code.clone(),
                        city.code.clone(),
                        relay.hostname.clone(),
                    )
                })
            })
        })
    });
    matched.ok_or_else(|| {
        IntegrationError::Validation(format!("no active relay matches hostname: {hostname}"))
    })
}

/// Fetch the current normal `RelayConstraints`, replace the `location`
/// field with `geo`, and push the whole settings back. Shared by the
/// three relay setters (location/country/city) so the fetch-modify-set
/// sequence stays identical across the trio.
async fn write_location(
    client: &mut TolerantClient,
    geo: GeographicLocationConstraint,
) -> Result<(), IntegrationError> {
    let mut constraints = current_normal_constraints(client).await?;
    constraints.location = Constraint::Only(LocationConstraint::Location(geo));
    client
        .set_relay_settings(RelaySettings::Normal(constraints))
        .await?;
    Ok(())
}

/// Fetch current `RelaySettings`, returning `RelayConstraints` if it's a
/// `Normal` config or a `Validation` error if it's a `CustomTunnelEndpoint`
/// (which the TUI's relay editing flows can't sensibly modify).
async fn current_normal_constraints(
    client: &mut TolerantClient,
) -> Result<mullvad_types::relay_constraints::RelayConstraints, IntegrationError> {
    let settings = client.get_settings().await?;
    match settings.relay_settings {
        RelaySettings::Normal(constraints) => Ok(constraints),
        RelaySettings::CustomTunnelEndpoint(_) => Err(IntegrationError::Validation(
            "cannot change relay constraint while a custom tunnel endpoint is configured"
                .to_string(),
        )),
    }
}
