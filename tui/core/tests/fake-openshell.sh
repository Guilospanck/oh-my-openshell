#!/usr/bin/env bash
# Recording stand-in for the `openshell` CLI, for tests that must never reach a real gateway.
# Copy it into a temp dir as `openshell`, put that dir first on PATH, and drive it with:
#
#   OSH_FAKE_LOG          (required) file each call appends its argv to, one line per call,
#                         arguments joined by single spaces
#   OSH_FAKE_SANDBOXES    stdout of `sandbox list -o|--output json` (default {"sandboxes":[]})
#   OSH_FAKE_PROVIDERS    stdout of `provider list -o|--output json` (default {"providers":[]})
#   OSH_FAKE_GATEWAYS     stdout of `gateway list -o json` (default [])
#   OSH_FAKE_GATEWAY_INFO stdout of `gateway info -o json`
#                         (default {"compute_drivers":[{"name":"docker"}]})
#   OSH_FAKE_FAIL         when the joined argv contains this text, print "fake failure" on
#                         stderr and exit 1 (the call is still logged)
#   OSH_FAKE_EXEC_EXIT    exit code of `sandbox exec` (default 0)
#   OSH_FAKE_EXEC_STDOUT  printed on stdout by `sandbox exec`
#   OSH_FAKE_EXEC_STDERR  printed on stderr by `sandbox exec`
#   OSH_FAKE_ENV_KEYS     space-separated variable names; for each, a line `env NAME set` or
#                         `env NAME unset` is logged after the argv. Values are never logged, so
#                         a test can prove a secret came through the environment, not argv.
#
# A leading `-g NAME` (the gateway selector) is logged but ignored when matching the command.
# Every other call prints nothing and exits 0.
set -euo pipefail

log="${OSH_FAKE_LOG:?OSH_FAKE_LOG must name the log file}"
printf '%s\n' "$*" >>"$log"
for key in ${OSH_FAKE_ENV_KEYS:-}; do
    if [ -n "${!key:-}" ]; then
        printf 'env %s set\n' "$key" >>"$log"
    else
        printf 'env %s unset\n' "$key" >>"$log"
    fi
done

if [ -n "${OSH_FAKE_FAIL:-}" ] && [[ "$*" == *"$OSH_FAKE_FAIL"* ]]; then
    echo "fake failure" >&2
    exit 1
fi

no_sandboxes='{"sandboxes":[]}'
no_providers='{"providers":[]}'
docker_info='{"compute_drivers":[{"name":"docker"}]}'

if [ "${1:-}" = "-g" ]; then
    shift 2
fi

case "${1:-} ${2:-}" in
"sandbox list")
    printf '%s\n' "${OSH_FAKE_SANDBOXES:-$no_sandboxes}"
    ;;
"provider list")
    printf '%s\n' "${OSH_FAKE_PROVIDERS:-$no_providers}"
    ;;
"gateway list")
    printf '%s\n' "${OSH_FAKE_GATEWAYS:-[]}"
    ;;
"gateway info")
    printf '%s\n' "${OSH_FAKE_GATEWAY_INFO:-$docker_info}"
    ;;
"sandbox exec")
    if [ -n "${OSH_FAKE_EXEC_STDOUT:-}" ]; then printf '%s' "$OSH_FAKE_EXEC_STDOUT"; fi
    if [ -n "${OSH_FAKE_EXEC_STDERR:-}" ]; then printf '%s' "$OSH_FAKE_EXEC_STDERR" >&2; fi
    exit "${OSH_FAKE_EXEC_EXIT:-0}"
    ;;
esac
exit 0
