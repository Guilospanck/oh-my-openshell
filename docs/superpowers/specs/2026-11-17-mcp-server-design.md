# osh MCP server: a long-running, network-reachable server (design)

Status: design for approval. Two builds, in this order: **Part 1** the server, its service, its
dashboard tab; **Part 2** background jobs. This document decides both; each gets its own plan.

## Intent and why

`osh mcp` today is a stdio server: the agent that starts it owns it, and it ends with that agent.
The "factory" (a separate app) is an MCP client of osh. It polls its own ticket system, which
osh knows nothing about. For each ticket it calls osh's tools to create, start or reuse sandboxes,
drive Claude inside them, and gather results. It may reach osh on localhost or across a VPN. So osh
needs one server that stays up, independent of the dashboard, that clients connect to over HTTP,
and the dashboard becomes one way to switch it on and off and to see who is connected.

## Goals

- `osh mcp serve` listens over HTTP and serves the existing tools through `osh_core::mcp::handle`.
- It runs as a system service (launchd agent on macOS, systemd user unit on Linux), like the
  gateways, so it survives closing the dashboard and rebooting.
- On/off from the dashboard (like the gateway status) and from `osh mcp start|stop|status`.
- A dashboard MCP tab that lists connected clients (and, after Part 2, running jobs).
- Part 2: a command may run for hours and must not be tied to an HTTP request or to the server
  process: it keeps running when the server restarts.
- Every tool takes an optional `gateway` argument, so one server can drive several gateways.
- Server logic lives in `osh-core` (no TUI, no Zig); the dashboard only views and drives.

## Non-goals

- TLS inside osh (the VPN or an SSH tunnel provides encryption).
- Server-pushed streams (SSE) and MCP notifications. Clients poll.
- Per-client permissions or several tokens. One token, full access to the tools (the existing
  share and repo-variable limits still apply).
- Windows. Docker Desktop paths. Anything beyond macOS and Linux.
- Changing which tools exist other than `sandbox_exec` (Part 2), the job tools and the `gateway`
  argument.

## Anti-goals (a failure even if it works)

- A listening port that creates sandboxes for anyone who can reach it.
- A restart of the server, or a dropped connection, that loses a running ticket's work.
- A change that breaks stdio `osh mcp` for agents already using it.
- The token on a command line, in a log, or in the state file.

## Constraints

- Hand-written HTTP on `std::net` and threads; no new dependencies in `osh-core` unless the
  builder shows the hand-written parser cannot be made safe (size limits, timeouts, no panics on
  garbage). Part of the dependency policy: `tui/core/Cargo.toml` stays small.
- New host-touching code goes through `Host` and returns `Step`s, tested with `FakeHost`
  (as `gwops.rs`). Unit text for launchd and systemd is generated like `gwops::launchd_plist` and
  `gwops::systemd_unit`.
- Settings follow the settings design: one `Def` in `settings.rs`, a README `## Settings` row,
  env > `osh.toml` > default.
- Every new dashboard key needs an entry in `tests/e2e/actions.txt` and a scenario; anything that
  runs inside a sandbox needs a check in `tests/sandbox/checks.sh` (ask before `just test-sandbox`).
- Never pass credentials in argv, never print them.

## Approaches considered

1. **How the server runs.**
   - A: system service (chosen). Survives reboot and crash; costs per-platform unit text and tests.
   - B: detached process with a pid file. Simple, portable; gone after reboot or crash.
   - C: foreground only. Least work; no switch, which was asked for.
   The server is the foreground command `osh mcp serve` in all cases, so B or C remain possible
   as thin wrappers later.
2. **Long commands.**
   - Keep `sandbox_exec` synchronous and add `exec_start` (caller must guess which). Rejected: a
     wrong guess kills a long run at the 10 minute stop.
   - Every `sandbox_exec` is a job from the start; the call waits `wait_secs` and, if not done,
     returns the job id (chosen).
3. **How the dashboard sees clients.** A state file written atomically by the server and read by
   the dashboard (chosen: no extra port, no second login) versus an admin HTTP endpoint (rejected:
   a second surface to authenticate).

## Part 1: the server

Decided by the user: runs as a service; started and stopped from the dashboard and `osh mcp`;
localhost or VPN reach; MCP tab with connected clients; hand-written HTTP; logic in `osh-core`.

