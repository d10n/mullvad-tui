// SPDX-License-Identifier: GPL-3.0-or-later

use super::{InputFocus, InputOutcome};
use crate::{app::App, integration::MullvadService, tui::error::format_action_error};
use crossterm::event::{KeyCode, KeyEvent};

#[derive(Default)]
pub struct VoucherInputState {
    pub buffer: String,
    pub focus: InputFocus,
}

impl VoucherInputState {
    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        if let Some(outcome) = super::handle_nav_key(&mut self.focus, key) {
            return outcome;
        }
        if !matches!(self.focus, InputFocus::Field) {
            return InputOutcome::NotHandled;
        }
        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() => {
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

    /// Returns `true` to keep the modal open (so the user can correct
    /// an empty entry without retyping); `false` to close it.
    pub async fn submit<S: MullvadService>(&self, app: &mut App, service: &S) -> bool {
        let voucher = self.buffer.trim().to_string();
        if voucher.is_empty() {
            app.show_notification("Voucher code cannot be empty");
            return true;
        }
        match app.submit_voucher(service, voucher).await {
            Ok(submission) => {
                let days = submission.time_added / 86_400;
                app.show_notification(format!(
                    "Voucher accepted - added {days} days. New expiry: {}",
                    submission
                        .new_expiry
                        .with_timezone(&chrono::Local)
                        .format("%b %-d, %Y %-I:%M %p")
                ));
            }
            Err(error) => {
                app.show_notification(format_action_error("redeem voucher", &error));
            }
        }
        false
    }
}
