// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{app::App, integration::MullvadService};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub mod account;
pub mod custom_dns;
pub mod port;
pub mod relay_override;
pub mod split_tunnel;
pub mod voucher;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputOutcome {
    NotHandled,
    Handled,
    Submit,
    Cancel,
}

/// Which element of an input popup currently has focus. Each input
/// modal carries its own `InputFocus` rather than going through the
/// page-level focus engine - the run loop already routes every
/// keystroke through `InputMode::handle_key` while a modal is open,
/// so this state machine is self-contained.
///
/// Layout: the text field is on top; `[Cancel]` and `[Submit]` (or a
/// modal-specific submit label) sit on a row below it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InputFocus {
    /// Buffer is editable: char/backspace input goes here.
    #[default]
    Field,
    Cancel,
    Submit,
}

impl InputFocus {
    /// Tab order: Field -> Cancel -> Submit -> Field.
    fn next(self) -> Self {
        match self {
            Self::Field => Self::Cancel,
            Self::Cancel => Self::Submit,
            Self::Submit => Self::Field,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Field => Self::Submit,
            Self::Cancel => Self::Field,
            Self::Submit => Self::Cancel,
        }
    }
}

/// Try to dispatch a navigation/activation keystroke that's shared
/// by every input modal: Tab/Shift+Tab cycle focus, ↑↓ jump between
/// the field and the button row, ←→ move along the button row,
/// Enter activates the focused element (Submit on Field/Submit,
/// Cancel on Cancel), and Esc always cancels.
///
/// Returns `Some(outcome)` when the key is owned by the navigation
/// layer; the caller should return that outcome from its own
/// `handle_key`. `None` means the key isn't a nav/activation key -
/// the caller's per-modal char/backspace logic should run, gated on
/// whether `focus` is currently `Field`.
pub fn handle_nav_key(focus: &mut InputFocus, key: KeyEvent) -> Option<InputOutcome> {
    match key.code {
        KeyCode::Tab if !key.modifiers.contains(KeyModifiers::SHIFT) => {
            *focus = focus.next();
            Some(InputOutcome::Handled)
        }
        KeyCode::Tab | KeyCode::BackTab => {
            *focus = focus.prev();
            Some(InputOutcome::Handled)
        }
        KeyCode::Down if matches!(*focus, InputFocus::Field) => {
            *focus = InputFocus::Cancel;
            Some(InputOutcome::Handled)
        }
        KeyCode::Up if !matches!(*focus, InputFocus::Field) => {
            *focus = InputFocus::Field;
            Some(InputOutcome::Handled)
        }
        KeyCode::Left if matches!(*focus, InputFocus::Submit) => {
            *focus = InputFocus::Cancel;
            Some(InputOutcome::Handled)
        }
        KeyCode::Right if matches!(*focus, InputFocus::Cancel) => {
            *focus = InputFocus::Submit;
            Some(InputOutcome::Handled)
        }
        KeyCode::Enter => Some(match *focus {
            InputFocus::Cancel => InputOutcome::Cancel,
            InputFocus::Field | InputFocus::Submit => InputOutcome::Submit,
        }),
        KeyCode::Esc => Some(InputOutcome::Cancel),
        _ => None,
    }
}

#[derive(Default)]
pub enum InputMode {
    #[default]
    Default,
    AccountInput(account::AccountInputState),
    PortInput(port::PortInputState),
    VoucherInput(voucher::VoucherInputState),
    CustomDnsInput(custom_dns::CustomDnsInputState),
    SplitTunnelPathInput(split_tunnel::SplitTunnelPathInputState),
    SplitTunnelPidInput(split_tunnel::SplitTunnelPidInputState),
    RelayOverrideInput(relay_override::RelayOverrideInputState),
}

