#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Verify that mullvad-tui builds + tests cleanly against both the
# tip-of-main `mullvadvpn-app` submodule pin (default development
# target) and the latest stable release tag (what the Mullvad stable
# daemon ships today). Useful before cutting a release or merging an
# upstream bump.
#
# Usage:
#   scripts/check-both-pins.sh           # auto-detect latest stable tag
#   scripts/check-both-pins.sh 2026.3    # pin to a specific tag/branch/sha
#
# The stable tag defaults to the highest tag matching `YYYY.N` (no
# `-betaN` suffix) reachable from `origin`. Pass an explicit ref if you
# need a beta, a custom branch, or an older release.
#
# The script flips the submodule between the two pins, runs the cargo
# triad (check / test / clippy), and restores the original pin on exit -
# even on failure.

set -euo pipefail

cd "$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)"

# Refuse to run on a dirty submodule - checkout would lose those edits.
if ! git -C mullvadvpn-app diff --quiet HEAD 2>/dev/null \
    || ! git -C mullvadvpn-app diff --cached --quiet HEAD 2>/dev/null; then
    echo >&2 "ERROR: mullvadvpn-app submodule has uncommitted changes."
    echo >&2 "Stash or commit them inside the submodule first."
    exit 1
fi

original_pin="$(git -C mullvadvpn-app rev-parse HEAD)"
restore_pin() {
    git -C mullvadvpn-app checkout -q "$original_pin"
    echo "Submodule restored to $original_pin"
}
trap restore_pin EXIT

echo "Fetching origin/main and tags..."
git -C mullvadvpn-app fetch -q --tags origin main

STABLE_REF="${1:-}"
if [[ -z "$STABLE_REF" ]]; then
    # Auto-detect: highest tag matching `YYYY.N` (skip beta suffixes).
    # `sort -V` sorts version-style so `2026.10` > `2026.9`.
    STABLE_REF="$(
        git -C mullvadvpn-app tag -l '20[0-9][0-9].[0-9]*' \
            | grep -E '^[0-9]{4}\.[0-9]+$' \
            | sort -V \
            | tail -n1
    )"
    if [[ -z "$STABLE_REF" ]]; then
        echo >&2 "ERROR: could not auto-detect latest stable tag."
        echo >&2 "Pass one explicitly: ${0##*/} <tag-or-ref>"
        exit 1
    fi
    echo "Auto-detected latest stable tag: $STABLE_REF"
fi

run_against() {
    local pin="$1"
    local label="$2"
    echo
    echo "========================================"
    echo "  $label  ($pin)"
    echo "========================================"
    git -C mullvadvpn-app checkout -q "$pin"
    cargo check -p mullvad-tui --all-targets
    cargo test -p mullvad-tui --all-targets
    cargo clippy -p mullvad-tui --all-targets --all-features -- -D warnings
}

run_against "origin/main" "Forward-compat: tip of upstream main"
run_against "$STABLE_REF" "Backward-compat: stable ref $STABLE_REF"

echo
echo "Both pins built, tested, and clippy-clean."
