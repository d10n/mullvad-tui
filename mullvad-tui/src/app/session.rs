// SPDX-License-Identifier: GPL-3.0-or-later

//! Account / device / voucher methods on `App`. Holds the cached
//! `AccountInfo` accessors plus every RPC that mutates the
//! authenticated session: login, logout, list/remove devices, voucher
//! redemption, account refresh.
//!
//! Named `session.rs` (not `account.rs`) because there is already an
//! `app/pages/account.rs` for per-page UI state; collapsing both into
//! the same name would shadow.

use super::{App, Operation};
use crate::integration::{
    AccountData, AccountInfo, AccountNumber, Device, DeviceId, DeviceState, IntegrationError,
    MullvadService, VoucherSubmission,
};

impl App {
    pub fn account_info(&self) -> Option<&AccountInfo> {
        self.account_info.as_ref()
    }

    /// Apply a `DeviceState` push from the daemon's event stream. Replaces
    /// only the device half of `AccountInfo`; the data half is updated
    /// separately via [`Self::set_account_data`] (the daemon's device push
    /// doesn't carry account data, so a follow-up RPC is required).
    /// Invalidates the cached Manage-devices list on transitions out
    /// of `LoggedIn` so the next entry to that sub-page re-fetches
    /// against the new account context. Resolves any pending
    /// [`PendingPushMatch::Device`] op so Login/Logout flip from
    /// `Running` to `Success` (or `Failed` on a mismatching device
    /// state) at the moment the daemon confirms.
    pub fn set_device(&mut self, device: DeviceState) {
        let leaving_logged_in = matches!(
            self.account_info.as_ref().map(|i| &i.device),
            Some(DeviceState::LoggedIn(_)),
        ) && !matches!(device, DeviceState::LoggedIn(_));
        // Cache the device first so the resolver's downstream effect
        // (renderer reads cached state when status flips) sees the
        // post-push value.
        match self.account_info.as_mut() {
            Some(info) => info.device = device.clone(),
            None => {
                self.account_info = Some(AccountInfo {
                    device: device.clone(),
                    data: None,
                })
            }
        }
        if leaving_logged_in {
            self.page_states.account.invalidate_devices();
        }
        self.resolve_pending_for_device(&device);
    }

    /// Replace the cached `AccountData` (expiry, paid_until). Pass `None` to
    /// clear (e.g. on logout). Always meaningful: `account_info` is initialized
    /// to `Some(LoggedOut, None)` in [`Self::new`] and never reset to `None`.
    pub fn set_account_data(&mut self, data: Option<AccountData>) {
        if let Some(info) = self.account_info.as_mut() {
            info.data = data;
        }
    }

