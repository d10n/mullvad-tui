// SPDX-License-Identifier: GPL-3.0-or-later

use super::{InputFocus, InputOutcome};
use crate::{app::App, integration::MullvadService, tui::error::format_action_error};
use crossterm::event::{KeyCode, KeyEvent};

#[derive(Default)]
pub struct PortInputState {
    pub buffer: String,
    /// The anti-censorship mode we're editing the port for.
    pub mode: crate::integration::SelectedObfuscation,
    pub focus: InputFocus,
}

impl PortInputState {
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
        match parse_port_input(&self.buffer) {
            Ok(port) => {
                if let Err(error) = app.set_anti_censorship_port(service, self.mode, port).await {
                    app.show_notification(format_action_error("port update", &error));
                }
            }
            Err(message) => app.show_notification(message),
        }
    }
}

pub fn parse_port_input(buffer: &str) -> Result<Option<u16>, String> {
    let trimmed = buffer.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.parse::<u16>() {
        Ok(0) => Err("Port 0 is reserved; use 1..65535".to_string()),
        Ok(port) => Ok(Some(port)),
        Err(_) => Err("Port must be a number between 1 and 65535".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_port_input_handles_blank_valid_zero_and_oversize() {
        assert_eq!(parse_port_input(""), Ok(None));
        assert_eq!(parse_port_input("1234"), Ok(Some(1234)));
        assert!(parse_port_input("0").is_err());
        assert!(parse_port_input("65536").is_err());
    }
}
