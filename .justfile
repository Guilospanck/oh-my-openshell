# Installs OpenShell
install:
	#!/usr/bin/env bash
	set -euo pipefail

	if ! command -v brew >/dev/null 2>&1; then
	    echo "Homebrew is required. Install it from https://brew.sh" >&2
	    exit 1
	fi

	brew install e2fsprogs
	curl -LsSf https://raw.githubusercontent.com/NVIDIA/OpenShell/main/install.sh | sh

	config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/openshell"
	config_source="{{justfile_directory()}}/gateway.toml"
	mkdir -p "$config_dir"
	# Copy, don't symlink: the gateway refuses a symlinked config.
	rm -f "$config_dir/gateway.toml"
	install -m 644 "$config_source" "$config_dir/gateway.toml"

	brew services restart openshell
	openshell status

# Needs the Homebrew service to have run once (shared TLS). The vm gateway
# ignores per-sandbox --cpu/--memory; this one honours them.
# Installs a launchd agent and registers a Docker-driver gateway, openshell-docker.
gateway-docker:
	#!/usr/bin/env bash
	set -euo pipefail

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
      npm|pypi) ;;
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
