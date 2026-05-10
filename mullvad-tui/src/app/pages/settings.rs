// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistent state for the Settings family pages.
//!
//! Today this is just the inline MTU edit field on the VPN settings
//! sub-page: a digits-only buffer that the user can type into, mirroring
//! the search-anchor pattern on `Status > Select location`. The buffer
//! is persistent so navigating away and back doesn't drop in-flight
//! edits, and so the renderer can paint the daemon's value without the
//! flash-of-empty-field a "rebuild on each daemon push" approach would
//! show.
//!
//! `RefCell` interior mutability matches the [`std::cell::Cell`]
//! `scroll_offset` pattern used elsewhere (e.g. select-location): the
//! renderer holds `&App` and still needs to push the daemon's MTU value
//! into the buffer when the field is not focused, without taking a
//! `&mut` borrow.

use std::cell::RefCell;

/// Hard cap on characters in the inline MTU buffer. The daemon-accepted
/// range is `1280..=1420` ([`crate::app::WIREGUARD_MTU_RANGE`]), whose
/// upper bound is 4 digits - typing past this can only produce an
/// out-of-range value, so silently dropping the keystroke is friendlier
/// than letting the user enter a value the validator will reject on
/// Enter.
pub const MTU_BUFFER_MAX_LEN: usize = 4;

/// Number of pages in the DAITA sub-page descriptive blurb. Two pages
/// of prose split the explanatory text so it fits above the toggle
/// rows on small terminals without forcing the user to scroll.
pub const DAITA_BLURB_PAGES: usize = 2;

#[derive(Debug, Default)]
pub struct PageState {
    /// What the inline MTU input pill displays. Synced from the daemon
    /// when the field is **not** focused (see
    /// [`Self::sync_mtu_buffer_from_daemon`]); when the user focuses the
    /// field and starts typing, their edits stay until they defocus and
    /// the next sync overwrites the draft.
    mtu_buffer: RefCell<String>,
    /// Currently visible DAITA blurb page (0-indexed, < [`DAITA_BLURB_PAGES`]).
    /// Persists across navigation so a user who flipped to page 2,
    /// drilled out, and came back lands on page 2 again.
    daita_blurb_page: usize,
}

impl PageState {
    pub fn mtu_buffer(&self) -> String {
        self.mtu_buffer.borrow().clone()
    }

    /// Append a digit typed into the focused MTU field. Reserved for
    /// `Char` keystrokes the run loop has already vetted (digit, no
    /// `Ctrl`/`Alt`). Silently drops the keystroke once the buffer
    /// reaches [`MTU_BUFFER_MAX_LEN`] - see that const for why.
    pub fn push_mtu_char(&self, c: char) {
        let mut buf = self.mtu_buffer.borrow_mut();
        if buf.len() >= MTU_BUFFER_MAX_LEN {
            return;
        }
        buf.push(c);
    }

    /// Pop the last char (one Backspace press). No-op on an empty buffer.
    pub fn pop_mtu_char(&self) {
        self.mtu_buffer.borrow_mut().pop();
    }

    pub fn daita_blurb_page(&self) -> usize {
        self.daita_blurb_page
    }

    /// Step the DAITA blurb page index by `delta` and clamp to the
    /// `[0, DAITA_BLURB_PAGES)` range. Used by the `[<]` / `[>]` pager
    /// buttons on the DAITA sub-page.
    pub fn step_daita_blurb_page(&mut self, delta: i32) {
        let next =
            (self.daita_blurb_page as i32 + delta).clamp(0, DAITA_BLURB_PAGES as i32 - 1) as usize;
        self.daita_blurb_page = next;
    }

    /// Replace the buffer with `value` rendered as decimal, or with the
    /// empty string when `value` is `None` ("Default" - daemon-managed).
    /// Called by the renderer's per-frame sync when the MTU field isn't
    /// focused, so the displayed value tracks daemon-side updates.
    pub fn sync_mtu_buffer_from_daemon(&self, value: Option<u16>) {
        let mut buf = self.mtu_buffer.borrow_mut();
        buf.clear();
        if let Some(v) = value {
            use std::fmt::Write;
            let _ = write!(&mut *buf, "{v}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_pop_round_trip() {
        let s = PageState::default();
        s.push_mtu_char('1');
        s.push_mtu_char('4');
        s.push_mtu_char('0');
        s.push_mtu_char('6');
        assert_eq!(s.mtu_buffer(), "1406");
        s.pop_mtu_char();
        assert_eq!(s.mtu_buffer(), "140");
    }

    #[test]
    fn pop_on_empty_is_noop() {
        let s = PageState::default();
        s.pop_mtu_char();
        assert_eq!(s.mtu_buffer(), "");
    }

    #[test]
    fn push_drops_keystrokes_past_the_max_len() {
        // The daemon's accepted MTU range tops out at 1420 / 4 digits.
        // A 5th digit can only ever produce an out-of-range value, so
        // the model drops it silently.
        let s = PageState::default();
        for c in ['1', '2', '3', '4', '5'] {
            s.push_mtu_char(c);
        }
        assert_eq!(s.mtu_buffer(), "1234");
        // Backspace then re-push should accept again, confirming the
        // gate is on length, not on a one-shot "saturated" flag.
        s.pop_mtu_char();
        s.push_mtu_char('9');
        assert_eq!(s.mtu_buffer(), "1239");
    }

    #[test]
    fn sync_from_daemon_replaces_buffer() {
        let s = PageState::default();
        s.push_mtu_char('9');
        s.sync_mtu_buffer_from_daemon(Some(1380));
        assert_eq!(s.mtu_buffer(), "1380");
        s.sync_mtu_buffer_from_daemon(None);
        assert_eq!(s.mtu_buffer(), "");
    }
}
