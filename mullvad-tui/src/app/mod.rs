// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::VecDeque;

use tokio::sync::mpsc;

use crate::{
    integration::{
        AccessMethodId, AccountInfo, AppVersionInfo, DeviceState, IntegrationError, MullvadService,
        RelayLocation, Settings, TunnelState,
    },
    logging::LogEntry,
};

/// Maximum number of [`LogEntry`]s retained in `App.log_buffer`. Older
/// entries are evicted FIFO when the buffer is full. Sized for several
/// minutes of normal traffic at info level - generous for debugging without
/// holding megabytes of RAM.
const LOG_BUFFER_CAPACITY: usize = 2000;

/// Title rendered in the TUI header. Promote to a clap flag if a
/// user-facing override becomes a real ask.
const TITLE: &str = "mullvad-tui";

mod connection;
pub mod focus;
mod navigation;
mod operations;
pub mod pages;
mod relay;
mod session;
mod settings;

use connection::PendingPushConfirmation;
pub use focus::{ArrowDir, FocusKind, FocusRegistry, FocusableWidget, PageFocus, WidgetId};
pub use navigation::{ConfirmAction, NavigationState, PageId, TOP_LEVEL_PAGES};
pub use operations::{Operation, OperationStatus};
pub(crate) use relay::mode_has_configurable_port;
pub use relay::{ANTI_CENSORSHIP_PORT_RANGE, CurrentRelaySelection};
pub use settings::{DNS_BLOCKERS, DnsBlocker, WIREGUARD_MTU_RANGE};

/// One frame of sub-page navigation history. `page` is the sub-page
/// the user drilled into; `return_focus` is the widget that was
/// focused at the moment of entry, so popping the stack via `Esc`
/// can restore the user to whichever button they activated.
#[derive(Debug, Clone, Copy)]
struct SubPageEntry {
    page: PageId,
    return_focus: Option<WidgetId>,
}

pub struct App {
    should_quit: bool,
    navigation: NavigationState,
    connection_status: Option<TunnelState>,
    account_info: Option<AccountInfo>,
    relay_locations: Vec<RelayLocation>,
    settings: Option<Settings>,
    daemon_version: Option<String>,
    app_version_info: Option<AppVersionInfo>,
    /// The id of the API access method the daemon is currently using.
    /// Populated lazily on entry to the API access sub-page (no
    /// `DaemonEvent` variant pushes this), and after each
    /// `set_access_method` the run loop refreshes it to keep the
    /// `*` "currently using" marker honest.
    current_api_access_id: Option<AccessMethodId>,
    /// Linux split-tunnel PID list. The daemon doesn't push changes
    /// for this (no `DaemonEvent` variant), so the cache is refreshed
    /// on entry to the Split tunneling sub-page and after each
    /// add/remove. Stored as `Option` so the renderer can distinguish
    /// "not yet fetched" (None) from "fetched, list is empty" (Some(vec![])).
    /// Unconditional across platforms - non-Linux daemons return an
    /// error and the field stays `None`, which the renderer hides.
    split_tunnel_pids: Option<Vec<i32>>,
    operation_status: OperationStatus,
    /// Per-frame focus state. Persists across frames so the focused
    /// widget stays focused while the renderer rebuilds the registry.
    /// Reset to the page's first focusable widget on cross-page
    /// navigation.
    page_focus: PageFocus,
    /// Last-rendered focus registry. The renderer rebuilds it each frame;
    /// the input handler reads it to translate arrow keys into focus
    /// moves on the *next* keystroke.
    last_focus_registry: FocusRegistry,
    /// Last seen mouse-cursor `(column, row)` from any mouse event
    /// (move, click, drag, scroll). Drives the hover-highlight pass:
    /// the run loop hit-tests this against the just-rendered registry
    /// and paints a gray bg on the cell range under the cursor.
    /// `None` until the first mouse event arrives.
    cursor: Option<(u16, u16)>,
    /// A connect/disconnect/reconnect for which the daemon's RPC has returned
    /// successfully but the tunnel hasn't yet transitioned to the target
    /// state. The op stays in `Running` status until the matching
    /// `DaemonEvent::TunnelState` push arrives (see [`Self::set_connection_status`]).
    pending_push_confirmation: Option<PendingPushConfirmation>,
    /// Bounded FIFO of captured `tracing` events, sourced from the layer
    /// installed in [`crate::logging::init`] and drained into the buffer by
    /// the run loop. Renderer reads via [`Self::log_buffer`].
    log_buffer: VecDeque<LogEntry>,
    /// Per-page transient UI state (collapse flags, scroll cursors, etc.).
    /// Persists across navigation.
    page_states: pages::PageStates,
    /// Sub-page navigation stack. Empty when the user is on a
    /// top-level page; each entry pushes one breadcrumb level deeper
    /// (so `Status > Select location > Filter` has two entries on
    /// the stack). Each entry remembers the focused widget at the
    /// time of entry so `Esc` can restore focus to whichever button
    /// the user activated to drill in. `[Back]` / `Esc` pop one
    /// level; activating a top-level tab clears the whole stack.
    sub_page_stack: Vec<SubPageEntry>,
    /// Outbound notifications from anywhere in `App` to the run-loop
    /// overlay. `App::show_notification` calls `.send` on this; the
    /// run loop drains the receiver in `select!` and writes into
    /// [`crate::tui::OverlayMode::Notification`]. Decouples the 40+
    /// notification call sites from `&mut OverlayMode` threading.
    notification_tx: mpsc::UnboundedSender<String>,
    /// Receive end of the notification channel. Held inside `App` at
    /// startup so callers can construct the App with a single
    /// constructor; the run loop calls `take_notification_receiver`
    /// once on entry to consume it.
    notification_rx: Option<mpsc::UnboundedReceiver<String>>,
}

/// Helper macro for settings-mutating toggles that read a boolean from
/// cached `Settings`, send the inverse to the daemon, and wait for the
/// matching `DaemonEvent::Settings` push.
macro_rules! settings_toggle {
    ($self:ident, $service:ident, $op:ident, $getter:expr, $rpc_method:ident) => {{
        let current = $self.settings.as_ref().is_some_and($getter);
        $self
            .start_settings_push_op($crate::app::Operation::$op, async || {
                $service.$rpc_method(!current).await
            })
            .await
    }};
}

pub(crate) use settings_toggle;

/// Generate `{get}` (and optionally `{get_mut}`) accessors for one
/// per-page transient state slot on `App.page_states`.
macro_rules! page_state_accessors {
    ($get:ident, $get_mut:ident, $field:ident, $ty:ty) => {
        pub fn $get(&self) -> &$ty {
            &self.page_states.$field
        }
        pub fn $get_mut(&mut self) -> &mut $ty {
            &mut self.page_states.$field
        }
    };
    ($get:ident, $field:ident, $ty:ty) => {
        pub fn $get(&self) -> &$ty {
            &self.page_states.$field
        }
    };
}

