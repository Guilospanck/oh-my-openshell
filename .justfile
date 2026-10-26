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
	want openshell "just install; only needed to run osh, not to build or test it"
	want docker "only needed for image builds and the docker gateway"
	if [[ "$(uname)" == Darwin ]]; then
	    want pngpaste "just install installs it; host-image paste (Ctrl-V)"
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

# Installs or upgrades OpenShell (OPENSHELL_VERSION=v0.1.5 pins a release) and links osh into PATH
install: link
	#!/usr/bin/env bash
	set -euo pipefail

	if ! command -v brew >/dev/null 2>&1; then
	    echo "Homebrew is required. Install it from https://brew.sh" >&2
	    exit 1
	fi

	brew install e2fsprogs
	# zig@0.15 builds osh's embedded terminal; pngpaste reads host clipboard images for Ctrl-V in a shell.
	# On Linux install wl-clipboard (Wayland) or xclip (X11) with your package manager for that.
	brew install zig@0.15
	[[ "$(uname)" != Darwin ]] || brew install pngpaste
	curl -LsSf https://raw.githubusercontent.com/NVIDIA/OpenShell/main/install.sh | sh

	config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/openshell"
	config_source="{{justfile_directory()}}/gateway.toml"
	mkdir -p "$config_dir"
	# Copy, don't symlink: the gateway refuses a symlinked config.
	rm -f "$config_dir/gateway.toml"
	install -m 644 "$config_source" "$config_dir/gateway.toml"

	brew services restart openshell
	openshell --version
	# The service needs a moment to listen again after the restart.
	for _ in $(seq 20); do openshell status >/dev/null 2>&1 && break; sleep 1; done
	openshell status

# Upgrades OpenShell to the latest release, then restarts the Docker gateway if it is set up
upgrade: install
	#!/usr/bin/env bash
	set -euo pipefail

	label="dev.guilospanck.openshell-docker"
	if launchctl print "gui/$(id -u)/$label" >/dev/null 2>&1; then
	    launchctl kickstart -k "gui/$(id -u)/$label"
	fi

# Needs the Homebrew service to have run once (shared TLS). The vm gateway
# ignores per-sandbox --cpu/--memory; this one honours them.
# Installs a launchd agent and registers a Docker-driver gateway, openshell-docker.
gateway-docker:
	#!/usr/bin/env bash
	set -euo pipefail
	[[ "$(uname)" == Darwin ]] || { echo "gateway-docker is macOS only (launchd agent, Colima)" >&2; exit 1; }

	service="$(readlink -f "{{justfile_directory()}}/gateway-docker-service")"
	label="dev.guilospanck.openshell-docker"
	plist="$HOME/Library/LaunchAgents/$label.plist"
	logs="$HOME/.local/state/openshell/gateway-docker"
	config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/openshell"
	mkdir -p "$logs" "$(dirname "$plist")"

	cat > "$plist" <<EOF
	<?xml version="1.0" encoding="UTF-8"?>
	<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
	<plist version="1.0"><dict>
	  <key>Label</key><string>$label</string>
	  <key>ProgramArguments</key><array><string>$service</string></array>
	  <key>RunAtLoad</key><true/>
	  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
	  <key>StandardOutPath</key><string>$logs/gateway.out.log</string>
	  <key>StandardErrorPath</key><string>$logs/gateway.err.log</string>
	</dict></plist>
	EOF

	launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
	launchctl bootstrap "gui/$(id -u)" "$plist"

	# Same client certificates as the Homebrew gateway: the server cert is shared.
	mkdir -p "$config_dir/gateways/openshell-docker"
	cp -R "$config_dir/gateways/openshell/mtls" "$config_dir/gateways/openshell-docker/"
	previous="$(cat "$config_dir/active_gateway" 2>/dev/null || echo openshell)"
	if ! openshell gateway list -o json | grep -q '"openshell-docker"'; then
	    openshell gateway add https://localhost:17671 --local --name openshell-docker
	    openshell gateway select "$previous"
	fi
	sleep 3
	openshell -g openshell-docker status

# Create-only: lint/import the profile, then create its provider instance.
# Existing profiles/providers are not overwritten. Export/update them separately.
# Credentials must be exported in the calling shell; values never appear in argv.

# Requires CLAUDE_CODE_OAUTH_TOKEN from `claude setup-token`.
[group('providers')]
provider-claude-code: (_create-provider "claude-code")

# Requires GITHUB_TOKEN or GH_TOKEN.
[group('providers')]
provider-github: (_create-provider "github")

# Requires FORGEJO_HOST and FORGEJO_TOKEN; renders host before importing.
[group('providers')]
provider-forgejo: (_create-provider "forgejo")

# Anonymous download providers: no credential variables required.
[group('providers')]
provider-npm: (_create-provider "npm")

[group('providers')]
provider-pypi: (_create-provider "pypi")

# Read-only github.com for `claude plugin install`; no credential variables required.
[group('providers')]
provider-claude-marketplace: (_create-provider "claude-marketplace")

