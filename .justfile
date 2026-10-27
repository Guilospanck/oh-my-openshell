# Lists the recipes. First on purpose: a bare `just` must not run `install`.
default:
	@just --justfile "{{justfile_directory()}}/.justfile" --list

# Checks the build prerequisites (cargo, Zig 0.15, just, gitleaks) and optional tools; read-only
doctor:
	#!/usr/bin/env bash
	set -u
	bad=0
	need() { if command -v "$1" >/dev/null 2>&1; then echo "ok       $1"; else echo "MISSING  $1  ($2)"; bad=1; fi; }
	want() { if command -v "$1" >/dev/null 2>&1; then echo "ok       $1"; else echo "optional $1  ($2)"; fi; }
	need cargo "https://rustup.rs"
	need just "brew install just"
	need gitleaks "brew install gitleaks or a release binary; the pre-commit hook needs it"
	source "{{justfile_directory()}}/dev-env"
	if osh_use_zig; then echo "ok       zig 0.15"; else echo "MISSING  zig 0.15  (macOS: brew install zig@0.15; Linux: Zig 0.15 on PATH or Linuxbrew zig@0.15)"; bad=1; fi
	want openshell "osh setup; only needed to run osh, not to build or test it"
	want docker "image builds and the docker gateway; the Linux gateway needs it too"
	if [[ "$(uname)" == Darwin ]]; then
	    want pngpaste "osh setup installs it; host-image paste (Ctrl-V)"
	else
	    command -v wl-paste >/dev/null 2>&1 || want xclip "host-image paste (Ctrl-V) needs wl-paste (Wayland) or xclip (X11)"
	fi
	[[ "$(git -C "{{justfile_directory()}}" config core.hooksPath)" == .githooks ]] && echo "ok       pre-commit hook" || echo "optional pre-commit hook  (just install-hooks)"
	exit $bad

# Builds the TUI in release mode, finding Zig 0.15 itself (osh does this on its own)
build:
	#!/usr/bin/env bash
	set -euo pipefail
	source "{{justfile_directory()}}/dev-env"
	osh_use_zig || { echo "Zig 0.15 is required; run: just doctor" >&2; exit 1; }
	cargo build --release --manifest-path "{{justfile_directory()}}/tui/Cargo.toml"

# Runs the TUI unit tests; they need no OpenShell, gateway, or Docker
test:
	#!/usr/bin/env bash
	set -euo pipefail
	source "{{justfile_directory()}}/dev-env"
	osh_use_zig || { echo "Zig 0.15 is required; run: just doctor" >&2; exit 1; }
	cargo test --manifest-path "{{justfile_directory()}}/tui/Cargo.toml"

# Format check, clippy, and tests: run before every commit
check:
	#!/usr/bin/env bash
	set -euo pipefail
	source "{{justfile_directory()}}/dev-env"
	osh_use_zig || { echo "Zig 0.15 is required; run: just doctor" >&2; exit 1; }
	manifest="{{justfile_directory()}}/tui/Cargo.toml"
	cargo fmt --check --manifest-path "$manifest"
	cargo clippy --all-targets --manifest-path "$manifest" -- -D warnings
	cargo test --manifest-path "$manifest"

# Runs the Linux test suite in Docker (real build and checks, stubbed install/uninstall/gateway-reset)
test-linux *args:
	@"{{justfile_directory()}}/tests/linux/run.sh" {{args}}

# Developer wrappers: the logic lives in the osh binary (`osh setup`, `osh upgrade`, ...). They run the
# checkout's launcher, which rebuilds osh when its sources changed.

# Installs OpenShell, a container runtime and the gateway for this OS (asks before each step)
install *args:
	@"{{justfile_directory()}}/osh" setup {{args}}

# Upgrades OpenShell to the latest release, then restarts the Docker gateway if it is set up
upgrade:
	@"{{justfile_directory()}}/osh" upgrade

# macOS: a second gateway on the Docker driver (honours per-sandbox --cpu/--memory)
gateway-docker:
	@"{{justfile_directory()}}/osh" gateway-docker

# Remove all OpenShell files and services (asks first; sandboxes are lost)
uninstall:
	@"{{justfile_directory()}}/osh" uninstall

# Interactive dashboard
tui:
	@"{{justfile_directory()}}/osh"

# enable the tracked pre-commit secret/PII hook (run once per clone)
install-hooks:
	sh "{{justfile_directory()}}/.githooks/install.sh"
