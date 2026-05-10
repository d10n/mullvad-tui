// SPDX-License-Identifier: GPL-3.0-or-later

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::app::{ArrowDir, PageId};

/// Global key actions. Most user actions are reached through the focus
/// engine (arrow keys move focus, Enter activates the focused widget);
/// the variants here are the global accelerators that don't have a
/// sensible "focusable widget" home:
///
/// - `Quit`: app-wide command, not page-specific.
/// - `Arrow` / `Activate` / `Cancel`: the focus-engine primitives.
/// - `Home`/`End`/`PageUp`/`PageDown`: bulk navigation through focusable rows - useful in
///   scrollable lists (Select location, Logs).
/// - `NavigateTab(PageId)`: the `1`-`4` digit shortcuts that jump between the four top-level tabs
///   (Status/Account/Settings/Logs).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Quit,
    /// Move focus in a direction. Arrows-only when no
    /// modal/text-entry/selector is active; existing modal handlers
    /// consume arrows when they're open.
    Arrow(ArrowDir),
    /// `Esc`. Closes a daemon-side modal, cancels in-flight text entry,
    /// or pops the current sub-page back to its parent - whichever is
    /// applicable in priority order. No-op on a top-level page with no
    /// open overlay.
    Cancel,
    /// `Enter`. Activates the focused widget (when no overlay is open)
    /// or confirms the open daemon-side modal. The two routes share
    /// `Enter` because there's no situation where both a focusable
    /// widget and a modal are simultaneously interactable.
    Activate,
    /// `Home`: jump focus to the first body row (skipping the tab bar).
    Home,
    /// `End`: jump focus to the last body row.
    End,
    /// `PageUp`: move focus up by [`PAGE_STEP_ROWS`] rows.
    PageUp,
    /// `PageDown`: move focus down by [`PAGE_STEP_ROWS`] rows.
    PageDown,
    /// `Tab`: cycle focus to the next pane on the current page. A
    /// pane is the tab bar, a scroll group, or a contiguous run of
    /// non-grouped body rows; the focus engine derives the partition
    /// from the registry's scroll-group markers.
    CycleNextPane,
    /// `Shift+Tab` / `BackTab`: cycle focus to the previous pane.
    CyclePrevPane,
    /// Jump directly to a top-level tab via the `1`-`4` digit keys.
    /// Keeps muscle memory working for users who want to skip the
    /// arrow-key dance for a known target.
    NavigateTab(PageId),
}

/// Number of focus rows skipped per `PageUp`/`PageDown` press. Tuned
/// empirically against the default `MAX_APP_HEIGHT` (36 rows) - the
/// scrollable lists fit ~25 rows of content at most, so 10 rows per
/// page-key gives 2-3 presses to traverse a fully-loaded list without
/// jumping past it in a single keystroke.
pub const PAGE_STEP_ROWS: usize = 10;

pub fn map_key_event(key: KeyEvent) -> Option<Action> {
    if key.kind != KeyEventKind::Press {
        return None;
    }

    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Up => Some(Action::Arrow(ArrowDir::Up)),
        KeyCode::Down => Some(Action::Arrow(ArrowDir::Down)),
        KeyCode::Left => Some(Action::Arrow(ArrowDir::Left)),
        KeyCode::Right => Some(Action::Arrow(ArrowDir::Right)),
        KeyCode::Home => Some(Action::Home),
        KeyCode::End => Some(Action::End),
        KeyCode::PageUp => Some(Action::PageUp),
        KeyCode::PageDown => Some(Action::PageDown),
        KeyCode::Tab => {
            // Shift+Tab on most terminals comes through as
            // `KeyCode::BackTab`, but a few send `Tab` with the
            // `SHIFT` modifier set. Handle both for portability.
            if key
                .modifiers
                .contains(crossterm::event::KeyModifiers::SHIFT)
            {
                Some(Action::CyclePrevPane)
            } else {
                Some(Action::CycleNextPane)
            }
        }
        KeyCode::BackTab => Some(Action::CyclePrevPane),
        KeyCode::Esc => Some(Action::Cancel),
        KeyCode::Enter => Some(Action::Activate),
        // Tab-jump accelerators in display order (see `TOP_LEVEL_PAGES`).
        KeyCode::Char('1') => Some(Action::NavigateTab(PageId::Status)),
        KeyCode::Char('2') => Some(Action::NavigateTab(PageId::Account)),
        KeyCode::Char('3') => Some(Action::NavigateTab(PageId::Settings)),
        KeyCode::Char('4') => Some(Action::NavigateTab(PageId::Logs)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{Action, map_key_event};
    use crate::app::PageId;

    #[test]
    fn maps_navigation_keys() {
        let two = KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(
            map_key_event(two),
            Some(Action::NavigateTab(PageId::Account))
        );

        let confirm = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(map_key_event(confirm), Some(Action::Activate));
    }

    #[test]
    fn maps_bulk_navigation_keys() {
        for (code, expected) in [
            (KeyCode::Home, Action::Home),
            (KeyCode::End, Action::End),
            (KeyCode::PageUp, Action::PageUp),
            (KeyCode::PageDown, Action::PageDown),
        ] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            assert_eq!(map_key_event(key), Some(expected), "{code:?}");
        }
    }

    #[test]
    fn maps_tab_and_back_tab_to_pane_cycling() {
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(map_key_event(tab), Some(Action::CycleNextPane));
        let back_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(map_key_event(back_tab), Some(Action::CyclePrevPane));
        // Shift+Tab on terminals that don't send BackTab.
        let shift_tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(map_key_event(shift_tab), Some(Action::CyclePrevPane));
    }
}
