// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistent state for the `Status > Select location > Filter`
//! sub-page: an Ownership radio group (Any / Mullvad-owned / Rented)
//! and a per-provider checkbox group with an "All providers" master.
//!
//! The state is kept on `App` and persists across navigation. The
//! filter is captured but not yet wired into
//! `integration::project_relay_list`.

use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Ownership {
    #[default]
    Any,
    MullvadOwned,
    Rented,
}

#[derive(Debug, Default)]
pub struct PageState {
    ownership: Ownership,
    /// Providers explicitly excluded from the filter. Empty set =
    /// "all selected" (no exclusion). Inverted because the default
    /// is "all checked", so an empty exclusion list is the natural
    /// default.
    excluded_providers: BTreeSet<String>,
}

impl PageState {
    pub fn ownership(&self) -> Ownership {
        self.ownership
    }

    pub fn set_ownership(&mut self, ownership: Ownership) {
        self.ownership = ownership;
    }

    /// True when `provider` is currently included in the filter (i.e.
    /// its row's checkbox is `[x]`).
    pub fn is_provider_selected(&self, provider: &str) -> bool {
        !self.excluded_providers.contains(provider)
    }

    /// Toggle the exclusion of `provider`. Selected -> unselected and
    /// vice-versa; the "All providers" master is derived from
    /// [`Self::all_providers_selected`].
    pub fn toggle_provider(&mut self, provider: &str) {
        if !self.excluded_providers.remove(provider) {
            self.excluded_providers.insert(provider.to_string());
        }
    }

    /// True when the exclusion list is empty - every known provider
    /// is included in the filter, which renders the "All providers"
    /// master row as `[x]`.
    pub fn all_providers_selected(&self) -> bool {
        self.excluded_providers.is_empty()
    }

    /// Master toggle: clicking the `All providers` row flips the
    /// state between "all selected" (exclusion list empty) and
    /// "all excluded" (every known provider in the exclusion list).
    pub fn toggle_all_providers(&mut self, known_providers: &[&str]) {
        if self.all_providers_selected() {
            // Currently all selected -> exclude every known provider.
            self.excluded_providers = known_providers.iter().map(|s| s.to_string()).collect();
        } else {
            self.excluded_providers.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_defaults_to_any() {
        let s = PageState::default();
        assert_eq!(s.ownership(), Ownership::Any);
    }

    #[test]
    fn provider_toggles_are_inversion_of_exclusion() {
        let mut s = PageState::default();
        // Default: every provider is "selected" (empty exclusion list).
        assert!(s.is_provider_selected("100TB"));
        assert!(s.all_providers_selected());

        s.toggle_provider("100TB");
        assert!(!s.is_provider_selected("100TB"));
        assert!(!s.all_providers_selected());

        s.toggle_provider("100TB");
        assert!(s.is_provider_selected("100TB"));
        assert!(s.all_providers_selected());
    }

    #[test]
    fn toggle_all_providers_flips_between_full_and_empty() {
        let mut s = PageState::default();
        let known = ["100TB", "31173", "Blix"];
        // Start: all selected.
        assert!(s.all_providers_selected());
        // Master toggle -> all excluded.
        s.toggle_all_providers(&known);
        for p in known {
            assert!(!s.is_provider_selected(p), "{p} should be excluded");
        }
        // Toggle back.
        s.toggle_all_providers(&known);
        assert!(s.all_providers_selected());
    }
}