impl App {
    pub fn new() -> Self {
        let (notification_tx, notification_rx) = mpsc::unbounded_channel();
        Self {
            should_quit: false,
            navigation: NavigationState::default(),
            connection_status: None,
            // Initialize as Some(LoggedOut) so `set_account_data` always has
            // an `AccountInfo` to write into. Saves callers from having to
            // call `set_device` before `set_account_data`, and matches the
            // daemon's actual default (every fresh install is logged-out).
            account_info: Some(AccountInfo {
                device: DeviceState::LoggedOut,
                data: None,
            }),
            relay_locations: Vec::new(),
            settings: None,
            daemon_version: None,
            app_version_info: None,
            current_api_access_id: None,
            split_tunnel_pids: None,
            operation_status: OperationStatus::Idle,
            page_focus: PageFocus::default(),
            last_focus_registry: FocusRegistry::new(),
            cursor: None,
            pending_push_confirmation: None,
            log_buffer: VecDeque::with_capacity(LOG_BUFFER_CAPACITY),
            page_states: pages::PageStates::default(),
            sub_page_stack: Vec::new(),
            notification_tx,
            notification_rx: Some(notification_rx),
        }
    }

    /// One-shot: hand the notification receiver off to the run loop.
    /// Subsequent calls return `None`; only the one consumer that
    /// actually drains the channel into [`crate::tui::OverlayMode`]
    /// should hold it.
    pub fn take_notification_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<String>> {
        self.notification_rx.take()
    }

    page_state_accessors!(
        status_page_state,
        status_page_state_mut,
        status,
        pages::status::PageState
    );

    /// Push the latest camera target into the Status page's animation
    /// tracker. Called by the run loop once per frame: the target
    /// depends on `connection_status` + the user's relay selection,
    /// both of which can change between renders, and the animation
    /// only restarts when the target actually moves.
    ///
    /// Pure data - `compute_target` is supplied by the renderer side
    /// (which owns the state->camera mapping) so this method stays
    /// independent of `tui_globe`.
    pub fn advance_status_camera(
        &mut self,
        compute_target: impl FnOnce(&Self) -> pages::status::CameraState,
        now: std::time::Instant,
    ) {
        let target = compute_target(self);
        self.page_states.status.camera_anim.set_target(target, now);
    }

    /// True while the Status page's globe camera is mid-transition.
    /// The run loop's frame ticker reads this to decide whether to
    /// keep firing 30 fps redraws or stay idle until the next input
    /// event.
    pub fn is_status_camera_animating(&self) -> bool {
        self.page_states
            .status
            .camera_anim
            .is_active(std::time::Instant::now())
    }

    /// True when the run loop should keep firing animation ticks even
    /// without an input or daemon event - either the globe camera is
    /// lerping, a Connect/Disconnect/Reconnect op is in flight (button
    /// spinner), or the tunnel itself is in a transitional state
    /// (status-label spinner). Without this, both spinners freeze the
    /// moment the camera settles.
    pub fn needs_animation_tick(&self) -> bool {
        if self.is_status_camera_animating() {
            return true;
        }
        if matches!(
            self.operation_status,
            OperationStatus::Running(Operation::Connect)
                | OperationStatus::Running(Operation::Disconnect)
                | OperationStatus::Running(Operation::Reconnect),
        ) {
            return true;
        }
        matches!(
            self.connection_status,
            Some(TunnelState::Connecting { .. } | TunnelState::Disconnecting(_)),
        )
    }

    page_state_accessors!(
        account_page_state,
        account_page_state_mut,
        account,
        pages::account::PageState
    );
    page_state_accessors!(
        select_location_page_state,
        select_location_page_state_mut,
        select_location,
        pages::select_location::PageState
    );
    page_state_accessors!(
        select_location_filter_page_state,
        select_location_filter_page_state_mut,
        select_location_filter,
        pages::select_location_filter::PageState
    );
    // Logs-page transient state uses a `Cell`-backed scroll offset, so a
    // `&App` is enough; no `_mut` accessor is needed.
    page_state_accessors!(logs_page_state, logs, pages::logs::PageState);
    page_state_accessors!(
        settings_page_state,
        settings_page_state_mut,
        settings,
        pages::settings::PageState
    );

    /// Append a captured log entry to the in-app ring buffer, evicting the
    /// oldest entry when at capacity. Called from the run loop's log-channel
    /// arm; see [`crate::logging`] for the layer that produces these events.
    pub fn append_log_entry(&mut self, entry: LogEntry) {
        if self.log_buffer.len() == LOG_BUFFER_CAPACITY {
            self.log_buffer.pop_front();
        }
        self.log_buffer.push_back(entry);
    }

    pub fn log_buffer(&self) -> &VecDeque<LogEntry> {
        &self.log_buffer
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn navigate_to(&mut self, page: PageId) {
        // Don't clear `focused` here: tabs are present on every page,
        // so a tab focus id stays valid post-navigation. For per-page
        // body widgets that don't appear on the destination page,
        // `set_focus_registry`'s "if focused id is not in the new
        // registry, snap to first" handles the cleanup. Resetting
        // `focused` unconditionally would yank focus back to the
        // leftmost tab every time the user activated any other tab -
        // the "arrow keys don't start from where I was" symptom.

        // Activating any top-level tab leaves the entire sub-page
        // chain, even if the destination top-level matches a
        // sub-page's parent (clicking [Account] while on Manage
        // devices returns the user to the Account page). Drop the
        // captured return-focus values too - a tab click signals
        // "abandon this sub-page navigation", not "park it".
        self.sub_page_stack.clear();
        self.navigation.navigate_to(page);
    }

    /// Read the current page. When a sub-page is active, that wins;
    /// otherwise the navigation state's top-level page. Drives the
    /// tab bar's "active" indicator and the body-renderer dispatch.
    pub fn current_page(&self) -> PageId {
        self.sub_page_stack
            .last()
            .map(|e| e.page)
            .unwrap_or_else(|| self.navigation.current_page())
    }

    /// True when a sub-page is currently active. The tab bar uses this
    /// to decide whether to prepend a `[Back]` button.
    pub fn is_on_sub_page(&self) -> bool {
        !self.sub_page_stack.is_empty()
    }

    /// Push into a sub-page. Captures the currently-focused widget
    /// so a subsequent [`Self::leave_sub_page`] can restore focus to
    /// the button that triggered the entry. Caller is responsible
    /// for picking a `PageId` whose `top_level_root()` matches the
    /// parent page - otherwise the active-tab indicator disagrees
    /// with the body renderer.
    ///
    /// Doesn't clear `focused`: top-level tabs are present on every
    /// sub-page too, so a focused tab id stays valid post-transition
    /// (same reasoning as `navigate_to`). `set_focus_registry` handles
    /// the case where the focused widget *isn't* in the new registry
    /// by snapping to the first widget.
    pub fn enter_sub_page(&mut self, sub_page: PageId) {
        let return_focus = self.page_focus.focused;
        self.sub_page_stack.push(SubPageEntry {
            page: sub_page,
            return_focus,
        });
    }

    /// Pop one level off the sub-page stack. Restores focus to the
    /// widget that was active when the popped sub-page was entered,
    /// so `Esc` from `Settings > VPN > DNS content blockers` lands
    /// the user back on `Settings > VPN` with the
    /// `[DNS content blockers]` button highlighted (rather than the
    /// destination page's first body widget). No-op on top-level
    /// pages.
    pub fn leave_sub_page(&mut self) {
        if let Some(entry) = self.sub_page_stack.pop()
            && let Some(return_focus) = entry.return_focus
        {
            self.page_focus.focused = Some(return_focus);
        }
    }

    pub fn page_focus(&self) -> &PageFocus {
        &self.page_focus
    }

    /// Mutable accessor for the focus state. Used by the input handler
    /// to update `focused` in response to arrow / Enter / Esc.
    pub fn page_focus_mut(&mut self) -> &mut PageFocus {
        &mut self.page_focus
    }

    pub fn last_focus_registry(&self) -> &FocusRegistry {
        &self.last_focus_registry
    }

    /// Last seen mouse-cursor `(column, row)`. `None` until the first
    /// mouse event arrives. Read by the run loop's hover-highlight
    /// pass; written via [`Self::set_cursor`] from the mouse handler.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        self.cursor
    }

