// SPDX-License-Identifier: GPL-3.0-or-later

use crate::integration::IntegrationError;
use mullvad_management_interface::Error as DaemonError;

pub fn format_action_error(action: &str, error: &IntegrationError) -> String {
    if let Some(specific) = notification_for_typed_error(error) {
        return specific.to_string();
    }
    let detail = match error {
        IntegrationError::Validation(msg) => msg.clone(),
        IntegrationError::Rpc(arc) => render_error_chain(arc.as_ref()),
    };
    format!("Could not {action}. Check Mullvad daemon/login state and try again. Details: {detail}")
}

/// Walk an error's `source()` chain and join every link with `": "`.
pub fn render_error_chain(error: &dyn std::error::Error) -> String {
    let mut chain = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        chain.push_str(": ");
        chain.push_str(&cause.to_string());
        source = cause.source();
    }
    chain
}

/// Map a known typed daemon error to actionable user-facing copy.
pub fn notification_for_typed_error(error: &IntegrationError) -> Option<&'static str> {
    let IntegrationError::Rpc(arc) = error else {
        return None;
    };
    match &**arc {
        DaemonError::AlreadyLoggedIn => {
            Some("Already logged in. Log out first to switch accounts.")
        }
        DaemonError::InvalidAccount => Some("Account number not recognized by Mullvad."),
        DaemonError::DeviceNotFound => {
            Some("This device has been removed from the account. Log in again.")
        }
        DaemonError::TooManyDevices => {
            Some("Account has the maximum number of devices. Revoke one before logging in.")
        }
        DaemonError::InvalidVoucher => Some("Voucher code is not valid."),
        DaemonError::UsedVoucher => Some("Voucher code has already been redeemed."),
        DaemonError::NoLocationData => Some("Location data is unavailable from the daemon."),
        _ => None,
    }
}
