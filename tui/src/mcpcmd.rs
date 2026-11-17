//! `osh mcp [--gateway G]`: the stdio loop around `osh_core::mcp::handle`. Stdout carries
//! protocol lines only; diagnostics go to stderr and never include request bodies.
//! `osh mcp serve` is the same server over HTTP, kept running (see `osh_core::mcpserver`), and
//! `osh mcp token` prints the bearer token its clients present.

use osh_core::mcp;
use osh_core::mcpserver::{http, state, token};
use osh_core::ops::Ctx;
use osh_core::settings;
use std::io::{BufRead, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const USAGE: &str = "usage: osh mcp [--gateway G]\n\
       osh mcp serve    run the HTTP server in the foreground (the service runs this)\n\
       osh mcp token    print the bearer token HTTP clients must present\n\
       osh mcp start|stop|remove [--yes]    install and turn on, turn off, or delete the service\n\
       osh mcp status   say whether the server is on\n\
Serves MCP over stdio (newline-delimited JSON-RPC). What agents may do is set in osh.toml \
(see `osh config`): mcp.allow_share lets specs mount host folders inside the directories in \
mcp.share_roots, and mcp.repo_vars lists the variables a spec's repo.env_var may name. \
OSH_MCP_ALLOW_SHARE=1, OSH_MCP_SHARE_ROOT (separated like PATH) and OSH_MCP_REPO_VARS \
(comma-separated) in the server's environment override them.";

/// The answer to a line that is not UTF-8: a parse error with no id, as for bad JSON.
const NOT_UTF8: &str =
    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error: not UTF-8"}}"#;

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Stdio(Option<String>),
    Serve,
    Token,
    Status,
    /// `--yes` given: run the steps without asking.
    Service(Verb, bool),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Verb {
    Start,
    Stop,
    Remove,
}

fn mode(args: &[String]) -> Result<Mode, String> {
    let verb = match args.first().map(String::as_str) {
        Some("start") => Some(Verb::Start),
        Some("stop") => Some(Verb::Stop),
        Some("remove") => Some(Verb::Remove),
        _ => None,
    };
    if let Some(v) = verb {
        return match &args[1..] {
            [] => Ok(Mode::Service(v, false)),
            [y] if y == "--yes" => Ok(Mode::Service(v, true)),
            [other, ..] => Err(format!("unexpected argument {other:?}")),
        };
    }
    match args.first().map(String::as_str) {
        Some("status") if args.len() > 1 => Err(format!("unexpected argument {:?}", args[1])),
        Some("status") => Ok(Mode::Status),
        Some("serve") | Some("token") if args.len() > 1 => {
            Err(format!("unexpected argument {:?}", args[1]))
        }
        Some("serve") => Ok(Mode::Serve),
        Some("token") => Ok(Mode::Token),
        _ => parse(args).map(Mode::Stdio),
    }
}

fn parse(args: &[String]) -> Result<Option<String>, String> {
    let mut gateway = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--gateway" => {
                gateway = Some(it.next().ok_or("--gateway needs a value")?.clone());
            }
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    Ok(gateway)
}

/// What the operator allows, from the resolved settings (`mcp.*` in `osh.toml`, where the
/// `OSH_MCP_*` variables win): sharing on or off, the share roots, the repo variable names. No
/// flag or tool argument can change any of it.
fn limits(snap: &osh_core::settings::Snapshot) -> (bool, Vec<String>, Vec<String>) {
    use osh_core::settings::Value;
    let get = |k: &str| snap.rows.iter().find(|r| r.def.key == k).map(|r| &r.value);
    let list = |k: &str| match get(k) {
        Some(Value::List(l)) => l.clone(),
        _ => Vec::new(),
    };
    (
        matches!(get("mcp.allow_share"), Some(Value::Bool(true))),
        list("mcp.share_roots"),
        list("mcp.repo_vars"),
    )
}