    pub async fn refresh_account<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        self.run_operation(Operation::RefreshAccount, async |app| {
            app.account_info = Some(service.get_account().await?);
            Ok(())
        })
        .await
    }

    pub async fn login<S: MullvadService>(
        &mut self,
        service: &S,
        account: AccountNumber,
    ) -> Result<(), IntegrationError> {
        if account.trim().is_empty() {
            let error = IntegrationError::Validation("Account number cannot be empty".to_string());
            self.set_operation_failed(Operation::Login, &error);
            return Err(error);
        }
        // Two-stage: the RPC tells the daemon to register a device
        // with Mullvad's API; the operation flips to Success only when
        // the matching `DeviceChanged(LoggedIn)` push arrives via the
        // event stream. Account-data follow-up (`get_account_data`)
        // is handled in the run-loop's `apply_app_event` arm for
        // `DeviceChanged`, so we don't need to chain it here.
        self.start_push_op_unit(
            Operation::Login,
            super::connection::PendingPushMatch::Device {
                want: super::connection::DeviceWant::LoggedIn,
            },
            async || service.login(account.clone()).await,
        )
        .await
    }

    pub async fn logout<S: MullvadService>(&mut self, service: &S) -> Result<(), IntegrationError> {
        // Cache cleanup (clearing `account_info`, invalidating the
        // device list) is driven by the daemon's `DeviceChanged`
        // push: `set_device(LoggedOut|Revoked)` invalidates the
        // device list, and the run loop's `apply_app_event` arm
        // clears `account_data`. Logout's job is just to ask the
        // daemon to log out and wait for its push.
        self.start_push_op_unit(
            Operation::Logout,
            super::connection::PendingPushMatch::Device {
                want: super::connection::DeviceWant::LoggedOut,
            },
            async || service.logout().await,
        )
        .await
    }

    /// Fetch the device list for the currently logged-in account and
    /// cache it on `account_page_state.devices`. No-op (with a typed
    /// validation error) when the user isn't logged in. Used by the
    /// Manage devices sub-page; called on entry and on manual refresh.
    pub async fn list_devices<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        let Some(account) = self.current_account_number() else {
            let error =
                IntegrationError::Validation("cannot list devices: not logged in".to_string());
            self.set_operation_failed(Operation::ListDevices, &error);
            return Err(error);
        };
        self.page_states.account.devices_loading = true;
        let outcome = self
            .run_operation(Operation::ListDevices, async |app| {
                let devices = service.list_devices(account).await?;
                app.page_states.account.devices = Some(devices);
                app.page_states.account.devices_error = None;
                Ok(())
            })
            .await;
        self.page_states.account.devices_loading = false;
        if let Err(error) = &outcome {
            self.page_states.account.devices_error = Some(error.to_string());
        }
        outcome
    }

    /// Remove a device by id from the current account, then refresh
    /// the cached device list so the UI reflects the change. If the
    /// removed device is the *current* device the daemon will emit a
    /// `DeviceChanged(LoggedOut)` push; that path drops the cached
    /// list independently so the renderer doesn't fault.
    pub async fn remove_device<S: MullvadService>(
        &mut self,
        service: &S,
        device_id: DeviceId,
    ) -> Result<(), IntegrationError> {
        let Some(account) = self.current_account_number() else {
            let error =
                IntegrationError::Validation("cannot remove device: not logged in".to_string());
            self.set_operation_failed(Operation::RemoveDevice, &error);
            return Err(error);
        };
        self.run_operation(Operation::RemoveDevice, async |app| {
            service
                .remove_device(account.clone(), device_id.clone())
                .await?;
            // Re-fetch the list so the row disappears immediately
            // rather than after the next manual refresh.
            let refreshed = service.list_devices(account.clone()).await?;
            app.page_states.account.devices = Some(refreshed);
            app.page_states.account.devices_error = None;
            Ok(())
        })
        .await
    }

    /// Submit a voucher code to extend the account expiry. On success,
    /// refreshes account data so the Account page's "Paid until" line
    /// reflects the new expiry, and returns the [`VoucherSubmission`]
    /// so the caller can show a "added X days" notification.
    /// Rotate the current device's WireGuard key. Connectivity drops
    /// briefly during the swap (the daemon registers the new key with
    /// Mullvad's API and transitions the tunnel onto it), so callers
    /// gate this behind a confirmation overlay rather than firing it
    /// from a bare button press.
    pub async fn rotate_wireguard_key<S: MullvadService>(
        &mut self,
        service: &S,
    ) -> Result<(), IntegrationError> {
        self.run_operation(Operation::RotateWireGuardKey, async |_app| {
            service.rotate_wireguard_key().await
        })
        .await
    }

    pub async fn submit_voucher<S: MullvadService>(
        &mut self,
        service: &S,
        voucher: String,
    ) -> Result<VoucherSubmission, IntegrationError> {
        if voucher.trim().is_empty() {
            let error = IntegrationError::Validation("Voucher code cannot be empty".to_string());
            self.set_operation_failed(Operation::SubmitVoucher, &error);
            return Err(error);
        }
        self.run_operation(Operation::SubmitVoucher, async |app| {
            let submission = service.submit_voucher(voucher.clone()).await?;
            // Refresh account data so the cached `expiry` matches the
            // daemon's view; the daemon doesn't push for voucher
            // submissions, so we pull explicitly.
            app.refresh_account(service).await?;
            Ok(submission)
        })
        .await
    }

    /// Pulls the current account number out of cached `account_info`,
    /// returning `None` when not logged in. Used by the device-list and
    /// voucher orchestration paths; both need an account to act on.
    fn current_account_number(&self) -> Option<AccountNumber> {
        match self.account_info.as_ref()?.device {
            DeviceState::LoggedIn(ref account_and_device) => {
                Some(account_and_device.account_number.clone())
            }
            DeviceState::LoggedOut | DeviceState::Revoked => None,
        }
    }

    /// Read the current cached device for the logged-in account, used
    /// by the Manage devices sub-page to mark "current" vs "other"
    /// devices and to find the current device's metadata.
    pub fn current_device(&self) -> Option<&Device> {
        match &self.account_info.as_ref()?.device {
            DeviceState::LoggedIn(account_and_device) => Some(&account_and_device.device),
            DeviceState::LoggedOut | DeviceState::Revoked => None,
        }
    }
}
