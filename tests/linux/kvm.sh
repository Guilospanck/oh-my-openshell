#!/usr/bin/env bash
# The VM gateway scenario of tests/linux/real.sh on a Mac, with real KVM: a separate Colima
# instance with nested virtualization (Apple Virtualization, M3 or later, macOS 15 or later) is
# the Docker host, so the systemd test machine gets a real /dev/kvm and OpenShell's VM driver
# boots real microVMs in it.
#
#   tests/linux/kvm.sh [--keep] [--preseed]        (or: just test-linux-kvm [--keep] [--preseed])
#
# --preseed sets OSH_PRESEED=1: real-suite.sh writes a vm gateway.toml before osh setup, to get
# past a known bug (OpenShell's installer cannot start its gateway before osh's VM config exists)
# and test the rest, including booting a microVM. Without it the run shows that bug as a failure.
#
# Nothing of an existing Colima is touched: the instance lives in its own COLIMA_HOME
# ($OSH_KVM_HOME, default ~/.cache/osh-kvm, profile osh-kvm), is started without switching the
# Docker context (--activate=false, and a throwaway DOCKER_CONFIG for colima itself) and without
# editing ~/.ssh/config, and the test talks to it through DOCKER_HOST for this run only. It is
# created when absent and deleted at the end, pass or fail, unless --keep (then it stays for the
# next run; delete it with: COLIMA_HOME=~/.cache/osh-kvm colima delete -p osh-kvm --data --force).
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
keep=0
for a in "$@"; do
  case "$a" in
    --keep) keep=1 ;;
    --preseed) export OSH_PRESEED=1 ;;
    *) echo "usage: kvm.sh [--keep] [--preseed]" >&2; exit 2 ;;
  esac
done
[[ "$(uname)" == Darwin ]] || { echo "kvm.sh runs on macOS; on Linux with KVM run: tests/linux/real.sh vm" >&2; exit 2; }
for t in colima limactl docker; do
  command -v "$t" >/dev/null 2>&1 || { echo "$t is required (brew install colima docker)" >&2; exit 2; }
done
chip="$(sysctl -n machdep.cpu.brand_string)"
if [[ "$chip" =~ Apple\ M([0-9]+) ]] && (( BASH_REMATCH[1] < 3 )); then
  echo "SKIPPED: nested virtualization needs an M3 or later ($chip)" >&2; exit 77
fi
(( $(sw_vers -productVersion | cut -d. -f1) >= 15 )) || { echo "SKIPPED: nested virtualization needs macOS 15 or later" >&2; exit 77; }

profile=osh-kvm
export COLIMA_HOME="${OSH_KVM_HOME:-$HOME/.cache/osh-kvm}"
# colima writes a Docker context for each instance it starts; keep it out of ~/.docker.
colima_cfg="$(mktemp -d "${TMPDIR:-/tmp}/osh-kvm-docker.XXXXXX")"
col() { DOCKER_CONFIG="$colima_cfg" colima "$@"; }
created=0
# Colima caches its VM image in ~/Library/Caches/colima (about 330 MB); remove it again if this
# run is what created it.
colima_cache="$HOME/Library/Caches/colima"
[[ -e "$colima_cache" ]] && cache_was_there=1 || cache_was_there=0
teardown() {
  if [[ $keep -eq 1 ]]; then
    echo "keeping the Colima instance $profile in $COLIMA_HOME"
  elif [[ $created -eq 1 || -d "$COLIMA_HOME/$profile" ]]; then
    echo "== deleting the Colima instance $profile"
    col delete -p "$profile" --data --force >/dev/null 2>&1 || true
    rm -rf "$COLIMA_HOME"
  [[ $cache_was_there -eq 1 ]] || rm -rf "$colima_cache"
  fi
  rm -rf "$colima_cfg"
}
trap teardown EXIT
trap 'exit 130' INT TERM

if col status -p "$profile" >/dev/null 2>&1; then
  echo "== reusing the running Colima instance $profile ($COLIMA_HOME)"
else
  echo "== starting a separate Colima instance $profile with nested virtualization ($COLIMA_HOME)"
  created=1
  mkdir -p "$COLIMA_HOME"
  col start -p "$profile" --vm-type vz --nested-virtualization --cpu 4 --memory 8 --disk 60 \
    --activate=false --ssh-config=false >"$COLIMA_HOME/start.log" 2>&1 \
    || { tail -20 "$COLIMA_HOME/start.log"; exit 1; }
fi
sock="$COLIMA_HOME/$profile/docker.sock"
[[ -S "$sock" ]] || { echo "no Docker socket at $sock" >&2; exit 1; }
col ssh -p "$profile" -- test -c /dev/kvm || { echo "the Colima instance has no /dev/kvm (nested virtualization is off)" >&2; exit 1; }
echo "ok   /dev/kvm exists in the Colima instance"

echo "== the VM gateway scenario against that instance only"
unset DOCKER_CONTEXT
DOCKER_HOST="unix://$sock" "$repo/tests/linux/real.sh" vm