/// The folders a share must lie in, each canonicalized. An entry that is not an absolute path
/// of an existing directory is dropped, with a line saying so. No root refuses every share.
fn share_roots(entries: &[String]) -> (Vec<PathBuf>, Vec<String>) {
    let (mut roots, mut dropped) = (Vec::new(), Vec::new());
    for entry in entries {
        match root_dir(Path::new(entry)) {
            Some(real) if real == Path::new("/") => dropped.push(format!(
                "osh: mcp: mcp.share_roots entry {entry} is the whole disk, which would allow every folder; ignored"
            )),
            Some(real) => roots.push(real),
            None => dropped.push(format!(
                "osh: mcp: mcp.share_roots entry {entry} is not an absolute path of an existing directory; ignored"
            )),
        }
    }
    (roots, dropped)
}

fn root_dir(entry: &Path) -> Option<PathBuf> {
    if !entry.is_absolute() {
        return None;
    }
    std::fs::canonicalize(entry).ok().filter(|p| p.is_dir())
}

/// The server's context. An agent writes the specs, so a risky share folder is refused, not
/// only noted, and no share root is set (`run` adds the operator's roots). The version is this
/// binary's (the one `osh --version` prints and the release bumps), not osh-core's.
fn server_ctx(gateway: Option<String>, allow_share: bool, repo_vars: Vec<String>) -> Ctx {
    Ctx {
        dir: osh_core::assets::data_dir(),
        gateway,
        allow_share,
        refuse_risky_share: true,
        repo_vars: Some(repo_vars),
        share_roots: Some(Vec::new()),
        version: env!("CARGO_PKG_VERSION").into(),
    }
}

/// The context `osh mcp` serves with, from the resolved settings, and what to say on stderr
/// about entries that were dropped. Shares and repo variables are always `Some`: `None` would
/// mean no check at all.
fn build_ctx(gateway: Option<String>, snap: &osh_core::settings::Snapshot) -> (Ctx, Vec<String>) {
    let mut warnings: Vec<String> = snap
        .problems
        .iter()
        .filter(|p| p.key.as_deref().is_none_or(|k| k.starts_with("mcp.")))
        .map(|p| format!("osh: mcp: {}", p.msg))
        .collect();
    let (allow_share, root_entries, vars) = limits(snap);
    let (roots, dropped) = share_roots(&root_entries);
    warnings.extend(dropped);
    let ctx = Ctx {
        share_roots: Some(roots),
        ..server_ctx(gateway, allow_share, vars)
    };
    (ctx, warnings)
}

/// Answer each line until EOF. A line that is not UTF-8 is answered with a parse error (never
/// altered and run), and the loop goes on.
pub fn serve(input: impl BufRead, mut out: impl Write, ctx: &Ctx) -> std::io::Result<()> {
    for raw in input.split(b'\n') {
        let resp = match String::from_utf8(raw?) {
            Ok(line) if line.trim().is_empty() => continue,
            Ok(line) => mcp::handle(line.trim(), ctx),
            Err(_) => Some(NOT_UTF8.to_string()),
        };
        if let Some(resp) = resp {
            writeln!(out, "{resp}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// The context for each HTTP request, built from the settings as they are now: a changed
/// `osh.toml` applies to the next request without restarting the service. The variables still
/// win over the file, as for stdio.
fn ctx_source(
    dir: PathBuf,
    env: impl Fn(&str) -> Option<std::ffi::OsString> + Send + Sync + 'static,
) -> impl Fn() -> Ctx + Send + Sync {
    move || {
        let snap = settings::load(&dir, &env);
        let gateway = snap
            .rows
            .iter()
            .find(|r| r.def.key == "mcp.server_gateway")
            .and_then(|r| match &r.value {
                settings::Value::Str(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            });
        build_ctx(gateway, &snap).0
    }
}

/// A line to print when the server listens beyond this machine, `None` for loopback.
fn exposure_note(ip: IpAddr) -> Option<String> {
    (!ip.is_loopback()).then(|| {
        format!(
            "osh: mcp: listening on {ip}, which other machines may reach; every request needs the \
             bearer token (`osh mcp token`), and the address belongs behind a VPN or tunnel"
        )
    })
}

/// Claim the state file, then listen. A taken port gives back the claim, so the dashboard
/// does not show a server that never started; a live server elsewhere is never disturbed.
fn bind_and_claim(dir: &Path, ip: IpAddr, port: u16) -> Result<TcpListener, String> {
    let wanted = SocketAddr::new(ip, port);
    state::claim(dir, &wanted.to_string())?;
    TcpListener::bind(wanted).map_err(|e| {
        state::release(dir);
        format!("cannot listen on {wanted}: {e}")
    })
}

fn serve_cmd() -> i32 {
    let dir = osh_core::assets::data_dir();
    if let Err(e) = token::ensure(&dir) {
        eprintln!("osh: mcp: cannot create the token file: {e}");
        return 1;
    }
    settings::init(&dir);
    let snap = settings::snapshot();
    for p in snap
        .problems
        .iter()
        .filter(|p| p.key.as_deref().is_none_or(|k| k.starts_with("mcp.")))
    {
        eprintln!("osh: mcp: {}", p.msg);
    }
    let ip = settings::mcp_server_bind()
        .parse::<IpAddr>()
        .unwrap_or_else(|_| "127.0.0.1".parse().expect("a literal address"));
    let port = settings::mcp_server_port();
    let listener = match bind_and_claim(&dir, ip, port) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("osh: mcp: {e}");
            return 1;
        }
    };
    if let Some(note) = exposure_note(ip) {
        eprintln!("{note}");
    }
    let listen = SocketAddr::new(ip, port).to_string();
    let registry = Arc::new(state::Registry::new(
        dir.clone(),
        listen.clone(),
        env!("CARGO_PKG_VERSION").into(),
        settings::mcp_client_idle,
    ));
    registry.flush();
    let ticker = registry.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        settings::reload_if_changed();
        ticker.flush();
    });
    eprintln!("osh: mcp: listening on http://{listen}/mcp");
    let result = http::serve(
        listener,
        dir.clone(),
        Arc::new(ctx_source(dir, |k| std::env::var_os(k))),
        registry.clone(),
        http::Limits::default(),
        Arc::new(AtomicBool::new(false)),
    );
    registry.remove();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("osh: mcp: {e}");
            1
        }
    }
}

