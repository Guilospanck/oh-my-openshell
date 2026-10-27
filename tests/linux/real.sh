#!/usr/bin/env bash
# The "real" Linux test: a privileged container with systemd as PID 1, an ordinary user with sudo,
# and nothing stubbed. It installs osh from a locally built release (scripts/release + install.sh),
# then runs osh setup with the real package manager, NVIDIA's real OpenShell installer, the real
# systemd user service and a real container runtime, and checks the gateway answers.
#
#   tests/linux/real.sh [podman|docker]     default: podman
#
# Needs Docker (privileged containers). Slow: it builds the Linux binary and installs real
# packages. The container is removed at the end, pass or fail, unless OSH_REAL_KEEP=1 is set
# (then docker exec -u tester -it NAME bash lets you poke at it).
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
runtime="${1:-podman}"
[[ "$runtime" == podman || "$runtime" == docker ]] || { echo "usage: real.sh [podman|docker]" >&2; exit 2; }
command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
docker info >/dev/null 2>&1 || { echo "the Docker daemon is not reachable" >&2; exit 1; }

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
docker run -d --name "$name" --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
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
docker exec "$name" loginctl enable-linger tester
docker exec "$name" systemctl start "user@${uid}.service"
for _ in $(seq 20); do docker exec "$name" test -S "/run/user/$uid/bus" && break; sleep 1; done

echo "== running the suite as an ordinary user"
docker exec -u tester -e XDG_RUNTIME_DIR="/run/user/$uid" \
  -e DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" \
  "$name" bash /repo/tests/linux/real-suite.sh "$runtime"
