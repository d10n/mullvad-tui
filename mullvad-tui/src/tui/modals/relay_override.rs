// SPDX-License-Identifier: GPL-3.0-or-later

//! `Set server IP override` modal. Three labeled buffers (hostname,
//! IPv4 in-address, IPv6 in-address) + Cancel/Save buttons. Submitting
//! routes through [`App::set_relay_override`]; submitting with a known
//! hostname and both addresses blank removes that hostname's override
//! (the daemon's `set_relay_override` swap-removes empty entries).

use std::{
    net::{Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

use super::InputOutcome;
use crate::{
    app::App,
    integration::{MullvadService, RelayOverride},
    tui::error::format_action_error,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Which of the modal's five focusables currently owns input. The
/// `[Cancel]`/`[Save]` half mirrors the single-field modals' [`InputFocus`];
/// the three field positions are local to this modal because no other
/// modal has multiple text fields.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FieldFocus {
    #[default]
    Hostname,
    Ipv4,
    Ipv6,
    Cancel,
    Save,
}

impl FieldFocus {
    /// Tab order: Hostname -> Ipv4 -> Ipv6 -> Cancel -> Save -> Hostname.
    fn next(self) -> Self {
        match self {
            Self::Hostname => Self::Ipv4,
            Self::Ipv4 => Self::Ipv6,
            Self::Ipv6 => Self::Cancel,
            Self::Cancel => Self::Save,
            Self::Save => Self::Hostname,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Hostname => Self::Save,
            Self::Ipv4 => Self::Hostname,
            Self::Ipv6 => Self::Ipv4,
            Self::Cancel => Self::Ipv6,
            Self::Save => Self::Cancel,
        }
    }

    fn is_field(self) -> bool {
        matches!(self, Self::Hostname | Self::Ipv4 | Self::Ipv6)
    }
}

#[derive(Default)]
pub struct RelayOverrideInputState {
    pub hostname: String,
    pub ipv4: String,
    pub ipv6: String,
    pub focus: FieldFocus,
}

impl RelayOverrideInputState {
    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        match key.code {
            KeyCode::Esc => return InputOutcome::Cancel,
            KeyCode::Enter => {
                return match self.focus {
                    FieldFocus::Cancel => InputOutcome::Cancel,
                    _ => InputOutcome::Submit,
                };
            }
            KeyCode::Tab if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.focus = self.focus.next();
                return InputOutcome::Handled;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = self.focus.prev();
                return InputOutcome::Handled;
            }
            KeyCode::Down => {
                self.focus = match self.focus {
                    FieldFocus::Hostname => FieldFocus::Ipv4,
                    FieldFocus::Ipv4 => FieldFocus::Ipv6,
                    FieldFocus::Ipv6 => FieldFocus::Cancel,
                    FieldFocus::Cancel | FieldFocus::Save => self.focus,
                };
                return InputOutcome::Handled;
            }
            KeyCode::Up => {
                self.focus = match self.focus {
                    FieldFocus::Hostname => self.focus,
                    FieldFocus::Ipv4 => FieldFocus::Hostname,
                    FieldFocus::Ipv6 => FieldFocus::Ipv4,
                    FieldFocus::Cancel | FieldFocus::Save => FieldFocus::Ipv6,
                };
                return InputOutcome::Handled;
            }
            KeyCode::Left if matches!(self.focus, FieldFocus::Save) => {
                self.focus = FieldFocus::Cancel;
                return InputOutcome::Handled;
            }
            KeyCode::Right if matches!(self.focus, FieldFocus::Cancel) => {
                self.focus = FieldFocus::Save;
                return InputOutcome::Handled;
            }
            _ => {}
        }
        if !self.focus.is_field() {
            return InputOutcome::NotHandled;
        }
        match key.code {
            KeyCode::Char(c) if char_allowed(self.focus, c) => {
                self.buffer_mut().push(c);
                InputOutcome::Handled
            }
            KeyCode::Backspace => {
                self.buffer_mut().pop();
                InputOutcome::Handled
            }
            _ => InputOutcome::NotHandled,
        }
    }

    fn buffer_mut(&mut self) -> &mut String {
        match self.focus {
            FieldFocus::Hostname => &mut self.hostname,
            FieldFocus::Ipv4 => &mut self.ipv4,
            FieldFocus::Ipv6 => &mut self.ipv6,
            FieldFocus::Cancel | FieldFocus::Save => unreachable!("guarded by is_field"),
        }
    }

    /// Returns `true` to keep the modal open after a validation error;
    /// `false` to close. Empty hostname keeps the modal open; an invalid
    /// IP address keeps the modal open; an unparseable empty pair (both
    /// IPs blank) is rejected because the daemon would treat it as a
    /// remove request, which isn't what this modal is for.
    pub async fn submit<S: MullvadService>(&self, app: &mut App, service: &S) -> bool {
        let hostname = self.hostname.trim().to_string();
        if hostname.is_empty() {
            app.show_notification("Hostname is required");
            return true;
        }
        let ipv4_trim = self.ipv4.trim();
        let ipv6_trim = self.ipv6.trim();
        if ipv4_trim.is_empty() && ipv6_trim.is_empty() {
            app.show_notification(
                "Provide at least one of IPv4 or IPv6. To remove an existing override, use [Remove] on its row.",
            );
            return true;
        }
        let ipv4_addr_in = if ipv4_trim.is_empty() {
            None
        } else {
            match Ipv4Addr::from_str(ipv4_trim) {
                Ok(addr) => Some(addr),
                Err(_) => {
                    app.show_notification(format!("`{ipv4_trim}` is not a valid IPv4 address"));
                    return true;
                }
            }
        };
        let ipv6_addr_in = if ipv6_trim.is_empty() {
            None
        } else {
            match Ipv6Addr::from_str(ipv6_trim) {
                Ok(addr) => Some(addr),
                Err(_) => {
                    app.show_notification(format!("`{ipv6_trim}` is not a valid IPv6 address"));
                    return true;
                }
            }
        };
        let relay_override = RelayOverride {
            hostname,
            ipv4_addr_in,
            ipv6_addr_in,
        };
        if let Err(error) = app.set_relay_override(service, relay_override).await {
            app.show_notification(format_action_error("set relay override", &error));
        }
        false
    }
}