/// One line for `osh mcp status`.
fn status_line(svc: crate::mcpsvc::Svc, st: Option<&state::State>) -> String {
    use crate::mcpsvc::Svc;
    match (svc, st) {
        (Svc::Running, Some(s)) => format!(
            "MCP server: on, listening on {}, {} client{}",
            s.listen,
            s.clients.len(),
            if s.clients.len() == 1 { "" } else { "s" }
        ),
        (Svc::Running, None) => "MCP server: on".into(),
        (Svc::External, Some(s)) => format!(
            "MCP server: on, listening on {}, {} client{} (started outside the service: stop it where it runs)",
            s.listen,
            s.clients.len(),
            if s.clients.len() == 1 { "" } else { "s" }
        ),
        (Svc::External, None) => "MCP server: on (started outside the service)".into(),
        (Svc::Stopped, _) => {
            "MCP server: off (the service is installed; `osh mcp start` turns it on)".into()
        }
        (Svc::NotInstalled, _) => {
            "MCP server: off (not installed; `osh mcp start` installs and starts it)".into()
        }
    }
}

/// `osh mcp start|stop|remove|status`: steps through the same consent as the gateway commands.
fn service_cmd(m: &Mode) -> i32 {
    use crate::mcpsvc::{self, Svc};
    use std::io::IsTerminal;
    let h = crate::platform::RealHost;
    let f = crate::platform::gather(&h);
    let p = crate::gateway::Paths::detect();
    let svc = mcpsvc::status(f.os, &h, &p);
    let (verb, yes) = match m {
        Mode::Status => {
            println!("{}", status_line(svc, state::read(&p.data).as_ref()));
            return 0;
        }
        Mode::Service(v, y) => (*v, *y),
        _ => return 2,
    };
    let steps = match (verb, svc) {
        (Verb::Start, Svc::Running | Svc::External) => {
            println!("The MCP server is already on.");
            return 0;
        }
        (Verb::Stop, Svc::External) => {
            eprintln!(
                "The MCP server was started outside the service (osh mcp serve): stop it where it runs."
            );
            return 1;
        }
        (Verb::Start, Svc::NotInstalled) => mcpsvc::install_steps(&f, &p),
        (Verb::Start, Svc::Stopped) => mcpsvc::start_steps(&f, &p),
        (Verb::Stop, Svc::NotInstalled) => {
            println!("The MCP server is not installed.");
            return 0;
        }
        (Verb::Stop, _) => mcpsvc::stop_steps(&f, &p),
        (Verb::Remove, Svc::NotInstalled | Svc::External) => {
            println!("The MCP server is not installed.");
            return 0;
        }
        (Verb::Remove, _) => mcpsvc::remove_steps(&f, &p),
    };
    let o = crate::setup::Options {
        yes,
        ..Default::default()
    };
    i32::from(!crate::setup::run_steps(
        &h,
        steps,
        &o,
        std::io::stdin().is_terminal(),
    ))
}

