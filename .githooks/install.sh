#!/usr/bin/env sh
#
# Point this clone's git at the repo's tracked hooks. Run once per fresh clone:
#   sh .githooks/install.sh
set -eu

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
git -C "$repo_root" config core.hooksPath .githooks
echo "core.hooksPath set to .githooks"