fn char_allowed(focus: FieldFocus, ch: char) -> bool {
    match focus {
        // Hostnames are mullvad-server-name shaped (e.g. `se-got-wg-001`).
        // Accept ASCII alphanumeric, hyphen, and dot for forward
        // compatibility with future naming.
        FieldFocus::Hostname => ch.is_ascii_alphanumeric() || ch == '-' || ch == '.',
        // IPv4 in dotted-decimal: digits and dots.
        FieldFocus::Ipv4 => ch.is_ascii_digit() || ch == '.',
        // IPv6: hex digits and colons; brackets are not needed inside
        // an address-only field but accept them for `[::1]` paste tolerance.
        FieldFocus::Ipv6 => ch.is_ascii_hexdigit() || matches!(ch, ':' | '[' | ']'),
        FieldFocus::Cancel | FieldFocus::Save => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn tab_cycles_field_to_button_row_and_back() {
        let mut state = RelayOverrideInputState::default();
        assert_eq!(state.focus, FieldFocus::Hostname);
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.focus, FieldFocus::Ipv4);
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.focus, FieldFocus::Ipv6);
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.focus, FieldFocus::Cancel);
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.focus, FieldFocus::Save);
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.focus, FieldFocus::Hostname);
    }

    #[test]
    fn arrow_navigation_walks_fields_and_buttons() {
        let mut state = RelayOverrideInputState::default();
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.focus, FieldFocus::Ipv4);
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.focus, FieldFocus::Ipv6);
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.focus, FieldFocus::Cancel);
        state.handle_key(key(KeyCode::Right));
        assert_eq!(state.focus, FieldFocus::Save);
        state.handle_key(key(KeyCode::Left));
        assert_eq!(state.focus, FieldFocus::Cancel);
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.focus, FieldFocus::Ipv6);
    }

    #[test]
    fn typing_on_hostname_field_appends_to_hostname_only() {
        let mut state = RelayOverrideInputState::default();
        for c in "se-got-wg-001".chars() {
            state.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(state.hostname, "se-got-wg-001");
        assert!(state.ipv4.is_empty());
        assert!(state.ipv6.is_empty());
    }

    #[test]
    fn ipv4_field_rejects_non_digit_characters() {
        let mut state = RelayOverrideInputState {
            focus: FieldFocus::Ipv4,
            ..Default::default()
        };
        for c in "1.2.3.4".chars() {
            state.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(state.ipv4, "1.2.3.4");
        let outcome = state.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::NotHandled));
        assert_eq!(state.ipv4, "1.2.3.4");
    }

    #[test]
    fn enter_on_cancel_button_cancels() {
        let mut state = RelayOverrideInputState {
            focus: FieldFocus::Cancel,
            ..Default::default()
        };
        let outcome = state.handle_key(key(KeyCode::Enter));
        assert!(matches!(outcome, InputOutcome::Cancel));
    }

    #[test]
    fn enter_on_any_field_or_save_submits() {
        for focus in [
            FieldFocus::Hostname,
            FieldFocus::Ipv4,
            FieldFocus::Ipv6,
            FieldFocus::Save,
        ] {
            let mut state = RelayOverrideInputState {
                focus,
                ..Default::default()
            };
            let outcome = state.handle_key(key(KeyCode::Enter));
            assert!(
                matches!(outcome, InputOutcome::Submit),
                "Enter on {focus:?} should submit",
            );
        }
    }

    #[test]
    fn esc_always_cancels() {
        for focus in [
            FieldFocus::Hostname,
            FieldFocus::Ipv4,
            FieldFocus::Ipv6,
            FieldFocus::Cancel,
            FieldFocus::Save,
        ] {
            let mut state = RelayOverrideInputState {
                focus,
                ..Default::default()
            };
            let outcome = state.handle_key(key(KeyCode::Esc));
            assert!(
                matches!(outcome, InputOutcome::Cancel),
                "Esc on {focus:?} should cancel",
            );
        }
    }
}
