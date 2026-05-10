// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-page transient UI state owned by [`crate::app::App`].
//!
//! Each module declares the state struct for one page (collapse flags,
//! input draft buffers, scroll cursors, etc.). The state survives across
//! frames and across cross-page navigation - switching tabs and coming
//! back should restore what the user was looking at. Render code in
//! `crate::tui::pages::*` reads (and via App-mutators, writes) these
//! structs; it does not own them.

pub mod account;
pub mod logs;
pub mod select_location;
pub mod select_location_filter;
pub mod settings;
pub mod status;

/// All per-page transient UI states, unified into one composition object.
/// `App` holds this as a single field, and `App::new()` initializes it via
/// `Default`.
#[derive(Default)]
pub struct PageStates {
    pub status: status::PageState,
    pub account: account::PageState,
    pub select_location: select_location::PageState,
    pub select_location_filter: select_location_filter::PageState,
    pub logs: logs::PageState,
    pub settings: settings::PageState,
}
