# SPDX-License-Identifier: GPL-3.0-or-later
.PHONY: build run test fmt fmt-check lint check license-check static clean

build:
	cargo build

run:
	cargo run -p mullvad-tui

test:
	cargo test --all-targets

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

lint:
	cargo clippy --all-targets --all-features -- -D warnings

# Local quality gate - matches CI (.github/workflows/ci.yml).
check: fmt-check lint test

license-check:
	@missing=`find . -path ./mullvadvpn-app -prune -o -path ./target -prune -o \
		-type f -name "*.rs" \
		-exec sh -c 'head -1 "$$1" | grep -q "^// SPDX-License[-]Identifier:" || echo "$$1"' _ {} \;`; \
	if [ -n "$$missing" ]; then \
		echo "Missing SPDX-License-Identifier header on line 1:"; \
		echo "$$missing"; \
		exit 1; \
	fi
	reuse lint

# The musl std component is provisioned on demand - `rustup target add` is
# idempotent, so re-runs are a no-op once the target is installed.
static:
	@rustup target add x86_64-unknown-linux-musl >/dev/null
	cargo build --release -p mullvad-tui --target x86_64-unknown-linux-musl

clean:
	cargo clean
