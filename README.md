# oh-my-openshell

Portable gateway config, image definitions, and explicitly imported provider
profiles. Run recipes with `just --justfile .justfile <recipe>` from this repo
(or `just --justfile ~/.config/openshell/.justfile <recipe>`).

This repo is consumed as a git submodule by
[dotfiles](https://github.com/Guilospanck/dotfiles) at `openshell/`, which links
it into `~/.config/openshell/` (see "Config links" in the dotfiles README).
Change it here, not in the dotfiles checkout.

## Interactive TUI

`just --justfile .justfile tui` (or `~/.config/openshell/osh`) opens
a ratatui dashboard, built from `tui/` with cargo on first run and
whenever its sources change. Needs cargo; `just` for installing OpenShell and
Docker for building images.

- It offers to install OpenShell if missing and shows whether the gateway is
  reachable.
- `g` switches between registered gateways; the header shows the active one and
  its compute driver. Sandboxes, providers and profiles are per gateway.
- **Sandboxes** refresh every two seconds with phase, conditions and a log tail
  for the selected one. `l` opens a full-screen live log viewer (streams
  `openshell logs --tail`; `j`/`k`/PgUp/PgDn scroll, `g`/`G` top/bottom, `f`
  toggles follow, `s` cycles the source filter (all, gateway, sandbox), `/` searches, `n`/`N` next/previous match, `h`/`l` pan, `q`
  closes). `Enter` attaches, `n` creates (presets or custom
  image/providers), `d` deletes, `s`/`S` stop and start, `a`/`x` attach or
  detach a provider. `n` also prompts for an optional one-shot command: when
  set, the sandbox runs `sh -c '<command>'` with `--no-keep` and is deleted when
  it exits. On a non-vm gateway (Docker) it first prompts for optional CPU
  (`500m`, `2`) and memory (`512Mi`, `4Gi`) limits; empty means unlimited.
- **Providers**: `n` creates one from a `providers/*.yaml` profile (credentials
  are taken from the TUI's environment when exported, else prompted hidden;
  always passed by environment, never argv), `u` updates credentials, `d`
  deletes. For `claude-code` you choose between generating a token (runs
  `claude setup-token` on the terminal; copy the printed token, press Enter, then
  paste it into the hidden prompt, because the command has no machine-readable
  output) and pasting one directly.
- **Profiles**: `u` imports a profile or replaces the gateway copy with the one
  in `providers/` (it handles `resource_version`).
- **Images**: `b` builds a `images/Dockerfile-*`, labelling the image with a
  hash of the Dockerfile. The state column shows `outdated` when the Dockerfile
  changed since the build, `unverified` for images built without the label
  (rebuild once to track them) and `up to date` otherwise. It does not detect a
  newer upstream release behind `CLAUDE_CODE_VERSION=latest`; rebuild with
  `--no-cache` for that.
- **Gateways**: lists registered gateways; `Enter` selects one, `d` wipes it
  after a confirmation. The wipe runs `gateway-reset <name>` (`openshell` or
  `openshell-docker` only): it deletes the gateway's sandboxes through the
  gateway, sweeps leftover containers/volumes (Docker) or microVM state (vm),
  deletes its SQLite database and restarts it empty. Providers, credentials and
  profiles on that gateway are lost; TLS material and registration are kept.

Attaching, building and creating a sandbox hand the terminal to the command and
return when you press Enter.

## Two gateways: vm and docker

The Homebrew service runs the `openshell` gateway with the **vm** (libkrun)
driver. Every VM gets the gateway-wide size (2 vCPUs, 2048 MiB unless
`vcpus`/`mem_mib` are set under `[openshell.drivers.vm]`); `--cpu`/`--memory`
and `--driver-config-json` cannot change it, so per-sandbox sizing needs a
second gateway with the **docker** driver (Colima), which applies them as
container limits. Containers share Colima's kernel, so isolation is weaker than a
microVM's.

`just gateway-docker` installs a launchd agent (`gateway-docker-service`, config
`gateway-docker.toml`, port 17671, separate database) and registers it as
`openshell-docker`. It reuses the Homebrew gateway's TLS material and
JWT signing key, so run the Homebrew service once first. Providers and profiles
must be created on each gateway. The vm gateway's `gateway.toml` is copied to
`~/.config/openshell/` because the gateway rejects a symlinked config.

## Subscription providers

| Recipe / profile ID | Client | Authentication |
| --- | --- | --- |
| `provider-claude-code` / `claude-code` | Claude Code CLI | `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token` |

The API-key-only `anthropic` profile and recipes were removed.
Subscription availability and
third-party-client eligibility are controlled by the vendor, not OpenShell;
these profiles do not bypass plan limits or guarantee account access.

Recipes lint/import the profile and create a same-named provider. They are
**create-only**, not updates. They do not install clients or build images.
Attach only the profiles needed by that sandbox using repeatable `--provider`.

### Claude Code

On the host, run `claude setup-token`. Store the returned token privately and
export it as `CLAUDE_CODE_OAUTH_TOKEN` without putting it in shell history, then:

```sh
just --justfile .justfile provider-claude-code
openshell sandbox create --from <claude-image> --provider claude-code --detach
```

Start a session with `openshell sandbox exec -n <name> --tty -- claude` (or `bash`).
`--detach` keeps the sandbox's shell running in the background, so each `exec`
session is independent: `/q` or `exit` ends only that session and the next
`exec` works immediately. Do not use `sandbox connect`; it attaches to the main
process, and exiting it ends the sandbox.

The recipe reads the exported variable by name, never putting its value in
command arguments. OpenShell keeps the real token outside the sandbox and
resolves its environment placeholder in outbound API requests. This profile
allows only `api.anthropic.com`; telemetry endpoints are intentionally omitted.
Do not supply Anthropic API-key providers alongside it if you want subscription
billing. Claude's `--bare` mode does not read this OAuth variable.

Setup tokens have an expiry and are **not automatically refreshed** here. Before
expiry, generate a replacement, export it, and run:

```sh
openshell provider update claude-code --credential CLAUDE_CODE_OAUTH_TOKEN
```

Wait for the provider change to apply (check installed CLI update/status help),
then start a new Claude process to inherit the new placeholder.

## Migrating existing gateway objects

Changing repository files does **not** update or delete gateway resources.
Before replacing an existing `claude-code` profile, inspect its
providers and attached sandboxes. Its credential schema changed: it now requires
a setup token. Plan downtime, detach affected providers, and remove the old
provider instances before changing the profile schema.

Export the existing profile, preserve its `resource_version`, replace its
definition with the reviewed local definition, and use
`openshell profile update <id> -f <exported-and-edited-file>`. Recreate provider
instances with the new credentials; the create-only recipes
cannot be rerun unchanged once a profile already exists. Reattach the new
instances and restart client processes. Remove unused old `anthropic` providers and profiles explicitly
only after checking their consumers. No gateway mutations are performed merely
by editing these files or running lint.