[private]
_create-provider provider:
    #!/usr/bin/env bash
    set -euo pipefail
    provider='{{provider}}'
    required=()
    optional=()
    alternatives=()
    case "$provider" in
      claude-code) required=(CLAUDE_CODE_OAUTH_TOKEN) ;;
      github) alternatives=(GITHUB_TOKEN GH_TOKEN) ;;
      forgejo) required=(FORGEJO_HOST FORGEJO_TOKEN) ;;
      npm|pypi|claude-marketplace) ;;
      *) echo "Unknown provider: $provider" >&2; exit 1 ;;
    esac

    credentials=()
    missing=0
    for key in "${required[@]}"; do
      if [[ -z "${!key:-}" ]]; then
        echo "Required environment variable is unset or empty: $key" >&2
        missing=1
      elif [[ "$key" != FORGEJO_HOST ]]; then
        credentials+=(--credential "$key")
      fi
    done
    if [[ ${#alternatives[@]} -gt 0 ]]; then
      found=0
      for key in "${alternatives[@]}"; do
        if [[ -n "${!key:-}" ]]; then
          credentials+=(--credential "$key")
          found=1
          break
        fi
      done
      if [[ "$found" -eq 0 ]]; then
        echo "Export at least one nonempty variable: ${alternatives[*]}" >&2
        missing=1
      fi
    fi
    [[ "$missing" -eq 0 ]] || exit 1
    for key in "${optional[@]}"; do
      [[ -z "${!key:-}" ]] || credentials+=(--credential "$key")
    done
    command -v openshell >/dev/null || { echo 'openshell is required' >&2; exit 1; }

    profile="{{justfile_directory()}}/providers/$provider.yaml"
    if [[ "$provider" == forgejo ]]; then
      command -v python3 >/dev/null || { echo 'python3 is required to render FORGEJO_HOST' >&2; exit 1; }
      render_dir=$(mktemp -d)
      rendered="$render_dir/forgejo.yaml"
      trap 'rm -f "$rendered"; rmdir "$render_dir"' EXIT
      python3 - "$profile" "$rendered" <<'PY'
    import os, re, sys
    from pathlib import Path
    host = os.environ.get("FORGEJO_HOST", "")
    label = r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?"
    if len(host) > 253 or not re.fullmatch(rf"{label}(?:\.{label})*", host):
        raise SystemExit("FORGEJO_HOST must be a hostname, without scheme/port/path/wildcards")
    Path(sys.argv[2]).write_text(Path(sys.argv[1]).read_text().replace("${FORGEJO_HOST}", host))
    PY
      profile="$rendered"
    fi
    openshell provider profile lint -f "$profile"
    openshell provider profile import -f "$profile"
    openshell provider create --name "$provider" --type "$provider" "${credentials[@]}"
    case "$provider" in
      claude-code) echo 'Setup token has no automatic refresh; rotate before expiry. See README.md.' ;;
    esac

# Interactive menu: install, sandboxes, providers, images.
tui:
	@{{justfile_directory()}}/osh

# enable the tracked pre-commit secret/PII hook (run once per clone)
install-hooks:
	sh "{{justfile_directory()}}/.githooks/install.sh"

# Symlink osh into ~/.local/bin so `osh` runs from anywhere
link:
	#!/usr/bin/env bash
	set -euo pipefail
	bin="${OSH_BIN_DIR:-$HOME/.local/bin}"
	mkdir -p "$bin"
	ln -sfn "{{justfile_directory()}}/osh" "$bin/osh"
	echo "linked $bin/osh"
	case ":$PATH:" in *":$bin:"*) ;; *) echo "warning: $bin is not on PATH" >&2 ;; esac

# Remove the osh symlink (only if it points at this checkout)
unlink:
	#!/usr/bin/env bash
	set -euo pipefail
	target="${OSH_BIN_DIR:-$HOME/.local/bin}/osh"
	if [[ -L "$target" && "$(readlink "$target")" == "{{justfile_directory()}}/osh" ]]; then
	    rm "$target"
	    echo "removed $target"
	fi

# Remove all OpenShell files, services and the osh link (asks first; sandboxes are lost)
uninstall:
	#!/usr/bin/env bash
	set -euo pipefail
	config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/openshell"
	state_dir="$HOME/.local/state/openshell"
	share_dir="$HOME/.local/share/openshell"
	label="dev.guilospanck.openshell-docker"
	plist="$HOME/Library/LaunchAgents/$label.plist"

	echo "This will remove:"
	echo "  launchd agent  $label"
	echo "  brew service + formula  openshell"
	echo "  $config_dir"
	echo "  $state_dir"
	echo "  $share_dir"
	echo "  osh link in ~/.local/bin"
	read -r -p "Type 'uninstall' to continue: " answer
	[[ "$answer" == uninstall ]] || { echo "aborted"; exit 1; }

	launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
	rm -f "$plist"
	if command -v brew >/dev/null 2>&1; then
	    brew services stop openshell 2>/dev/null || true
	    brew uninstall openshell 2>/dev/null || true
	fi
	# rm -rf does not follow the symlinks in the config dir (images, providers).
	rm -rf "$config_dir" "$state_dir" "$share_dir"
	just --justfile "{{justfile_directory()}}/.justfile" unlink
	echo "OpenShell removed. Repo checkout left in place."

# Show how the host's agent setup (Claude Code and pi) would be reproduced in a sandbox:
# install commands where an installer exists, uploads otherwise. Only --dry-run and
# --json and --preflight SANDBOX [--gateway NAME] are implemented; none installs, copies or
# uploads anything. --preflight checks the install steps' network needs against the sandbox policy.
# Example: just sync-agents --dry-run --agent pi
sync-agents *args:
	@"{{justfile_directory()}}/sync-agents" {{args}}
