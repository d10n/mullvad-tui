// SPDX-License-Identifier: GPL-3.0-or-later

use super::{InputFocus, InputOutcome};
use crate::{app::App, integration::MullvadService, tui::error::format_action_error};
use crossterm::event::{KeyCode, KeyEvent};

#[derive(Default)]
pub struct SplitTunnelPathInputState {
    pub buffer: String,
    pub focus: InputFocus,
}

impl SplitTunnelPathInputState {
    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        if let Some(outcome) = super::handle_nav_key(&mut self.focus, key) {
            return outcome;
        }
        if !matches!(self.focus, InputFocus::Field) {
            return InputOutcome::NotHandled;
        }
        match key.code {
            KeyCode::Char(c) => {
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

    /// Returns `true` to keep the modal open (empty path); `false` to
    /// close it (success path or daemon-side failure).
    pub async fn submit<S: MullvadService>(&self, app: &mut App, service: &S) -> bool {
        let path = self.buffer.trim().to_string();
        if path.is_empty() {
            app.show_notification("Path is required");
            return true;
        }
        if let Err(error) = app.add_split_tunnel_app(service, path).await {
            app.show_notification(format_action_error("add split-tunnel app", &error));
        }
        false
    }
}

#[derive(Default)]
pub struct SplitTunnelPidInputState {
    pub buffer: String,
    pub focus: InputFocus,
}

impl SplitTunnelPidInputState {
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

    /// Returns `true` on parse failure to keep the modal open (so the
    /// user can correct a typo without retyping); `false` once a parse
    /// succeeds (regardless of whether the daemon accepts the PID).
    pub async fn submit<S: MullvadService>(&self, app: &mut App, service: &S) -> bool {
        match parse_pid_input(&self.buffer) {
            Ok(pid) => {
                if let Err(error) = app.add_split_tunnel_process(service, pid).await {
                    app.show_notification(format_action_error("add split-tunnel PID", &error));
                }
                false
            }
            Err(message) => {
                app.show_notification(message);
                true
            }
        }
    }
}

pub fn parse_pid_input(buffer: &str) -> Result<i32, String> {
    match buffer.parse::<i32>() {
        Ok(pid) if pid > 0 => Ok(pid),
        Ok(_) => Err("PID must be a positive integer".to_string()),
        Err(_) => Err("PID must be a numeric integer".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pid_input_handles_valid_zero_and_oversize() {
        assert_eq!(parse_pid_input("1234"), Ok(1234));
        assert!(parse_pid_input("0").is_err());
        assert!(parse_pid_input("-1").is_err());
        assert!(parse_pid_input("not-a-number").is_err());
    }
}