fn token_cmd() -> i32 {
    let dir = osh_core::assets::data_dir();
    if let Err(e) = token::ensure(&dir) {
        eprintln!("osh: mcp: cannot create the token file: {e}");
        return 1;
    }
    match token::read(&dir) {
        Some(t) => {
            println!("{t}");
            0
        }
        None => {
            eprintln!("osh: mcp: the token file is empty");
            1
        }
    }
}

pub fn run(args: &[String]) -> i32 {
    let gateway = match mode(args) {
        Ok(Mode::Stdio(g)) => g,
        Ok(Mode::Serve) => return serve_cmd(),
        Ok(Mode::Token) => return token_cmd(),
        Ok(m @ (Mode::Status | Mode::Service(..))) => return service_cmd(&m),
        Err(e) => {
            eprintln!("osh: {e}\n{USAGE}");
            return 2;
        }
    };
    let dir = osh_core::assets::data_dir();
    let snap = osh_core::settings::load(&dir, &|k| std::env::var_os(k));
    let (ctx, warnings) = build_ctx(gateway, &snap);
    for line in warnings {
        eprintln!("{line}");
    }
    match serve(std::io::stdin().lock(), std::io::stdout().lock(), &ctx) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("osh: mcp: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn ctx() -> Ctx {
        Ctx {
            dir: "/nonexistent".into(),
            gateway: None,
            allow_share: false,
            refuse_risky_share: true,
            repo_vars: Some(Vec::new()),
            share_roots: Some(Vec::new()),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("osh-mcpcmd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    #[test]
    fn mode_accepts_serve_and_token_and_keeps_stdio() {
        assert_eq!(mode(&a(&[])), Ok(Mode::Stdio(None)));
        assert_eq!(
            mode(&a(&["--gateway", "x"])),
            Ok(Mode::Stdio(Some("x".into())))
        );
        assert_eq!(mode(&a(&["serve"])), Ok(Mode::Serve));
        assert_eq!(mode(&a(&["token"])), Ok(Mode::Token));
        assert!(mode(&a(&["serve", "--bogus"])).is_err());
        assert!(mode(&a(&["token", "x"])).is_err());
        assert!(mode(&a(&["--gateway"])).is_err());
    }

    #[test]
    fn mode_accepts_the_service_verbs_with_an_optional_yes() {
        assert_eq!(mode(&a(&["start"])), Ok(Mode::Service(Verb::Start, false)));
        assert_eq!(
            mode(&a(&["stop", "--yes"])),
            Ok(Mode::Service(Verb::Stop, true))
        );
        assert_eq!(
            mode(&a(&["remove", "--yes"])),
            Ok(Mode::Service(Verb::Remove, true))
        );
        assert_eq!(mode(&a(&["status"])), Ok(Mode::Status));
        assert!(mode(&a(&["start", "--bogus"])).is_err());
        assert!(mode(&a(&["status", "--yes"])).is_err());
    }

    #[test]
    fn the_status_line_says_on_off_or_not_installed() {
        use crate::mcpsvc::Svc;
        let st = osh_core::mcpserver::state::State {
            pid: 1,
            listen: "127.0.0.1:17800".into(),
            started: 0,
            version: "v".into(),
            clients: vec![],
        };
        assert_eq!(
            status_line(Svc::Running, Some(&st)),
            "MCP server: on, listening on 127.0.0.1:17800, 0 clients"
        );
        assert_eq!(
            status_line(Svc::External, Some(&st)),
            "MCP server: on, listening on 127.0.0.1:17800, 0 clients (started outside the service: stop it where it runs)"
        );
        assert_eq!(
            status_line(Svc::Stopped, None),
            "MCP server: off (the service is installed; `osh mcp start` turns it on)"
        );
        assert_eq!(
            status_line(Svc::NotInstalled, None),
            "MCP server: off (not installed; `osh mcp start` installs and starts it)"
        );
    }

    #[test]
    fn the_server_context_follows_settings_changes_without_a_restart() {
        let d = tmp("ctx");
        let file = d.0.join("osh.toml");
        let src = ctx_source(d.0.clone(), |_| None);
        assert!(!src().allow_share);
        assert_eq!(src().gateway, None);
        std::fs::write(
            &file,
            "[mcp]\nallow_share = true\nserver_gateway = \"openshell-docker\"\nrepo_vars = [\"MY_FORGE_VAR\"]\n",
        )
        .unwrap();
        let c = src();
        assert!(c.allow_share);
        assert_eq!(c.gateway.as_deref(), Some("openshell-docker"));
        assert_eq!(c.repo_vars, Some(vec!["MY_FORGE_VAR".to_string()]));
        // The variables still win over the file, as for stdio `osh mcp`.
        let forced = ctx_source(d.0.clone(), |k| {
            (k == "OSH_MCP_REPO_VARS").then(|| "ONLY_THIS".into())
        });
        assert_eq!(forced().repo_vars, Some(vec!["ONLY_THIS".to_string()]));
        // Share and repo limits are never absent: `None` would mean no check at all.
        assert!(c.share_roots.is_some() && c.refuse_risky_share);
    }

    #[test]
    fn a_taken_port_is_a_clear_error_and_leaves_no_claim_behind() {
        let d = tmp("port");
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let e = bind_and_claim(&d.0, "127.0.0.1".parse().unwrap(), port).unwrap_err();
        assert!(e.contains(&format!("127.0.0.1:{port}")), "{e}");
        assert!(
            osh_core::mcpserver::state::read(&d.0).is_none(),
            "a claim this server could not use is released"
        );
        assert!(!osh_core::mcpserver::state::path(&d.0).exists());
    }

    #[test]
    fn a_live_other_server_keeps_its_state_file() {
        let d = tmp("other");
        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap().to_string();
        let theirs = format!(
            r#"{{"pid":{},"listen":"{addr}","started":1,"version":"v","clients":[]}}"#,
            other.id()
        );
        std::fs::write(osh_core::mcpserver::state::path(&d.0), &theirs).unwrap();
        let e = bind_and_claim(&d.0, "127.0.0.1".parse().unwrap(), 0).unwrap_err();
        assert!(e.contains("already running") && e.contains(&addr), "{e}");
        assert_eq!(
            std::fs::read_to_string(osh_core::mcpserver::state::path(&d.0)).unwrap(),
            theirs
        );
        let _ = other.kill();
        let _ = other.wait();
    }

    #[test]
    fn only_a_loopback_bind_is_quiet() {
        assert!(exposure_note("127.0.0.1".parse().unwrap()).is_none());
        assert!(exposure_note("::1".parse().unwrap()).is_none());
        let n = exposure_note("10.1.2.3".parse().unwrap()).unwrap();
        assert!(n.contains("10.1.2.3") && n.contains("token"), "{n}");
        assert!(exposure_note("0.0.0.0".parse().unwrap()).is_some());
    }

    #[test]
    fn parse_gateway() {
        assert_eq!(parse(&a(&[])), Ok(None));
        assert_eq!(parse(&a(&["--gateway", "g"])), Ok(Some("g".into())));
        assert!(parse(&a(&["--gateway"])).is_err());
        assert!(parse(&a(&["--allow-share"])).is_err());
    }

    fn strs(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_servers_limits_come_from_the_resolved_settings() {
        let d = std::env::temp_dir().join(format!("osh-mcp-set-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("osh.toml"),
            "[mcp]\nallow_share = true\nshare_roots = [\"/from/file\"]\nrepo_vars = [\"FILE_VAR\"]\n",
        )
        .unwrap();
        let none = |_: &str| None;
        let l = limits(&osh_core::settings::load(&d, &none));
        assert!(l.0);
        assert_eq!((l.1, l.2), (strs(&["/from/file"]), strs(&["FILE_VAR"])));
        let env = |k: &str| match k {
            "OSH_MCP_SHARE_ROOT" => Some(std::ffi::OsString::from("/from/env")),
            "OSH_MCP_REPO_VARS" => Some(std::ffi::OsString::from("ENV_VAR")),
            _ => None,
        };
        let l = limits(&osh_core::settings::load(&d, &env));
        assert_eq!(
            (l.1, l.2),
            (strs(&["/from/env"]), strs(&["ENV_VAR"])),
            "env replaces the file"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn share_roots_keep_only_existing_directories() {
        assert_eq!(share_roots(&[]), (Vec::new(), Vec::new()));
        let base = std::env::temp_dir().join(format!("osh-mcp-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in ["a", "b"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::write(base.join("file"), "").unwrap();
        std::os::unix::fs::symlink(base.join("b"), base.join("to-b")).unwrap();
        let entries: Vec<String> = [
            base.join("a"),
            PathBuf::from("rel/dir"),
            base.join("missing"),
            base.join("file"),
            base.join("to-b"),
        ]
        .iter()
        .map(|p| p.to_str().unwrap().to_string())
        .collect();
        let (roots, dropped) = share_roots(&entries);
        let real = |p: &str| std::fs::canonicalize(base.join(p)).unwrap();
        assert_eq!(roots, [real("a"), real("b")]);
        assert_eq!(dropped.len(), 3, "{dropped:?}");
        for (line, bad) in dropped.iter().zip(&entries[1..4]) {
            assert!(
                line.contains("mcp.share_roots") && line.contains(bad.as_str()),
                "{line}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_filesystem_root_is_not_a_share_root() {
        let (roots, dropped) = share_roots(&strs(&["/"]));
        assert!(roots.is_empty(), "{roots:?}");
        assert!(
            dropped[0].contains("mcp.share_roots") && dropped[0].contains("whole disk"),
            "{dropped:?}"
        );
    }

    #[test]
    fn the_context_osh_mcp_runs_with_never_leaves_shares_or_repo_vars_unchecked() {
        let d = std::env::temp_dir().join(format!("osh-mcp-ctx-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let snap = osh_core::settings::load(&d, &|_| None);
        let (c, _) = build_ctx(None, &snap);
        assert_eq!(
            c.share_roots,
            Some(Vec::new()),
            "None would mean every folder is allowed"
        );
        assert_eq!(c.repo_vars, Some(Vec::new()));
        assert!(!c.allow_share && c.refuse_risky_share);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_server_context_is_strict_and_carries_the_binarys_version() {
        let c = server_ctx(None, false, Vec::new());
        assert!(c.refuse_risky_share && !c.allow_share);
        assert_eq!(c.repo_vars, Some(Vec::new()));
        assert_eq!(c.version, env!("CARGO_PKG_VERSION"));
        let c = server_ctx(Some("g".into()), true, strs(&["A"]));
        assert!(c.allow_share && c.refuse_risky_share);
        assert_eq!(c.repo_vars, Some(vec!["A".to_string()]));
        let r = mcp::handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#, &c).unwrap();
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(
            v["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn serve_answers_requests_skips_notifications_and_survives_garbage() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\
{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\xff\xfe\n\n\
{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}";
        let mut out = Vec::new();
        serve(&input[..], &mut out, &ctx()).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].contains("\"id\":1"));
        assert!(lines[1].contains("-32700"));
        assert!(lines[2].contains("\"id\":2"));
    }

    #[test]
    fn invalid_utf8_inside_a_valid_request_is_a_parse_error_not_a_call() {
        // Lossy decoding would turn this into a valid request with U+FFFD in it and answer
        // id 5; strict decoding answers -32700 with a null id.
        let mut input = b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"ping\",\"x\":\"".to_vec();
        input.extend_from_slice(b"\xc3\x28\"}\n");
        let mut out = Vec::new();
        serve(&input[..], &mut out, &ctx()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["error"]["code"], -32700, "{v}");
        assert!(v["id"].is_null(), "{v}");
    }
}
