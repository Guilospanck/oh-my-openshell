#!/usr/bin/env bash
# macOS for real, in a disposable macOS VM (Tart, Apple Virtualization): a clone of a Cirrus Labs
# macOS image gets osh from a locally built release and runs tests/macos/vm-suite.sh, which drives
# the real Homebrew, `brew services`, OpenShell's real installer and launchd (`launchctl
# bootstrap`, `print`, `bootout` on osh's own LaunchAgent) through setup, reset, remove, add and
# uninstall. The clone is deleted at the end, pass or fail, unless --keep.
#
#   tests/macos/vm.sh [--keep] [--preseed]        (or: just test-macos-vm [--keep] [--preseed])
#
# A macOS guest has no hypervisor (kern.hv_support is 0: Apple's nested virtualization is for
# Linux guests only), so nothing that boots a VM can run in it: no microVM sandbox, no Colima, no
# Podman machine (the suite checks those fail, with the reason). The Docker gateway therefore uses
# a Docker engine on the host: a separate, disposable Colima instance (its own COLIMA_HOME, never
# the user's), whose socket is forwarded over SSH into the guest at ~/.colima/default/docker.sock,
# where Colima itself would put it, with the same `colima` Docker context Colima creates.
#
# --preseed sets OSH_PRESEED=1: the suite writes a vm gateway.toml before osh setup, to get past a
# known bug (OpenShell's installer starts its Homebrew service before osh's VM config exists, and
# OpenShell never auto-detects the vm driver) and test the rest.
#
# Needs: macOS on Apple silicon, tart (https://tart.run; $TART or on PATH), colima, limactl and
# docker on PATH. The first run pulls the image ($OSH_MACOS_IMAGE, default
# ghcr.io/cirruslabs/macos-tahoe-base:latest: about 26 GB to download, 50 GB on disk in ~/.tart);
# it is kept for the next run (tart delete IMAGE frees it).
set -euo pipefail

[[ "$(uname)" == Darwin ]] || { echo "tests/macos/vm.sh runs on macOS only" >&2; exit 2; }
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
keep=0
for a in "$@"; do
  case "$a" in
    --keep) keep=1 ;;
    --preseed) export OSH_PRESEED=1 ;;
    *) echo "usage: vm.sh [--keep] [--preseed]" >&2; exit 2 ;;
  esac
done
TART="${TART:-$(command -v tart || true)}"
[[ -x "$TART" ]] || { echo "tart is required (https://tart.run), or set TART=/path/to/tart" >&2; exit 2; }
for t in colima limactl docker ssh ssh-keygen; do
  command -v "$t" >/dev/null 2>&1 || { echo "$t is required" >&2; exit 2; }
done
TIMEOUT="$(command -v timeout || command -v gtimeout)" || { echo "timeout (coreutils) is required" >&2; exit 2; }
image="${OSH_MACOS_IMAGE:-ghcr.io/cirruslabs/macos-tahoe-base:latest}"
vm="osh-macos-$$"
# Short: Lima's sockets live under it.
work="$(cd "$(mktemp -d /private/tmp/osh-mvm.XXXXXX)" && pwd -P)"
export COLIMA_HOME="$work/colima"
mkdir -p "$COLIMA_HOME" "$work/dist" "$work/docker"
col() { DOCKER_CONFIG="$work/docker" colima "$@"; }
run_pid="" fwd_pid=""
# Colima caches its VM image in ~/Library/Caches/colima (about 330 MB); remove it again if this
# run is what created it.
colima_cache="$HOME/Library/Caches/colima"
[[ -e "$colima_cache" ]] && cache_was_there=1 || cache_was_there=0
teardown() {
  [[ -n "$fwd_pid" ]] && kill "$fwd_pid" 2>/dev/null || true
  if [[ $keep -eq 1 ]]; then
    echo "keeping VM $vm (tart stop $vm; tart delete $vm) and $work (COLIMA_HOME=$COLIMA_HOME colima delete -p osh-macvm --data --force)"
    return
  fi
  echo "== deleting the VM clone and the host Colima instance"
  "$TART" stop "$vm" >/dev/null 2>&1 || true
  [[ -n "$run_pid" ]] && kill "$run_pid" 2>/dev/null || true
  "$TART" delete "$vm" >/dev/null 2>&1 || true
  col delete -p osh-macvm --data --force >/dev/null 2>&1 || true
  pkill -f "$work/" 2>/dev/null || true
  rm -rf "$work"
  [[ $cache_was_there -eq 1 ]] || rm -rf "$colima_cache"
}
trap teardown EXIT
trap 'exit 130' INT TERM

free_gb="$(df -g "$HOME" | awk 'NR==2 {print $4}')"
if ! "$TART" list --source oci -q 2>/dev/null | grep -qxF "$image"; then
  (( free_gb >= 90 )) || { echo "pulling $image needs about 80 GB free; $free_gb GB left" >&2; exit 1; }
  echo "== pulling $image (about 26 GB, once)"
  "$TIMEOUT" 7200 "$TART" pull "$image" >"$work/pull.log" 2>&1 || { tail -5 "$work/pull.log"; exit 1; }
fi

echo "== building the macOS release archive"
"$repo/scripts/release" v0.0.0-macvm --out "$work/dist" >"$work/release.log" 2>&1 || { tail -20 "$work/release.log"; exit 1; }

echo "== starting the host Docker engine for the guest (Colima instance osh-macvm, $COLIMA_HOME)"
"$TIMEOUT" 900 env DOCKER_CONFIG="$work/docker" colima start -p osh-macvm --vm-type vz --cpu 2 --memory 4 --disk 20 \
  --activate=false --ssh-config=false >"$work/colima.log" 2>&1 || { tail -20 "$work/colima.log"; exit 1; }
host_sock="$COLIMA_HOME/osh-macvm/docker.sock"
[[ -S "$host_sock" ]] || { echo "no Docker socket at $host_sock" >&2; exit 1; }

echo "== cloning $image as $vm and booting it"
"$TART" clone "$image" "$vm"
"$TART" run --no-graphics --dir="dist:$work/dist:ro" --dir="repo:$repo:ro" "$vm" >"$work/run.log" 2>&1 &
run_pid=$!
ready=0
for _ in $(seq 120); do "$TART" exec "$vm" true >/dev/null 2>&1 && { ready=1; break; }; sleep 3; done
[[ $ready -eq 1 ]] || { echo "the guest agent did not answer within 6 minutes"; tail -5 "$work/run.log"; exit 1; }
ip="$("$TART" ip "$vm" --wait 60)"

echo "== forwarding the host Docker socket into the guest ($ip)"
ssh-keygen -q -t ed25519 -N '' -f "$work/key"
"$TART" exec -i "$vm" sh -c 'mkdir -p ~/.ssh ~/.colima/default && cat >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys' <"$work/key.pub"
ssh -i "$work/key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -o ExitOnForwardFailure=yes -o StreamLocalBindUnlink=yes -o ServerAliveInterval=15 -o LogLevel=ERROR \
  -N -R "/Users/admin/.colima/default/docker.sock:$host_sock" "admin@$ip" &
fwd_pid=$!

echo "== running the suite in the guest"
"$TART" exec "$vm" env OSH_PRESEED="${OSH_PRESEED:-}" bash "/Volumes/My Shared Files/repo/tests/macos/vm-suite.sh"
