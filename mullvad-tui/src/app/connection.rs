// SPDX-License-Identifier: GPL-3.0-or-later

//! Tunnel-connection methods on `App`: `connect` / `disconnect` /
//! `reconnect`, the generalized two-stage `start_push_op` driver, and
//! the per-push-kind resolution helpers that flip a pending
//! confirmation to Success/Failed when a matching daemon event arrives.
//!
//! The two-stage idea: a daemon-affecting RPC returns success the
//! moment the daemon accepts the request, but the actual state change
//! the user cares about can take measurable time (tunnel handshake,
//! device registration, settings replication). To avoid prematurely
//! flipping `OperationStatus` to `Success`, we keep the op in `Running`
//! and stash a [`PendingPushConfirmation`]. When the corresponding
//! event-handler arm on `App` (`set_connection_status` /  `set_device` /
//! `set_settings`) sees a matching push, the pending entry is resolved.
//! Connect/Disconnect/Reconnect, Login/Logout, and the settings toggles
//! all ride this machinery.

use super::{App, Operation, OperationStatus};
use crate::integration::{DeviceState, IntegrationError, MullvadService, TargetState, TunnelState};

/// Predicate for which kind of daemon push event resolves a pending
/// two-stage operation. The variant is set when the RPC returns Ok and
/// matched against incoming `AppEvent`s in the corresponding `App::set_*`
/// event-handler arm.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PendingPushMatch {
    /// Tunnel reached `target`. Connect/Reconnect want `Secured`,
    /// Disconnect wants `Unsecured`. An `Error` tunnel state resolves
    /// the pending entry to `Failed` regardless.
    Tunnel { target: TargetState },
    /// Device push arrived in the matching state - Login waits for
    /// `LoggedIn`, Logout waits for `LoggedOut` or `Revoked`. Any
    /// other state (a Login that resolves to `LoggedOut`, a Logout
    /// that somehow lands in `LoggedIn`) marks the op `Failed`.
    Device { want: DeviceWant },
    /// The next `SettingsChanged` push resolves this entry. The daemon
    /// emits exactly one settings push per accepted write, and
    /// `set_operation_running` clears any prior pending entry, so a
    /// concurrent toggle can't have its push misattributed to a stale
    /// pending. Edge case: a daemon-side settings change made through
    /// a different client between RPC ack and push arrival can resolve
    /// this op early - harmless since the user-initiated toggle's
    /// effect is already in the cached settings either way.
    Settings,
}

/// Which device-state Login/Logout are waiting on. Mirrors the
/// `DeviceState` shape minus `Revoked`, which is treated as a
/// resolution synonym for `LoggedOut` (the user's logout intent
/// completes whether the daemon hits clean-logout or revoke-during-
/// logout).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DeviceWant {
    LoggedIn,
    LoggedOut,
}

/// Bookkeeping for an operation waiting for a daemon push to confirm
/// completion before flipping `OperationStatus` to Success.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PendingPushConfirmation {
    pub(super) operation: Operation,
    pub(super) wait: PendingPushMatch,
}

impl App {
    /// Apply a tunnel-state update pushed by the daemon. Resolves a
    /// pending two-stage operation if the new state matches the
    /// target (or the daemon has gone into an error state).
    pub fn set_connection_status(&mut self, status: TunnelState) {
        self.resolve_pending_for_tunnel(&status);
        self.connection_status = Some(status);
    }

    /// Inspect a fresh [`TunnelState`] for a still-pending tunnel op
    /// (set by `start_push_op` with [`PendingPushMatch::Tunnel`]) and
    /// resolve it. The daemon's `is_*` predicates do the matching so
    /// the targets stay in sync if upstream adds variants.
    fn resolve_pending_for_tunnel(&mut self, status: &TunnelState) {
        let Some(pending) = self.pending_push_confirmation else {
            return;
        };
        let PendingPushMatch::Tunnel { target } = pending.wait else {
            // Pending is for a different push kind - leave it for that
            // kind's resolver.
            return;
        };
        if status.is_in_error_state() {
            self.operation_status = OperationStatus::Failed {
                operation: pending.operation,
                message: format!("Tunnel entered error state: {status:?}"),
            };
            self.pending_push_confirmation = None;
            return;
        }
        let reached = match target {
            TargetState::Secured => status.is_connected(),
            TargetState::Unsecured => status.is_disconnected(),
        };
        if reached {
            self.operation_status = OperationStatus::Success(pending.operation);
            self.pending_push_confirmation = None;
        }
        // Transitional variants (Connecting / Disconnecting) - keep waiting.
    }