    /// Stamp the latest cursor position from a `MouseEvent`. Called
    /// for every mouse event kind (Move, Down, Up, Drag, Scroll) so
    /// hover tracking stays current even when the user is also
    /// clicking or scrolling.
    pub fn set_cursor(&mut self, column: u16, row: u16) {
        self.cursor = Some((column, row));
    }

    /// Replace the persisted registry with the one just built by the
    /// renderer. If focus is currently `None` (page just opened, or
    /// the previously-focused widget is gone), snap into the page's
    /// body - first widget in row 1 - falling back to row 0's first
    /// widget if the page has no body widgets. Body-first matches the
    /// user's mental model after activating a button: focus belongs
    /// inside the new page, and pressing Up there takes them to the
    /// active tab via the focus-engine override.
    /// Hand the just-rendered registry to App and snap focus into it
    /// when the previously-focused widget is gone (or never set).
    ///
    /// Snap order:
    /// 1. First body widget - skips chrome rows (tab bar, breadcrumb `[<]`) so the user lands on
    ///    something actionable on the page itself instead of on the navigation chrome.
    /// 2. The active tab (`active_tab` argument), if it's in the registry. Used by pages whose body
    ///    has no focusables (e.g. Logs) - without this branch, step 3 would always pick the
    ///    leftmost tab id (`Status`), which visually reads as Esc / overlay-dismiss "jumping to a
    ///    random tab" on every other top-level page.
    /// 3. The first widget overall - last-resort fallback for degenerate registries with no body
    ///    and no active tab registered.
    pub fn set_focus_registry(&mut self, registry: FocusRegistry, active_tab: Option<WidgetId>) {
        if self
            .page_focus
            .focused
            .is_none_or(|id| !registry.contains(id))
        {
            self.page_focus.focused = registry
                .first_body_widget()
                .or_else(|| active_tab.filter(|id| registry.contains(*id)))
                .or_else(|| registry.first());
        }
        self.last_focus_registry = registry;
    }

    /// Send a user-visible notification to the run-loop overlay.
    /// Non-blocking; a saturated channel is silently dropped (the
    /// receiver is unbounded so this can only fail if the run loop
    /// has shut down its receiver, in which case the TUI is exiting
    /// and the notification is moot).
    pub fn show_notification(&mut self, message: impl Into<String>) {
        let _ = self.notification_tx.send(message.into());
    }

    /// Current operation status. Read by the Status page renderer
    /// to drive the spinner-on-button when a Connect / Disconnect /
    /// Reconnect is in flight. Two-stage operations use this through
    /// `start_push_op` / `run_operation`.
    pub fn operation_status(&self) -> &OperationStatus {
        &self.operation_status
    }

