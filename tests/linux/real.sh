#!/usr/bin/env bash
# The "real" Linux test: a privileged container with systemd as PID 1, an ordinary user with sudo,
# and nothing stubbed. It installs osh from a locally built release (scripts/release + install.sh),
# then runs osh setup with the real package manager, NVIDIA's real OpenShell installer, the real
# systemd user service and a real container runtime, and checks the gateway answers.
#
#   tests/linux/real.sh [podman|docker|vm]     default: podman
#
# vm boots the VM gateway (OpenShell's microVM driver), which needs KVM: the Docker host must
# expose /dev/kvm (a Linux host with virtualization on, or a VM with nested virtualization, such as
# Colima started with --nested-virtualization on an M3 or later Mac). The machine then gets
# --device /dev/kvm. Without it the run is SKIPPED with exit code 77, never reported as passed.
#
# Needs Docker (privileged containers). Slow: it builds the Linux binary and installs real
# packages. The container is removed at the end, pass or fail, unless OSH_REAL_KEEP=1 is set
# (then docker exec -u tester -it NAME bash lets you poke at it).
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
runtime="${1:-podman}"
[[ "$runtime" == podman || "$runtime" == docker || "$runtime" == vm ]] || { echo "usage: real.sh [podman|docker|vm]" >&2; exit 2; }
command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
docker info >/dev/null 2>&1 || { echo "the Docker daemon is not reachable" >&2; exit 1; }

devices=()
if [[ "$runtime" == vm ]]; then
  # Asked of the Docker host itself (on a Mac that is the Colima or Docker Desktop VM, not macOS).
  # A probe that fails for another reason (no image, no network) is an error, not a skip.
  docker run --rm ubuntu:24.04 true >/dev/null || { echo "cannot run ubuntu:24.04 to probe for /dev/kvm" >&2; exit 1; }
  if ! docker run --rm --device /dev/kvm ubuntu:24.04 test -c /dev/kvm >/dev/null 2>&1; then
    cat >&2 <<'EOF'
################################################################################
SKIPPED: the VM gateway scenario did not run.
The Docker host has no /dev/kvm, so no microVM can boot in the test machine.
Run it on a Linux host with KVM, or give the Docker VM nested virtualization
(Colima on an M3 or later Mac: colima start --nested-virtualization).
################################################################################
EOF
    exit 77
  fi
  devices=(--device /dev/kvm)
fi

case "$(docker info --format '{{.Architecture}}')" in aarch64|arm64) arch=arm64 ;; *) arch=amd64 ;; esac
# Inside the repo: a container VM shares the home folder, not /var/folders.
dist="$(mktemp -d "$repo/.real-dist.XXXXXX")"
name="osh-real-$$"
if [[ -n "${OSH_REAL_KEEP:-}" ]]; then
  echo "keeping container $name and $dist (remove them yourself)"
else
  trap 'docker rm -f "$name" >/dev/null 2>&1 || true; rm -rf "$dist"' EXIT
fi

echo "== building the Linux release archive ($arch)"
"$repo/scripts/release" v0.0.0-real --out "$dist" --no-host --linux "$arch" >"$dist/release.log" 2>&1 \
  || { tail -30 "$dist/release.log"; exit 1; }

echo "== building the systemd machine"
docker build -q -t osh-real -f "$repo/tests/linux/Dockerfile.real" "$repo/tests/linux" >/dev/null

echo "== booting it"
docker run -d --name "$name" --privileged --cgroupns=host ${devices[@]+"${devices[@]}"} -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
  -v "$dist:/dist:ro" -v "$repo:/repo:ro" osh-real >/dev/null
ready=0
for _ in $(seq 60); do
  state="$(docker exec "$name" systemctl is-system-running 2>/dev/null || true)"
  if [[ "$state" == running || "$state" == degraded ]]; then ready=1; break; fi
  sleep 1
done
[[ $ready -eq 1 ]] || { echo "systemd did not come up (state: ${state:-none})"; docker logs "$name" | tail -20; exit 1; }
# The image already has a user at UID 1000, so look the id up.
uid="$(docker exec "$name" id -u tester)"
if [[ "$runtime" == vm ]]; then
  # As on a stock host: /dev/kvm belongs to the kvm group and the user is not in it yet, so osh
  # has to add them and ask for a new login. (The node's group id comes from the Docker host.)
  docker exec "$name" bash -c 'getent group kvm >/dev/null || groupadd -r kvm; chgrp kvm /dev/kvm; chmod 660 /dev/kvm'
fi
docker exec "$name" loginctl enable-linger tester
docker exec "$name" systemctl start "user@${uid}.service"
for _ in $(seq 20); do docker exec "$name" test -S "/run/user/$uid/bus" && break; sleep 1; done

echo "== running the suite as an ordinary user"
docker exec -u tester -e XDG_RUNTIME_DIR="/run/user/$uid" -e OSH_PRESEED="${OSH_PRESEED:-}" \
  -e DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" \
  "$name" bash /repo/tests/linux/real-suite.sh "$runtime"
