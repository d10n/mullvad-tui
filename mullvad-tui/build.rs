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
//!
//! Besides proto fields, one Rust-source skew is sniffed the same way: the
//! access-method `Id` type is `Clone`-not-`Copy` on stable but `Copy` on
//! tip-of-`main`. The emitted `access_method_id_is_copy` cfg lets call sites
//! scope a `#[expect(clippy::clone_on_copy)]` to only the pin where that
//! lint actually fires.

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

    // Match only against the code portion of each line (everything before
    // the first `//`, see `code_of`). A commented-out field declaration -
    // e.g. `// AccessMethodSetting domain_fronting = 5;` on a pin where the
    // feature is staged in the proto but not yet landed - is byte-identical
    // to the real declaration apart from the leading `//`, so a raw
    // whole-file substring search would false-positive on it and gate in
    // references to a proto field prost never generated.
    for (flag, marker) in features {
        if proto.lines().any(|line| code_of(line).contains(marker)) {
            println!("cargo:rustc-cfg={flag}");
        }
    }

    // The access-method `Id` type is a Rust-source (not proto) skew:
    // `Clone`-not-`Copy` on stable `mullvadvpn-app`, but restructured into
    // `mullvad-types/src/access_method/id.rs` deriving `Copy` on
    // tip-of-`main`. Call sites that must keep an `Id` binding alive across
    // a by-value use `.clone()` it - load-bearing on stable, but
    // `clippy::clone_on_copy` on `main`. Emit a cfg so those sites can scope
    // `#[cfg_attr(access_method_id_is_copy, expect(clippy::clone_on_copy))]`
    // to only the pin where the lint fires; a bare `#[expect]` would itself
    // warn (`unfulfilled_lint_expectations`) on stable, where it stays silent.
    println!("cargo:rustc-check-cfg=cfg(access_method_id_is_copy)");
    let mullvad_types_src = manifest_dir
        .join("..")
        .join("mullvadvpn-app")
        .join("mullvad-types")
        .join("src");
    // `main` split the type into `access_method/id.rs`; stable keeps it
    // inline in `access_method.rs`. Check both so the flag tracks the `Id`
    // derive wherever the type currently lives.
    let id_files = [
        mullvad_types_src.join("access_method").join("id.rs"),
        mullvad_types_src.join("access_method.rs"),
    ];
    let mut id_is_copy = false;
    for path in &id_files {
        println!("cargo:rerun-if-changed={}", path.display());
        if std::fs::read_to_string(path).is_ok_and(|src| struct_id_derives_copy(&src)) {
            id_is_copy = true;
        }
    }
    if id_is_copy {
        println!("cargo:rustc-cfg=access_method_id_is_copy");
    }
}

/// The code portion of a source line: everything before the first `//`
/// line comment. Both the proto marker search and the `Id`-derive sniff
/// match against this so a commented-out declaration never counts.
fn code_of(line: &str) -> &str {
    line.split_once("//").map_or(line, |(before, _)| before)
}

/// True if `src` declares a tuple struct `Id` whose immediately-preceding
/// `#[derive(...)]` includes `Copy`. Rust requires the derive to sit
/// directly above the item (attributes and comments may interleave, but
/// any other code line ends the run), so the derive still attached when
/// `struct Id(` is reached is the one that applies to it.
fn struct_id_derives_copy(src: &str) -> bool {
    let mut derive_has_copy = false;
    for line in src.lines() {
        let code = code_of(line);
        let trimmed = code.trim_start();
        if trimmed.is_empty() {
            // Blank or comment-only line: keep any pending derive attached.
            continue;
        }
        if trimmed.starts_with("pub struct Id(") || trimmed.starts_with("struct Id(") {
            return derive_has_copy;
        }
        if trimmed.starts_with("#[derive(") {
            derive_has_copy = code.contains("Copy");
        } else if !trimmed.starts_with("#[") {
            // A non-attribute code line (e.g. another struct) ends the run.
            derive_has_copy = false;
        }
    }
    false
}
