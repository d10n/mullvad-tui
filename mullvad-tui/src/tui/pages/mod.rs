// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-page modules. Each module owns one page's render code,
//! focus-id constants, page-local transient state (collapse flags,
//! etc.), and an Enter-dispatch handler. Pages are listed in tab-bar
//! order: Status, Account, Settings, Logs.

pub mod account;
pub mod logs;
pub mod select_location;
pub mod select_location_filter;
pub mod settings;
pub mod status;
