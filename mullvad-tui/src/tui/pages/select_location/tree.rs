// SPDX-License-Identifier: GPL-3.0-or-later

//! Daemon relay-list -> page-tree projection and filter logic.
//!
//! The daemon's `RelayList` is a flat list of relay locations carrying
//! country/city codes alongside the relay hostname. The renderer wants
//! it grouped: country -> cities -> relays, ordered alphabetically by
//! country code, then city code. [`project_tree`] does that grouping
//! against `app.relay_locations()`; [`filter_tree`] then prunes the
//! grouped view to whatever the user typed in the search anchor.
//!
//! All borrowed: the `CountryNode<'_>` / `CityNode<'_>` lifetime is the
//! `App` borrow, so a tree projection is cheap to rebuild every frame
//! and at dispatch time (so an `index` passed to a dispatch handler
//! maps to the same row in both walks).

use std::collections::BTreeMap;

use crate::{app::App, integration::RelayLocation};

/// One country-with-cities-with-relays slice of the daemon's relay
/// list, ordered alphabetically by country name then city name. Built
/// fresh every render and used at dispatch time too (so an `index`
/// passed to a dispatch handler maps to the same row both times).
///
/// The lifetime borrows from `app.relay_locations()`, so callers don't
/// pay the cost of cloning country/city strings just to read them.
pub fn project_tree(app: &App) -> Vec<CountryNode<'_>> {
    // BTreeMap gives us ordered iteration on country code (which
    // matches the desktop app's behavior). Within a country, we
    // group by city then collect relays.
    let mut by_country: BTreeMap<&str, CountryAccum<'_>> = BTreeMap::new();
    for relay in app.relay_locations() {
        let entry = by_country
            .entry(relay.country_code.as_str())
            .or_insert_with(|| CountryAccum {
                name: relay.country_name.as_str(),
                code: relay.country_code.as_str(),
                cities: BTreeMap::new(),
            });
        let city_entry = entry
            .cities
            .entry(relay.city_code.as_str())
            .or_insert_with(|| CityAccum {
                name: relay.city_name.as_str(),
                code: relay.city_code.as_str(),
                relays: Vec::new(),
            });
        city_entry.relays.push(relay);
    }
    by_country
        .into_values()
        .map(|c| CountryNode {
            name: c.name,
            code: c.code,
            cities: c
                .cities
                .into_values()
                .map(|city| CityNode {
                    name: city.name,
                    code: city.code,
                    relays: city.relays,
                    // No filter active in `project_tree` - there's
                    // nothing to force-expand against, so the flag is
                    // false. `filter_tree` recomputes it.
                    force_expand_under_filter: false,
                })
                .collect(),
        })
        .collect()
}

struct CountryAccum<'a> {
    name: &'a str,
    code: &'a str,
    cities: BTreeMap<&'a str, CityAccum<'a>>,
}
struct CityAccum<'a> {
    name: &'a str,
    code: &'a str,
    relays: Vec<&'a RelayLocation>,
}

pub struct CountryNode<'a> {
    pub name: &'a str,
    pub code: &'a str,
    pub cities: Vec<CityNode<'a>>,
}
pub struct CityNode<'a> {
    pub name: &'a str,
    pub code: &'a str,
    /// True when this city should be **force-expanded** by the
    /// renderer because the filter matched at least one of its relay
    /// hostnames *and* neither the country nor the city (name or
    /// code) matched. The reasoning: a country/city match means the
    /// user is searching for a place to browse, so auto-expanding
    /// the hostnames would clutter the view; a pure-hostname match
    /// means they're searching for specific relays, so showing the
    /// matches directly is what they want.
    ///
    /// The relay list itself is always preserved, so manual expansion
    /// works regardless. Always `false` when no filter is active
    /// (`project_tree`'s passthrough).
    pub force_expand_under_filter: bool,
    pub relays: Vec<&'a RelayLocation>,
}