impl InputMode {
    /// Forward the keystroke to whichever modal is open. Returns
    /// [`InputOutcome::NotHandled`] when no modal is open (the run
    /// loop falls through to its non-modal handlers).
    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        match self {
            Self::Default => InputOutcome::NotHandled,
            Self::AccountInput(state) => state.handle_key(key),
            Self::PortInput(state) => state.handle_key(key),
            Self::VoucherInput(state) => state.handle_key(key),
            Self::CustomDnsInput(state) => state.handle_key(key),
            Self::SplitTunnelPathInput(state) => state.handle_key(key),
            Self::SplitTunnelPidInput(state) => state.handle_key(key),
            Self::RelayOverrideInput(state) => state.handle_key(key),
        }
    }

    /// Route a 0-based field index to the open modal's internal focus
    /// state. Mouse-click dispatch calls this when the user clicks a
    /// specific buffer row in a multi-field modal; single-field modals
    /// treat any non-zero index as a no-op (they only have one buffer).
    pub fn set_field_index(&mut self, index: usize) {
        match self {
            Self::Default => {}
            // Single-field modals: only index 0 maps to "the field".
            // A click on a non-existent further field is silently
            // ignored - in practice this can't happen because the
            // renderer for a single-field modal only registers one
            // buffer rect, but defensive against future refactors.
            Self::AccountInput(state) => {
                if index == 0 {
                    state.focus = InputFocus::Field;
                }
            }
            Self::PortInput(state) => {
                if index == 0 {
                    state.focus = InputFocus::Field;
                }
            }
            Self::VoucherInput(state) => {
                if index == 0 {
                    state.focus = InputFocus::Field;
                }
            }
            Self::CustomDnsInput(state) => {
                if index == 0 {
                    state.focus = InputFocus::Field;
                }
            }
            Self::SplitTunnelPathInput(state) => {
                if index == 0 {
                    state.focus = InputFocus::Field;
                }
            }
            Self::SplitTunnelPidInput(state) => {
                if index == 0 {
                    state.focus = InputFocus::Field;
                }
            }
            Self::RelayOverrideInput(state) => {
                let next = match index {
                    0 => relay_override::FieldFocus::Hostname,
                    1 => relay_override::FieldFocus::Ipv4,
                    2 => relay_override::FieldFocus::Ipv6,
                    _ => return,
                };
                state.focus = next;
            }
        }
    }

    /// Set the open modal's internal focus. Click dispatch uses this
    /// to land focus on the field (when the user clicks the buffer
    /// row) so the next keystroke starts typing instead of triggering
    /// whatever button was previously focused.
    pub fn set_focus(&mut self, focus: InputFocus) {
        match self {
            Self::Default => {}
            Self::AccountInput(state) => state.focus = focus,
            Self::PortInput(state) => state.focus = focus,
            Self::VoucherInput(state) => state.focus = focus,
            Self::CustomDnsInput(state) => state.focus = focus,
            Self::SplitTunnelPathInput(state) => state.focus = focus,
            Self::SplitTunnelPidInput(state) => state.focus = focus,
            // The relay-override modal has 3 typeable buffers, so a
            // bare `[Cancel]/[Save]` focus is mapped to its 5-position
            // focus: `Field` -> hostname (the modal's default landing);
            // `Cancel`/`Submit` -> the matching button.
            Self::RelayOverrideInput(state) => {
                state.focus = match focus {
                    InputFocus::Field => relay_override::FieldFocus::Hostname,
                    InputFocus::Cancel => relay_override::FieldFocus::Cancel,
                    InputFocus::Submit => relay_override::FieldFocus::Save,
                };
            }
        }
    }

    /// Submit the open modal. Returns `true` to keep the modal open
    /// (validation failure that should preserve the buffer so the user
    /// can correct it), `false` to close it. `Default` returns `false`
    /// - there is nothing to submit.
    pub async fn submit<S: MullvadService>(&self, app: &mut App, service: &S) -> bool {
        match self {
            Self::Default => false,
            Self::AccountInput(state) => {
                state.submit(app, service).await;
                false
            }
            Self::PortInput(state) => {
                state.submit(app, service).await;
                false
            }
            Self::VoucherInput(state) => state.submit(app, service).await,
            Self::CustomDnsInput(state) => state.submit(app, service).await,
            Self::SplitTunnelPathInput(state) => state.submit(app, service).await,
            Self::SplitTunnelPidInput(state) => state.submit(app, service).await,
            Self::RelayOverrideInput(state) => state.submit(app, service).await,
        }
    }

    /// True for every variant except [`Self::Default`]. Used by the
    /// run loop to detect open <-> close transitions so it can
    /// save/restore page focus around the modal's lifetime - the
    /// modal-render block in [`crate::tui::run_app`] wipes the focus
    /// registry to its own buttons, so without an externally-tracked
    /// return slot the page focus would snap to the page's first body
    /// widget on close.
    pub fn is_open(&self) -> bool {
        !matches!(self, Self::Default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_focus_cycles_forward_and_back() {
        // Tab order: Field -> Cancel -> Submit -> Field; Shift+Tab the
        // reverse. Locks the layout assumption that Cancel is left
        // of Submit on the button row.
        assert_eq!(InputFocus::Field.next(), InputFocus::Cancel);
        assert_eq!(InputFocus::Cancel.next(), InputFocus::Submit);
        assert_eq!(InputFocus::Submit.next(), InputFocus::Field);
        assert_eq!(InputFocus::Field.prev(), InputFocus::Submit);
        assert_eq!(InputFocus::Cancel.prev(), InputFocus::Field);
        assert_eq!(InputFocus::Submit.prev(), InputFocus::Cancel);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn enter_on_field_or_submit_submits_cancel_button_cancels() {
        let mut focus = InputFocus::Field;
        assert!(matches!(
            handle_nav_key(&mut focus, key(KeyCode::Enter)),
            Some(InputOutcome::Submit)
        ));

        let mut focus = InputFocus::Submit;
        assert!(matches!(
            handle_nav_key(&mut focus, key(KeyCode::Enter)),
            Some(InputOutcome::Submit)
        ));

        let mut focus = InputFocus::Cancel;
        assert!(matches!(
            handle_nav_key(&mut focus, key(KeyCode::Enter)),
            Some(InputOutcome::Cancel)
        ));
    }

    #[test]
    fn esc_always_cancels_regardless_of_focus() {
        for start in [InputFocus::Field, InputFocus::Cancel, InputFocus::Submit] {
            let mut focus = start;
            assert!(
                matches!(
                    handle_nav_key(&mut focus, key(KeyCode::Esc)),
                    Some(InputOutcome::Cancel)
                ),
                "Esc on {start:?} must always cancel",
            );
        }
    }

    #[test]
    fn arrow_keys_navigate_field_and_button_row() {
        // ↓ from Field jumps to Cancel; ↑ from any button returns to
        // Field; ← / → move along the button row.
        let mut focus = InputFocus::Field;
        handle_nav_key(&mut focus, key(KeyCode::Down));
        assert_eq!(focus, InputFocus::Cancel);

        handle_nav_key(&mut focus, key(KeyCode::Right));
        assert_eq!(focus, InputFocus::Submit);

        handle_nav_key(&mut focus, key(KeyCode::Left));
        assert_eq!(focus, InputFocus::Cancel);

        handle_nav_key(&mut focus, key(KeyCode::Up));
        assert_eq!(focus, InputFocus::Field);
    }

    #[test]
    fn tab_cycles_and_shift_tab_reverses() {
        let mut focus = InputFocus::Field;
        handle_nav_key(&mut focus, key(KeyCode::Tab));
        assert_eq!(focus, InputFocus::Cancel);
        handle_nav_key(&mut focus, key(KeyCode::Tab));
        assert_eq!(focus, InputFocus::Submit);
        handle_nav_key(&mut focus, key(KeyCode::Tab));
        assert_eq!(focus, InputFocus::Field);

        // BackTab from terminals that emit it explicitly.
        handle_nav_key(&mut focus, key(KeyCode::BackTab));
        assert_eq!(focus, InputFocus::Submit);

        // Shift+Tab on terminals that send Tab+SHIFT instead.
        let shift_tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        handle_nav_key(&mut focus, shift_tab);
        assert_eq!(focus, InputFocus::Cancel);
    }
}
