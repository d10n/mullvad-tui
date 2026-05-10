// SPDX-License-Identifier: GPL-3.0-or-later

//! `Operation` taxonomy and the [`OperationStatus`] state machine.
//!
//! Every long-running App-driven operation (Connect, Login, ToggleLan, etc)
//! flows through [`App::run_operation`] (or, for early-validation paths,
//! a direct [`App::set_operation_failed`] call). The two-stage layer in
//! `connection.rs` (`start_push_op` and friends) calls these helpers
//! under the hood.
//!
//! Both enums are re-exported from `mod.rs` so external callers can
//! write `crate::app::Operation` etc.

use std::fmt;

use super::App;
use crate::integration::IntegrationError;

/// The full set of long-running operations the App can drive. Each variant is
/// `Copy` and feeds [`OperationStatus`] without per-transition allocations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Connect,
    Disconnect,
    Reconnect,
    Login,
    Logout,
    SelectRelay,
    SelectRelayCountry,
    SelectRelayCity,
    ToggleMultihop,
    SetIpVersionPreference,
    Resync,
    RefreshAccount,
    /// Constructed only by the test-only `refresh_full_settings`
    /// seeder. The seeder is gated (not removed), so rustc still
    /// considers this variant alive in non-test builds.
    RefreshSettings,
    ToggleLan,
    ToggleAutoConnect,
    ToggleLockdown,
    SetAntiCensorshipMode,
    SetAntiCensorshipPort,
    ListDevices,
    RemoveDevice,
    SubmitVoucher,
    ToggleQuantumResistant,
    ToggleIpv6,
    SetMtu,
    ToggleDaita,
    ToggleDaitaDirectOnly,
    ToggleDnsBlocker,
    ToggleCustomDns,
    AddCustomDns,
    RemoveCustomDns,
    ReplaceCustomDns,
    RotateWireGuardKey,
    ToggleAccessMethod,
    SetActiveAccessMethod,
    RefreshCurrentAccessMethod,
    ToggleSplitTunnel,
    AddSplitTunnelApp,
    RemoveSplitTunnelApp,
    AddSplitTunnelProcess,
    RemoveSplitTunnelProcess,
    RefreshSplitTunnelProcesses,
    SetRelayOverride,
    ClearRelayOverrides,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Operation::Connect => "Connect",
            Operation::Disconnect => "Disconnect",
            Operation::Reconnect => "Reconnect",
            Operation::Login => "Login",
            Operation::Logout => "Logout",
            Operation::SelectRelay => "Select relay",
            Operation::SelectRelayCountry => "Select relay country",
            Operation::SelectRelayCity => "Select relay city",
            Operation::ToggleMultihop => "Toggle multihop",
            Operation::SetIpVersionPreference => "Set device IP version",
            Operation::Resync => "Resync state from daemon",
            Operation::RefreshAccount => "Refresh account",
            Operation::RefreshSettings => "Refresh settings",
            Operation::ToggleLan => "Toggle LAN",
            Operation::ToggleAutoConnect => "Toggle auto-connect",
            Operation::ToggleLockdown => "Toggle lockdown",
            Operation::SetAntiCensorshipMode => "Set anti-censorship mode",
            Operation::SetAntiCensorshipPort => "Set anti-censorship port",
            Operation::ListDevices => "List account devices",
            Operation::RemoveDevice => "Remove device",
            Operation::SubmitVoucher => "Submit voucher",
            Operation::ToggleQuantumResistant => "Toggle quantum-resistant tunnel",
            Operation::ToggleIpv6 => "Toggle in-tunnel IPv6",
            Operation::SetMtu => "Set WireGuard MTU",
            Operation::ToggleDaita => "Toggle DAITA",
            Operation::ToggleDaitaDirectOnly => "Toggle DAITA Direct only",
            Operation::ToggleDnsBlocker => "Toggle DNS content blocker",
            Operation::ToggleCustomDns => "Toggle custom DNS",
            Operation::AddCustomDns => "Add custom DNS server",
            Operation::RemoveCustomDns => "Remove custom DNS server",
            Operation::ReplaceCustomDns => "Replace custom DNS server",
            Operation::RotateWireGuardKey => "Rotate WireGuard key",
            Operation::ToggleAccessMethod => "Toggle API access method",
            Operation::SetActiveAccessMethod => "Set active API access method",
            Operation::RefreshCurrentAccessMethod => "Refresh current API access method",
            Operation::ToggleSplitTunnel => "Toggle split tunneling",
            Operation::AddSplitTunnelApp => "Add split-tunnel app",
            Operation::RemoveSplitTunnelApp => "Remove split-tunnel app",
            Operation::AddSplitTunnelProcess => "Add split-tunnel process",
            Operation::RemoveSplitTunnelProcess => "Remove split-tunnel process",
            Operation::RefreshSplitTunnelProcesses => "Refresh split-tunnel processes",
            Operation::SetRelayOverride => "Set relay override",
            Operation::ClearRelayOverrides => "Clear relay overrides",
        };
        f.write_str(label)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationStatus {
    Idle,
    Running(Operation),
    Success(Operation),
    Failed {
        operation: Operation,
        message: String,
    },
}

impl App {
    pub(super) fn set_operation_running(&mut self, operation: Operation) {
        // Starting any new operation cancels any still-pending push
        // confirmation: the user moved on, so the eventual push for
        // the earlier op should no longer be allowed to clobber the
        // new op's status (the actual daemon state - tunnel,
        // settings, device - is still visible via the cached fields
        // regardless).
        self.pending_push_confirmation = None;
        self.operation_status = OperationStatus::Running(operation);
    }

    pub(super) fn set_operation_success(&mut self, operation: Operation) {
        self.operation_status = OperationStatus::Success(operation);
    }

    pub(super) fn set_operation_failed(&mut self, operation: Operation, error: &IntegrationError) {
        self.operation_status = OperationStatus::Failed {
            operation,
            message: error.to_string(),
        };
    }

    /// Run an async operation while keeping [`OperationStatus`] in sync.
    /// Sets `Running(op)` on entry; on completion sets `Success(op)` for `Ok`
    /// or `Failed { op, message }` for `Err`. The error is also returned to
    /// the caller so it can be surfaced as a notification.
    pub(super) async fn run_operation<F, T>(
        &mut self,
        op: Operation,
        f: F,
    ) -> Result<T, IntegrationError>
    where
        F: AsyncFnOnce(&mut Self) -> Result<T, IntegrationError>,
    {
        self.set_operation_running(op);
        match f(self).await {
            Ok(value) => {
                self.set_operation_success(op);
                Ok(value)
            }
            Err(error) => {
                self.set_operation_failed(op, &error);
                Err(error)
            }
        }
    }
}