    pub fn title(&self) -> &'static str {
        TITLE
    }

    pub fn connection_status(&self) -> Option<&TunnelState> {
        self.connection_status.as_ref()
    }

    pub fn relay_locations(&self) -> &[RelayLocation] {
        &self.relay_locations
    }

    /// Replace the cached relay list. Called by the
    /// `DaemonEvent::RelayList` push handler and by [`Self::resync`].
    /// The user's *selection* lives in `Settings.relay_settings`
    /// (push-driven), so there's nothing selection-related to clean up
    /// here when the candidate list changes.
    pub fn set_relay_locations(&mut self, relays: Vec<RelayLocation>) {
        self.relay_locations = relays;
    }

    pub fn daemon_version(&self) -> Option<&str> {
        self.daemon_version.as_deref()
    }

    pub fn current_api_access_id(&self) -> Option<&AccessMethodId> {
        self.current_api_access_id.as_ref()
    }

    /// Cached Linux split-tunnel PID list. `None` until the first
    /// successful `refresh_split_tunnel_pids` call.
    pub fn split_tunnel_pids(&self) -> Option<&[i32]> {
        self.split_tunnel_pids.as_deref()
    }

    /// Cached `AppVersionInfo` from the daemon. The push handler
    /// keeps the field current; the eventual Settings -> Support page
    /// will surface it.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Settings > Support page is the planned consumer; field is kept current by the push handler"
        )
    )]
    pub fn app_version_info(&self) -> Option<&AppVersionInfo> {
        self.app_version_info.as_ref()
    }

    /// Apply an [`AppVersionInfo`] update. Called by the
    /// `DaemonEvent::AppVersionInfo` push handler and by [`Self::resync`].
    pub fn set_app_version_info(&mut self, info: AppVersionInfo) {
        self.app_version_info = Some(info);
    }

    /// Force-refetch every push-cached value. Called at startup -
    /// `events_listen` only emits on *changes*, so a fresh TUI
    /// subscribed mid-session would see no events and stay blank until
    /// the user did something. The initial resync populates the cache
    /// once so every panel has data on the very first frame.
    ///
    /// Linux split-tunnel PIDs are intentionally excluded - they have
    /// no push event and are fetched on entry to the Split tunneling
    /// sub-page instead.
    pub async fn resync<S: MullvadService>(&mut self, service: &S) -> Result<(), IntegrationError> {
        self.run_operation(Operation::Resync, async |app| {
            app.settings = Some(service.get_full_settings().await?);
            app.account_info = Some(service.get_account().await?);
            let daemon_version = service.get_daemon_version().await?;
            // Logged at INFO so any version-skew WARN emitted by the
            // tolerant-deserialization shim (`integration::tolerant`) can be
            // correlated to a specific daemon version in the Logs panel.
            tracing::info!(daemon_version = %daemon_version, "daemon version");
            app.daemon_version = Some(daemon_version);
            app.app_version_info = Some(service.get_app_version_info().await?);
            app.connection_status = Some(service.get_status().await?);
            // Relay list rarely changes (the daemon refreshes from the
            // upstream API every few hours), but pulling it here means
            // the Select location sub-page has data on first entry.
            app.set_relay_locations(service.list_relays().await?);
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::{App, IntegrationError, Operation, OperationStatus, Settings};
    use crate::{
        integration::{SelectedObfuscation, TunnelState},
        test_support::{StubService, connected_state, disconnected_state, error_state},
    };

    fn app() -> App {
        App::new()
    }

    #[test]
    fn set_connection_status_overwrites_state() {
        let mut app = app();
        app.set_connection_status(disconnected_state());
        assert!(matches!(
            app.connection_status(),
            Some(TunnelState::Disconnected { .. })
        ));
    }

    // --- CurrentRelaySelection ---

    #[test]
    fn current_relay_selection_returns_unknown_before_settings_prime() {
        let app = app();
        assert_eq!(
            app.current_relay_selection(),
            super::CurrentRelaySelection::Unknown
        );
    }

    #[test]
    fn current_relay_selection_extracts_each_relay_settings_variant() {
        use std::str::FromStr;

        use mullvad_types::{
            constraints::Constraint,
            custom_list::Id,
            relay_constraints::{
                GeographicLocationConstraint, LocationConstraint, RelayConstraints, RelaySettings,
            },
        };

        // The TUI never constructs custom-list `Id`s for real (the daemon
        // generates them); this test just needs *some* id to wedge into the
        // `CustomList` variant. `Id: FromStr` over a fixed UUID gives us one.
        let any_id: Id = Id::from_str("00000000-0000-0000-0000-000000000000").unwrap();

        /// Build `Settings` whose `relay_settings` is `Normal` with a given
        /// `LocationConstraint`. Other settings fields stay at default - only
        /// the relay-settings field affects `current_relay_selection`.
        fn settings_with(location: Constraint<LocationConstraint>) -> Settings {
            Settings {
                relay_settings: RelaySettings::Normal(RelayConstraints {
                    location,
                    ..RelayConstraints::default()
                }),
                ..Settings::default()
            }
        }

        let mut app = app();

        // Default RelayConstraints has location: Constraint::Any.
        app.set_settings(settings_with(Constraint::Any));
        assert_eq!(
            app.current_relay_selection(),
            super::CurrentRelaySelection::Any
        );

        app.set_settings(settings_with(Constraint::Only(
            LocationConstraint::Location(GeographicLocationConstraint::Country("se".to_string())),
        )));
        assert_eq!(
            app.current_relay_selection(),
            super::CurrentRelaySelection::Country("se")
        );

        app.set_settings(settings_with(Constraint::Only(
            LocationConstraint::Location(GeographicLocationConstraint::City(
                "se".to_string(),
                "got".to_string(),
            )),
        )));
        assert_eq!(
            app.current_relay_selection(),
            super::CurrentRelaySelection::City {
                country: "se",
                city: "got"
            }
        );

        app.set_settings(settings_with(Constraint::Only(
            LocationConstraint::Location(GeographicLocationConstraint::Hostname(
                "se".to_string(),
                "got".to_string(),
                "se-got-wg-001".to_string(),
            )),
        )));
        assert_eq!(
            app.current_relay_selection(),
            super::CurrentRelaySelection::Hostname("se-got-wg-001")
        );

        app.set_settings(settings_with(Constraint::Only(
            LocationConstraint::CustomList { list_id: any_id },
        )));
        assert_eq!(
            app.current_relay_selection(),
            super::CurrentRelaySelection::CustomList
        );
    }

    #[tokio::test]
    async fn toggle_multihop_flips_cached_value() {
        use mullvad_types::relay_constraints::{
            RelayConstraints, RelaySettings, WireguardConstraints,
        };

        let mut app = app();
        let service = StubService::default();

        // Cache: multihop currently OFF.
        let with_multihop = |enabled: bool| Settings {
            relay_settings: RelaySettings::Normal(RelayConstraints {
                wireguard_constraints: WireguardConstraints {
                    use_multihop: enabled,
                    ..WireguardConstraints::default()
                },
                ..RelayConstraints::default()
            }),
            ..Settings::default()
        };
        *service.full_settings.borrow_mut() = with_multihop(false);
        app.refresh_full_settings(&service).await.unwrap();
        assert!(!app.is_multihop_enabled());

        // First toggle should send `true`.
        app.toggle_multihop(&service).await.expect("toggle ok");
        assert_eq!(service.set_multihop_calls.borrow().as_slice(), [true]);

        // Now seed the stub with multihop ON, refresh, toggle should send `false`.
        *service.full_settings.borrow_mut() = with_multihop(true);
        app.refresh_full_settings(&service).await.unwrap();
        assert!(app.is_multihop_enabled());
        app.toggle_multihop(&service).await.expect("toggle ok");
        assert_eq!(
            service.set_multihop_calls.borrow().as_slice(),
            [true, false]
        );
    }

    #[test]
    fn current_entry_relay_selection_projects_entry_location() {
        use mullvad_types::{
            constraints::Constraint,
            relay_constraints::{
                GeographicLocationConstraint, LocationConstraint, RelayConstraints, RelaySettings,
                WireguardConstraints,
            },
        };

        // Build `Settings` whose multihop `entry_location` is `entry`;
        // only that field affects `current_entry_relay_selection`.
        fn settings_with_entry(entry: Constraint<LocationConstraint>) -> Settings {
            Settings {
                relay_settings: RelaySettings::Normal(RelayConstraints {
                    wireguard_constraints: WireguardConstraints {
                        entry_location: entry,
                        ..WireguardConstraints::default()
                    },
                    ..RelayConstraints::default()
                }),
                ..Settings::default()
            }
        }

        let mut app = app();
        // Unknown before settings prime, just like the exit projection.
        assert_eq!(
            app.current_entry_relay_selection(),
            super::CurrentRelaySelection::Unknown
        );

        app.set_settings(settings_with_entry(Constraint::Any));
        assert_eq!(
            app.current_entry_relay_selection(),
            super::CurrentRelaySelection::Any
        );

        app.set_settings(settings_with_entry(Constraint::Only(
            LocationConstraint::Location(GeographicLocationConstraint::Country("se".to_string())),
        )));
        assert_eq!(
            app.current_entry_relay_selection(),
            super::CurrentRelaySelection::Country("se")
        );

        app.set_settings(settings_with_entry(Constraint::Only(
            LocationConstraint::Location(GeographicLocationConstraint::City(
                "se".to_string(),
                "got".to_string(),
            )),
        )));
        assert_eq!(
            app.current_entry_relay_selection(),
            super::CurrentRelaySelection::City {
                country: "se",
                city: "got"
            }
        );

        app.set_settings(settings_with_entry(Constraint::Only(
            LocationConstraint::Location(GeographicLocationConstraint::Hostname(
                "se".to_string(),
                "got".to_string(),
                "se-got-wg-001".to_string(),
            )),
        )));
        assert_eq!(
            app.current_entry_relay_selection(),
            super::CurrentRelaySelection::Hostname("se-got-wg-001")
        );
    }

    #[test]
    fn daita_overrides_entry_only_when_multihop_daita_and_not_direct_only() {
        use mullvad_types::relay_constraints::{
            RelayConstraints, RelaySettings, WireguardConstraints,
        };

        // `use_multihop_if_necessary == true` is the "Direct only OFF"
        // state - the only one in which DAITA inserts its own entry hop.
        fn settings(
            multihop: bool,
            daita_enabled: bool,
            use_multihop_if_necessary: bool,
        ) -> Settings {
            let mut s = Settings {
                relay_settings: RelaySettings::Normal(RelayConstraints {
                    wireguard_constraints: WireguardConstraints {
                        use_multihop: multihop,
                        ..WireguardConstraints::default()
                    },
                    ..RelayConstraints::default()
                }),
                ..Settings::default()
            };
            s.tunnel_options.wireguard.daita.enabled = daita_enabled;
            s.tunnel_options.wireguard.daita.use_multihop_if_necessary = use_multihop_if_necessary;
            s
        }

        let mut app = app();
        assert!(!app.daita_overrides_entry(), "false before settings load");

        app.set_settings(settings(true, true, true));
        assert!(app.daita_overrides_entry());

        app.set_settings(settings(false, true, true));
        assert!(!app.daita_overrides_entry(), "needs multihop");

        app.set_settings(settings(true, false, true));
        assert!(!app.daita_overrides_entry(), "needs DAITA enabled");

        app.set_settings(settings(true, true, false));
        assert!(
            !app.daita_overrides_entry(),
            "Direct only (use_multihop_if_necessary=false) leaves the entry user-controlled"
        );
    }

    #[tokio::test]
    async fn select_entry_relay_writes_entry_setters() {
        let mut app = app();
        let service = StubService::default();

        app.select_entry_relay(&service, "se-got-wg-001")
            .await
            .expect("entry hostname ok");
        app.select_entry_relay_country(&service, "se")
            .await
            .expect("entry country ok");
        app.select_entry_relay_city(&service, "se", "got")
            .await
            .expect("entry city ok");

        assert_eq!(
            service.set_entry_calls.borrow().as_slice(),
            &["se-got-wg-001".to_string()]
        );
        assert_eq!(
            service.set_entry_country_calls.borrow().as_slice(),
            &["se".to_string()]
        );
        assert_eq!(
            service.set_entry_city_calls.borrow().as_slice(),
            &[("se".to_string(), "got".to_string())]
        );
        // The exit setters must stay untouched.
        assert!(service.set_relay_calls.borrow().is_empty());
        assert!(service.set_relay_country_calls.borrow().is_empty());
        assert!(service.set_relay_city_calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn select_relay_sets_specific_location() {
        let mut app = app();
        let service = StubService::default();

        app.select_relay(&service, "se-got-wg-001")
            .await
            .expect("direct relay selection should succeed");

        // The selection truth lives in the daemon's `RelaySettings`
        // (push-driven); here we just verify the RPC call landed.
        assert_eq!(
            service.set_relay_calls.borrow().as_slice(),
            ["se-got-wg-001"]
        );
        // Two-stage: settings push resolves the pending op.
        app.set_settings(Settings::default());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::SelectRelay)
        );
    }

    #[tokio::test]
    async fn select_relay_country_routes_to_country_setter() {
        let mut app = app();
        let service = StubService::default();

        app.select_relay_country(&service, "se")
            .await
            .expect("country select ok");

        assert_eq!(service.set_relay_country_calls.borrow().as_slice(), ["se"]);
        assert!(
            service.set_relay_calls.borrow().is_empty(),
            "hostname-setter must NOT be touched for a country pick"
        );
        assert!(
            service.set_relay_city_calls.borrow().is_empty(),
            "city-setter must NOT be touched for a country pick"
        );
        app.set_settings(Settings::default());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::SelectRelayCountry)
        );
    }

    #[tokio::test]
    async fn select_relay_city_routes_to_city_setter() {
        let mut app = app();
        let service = StubService::default();

        app.select_relay_city(&service, "se", "got")
            .await
            .expect("city select ok");

        assert_eq!(
            service.set_relay_city_calls.borrow().as_slice(),
            [("se".to_string(), "got".to_string())]
        );
        assert!(service.set_relay_calls.borrow().is_empty());
        assert!(service.set_relay_country_calls.borrow().is_empty());
        app.set_settings(Settings::default());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::SelectRelayCity)
        );
    }

    #[tokio::test]
    async fn toggle_lockdown_flips_value_from_cache() {
        let mut app = app();
        let service = StubService::default();
        // Cache: lockdown on. Toggle sends false.
        let seeded = Settings {
            lockdown_mode: true,
            ..Settings::default()
        };
        *service.full_settings.borrow_mut() = seeded;
        app.refresh_full_settings(&service)
            .await
            .expect("settings prime should succeed");

        app.toggle_lockdown(&service)
            .await
            .expect("lockdown toggle should succeed");

        assert_eq!(service.set_lockdown_calls.borrow().as_slice(), [false]);
    }

    #[tokio::test]
    async fn set_dns_blocker_sends_full_options_with_one_field_flipped() {
        use crate::{
            app::DnsBlocker,
            integration::{DefaultDnsOptions, DnsOptions},
        };

        let mut app = app();
        let service = StubService::default();
        // Seed: malware already on, custom_options non-default. The
        // fetch-modify-set path must preserve both when flipping ads.
        let seeded_options = DnsOptions {
            default_options: DefaultDnsOptions {
                block_malware: true,
                ..DefaultDnsOptions::default()
            },
            ..DnsOptions::default()
        };
        service
            .full_settings
            .borrow_mut()
            .tunnel_options
            .dns_options = seeded_options;
        app.refresh_full_settings(&service)
            .await
            .expect("settings prime should succeed");

        // Flip ads on; everything else stays as seeded.
        app.set_dns_blocker(&service, DnsBlocker::Ads, true)
            .await
            .expect("dns blocker toggle should succeed");

        let calls = service.set_dns_options_calls.borrow();
        assert_eq!(calls.len(), 1, "exactly one set_dns_options RPC");
        let sent = &calls[0];
        assert!(sent.default_options.block_ads, "ads flipped on");
        assert!(
            sent.default_options.block_malware,
            "malware preserved across the call"
        );
        assert!(
            !sent.default_options.block_trackers,
            "untouched fields stay false"
        );
    }

    #[tokio::test]
    async fn toggle_custom_dns_flips_state_preserving_addresses() {
        use crate::integration::{DnsOptions, DnsState};
        use mullvad_types::settings::CustomDnsOptions;
        use std::net::{IpAddr, Ipv4Addr};

        let mut app = app();
        let service = StubService::default();
        // Seed: state=Default, but the user has staged two addresses.
        // Toggling on must keep them.
        let staged = vec![
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
        ];
        service
            .full_settings
            .borrow_mut()
            .tunnel_options
            .dns_options = DnsOptions {
            state: DnsState::Default,
            custom_options: CustomDnsOptions {
                addresses: staged.clone(),
            },
            ..DnsOptions::default()
        };
        app.refresh_full_settings(&service)
            .await
            .expect("settings prime should succeed");

        app.toggle_custom_dns(&service)
            .await
            .expect("toggle should succeed");

        let calls = service.set_dns_options_calls.borrow();
        assert_eq!(calls.len(), 1);
        let sent = &calls[0];
        assert!(matches!(sent.state, DnsState::Custom), "state flipped on");
        assert_eq!(
            sent.custom_options.addresses, staged,
            "addresses preserved across the toggle"
        );
    }

    #[tokio::test]
    async fn add_custom_dns_appends_preserving_state() {
        use crate::integration::{DnsOptions, DnsState};
        use std::net::{IpAddr, Ipv4Addr};

        let mut app = app();
        let service = StubService::default();
        // Seed: state=Custom (already on) so the test confirms `add`
        // doesn't accidentally flip state back to Default.
        service
            .full_settings
            .borrow_mut()
            .tunnel_options
            .dns_options = DnsOptions {
            state: DnsState::Custom,
            ..DnsOptions::default()
        };
        app.refresh_full_settings(&service)
            .await
            .expect("settings prime should succeed");

        let new_addr = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        app.add_custom_dns(&service, new_addr)
            .await
            .expect("add should succeed");

        let calls = service.set_dns_options_calls.borrow();
        assert_eq!(calls.len(), 1);
        let sent = &calls[0];
        assert!(
            matches!(sent.state, DnsState::Custom),
            "state preserved across add"
        );
        assert_eq!(sent.custom_options.addresses, [new_addr]);
    }

    #[tokio::test]
    async fn remove_custom_dns_drops_index_and_rejects_out_of_range() {
        use std::net::{IpAddr, Ipv4Addr};

        let mut app = app();
        let service = StubService::default();
        let addrs = vec![
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
        ];
        app.set_settings(Settings::default());
        app.set_custom_dns_addresses(&service, addrs.clone())
            .await
            .expect("seeding addresses should succeed");
        // Reset the call log so the assertions below only see the
        // remove-driven call.
        service.set_dns_options_calls.borrow_mut().clear();

        // Ensure the cached settings reflect the seeded addresses
        // (the seeding call writes through stub's `set_dns_options`,
        // which mirrors back via `full_settings`; refresh to pick up).
        app.refresh_full_settings(&service)
            .await
            .expect("re-prime should succeed");

        app.remove_custom_dns(&service, 1)
            .await
            .expect("remove middle should succeed");

        // Scope the borrow so it doesn't survive across the next
        // `.await` (clippy's `await_holding_refcell_ref` will refuse).
        {
            let calls = service.set_dns_options_calls.borrow();
            // 1 remove call. (The refresh_full_settings inside the
            // closure doesn't write - `get_full_settings` is read-only.)
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].custom_options.addresses,
                [addrs[0], addrs[2]],
                "index 1 dropped, others preserved"
            );
        }

        // Out-of-range remove is rejected without an RPC.
        let err = app
            .remove_custom_dns(&service, 99)
            .await
            .expect_err("out-of-range index should fail");
        assert!(matches!(err, IntegrationError::Validation(_)));
        assert_eq!(
            service.set_dns_options_calls.borrow().len(),
            1,
            "no extra RPC fired for the rejected remove"
        );
    }

    #[tokio::test]
    async fn toggle_lan_stays_running_until_settings_push_confirms() {
        // Lock in Pattern-1 generalization for the Settings push:
        // status must stay Running after the RPC ack and only flip
        // to Success when `set_settings` is invoked from the
        // event-handler arm.
        let mut app = app();
        let service = StubService::default();

        app.toggle_lan(&service)
            .await
            .expect("lan toggle should succeed");

        // RPC sent but no settings push yet.
        assert_eq!(service.set_lan_calls.borrow().as_slice(), [true]);
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Running(Operation::ToggleLan),
        );

        // Push arrives - pending entry resolves to Success.
        app.set_settings(Settings::default());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::ToggleLan),
        );
    }

    #[tokio::test]
    async fn toggle_lan_flips_value_from_cache_default() {
        let mut app = app();
        let service = StubService::default();
        // Cache empty defaults to false; toggle sends true.
        app.toggle_lan(&service)
            .await
            .expect("lan toggle should succeed");

        assert_eq!(service.set_lan_calls.borrow().as_slice(), [true]);
        // Two-stage: status stays Running until the matching settings
        // push arrives. Simulate that push.
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Running(Operation::ToggleLan)
        );
        app.set_settings(Settings::default());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::ToggleLan)
        );
    }

    #[tokio::test]
    async fn toggle_access_method_flips_enabled_for_targeted_id() {
        // Seed App.settings with an api_access_methods config so the
        // toggle path can find the target setting by id. Default settings
        // give all four built-ins enabled - toggle Direct -> expect a
        // single update_access_method call carrying enabled=false.
        let mut app = app();
        let service = StubService::default();
        let seeded = Settings::default();
        let direct_id = seeded.api_access_methods.direct().get_id();
        *service.full_settings.borrow_mut() = seeded;
        app.refresh_full_settings(&service).await.unwrap();

        // `direct_id` is reused by the assert below, so it must outlive this
        // call. `AccessMethodId` is `Clone`-not-`Copy` on the stable
        // `mullvadvpn-app` pin (clone required) and `Copy` on tip-of-`main`
        // (where `clippy::clone_on_copy` flags it). The cfg (set by
        // `build.rs`) scopes the `expect` to the `main` pin where the lint
        // fires; on stable it stays silent and the same source builds clean.
        #[cfg_attr(access_method_id_is_copy, expect(clippy::clone_on_copy))]
        app.toggle_access_method(&service, direct_id.clone())
            .await
            .expect("toggle ok");

        let calls = service.update_access_method_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].get_id(), direct_id);
        assert!(
            !calls[0].enabled,
            "default-enabled built-in should have flipped to disabled"
        );
        // Two-stage: simulate the settings push to land Success.
        app.set_settings(Settings::default());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::ToggleAccessMethod)
        );
    }

    #[tokio::test]
    async fn set_active_access_method_writes_and_refreshes_current_id() {
        let mut app = app();
        let service = StubService::default();
        let seeded = Settings::default();
        let target = seeded.api_access_methods.encrypted_dns_proxy().clone();
        *service.full_settings.borrow_mut() = seeded;
        // Stub: after the daemon "switches", `get_current_api_access_method`
        // returns the new target.
        *service.current_api_access_method.borrow_mut() = Some(Ok(target.clone()));

        app.set_active_access_method(&service, target.get_id())
            .await
            .expect("set active ok");

        assert_eq!(
            service.set_access_method_calls.borrow().as_slice(),
            [target.get_id()]
        );
        assert_eq!(app.current_api_access_id(), Some(&target.get_id()));
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::SetActiveAccessMethod)
        );
    }

    #[tokio::test]
    async fn toggle_split_tunnel_sends_inverse_of_cached_state() {
        let mut app = app();
        let service = StubService::default();
        app.toggle_split_tunnel(&service).await.expect("toggle ok");

        // Cache empty -> split_tunnel_enabled() defaults to false; toggle
        // sends true.
        assert_eq!(
            service.set_split_tunnel_state_calls.borrow().as_slice(),
            [true]
        );
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::ToggleSplitTunnel)
        );
    }

    #[tokio::test]
    async fn add_split_tunnel_process_records_call_and_refreshes_pids() {
        let mut app = app();
        let service = StubService::default();
        // Stub starts with an empty PID list; add 1234 and verify the
        // App refreshes from the stub-side mirror.
        app.add_split_tunnel_process(&service, 1234)
            .await
            .expect("add pid ok");
        assert_eq!(
            service.add_split_tunnel_process_calls.borrow().as_slice(),
            [1234]
        );
        assert_eq!(app.split_tunnel_pids(), Some(&[1234][..]));
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::AddSplitTunnelProcess)
        );
    }

    #[tokio::test]
    async fn remove_split_tunnel_process_strips_pid_from_cache() {
        let mut app = app();
        let service = StubService::default();
        // Seed: stub holds two PIDs already.
        *service.split_tunnel_processes.borrow_mut() = vec![100, 200];
        app.refresh_split_tunnel_pids(&service).await.unwrap();
        assert_eq!(app.split_tunnel_pids(), Some(&[100, 200][..]));

        app.remove_split_tunnel_process(&service, 100)
            .await
            .expect("remove pid ok");

        assert_eq!(
            service
                .remove_split_tunnel_process_calls
                .borrow()
                .as_slice(),
            [100]
        );
        assert_eq!(app.split_tunnel_pids(), Some(&[200][..]));
    }

    #[tokio::test]
    async fn refresh_current_access_method_caches_id_from_daemon() {
        let mut app = app();
        let service = StubService::default();
        let seeded = Settings::default();
        let direct = seeded.api_access_methods.direct().clone();
        *service.current_api_access_method.borrow_mut() = Some(Ok(direct.clone()));

        app.refresh_current_access_method(&service)
            .await
            .expect("refresh ok");

        assert_eq!(app.current_api_access_id(), Some(&direct.get_id()));
    }

    #[tokio::test]
    async fn toggle_auto_connect_flips_value_from_cache() {
        let mut app = app();
        let service = StubService::default();
        // Seed: auto_connect already on -> toggle sends false.
        service.full_settings.borrow_mut().auto_connect = true;
        app.refresh_full_settings(&service)
            .await
            .expect("settings prime should succeed");

        app.toggle_auto_connect(&service)
            .await
            .expect("auto-connect toggle should succeed");

        assert_eq!(service.set_auto_connect_calls.borrow().as_slice(), [false]);
        // Two-stage: simulate the matching settings push.
        app.set_settings(Settings::default());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::ToggleAutoConnect)
        );
    }

    // --- Relay overrides ---

    #[tokio::test]
    async fn set_relay_override_dispatches_to_service_and_stays_running_until_push() {
        use crate::integration::RelayOverride;
        use std::net::Ipv4Addr;

        let mut app = app();
        let service = StubService::default();
        let override_ = RelayOverride {
            hostname: "se-got-wg-001".to_string(),
            ipv4_addr_in: Some(Ipv4Addr::new(185, 213, 154, 66)),
            ipv6_addr_in: None,
        };
        app.set_relay_override(&service, override_.clone())
            .await
            .expect("set ok");

        let calls = service.set_relay_override_calls.borrow();
        assert_eq!(calls.as_slice(), std::slice::from_ref(&override_));
        drop(calls);
        // Two-stage: still Running until the settings push lands.
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Running(Operation::SetRelayOverride),
        );
        app.set_settings(Settings::default());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::SetRelayOverride),
        );
    }

    #[tokio::test]
    async fn remove_relay_override_sends_empty_override_for_hostname() {
        use crate::integration::RelayOverride;
        let mut app = app();
        let service = StubService::default();

        app.remove_relay_override(&service, "se-got-wg-001".to_string())
            .await
            .expect("remove ok");

        let calls = service.set_relay_override_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            RelayOverride::empty("se-got-wg-001".to_string()),
            "remove should send an empty override (daemon swap-removes empties)",
        );
    }

    #[tokio::test]
    async fn clear_relay_overrides_calls_service_and_succeeds_on_push() {
        let mut app = app();
        let service = StubService::default();

        app.clear_relay_overrides(&service).await.expect("clear ok");

        assert_eq!(*service.clear_all_relay_overrides_calls.borrow(), 1);
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Running(Operation::ClearRelayOverrides),
        );
        app.set_settings(Settings::default());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::ClearRelayOverrides),
        );
    }

    #[tokio::test]
    async fn relay_overrides_reads_through_cached_settings() {
        use crate::integration::RelayOverride;
        use std::net::Ipv6Addr;

        let mut app = app();
        assert!(
            app.relay_overrides().is_empty(),
            "no settings cached -> empty slice",
        );

        let mut seeded = Settings::default();
        seeded.relay_overrides.push(RelayOverride {
            hostname: "de-fra-wg-005".to_string(),
            ipv4_addr_in: None,
            ipv6_addr_in: Some(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x42)),
        });
        app.set_settings(seeded);

        let overrides = app.relay_overrides();
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].hostname, "de-fra-wg-005");
        assert!(overrides[0].ipv4_addr_in.is_none());
        assert!(overrides[0].ipv6_addr_in.is_some());
    }

    // --- Log buffer ---

    fn make_log_entry(message: &str) -> crate::logging::LogEntry {
        crate::logging::LogEntry {
            timestamp: chrono::Local::now(),
            source: crate::logging::LogSource::Tui {
                level: tracing::Level::INFO,
                target: "test".to_string(),
                message: message.to_string(),
            },
        }
    }

    /// Project a LogEntry to its message text - only valid for Tui
    /// entries (which is what `make_log_entry` produces). Daemon
    /// entries get an empty string fallback so the assertion failures
    /// surface clearly rather than panicking.
    fn entry_message(entry: &crate::logging::LogEntry) -> &str {
        match &entry.source {
            crate::logging::LogSource::Tui { message, .. } => message.as_str(),
            crate::logging::LogSource::Daemon { line } => line.as_str(),
        }
    }

    #[test]
    fn append_log_entry_evicts_oldest_when_at_capacity() {
        use super::LOG_BUFFER_CAPACITY;

        let mut app = app();
        // Fill to capacity with sentinel messages "0", "1", ..., "N-1".
        for i in 0..LOG_BUFFER_CAPACITY {
            app.append_log_entry(make_log_entry(&i.to_string()));
        }
        assert_eq!(app.log_buffer().len(), LOG_BUFFER_CAPACITY);
        assert_eq!(entry_message(app.log_buffer().front().unwrap()), "0");

        // One more - oldest "0" must evict, newest "overflow" must land at back.
        app.append_log_entry(make_log_entry("overflow"));
        assert_eq!(app.log_buffer().len(), LOG_BUFFER_CAPACITY);
        assert_eq!(entry_message(app.log_buffer().front().unwrap()), "1");
        assert_eq!(entry_message(app.log_buffer().back().unwrap()), "overflow");
    }

    #[test]
    fn log_entry_display_is_human_readable() {
        let entry = crate::logging::LogEntry {
            timestamp: chrono::Local
                .with_ymd_and_hms(2026, 5, 5, 14, 30, 45)
                .unwrap(),
            source: crate::logging::LogSource::Tui {
                level: tracing::Level::WARN,
                target: "mullvad_tui::app".to_string(),
                message: "something happened".to_string(),
            },
        };
        let line = entry.to_string();
        assert!(line.contains("14:30:45"), "{line}");
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("mullvad_tui::app"), "{line}");
        assert!(line.contains("something happened"), "{line}");
    }

    #[test]
    fn log_entry_display_renders_daemon_lines_with_prefix() {
        let entry = crate::logging::LogEntry {
            timestamp: chrono::Local::now(),
            source: crate::logging::LogSource::Daemon {
                // Daemon emits already-formatted lines with embedded
                // timestamp / level / target - and a trailing newline
                // we strip so the renderer's per-line layout doesn't
                // leave a blank row beneath each daemon entry.
                line: "[2026-05-07T14:30:45Z DEBUG mullvad_daemon] tunnel up\n".to_string(),
            },
        };
        let line = entry.to_string();
        assert!(line.starts_with("[daemon] "), "{line}");
        assert!(line.contains("tunnel up"), "{line}");
        assert!(!line.ends_with('\n'), "{line}");
    }

    #[test]
    fn mode_has_configurable_port_only_for_port_carrying_modes() {
        use super::mode_has_configurable_port;

        for mode in [
            SelectedObfuscation::Udp2Tcp,
            SelectedObfuscation::Shadowsocks,
            SelectedObfuscation::WireguardPort,
        ] {
            assert!(
                mode_has_configurable_port(mode),
                "{mode} should have a port"
            );
        }
        for mode in [
            SelectedObfuscation::Off,
            SelectedObfuscation::Auto,
            SelectedObfuscation::Quic,
            SelectedObfuscation::Lwo,
        ] {
            assert!(
                !mode_has_configurable_port(mode),
                "{mode} should not have a configurable port"
            );
        }
    }

    #[tokio::test]
    async fn set_anti_censorship_port_writes_target_mode_field_only() {
        use mullvad_types::{constraints::Constraint, relay_constraints::ShadowsocksSettings};

        let mut app = app();
        let service = StubService::default();
        // Seed Shadowsocks with a non-default port and Udp2Tcp at a different
        // port, so we can prove the write doesn't bleed across modes.
        let mut seeded = Settings::default();
        seeded.obfuscation_settings.shadowsocks = ShadowsocksSettings {
            port: Constraint::Only(443),
        };
        *service.full_settings.borrow_mut() = seeded;
        app.refresh_full_settings(&service).await.unwrap();

        app.set_anti_censorship_port(&service, SelectedObfuscation::Udp2Tcp, Some(80))
            .await
            .expect("set port ok");

        let calls = service.set_obfuscation_settings_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].udp2tcp.port, Constraint::Only(80));
        assert_eq!(
            calls[0].shadowsocks.port,
            Constraint::Only(443),
            "writing udp2tcp port must not clobber shadowsocks port"
        );
    }

    #[tokio::test]
    async fn set_anti_censorship_port_blank_means_any() {
        use mullvad_types::constraints::Constraint;

        let mut app = app();
        let service = StubService::default();

        app.set_anti_censorship_port(&service, SelectedObfuscation::Shadowsocks, None)
            .await
            .expect("set port ok");

        let calls = service.set_obfuscation_settings_calls.borrow();
        assert_eq!(calls[0].shadowsocks.port, Constraint::Any);
    }

    #[tokio::test]
    async fn set_anti_censorship_port_rejects_modes_without_port() {
        let mut app = app();
        let service = StubService::default();

        for mode in [
            SelectedObfuscation::Off,
            SelectedObfuscation::Auto,
            SelectedObfuscation::Quic,
            SelectedObfuscation::Lwo,
        ] {
            let result = app
                .set_anti_censorship_port(&service, mode, Some(443))
                .await;
            assert!(
                matches!(result, Err(IntegrationError::Validation(_))),
                "mode {mode} should reject port"
            );
        }
        // Should not have made any RPC calls.
        assert!(service.set_obfuscation_settings_calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn set_anti_censorship_mode_writes_target_and_preserves_port_settings() {
        use mullvad_types::{
            constraints::Constraint, relay_constraints::Udp2TcpObfuscationSettings,
        };

        let mut app = app();
        let service = StubService::default();
        // Cache: Auto with a non-default udp2tcp port. Switching to
        // Shadowsocks (skipping past Udp2Tcp in the daemon-defined
        // order) must keep the udp2tcp port intact.
        let mut seeded = Settings::default();
        seeded.obfuscation_settings.selected_obfuscation = SelectedObfuscation::Auto;
        seeded.obfuscation_settings.udp2tcp = Udp2TcpObfuscationSettings {
            port: Constraint::Only(80),
        };
        *service.full_settings.borrow_mut() = seeded;
        app.refresh_full_settings(&service)
            .await
            .expect("settings prime ok");

        app.set_anti_censorship_mode(&service, SelectedObfuscation::Shadowsocks)
            .await
            .expect("set mode ok");

        {
            let calls = service.set_obfuscation_settings_calls.borrow();
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].selected_obfuscation,
                SelectedObfuscation::Shadowsocks
            );
            assert_eq!(
                calls[0].udp2tcp.port,
                Constraint::Only(80),
                "switching modes must not clobber per-mode port settings"
            );
        }
        // Two-stage: settings push resolves the pending op.
        app.set_settings(Settings::default());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::SetAntiCensorshipMode)
        );
    }

    #[tokio::test]
    async fn resync_populates_every_push_cached_field() {
        let mut app = app();
        let service = StubService::default();
        // Pre-condition: nothing primed.
        assert!(app.settings().is_none());
        assert!(app.daemon_version().is_none());
        assert!(app.app_version_info().is_none());
        assert!(app.connection_status().is_none());

        app.resync(&service).await.expect("resync ok");

        assert!(app.settings().is_some());
        assert!(app.daemon_version().is_some());
        assert!(app.app_version_info().is_some());
        assert!(app.connection_status().is_some());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::Resync)
        );
    }

    // --- Two-stage operation status ---
    //
    // connect/disconnect/reconnect leave the operation in `Running` after the
    // RPC returns `Ok(true)`, then resolve to Success/Failed only when the
    // matching `DaemonEvent::TunnelState` push arrives via
    // `set_connection_status`. `Ok(false)` (already in target state) and
    // `Err` short-circuit straight to Success/Failed.

    #[tokio::test]
    async fn connect_stays_running_until_status_push_confirms() {
        let mut app = app();
        let service = StubService::default();

        app.connect(&service).await.expect("connect RPC ok");
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Running(Operation::Connect)
        );

        app.set_connection_status(connected_state());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::Connect)
        );
    }

    #[tokio::test]
    async fn connect_marks_failed_when_tunnel_enters_error_state() {
        let mut app = app();
        let service = StubService::default();

        app.connect(&service).await.expect("connect RPC ok");
        app.set_connection_status(error_state());

        assert!(matches!(
            app.operation_status(),
            OperationStatus::Failed {
                operation: Operation::Connect,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn connect_short_circuits_to_success_when_already_connected() {
        let mut app = app();
        let service = StubService {
            connect_result: Ok(false), // daemon was already in target state
            ..StubService::default()
        };

        let returned = app.connect(&service).await.expect("connect RPC ok");
        assert!(!returned, "Ok(false) should propagate");
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::Connect),
            "no-op connect resolves to Success without waiting for a push"
        );
    }

    #[tokio::test]
    async fn disconnect_waits_for_disconnected_state() {
        let mut app = app();
        let service = StubService::default();

        app.disconnect(&service).await.expect("disconnect RPC ok");
        // A Connected push during the wait must not resolve Disconnect.
        app.set_connection_status(connected_state());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Running(Operation::Disconnect)
        );

        app.set_connection_status(disconnected_state());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::Disconnect)
        );
    }

    #[tokio::test]
    async fn reconnect_waits_for_connected_state() {
        let mut app = app();
        let service = StubService::default();

        app.reconnect(&service).await.expect("reconnect RPC ok");
        app.set_connection_status(connected_state());

        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::Reconnect)
        );
    }

    #[tokio::test]
    async fn starting_a_new_operation_cancels_pending_confirmation() {
        let mut app = app();
        let service = StubService::default();

        app.connect(&service).await.expect("connect RPC ok");
        app.refresh_full_settings(&service)
            .await
            .expect("refresh ok");

        // Eventual Connected push must NOT clobber the new op's Success status.
        app.set_connection_status(connected_state());
        assert_eq!(
            app.operation_status(),
            &OperationStatus::Success(Operation::RefreshSettings),
            "abandoned pending Connect should not retroactively mark Success"
        );
    }
}
