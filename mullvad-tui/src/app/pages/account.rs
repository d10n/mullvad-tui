// SPDX-License-Identifier: GPL-3.0-or-later

//! Account-page transient UI state.
//!
//! - `account_number_visible` toggles the masked / unmasked rendering of the account number on the
//!   Account page (the `[Show]` / `[Hide]` button). Defaults to false so a casual viewer of the
//!   screen doesn't see the number on first paint.
//! - `devices` caches the result of the most recent `list_devices` call for the Manage devices
//!   sub-page. It's invalidated (set back to None) by login / logout / explicit refresh; the
//!   sub-page render triggers a fetch when this is None.
//! - `devices_error` holds the most recent fetch failure, surfaced as a "could not load devices"
//!   line on the sub-page so transient network errors don't disappear into the void.

use crate::integration::Device;

#[derive(Debug, Default)]
pub struct PageState {
    pub account_number_visible: bool,
    pub devices: Option<Vec<Device>>,
    pub devices_loading: bool,
    pub devices_error: Option<String>,
}

impl PageState {
    /// Drop the cached devices list and any associated error. Called on
    /// login / logout transitions so a stale list from the previous
    /// account never leaks across sessions.
    pub fn invalidate_devices(&mut self) {
        self.devices = None;
        self.devices_error = None;
    }
}
