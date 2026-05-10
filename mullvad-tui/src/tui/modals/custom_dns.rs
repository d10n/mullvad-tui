// SPDX-License-Identifier: GPL-3.0-or-later

use super::{InputFocus, InputOutcome};
use crate::{app::App, integration::MullvadService, tui::error::format_action_error};
use crossterm::event::{KeyCode, KeyEvent};
use std::{net::IpAddr, str::FromStr};

#[derive(Default)]
pub struct CustomDnsInputState {
    pub buffer: String,
    /// `Some(index)` when the modal was opened from a row's `[Edit]`
    /// button - submit replaces that row's address rather than
    /// appending a new one. `None` when opened from `[Add server]`.
    pub edit_index: Option<usize>,
    pub focus: InputFocus,
}

impl CustomDnsInputState {
    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        if let Some(outcome) = super::handle_nav_key(&mut self.focus, key) {
            return outcome;
        }
        if !matches!(self.focus, InputFocus::Field) {
            return InputOutcome::NotHandled;
        }
        match key.code {
            KeyCode::Char(c) if is_ip_char(c) => {
                self.buffer.push(c);
                InputOutcome::Handled
            }
            KeyCode::Backspace => {
                self.buffer.pop();
                InputOutcome::Handled
            }
            _ => InputOutcome::NotHandled,
        }
    }

    /// Submit the current buffer. Returns `true` if the modal should stay open
    /// (e.g. on validation error), `false` if it should close.
    pub async fn submit<S: MullvadService>(&self, app: &mut App, service: &S) -> bool {
        let trimmed = self.buffer.trim();
        match IpAddr::from_str(trimmed) {
            Ok(addr) => {
                let result = match self.edit_index {
                    Some(index) => app.replace_custom_dns(service, index, addr).await,
                    None => app.add_custom_dns(service, addr).await,
                };
                if let Err(error) = result {
                    app.show_notification(format_action_error("custom DNS server", &error));
                }
                false
            }
            Err(_) => {
                app.show_notification(format!("'{trimmed}' is not a valid IPv4 or IPv6 address"));
                true
            }
        }
    }
}

pub fn is_ip_char(ch: char) -> bool {
    // IPv4, IPv6 (hex digits + `:`), and the bracketed `[::1]` form.
    ch.is_ascii_hexdigit() || matches!(ch, '.' | ':' | '[' | ']')
}