My calls, as proposals the user accepted ("acceptable for now"): 
- **Transport.** `POST /mcp` with a JSON-RPC body, `Content-Type: application/json`; the reply is
  the JSON-RPC response body. A notification gets `202` with no body. Any other path or method is
  an HTTP error. No batches (as `handle` today). Limits: request body size, header size, a read
  timeout, a cap on simultaneous connections.
- **Security.** Binds `127.0.0.1` by default. Another address is an explicit `osh.toml` setting
  (`mcp.server_bind`), and the server refuses to start on a non-loopback address without a token.
  Every request needs `Authorization: Bearer <token>`; compared in constant time. The token is
  created by osh on first start and kept in an owner-only (`0600`) file in the data folder; it is
  shown only by an explicit command (`osh mcp token`), never in logs or the state file.
- **Clients.** A client is identified by the `Mcp-Session-Id` the server gives on `initialize`
  (plus the peer address). The state file lists each client's id, peer, first seen, last seen and
  the name and version it sent on `initialize`. A client unseen for a settable time is dropped
  from the list.
- **State file.** `<data>/mcp-server.json`: pid, listen address, start time, version, clients
  (and jobs in Part 2, each with its gateway). Written atomically on change; the dashboard shows "off" when the pid is
  dead or the file is absent.
- **Settings** (all in `osh.toml`, listed on the Settings screen): `mcp.server_bind`,
  `mcp.server_port`, `mcp.client_idle_secs`. The existing `mcp.allow_share`, `mcp.share_roots`,
  `mcp.repo_vars` apply to the server unchanged and are re-read while it runs.
- **Dashboard.** A fifth-plus tab "MCP": status (on/off, address), a switch, the client list. Keys
  and scenarios per the repo rules. The dashboard installs, starts and stops the service through
  `Step`s with the usual consent.
- **stdio unchanged.** `osh mcp` keeps working as today.

Decided by the user after the builder check: the gateway is **chosen per call**.
- Every tool has an optional `gateway` string. Not passed: the server's default gateway (the main
  one, or the one set in `osh.toml` as `mcp.server_gateway`), as `osh mcp --gateway` does today.
- My call on which names a client may use: the gateways osh knows (its registry plus the main).
  Any other name is an invalid-params error, so a client cannot aim osh at an arbitrary endpoint.
  The same argument is added to stdio `osh mcp`.
- A sandbox name is only unique within its gateway, so `sandbox_exec`, `sandbox_delete` and the
  job tools act on the gateway named in the call, and a job id also records its gateway.

Further calls of mine, after the builder check:
- **Token reads.** The token file is read on every request, so replacing it takes effect at once
  without a restart; the service unit and the environment never carry the token.
- **Sessions across restarts.** An unknown `Mcp-Session-Id` is accepted and added back to the
  client list; clients need not re-initialize after a restart.
- **Service.** One service per machine, `osh-mcp`. Its unit runs a wrapper subcommand
  (like `osh gateway-service`) that sets `PATH` (for `openshell` and the container runtime) and
  `OSH_DIR`. The default port is the builder's pick and a setting.

## Part 2: jobs

Decided by the user: every long run is a background job; a job survives a server restart; the
caller chooses how long to wait.

My calls: 
- `sandbox_exec` gains `wait_secs` (default 30; `0` returns a job id at once). Done within the wait:
  the result as today. Otherwise `{"job": "<id>", "status": "running"}`. The fixed 10 minute stop
  goes away for jobs.
- New tools `job_status`, `job_output` (from an offset, returns new text and the next offset) and
  `job_cancel`.
- A job runs inside the sandbox in a way that outlives the server (its output goes to a file in
  the sandbox); the server finds it again after a restart by id. A cap `max_secs` (setting
  `mcp.job_max_secs`, default generous, `0` for none) stops a stuck job.
- The MCP tab also lists running jobs.
- Starting a job assumes `sh` with `setsid` or `nohup` in the sandbox image (my call: the images are
  Debian-based). A check in `tests/sandbox/checks.sh` proves it in a real sandbox; if they are
  missing, starting a job fails with a clear error and nothing is left running.

## Left to the builder

Exact HTTP parsing details and limits; the state file schema; how launchd and systemd units are
named and where logs go; token file name; how a job is started inside the sandbox and found again;
defaults for the new settings; the order of tasks inside each part.

## Open points I could not settle from the conversation

- Whether a second factory client needs its own token (not now: one token).
- Whether the server should stop itself after long idle time (not now: it is a service).