    /// Resolution helper for `DeviceChanged` pushes. Called from
    /// `App::set_device` after the new device state is cached.
    /// Login completes on `LoggedIn`, fails on `LoggedOut`/`Revoked`
    /// (registration didn't take). Logout completes on either
    /// `LoggedOut` or `Revoked`, fails on `LoggedIn` (which would
    /// indicate a daemon bug - accepted the logout but the device
    /// reappears).
    pub(super) fn resolve_pending_for_device(&mut self, state: &DeviceState) {
        let Some(pending) = self.pending_push_confirmation else {
            return;
        };
        let PendingPushMatch::Device { want } = pending.wait else {
            return;
        };
        let success = matches!(
            (want, state),
            (DeviceWant::LoggedIn, DeviceState::LoggedIn(_))
                | (
                    DeviceWant::LoggedOut,
                    DeviceState::LoggedOut | DeviceState::Revoked,
                )
        );
        self.operation_status = if success {
            OperationStatus::Success(pending.operation)
        } else {
            OperationStatus::Failed {
                operation: pending.operation,
                message: format!("Daemon ended in unexpected device state: {state:?}"),
            }
        };
        self.pending_push_confirmation = None;
    }

    /// Resolution helper for `SettingsChanged` pushes. Called from
    /// `App::set_settings` after the new settings are cached. Any
    /// settings push resolves a pending `Settings` entry to Success -
    /// see the [`PendingPushMatch::Settings`] doc comment for the
    /// edge-case discussion.
    pub(super) fn resolve_pending_for_settings(&mut self) {
        let Some(pending) = self.pending_push_confirmation else {
            return;
        };
        if !matches!(pending.wait, PendingPushMatch::Settings) {
            return;
        }
        self.operation_status = OperationStatus::Success(pending.operation);
        self.pending_push_confirmation = None;
    }

    pub async fn connect<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<bool, IntegrationError> {
        self.start_push_op(
            Operation::Connect,
            PendingPushMatch::Tunnel {
                target: TargetState::Secured,
            },
            async || service.connect().await,
        )
        .await
    }

    pub async fn disconnect<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<bool, IntegrationError> {
        self.start_push_op(
            Operation::Disconnect,
            PendingPushMatch::Tunnel {
                target: TargetState::Unsecured,
            },
            async || service.disconnect().await,
        )
        .await
    }

    pub async fn reconnect<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<bool, IntegrationError> {
        self.start_push_op(
            Operation::Reconnect,
            PendingPushMatch::Tunnel {
                target: TargetState::Secured,
            },
            async || service.reconnect().await,
        )
        .await
    }

    /// Run an RPC in two-stage style: the operation stays in `Running`
    /// after the RPC returns and only flips to `Success` once the
    /// matching daemon push (per `wait`) is delivered to one of the
    /// `App::set_*` event-handler arms (`set_connection_status` for
    /// Tunnel, `set_device` for Device, `set_settings` for Settings).
    /// The bool the RPC returns indicates whether the daemon actually
    /// transitioned (`Ok(true)` -> wait for the push) or was already in
    /// the target state (`Ok(false)` -> immediate Success, no push).
    pub(super) async fn start_push_op<F>(
        &mut self,
        op: Operation,
        wait: PendingPushMatch,
        rpc: F,
    ) -> Result<bool, IntegrationError>
    where
        F: AsyncFnOnce() -> Result<bool, IntegrationError>,
    {
        self.set_operation_running(op);
        match rpc().await {
            Ok(true) => {
                self.pending_push_confirmation = Some(PendingPushConfirmation {
                    operation: op,
                    wait,
                });
                Ok(true)
            }
            Ok(false) => {
                self.set_operation_success(op);
                Ok(false)
            }
            Err(error) => {
                self.set_operation_failed(op, &error);
                Err(error)
            }
        }
    }

    /// `()`-returning sibling of [`Self::start_push_op`] for ops that
    /// don't return a bool from their RPC (Login/Logout/settings
    /// toggles). Always sets a pending entry on success - these ops
    /// don't have a "no-op already in target state" short-circuit;
    /// every successful RPC means the daemon will emit a push.
    pub(super) async fn start_push_op_unit<F>(
        &mut self,
        op: Operation,
        wait: PendingPushMatch,
        rpc: F,
    ) -> Result<(), IntegrationError>
    where
        F: AsyncFnOnce() -> Result<(), IntegrationError>,
    {
        self.set_operation_running(op);
        match rpc().await {
            Ok(()) => {
                self.pending_push_confirmation = Some(PendingPushConfirmation {
                    operation: op,
                    wait,
                });
                Ok(())
            }
            Err(error) => {
                self.set_operation_failed(op, &error);
                Err(error)
            }
        }
    }

    /// Sibling of [`Self::start_push_op_unit`] specifically for
    /// settings-mutating RPCs that wait for a `DaemonEvent::Settings` push.
    pub(super) async fn start_settings_push_op<F>(
        &mut self,
        op: Operation,
        rpc: F,
    ) -> Result<(), IntegrationError>
    where
        F: AsyncFnOnce() -> Result<(), IntegrationError>,
    {
        self.start_push_op_unit(op, PendingPushMatch::Settings, rpc)
            .await
    }
}
