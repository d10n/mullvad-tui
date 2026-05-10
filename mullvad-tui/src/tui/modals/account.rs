// SPDX-License-Identifier: GPL-3.0-or-later

use super::{InputFocus, InputOutcome};
use crate::{app::App, integration::MullvadService, tui::error::format_action_error};
use crossterm::event::{KeyCode, KeyEvent};

#[derive(Default)]
pub struct AccountInputState {
    pub buffer: String,
    pub focus: InputFocus,
}

impl AccountInputState {
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

    pub async fn submit<S: MullvadService>(&self, app: &mut App, service: &S) {
        let account = self.buffer.trim().to_string();
        if let Err(error_message) = validate_account_number(&account) {
            app.show_notification(error_message);
            return;
        }
        if let Err(error) = app.login(service, account).await {
            app.show_notification(format_action_error("login", &error));
        }
    }
}

pub fn validate_account_number(account: &str) -> Result<(), &'static str> {
    if account.is_empty() {
        return Err("Account number cannot be empty");
    }
    if !account.chars().all(|c| c.is_ascii_digit()) {
        return Err("Account number must contain only digits");
    }
    if account.len() != 16 {
        return Err("Account number must be exactly 16 digits");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_validation_covers_valid_invalid_and_empty() {
        assert!(validate_account_number("1234123412341234").is_ok());
        assert!(validate_account_number("123").is_err());
        assert!(validate_account_number("").is_err());
        assert!(validate_account_number("not-digits-16-len").is_err());
    }
}
