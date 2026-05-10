// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{ConfirmAction, WidgetId};

#[derive(Default, Debug)]
pub enum OverlayMode {
    #[default]
    None,
    /// Daemon-side confirmation prompt (Disconnect / Logout /
    /// ToggleLockdown / RotateWireGuardKey). Title and message are
    /// the user-facing copy; `action` is the [`ConfirmAction`] tag
    /// the run loop dispatches to via [`crate::tui::dispatch_confirm`] when the
    /// user activates `[Confirm]`.
    Confirm {
        title: String,
        message: String,
        action: ConfirmAction,
        /// Page-widget id focused immediately before the overlay
        /// opened, restored on dismissal. `None` only when the
        /// overlay opened with no prior focus (rare - happens during
        /// startup before any user interaction).
        return_focus: Option<WidgetId>,
    },
    /// One-line notification surfaced by `App::show_notification`,
    /// drained from `App.notification_tx` into here by the main
    /// `select!`. Dismissed via `[Dismiss]` (focus-engine button) or
    /// `Esc` from `Action::Cancel`.
    Notification {
        message: String,
        /// See [`OverlayMode::Confirm::return_focus`]. If a
        /// notification arrives while another overlay is already open,
        /// the original `return_focus` is preserved (the user
        /// dismissing the notification then sees their
        /// pre-original-overlay focus, not the now-stale
        /// overlay-button focus).
        return_focus: Option<WidgetId>,
    },
}

impl OverlayMode {
    /// Read the captured `return_focus`, if any. `None` when the
    /// overlay is `None` or opened with no prior focus. Used by
    /// Cancel/Activate dispatch to restore page focus when the
    /// overlay closes.
    pub fn return_focus(&self) -> Option<WidgetId> {
        match self {
            Self::None => None,
            Self::Confirm { return_focus, .. } | Self::Notification { return_focus, .. } => {
                *return_focus
            }
        }
    }
}
