set -e
# Installs OpenShell on a Linux host that has neither dpkg nor rpm (Arch, Alpine, NixOS...), where
# OpenShell's own installer stops with "Linux installs require either dpkg or rpm". Takes the
# release tarballs, checks them against the release's SHA-256 files, puts `openshell` and
# `openshell-gateway` and `openshell-driver-vm` (the VM driver is a separate binary the gateway
# looks up in ~/.local/bin; the Docker and Podman drivers are built into the gateway) in ~/.local/bin and writes the `openshell-gateway` systemd user unit the
# deb and rpm packages would have installed (gateway.toml is written by a later osh step).
case "$(uname -m)" in
  x86_64 | amd64) a=x86_64 ;;
  aarch64 | arm64) a=aarch64 ;;
  *) echo "no OpenShell release is published for $(uname -m)" >&2; exit 1 ;;
esac
base=https://github.com/NVIDIA/OpenShell/releases/latest/download
cli="openshell-$a-unknown-linux-musl.tar.gz"
gw="openshell-gateway-$a-unknown-linux-gnu.tar.gz"
vm="openshell-driver-vm-$a-unknown-linux-gnu.tar.gz"
d="$(mktemp -d)"
trap 'rm -rf "$d"' EXIT
cd "$d"
for f in "$cli" "$gw" "$vm" openshell-checksums-sha256.txt openshell-gateway-checksums-sha256.txt; do
  curl -fLsS "$base/$f" -o "$f"
done
# An empty match makes `sha256sum -c` fail, so a missing checksum line fails the step too.
grep -F "  $cli" openshell-checksums-sha256.txt | sha256sum -c - >/dev/null
grep -F "  $gw" openshell-gateway-checksums-sha256.txt | sha256sum -c - >/dev/null
grep -F "  $vm" openshell-checksums-sha256.txt | sha256sum -c - >/dev/null
mkdir bin
tar xzf "$cli" -C bin
tar xzf "$gw" -C bin
tar xzf "$vm" -C bin
mkdir -p "$HOME/.local/bin"
install -m 755 bin/openshell bin/openshell-gateway bin/openshell-driver-vm "$HOME/.local/bin/"
u="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
mkdir -p "$u"
# Shaped like the packaged unit, with the binaries under %h/.local/bin and no config-migration
# helper (osh writes gateway.toml itself) and no EnvironmentFile (osh writes a drop-in that points
# it at this machine's gateway-<os>.env).
cat > "$u/openshell-gateway.service" <<'UNIT'
[Unit]
Description=OpenShell Gateway (user)
Documentation=https://github.com/NVIDIA/OpenShell
After=podman.socket
Wants=podman.socket

[Service]
Type=exec
Environment=OPENSHELL_LOCAL_TLS_DIR=%h/.local/state/openshell/tls
ExecStartPre=%h/.local/bin/openshell-gateway config preflight
ExecStartPre=%h/.local/bin/openshell-gateway generate-certs --output-dir ${OPENSHELL_LOCAL_TLS_DIR} --server-san host.openshell.internal
ExecStart=%h/.local/bin/openshell-gateway
StateDirectory=openshell
Restart=on-failure
RestartSec=5
NoNewPrivileges=yes
ProtectSystem=strict
PrivateTmp=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX

[Install]
WantedBy=default.target
UNIT
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) echo "installed to $HOME/.local/bin, which is not on PATH: add it" >&2; exit 1 ;;
esac