/// True if (lower-cased) `needle` matches `hostname` (lower-cased)
/// with at least one occurrence whose match range extends beyond the
/// literal `-wg-` protocol segment that every WireGuard relay carries.
/// So the user must hit some character outside `-wg-` for the relay to
/// stay in the filtered tree:
///
/// - `"wg"` does **not** match `"se-got-wg-001"` (the only occurrence sits entirely inside `-wg-`).
/// - `"got-wg"` matches `"se-got-wg-001"` (occurrence covers `"got"` too - `-wg-` is allowed to be
///   part of the match because *another* part is also matching).
/// - `"wg-001"` matches `"se-got-wg-001"` (occurrence extends past `-wg-` into the relay number).
/// - `"001"` matches `"se-got-wg-001"` (occurrence is fully outside).
///
/// Hostnames without a `-wg-` segment fall through to a plain
/// substring match (no protocol segment to filter against).
pub(super) fn hostname_matches(hostname: &str, needle: &str) -> bool {
    // Mullvad relay hostnames are ASCII-lowercase by convention, so the
    // common path skips the allocation entirely. Fall back to an owned
    // lowered copy for any non-conforming entry.
    let lower_owned: String;
    let lower: &str = if hostname.bytes().any(|b| b.is_ascii_uppercase()) {
        lower_owned = hostname.to_ascii_lowercase();
        &lower_owned
    } else {
        hostname
    };
    if !lower.contains(needle) {
        return false;
    }
    let Some(wg_start) = lower.find("-wg-") else {
        return true;
    };
    let wg_end = wg_start + "-wg-".len();
    let needle_len = needle.len();
    let mut search_from = 0;
    while let Some(rel_pos) = lower[search_from..].find(needle) {
        let abs_pos = search_from + rel_pos;
        let abs_end = abs_pos + needle_len;
        // Reject only the occurrences that fit entirely inside
        // `-wg-` - those are the ones that would trivially match every
        // WireGuard relay in the daemon's list.
        if abs_pos < wg_start || abs_end > wg_end {
            return true;
        }
        search_from = abs_pos + 1;
    }
    false
}

/// Prune `tree` to nodes that match `query` (case-insensitive substring
/// over country / city names + codes and relay hostnames). When a
/// country or city itself matches, all of its descendants are kept so
/// the user can drill in. An empty `query` is a passthrough.
///
/// Hostname matching uses [`hostname_matches`] so the literal `-wg-`
/// protocol segment that every WireGuard relay carries doesn't
/// trivially match every relay in the list when the user types "wg" -
/// the match must extend beyond `-wg-`.
///
/// Each `CityNode.relays` carries the city's **full** relay list; the
/// `force_expand_under_filter` flag tracks whether the renderer
/// should expand the city under the active filter. That flag is
/// `true` only when the filter matched a relay hostname *and* neither
/// the country nor the city matched by name/code - a country/city
/// match implies "browse here", a pure-hostname match implies "find
/// specific relays".
pub(super) fn filter_tree<'a>(tree: Vec<CountryNode<'a>>, query: &str) -> Vec<CountryNode<'a>> {
    if query.is_empty() {
        return tree;
    }
    let needle = query.to_ascii_lowercase();
    let matches = |s: &str| {
        // Skip the allocation when `s` is already ASCII-lowercase, which is
        // the common case for relay-list country/city codes.
        if s.bytes().any(|b| b.is_ascii_uppercase()) {
            s.to_ascii_lowercase().contains(&needle)
        } else {
            s.contains(&needle)
        }
    };
    let matches_hostname = |s: &str| hostname_matches(s, &needle);
    tree.into_iter()
        .filter_map(|country| {
            let country_match = matches(country.name) || matches(country.code);
            let cities: Vec<CityNode<'_>> = country
                .cities
                .into_iter()
                .filter_map(|city| {
                    let city_match = matches(city.name) || matches(city.code);
                    let has_hostname_match = city
                        .relays
                        .iter()
                        .any(|r| matches_hostname(r.hostname.as_str()));
                    if !(country_match || city_match || has_hostname_match) {
                        return None;
                    }
                    // Force-expand only when this is a pure-hostname
                    // match - neither the country nor the city
                    // matched. Otherwise the user is searching for a
                    // place to browse and the relays stay collapsed
                    // (still manually expandable).
                    let force_expand_under_filter =
                        has_hostname_match && !country_match && !city_match;
                    Some(CityNode {
                        name: city.name,
                        code: city.code,
                        // Keep the full list - manual expansion can
                        // still browse it even when no relay matched.
                        relays: city.relays,
                        force_expand_under_filter,
                    })
                })
                .collect();
            if country_match || !cities.is_empty() {
                Some(CountryNode {
                    name: country.name,
                    code: country.code,
                    cities,
                })
            } else {
                None
            }
        })
        .collect()
}

/// Project the daemon's relay list into the page's tree, applying the
/// page-state filter query. Used by both the renderer and the
/// page-open focus path so flat indices match across the two walks.
/// Pub so the run loop's activation dispatch (`tui::mod`) maps
/// row-index -> relay against the same tree the renderer registered.
pub fn project_filtered_tree(app: &App) -> Vec<CountryNode<'_>> {
    let tree = project_tree(app);
    let query = app.select_location_page_state().query();
    filter_tree(tree, query)
}

/// True when a non-empty filter query is in effect - callers
/// override the user-driven country/city expansion sets in this case
/// so the matched subtree is visible without an extra click. Pub for
/// the same reason as [`project_filtered_tree`]: the activation
/// dispatch needs the same expansion semantics the renderer uses.
pub fn filter_active(app: &App) -> bool {
    !app.select_location_page_state().query().is_empty()
}
