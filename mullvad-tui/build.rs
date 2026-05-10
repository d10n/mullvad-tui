// SPDX-License-Identifier: GPL-3.0-or-later

//! Detect optional features in the submodule's `.proto` file and emit
//! `cargo:rustc-cfg` flags so the version-skew patcher in
//! `integration/tolerant.rs` can `#[cfg]`-gate references to types and
//! fields that only exist in newer submodule pins.
//!
//! Lets the same source tree compile against both the latest stable
//! `mullvadvpn-app` tag (e.g. `2026.2`) and a tip-of-`main` bump. The
//! `.proto` file is the source of truth: at build time we substring-sniff
//! it for the marker strings of each feature, and emit
//! `cargo:rustc-cfg=daemon_has_<feature>` when present.
//!
//! Adding a future field that needs the same treatment:
//!
//! 1. Add a `(flag, marker)` row to the `FEATURES` table below. The `marker` must be a substring
//!    unique to the feature's proto declaration (the message definition line is usually a safe
//!    pick).
//! 2. In `integration/tolerant.rs`, annotate the patcher entry and any struct-literal field
//!    references with `#[cfg(daemon_has_<flag>)]`.
//! 3. The byte-level test fixture (`settings_2026_2_shape`) should set the new field to `None`
//!    under the `cfg`, so it matches the pre-feature wire shape.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"),
    );
    let proto_path = manifest_dir
        .join("..")
        .join("mullvadvpn-app")
        .join("mullvad-management-interface")
        .join("proto")
        .join("management_interface.proto");
    println!("cargo:rerun-if-changed={}", proto_path.display());

    // Each entry: (cfg flag emitted when the marker is present, marker
    // substring that uniquely identifies the feature in the proto).
    // Keep the markers as the *field declaration line* (not just the
    // type name) so they don't false-positive on a comment that happens
    // to mention the type.
    let features: &[(&str, &str)] = &[
        // The `domain_fronting` built-in API access method (between
        // 2026.2 and `main`).
        (
            "daemon_has_domain_fronting",
            "AccessMethodSetting domain_fronting",
        ),
        // The `Lwo` obfuscation message (between 2026.2 and `main`).
        ("daemon_has_lwo", "message Lwo {"),
    ];

    // Always declare every flag so rustc's `unexpected_cfgs` lint passes
    // regardless of which pin is active.
    for (flag, _) in features {
        println!("cargo:rustc-check-cfg=cfg({flag})");
    }

    // Submodule not initialized / proto file missing: emit no feature
    // cfgs and let the rest of the build fail with its own (clearer)
    // error about the missing path dep.
    let Ok(proto) = std::fs::read_to_string(&proto_path) else {
        return;
    };

    for (flag, marker) in features {
        if proto.contains(marker) {
            println!("cargo:rustc-cfg={flag}");
        }
    }
}
