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

# Runs the Linux test suite in Docker on Ubuntu, Fedora and Arch (--distro NAME for one); real build, checks and package installs, stubbed OpenShell and systemd
test-linux *args:
	@"{{justfile_directory()}}/tests/linux/run.sh" {{args}}

# Developer wrappers: the logic lives in the osh binary (`osh setup`, `osh upgrade`, ...). They run the
# checkout's launcher, which rebuilds osh when its sources changed.

# Real, unstubbed Linux test: a systemd container, real packages, OpenShell and a real gateway (podman|docker|vm; vm needs /dev/kvm on the Docker host, else it is skipped with exit 77)
test-linux-real runtime="podman":
	@"{{justfile_directory()}}/tests/linux/real.sh" {{runtime}}

# macOS, real KVM: test-linux-real vm on a separate, disposable Colima instance with nested virtualization (M3+); --keep keeps it
test-linux-kvm *args:
	@"{{justfile_directory()}}/tests/linux/kvm.sh" {{args}}

# macOS only, changes nothing on the Mac: the macOS build against recording brew/launchctl/colima/podman stand-ins in a sandboxed throwaway HOME, plutil -lint on the agents
test-macos:
	@"{{justfile_directory()}}/tests/macos/run.sh"

# macOS: osh's real Colima and Podman machine steps in an isolated throwaway HOME (the user's Colima is checked untouched); --keep keeps the instances
test-macos-runtimes *args:
	@"{{justfile_directory()}}/tests/macos/runtimes.sh" {{args}}

# macOS: real Homebrew, brew services and launchd in a disposable Tart macOS VM clone (needs tart; pulls a 26 GB image once); --keep keeps the clone
test-macos-vm *args:
	@"{{justfile_directory()}}/tests/macos/vm.sh" {{args}}

# Runs the release workflow's Linux build and publish jobs for real with act in Docker (gh is a recording stand-in; macOS entries need GitHub)
test-release:
	@"{{justfile_directory()}}/tests/release/run.sh"

# Release, all from this machine (what .github/workflows/release.yml does in CI). Nothing here installs or runs osh.

# One-time and read-only: checks gh login, push access to the Homebrew tap, Docker for the Linux archives, brew, Zig
release-setup:
	#!/usr/bin/env bash
	set -u
	bad=0
	ok() { echo "ok       $1"; }
	no() { echo "MISSING  $1  ($2)"; bad=1; }
	command -v gh >/dev/null 2>&1 && ok gh || no gh "brew install gh"
	if gh auth status >/dev/null 2>&1; then
	    ok "gh logged in"
	    perm="$(gh repo view Guilospanck/homebrew-tap --json viewerPermission --jq .viewerPermission 2>/dev/null)"
	    [[ "$perm" =~ ^(ADMIN|MAINTAIN|WRITE)$ ]] && ok "push access to Guilospanck/homebrew-tap ($perm)" || no "push access to Guilospanck/homebrew-tap (got '${perm:-none}')" "gh auth refresh -s repo; or log in as the owner"
	    gh repo view Guilospanck/oh-my-openshell --json viewerPermission --jq .viewerPermission 2>/dev/null | grep -qE '^(ADMIN|MAINTAIN|WRITE)$' && ok "push access to Guilospanck/oh-my-openshell" || no "push access to Guilospanck/oh-my-openshell" "gh auth refresh -s repo"
	else
	    no "gh login" "run: gh auth login (HTTPS or SSH, scope repo)"
	fi
	git ls-remote git@github.com:Guilospanck/homebrew-tap.git HEAD >/dev/null 2>&1 && ok "git over SSH to the tap (the script clones and pushes it)" || no "git over SSH to github.com" "add an SSH key to GitHub: ssh -T git@github.com"
	command -v brew >/dev/null 2>&1 && ok brew || no brew "https://brew.sh (formula lint)"
	command -v just >/dev/null 2>&1 && ok just || no just "brew install just"
	source "{{justfile_directory()}}/dev-env"
	osh_use_zig && ok "zig 0.15" || no "zig 0.15" "brew install zig@0.15"
	if docker info >/dev/null 2>&1 || podman info >/dev/null 2>&1; then ok "container runtime running (Linux archives)"; else no "running Docker or Podman" "colima start; or release with --no-linux"; fi
	exit $bad

# Only for later CI use, safe to rerun: new deploy key on the tap replaces the earlier one; private half stored as the HOMEBREW_TAP_DEPLOY_KEY secret of this repo (the local release does not need it)
release-setup-ci-key:
	#!/usr/bin/env bash
	set -euo pipefail
	command -v gh >/dev/null || { echo "install gh first: brew install gh" >&2; exit 1; }
	title="oh-my-openshell release CI"
	tap=Guilospanck/homebrew-tap
	# Rerunnable: the new key replaces any earlier one with this title, and the old one goes only after the new one works.
	old_ids="$(gh repo deploy-key list -R "$tap" --json id,title --jq ".[] | select(.title == \"$title\") | .id")"
	umask 077
	tmp="$(mktemp -d)"
	trap 'rm -rf -- "$tmp"' EXIT
	ssh-keygen -t ed25519 -N "" -C "$title" -f "$tmp/key" >/dev/null
	gh repo deploy-key add "$tmp/key.pub" -R "$tap" --title "$title" --allow-write
	public_key="$(awk '{print $1 " " $2}' "$tmp/key.pub")"
	if ! gh secret set HOMEBREW_TAP_DEPLOY_KEY -R Guilospanck/oh-my-openshell < "$tmp/key"; then
	    echo "failed to store the private key; removing the new deploy key (the old one stays)" >&2
	    key_id="$(gh repo deploy-key list -R "$tap" --json id,key --jq ".[] | select(.key == \"$public_key\") | .id" || true)"
	    [[ -n "$key_id" ]] && gh repo deploy-key delete "$key_id" -R "$tap" || echo "remove the new deploy key from homebrew-tap by hand" >&2
	    exit 1
	fi
	for id in $old_ids; do gh repo deploy-key delete "$id" -R "$tap" && echo "removed the earlier deploy key $id"; done
	echo "deploy key added to homebrew-tap (write) and stored as HOMEBREW_TAP_DEPLOY_KEY"

# Builds the archives into dist/ (this Mac plus Linux amd64 and arm64 in containers); uploads nothing. Usage: just release-build v0.2.0
release-build version:
	@"{{justfile_directory()}}/scripts/release" {{version}} --linux amd64 arm64

# Prints the Homebrew formula for the archives already in dist/. Usage: just release-formula v0.2.0
release-formula version:
	@"{{justfile_directory()}}/scripts/render-formula" {{version}} "{{justfile_directory()}}/dist"

# Everything except pushing: check, version bump, archives, formula lint; then reverts the bump. Needs a clean main level with origin
release-dry-run version *args:
	@"{{justfile_directory()}}/scripts/publish" {{version}} --dry-run {{args}}

# Full release: check, bump Cargo version (commit), archives, formula lint, tag, push, GitHub release, push formula to Guilospanck/homebrew-tap. Usage: just release v0.2.0 ["notes"]
release version $notes="":
	@"{{justfile_directory()}}/scripts/publish" {{version}} --notes "$notes"

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
