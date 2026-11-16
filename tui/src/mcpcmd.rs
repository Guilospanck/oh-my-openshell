//! `osh mcp [--gateway G]`: the stdio loop around `osh_core::mcp::handle`. Stdout carries
//! protocol lines only; diagnostics go to stderr and never include request bodies.

use osh_core::mcp;
use osh_core::ops::Ctx;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: osh mcp [--gateway G]\n\
Serves MCP over stdio (newline-delimited JSON-RPC). What agents may do is set in osh.toml \
(see `osh config`): mcp.allow_share lets specs mount host folders inside the directories in \
mcp.share_roots, and mcp.repo_vars lists the variables a spec's repo.env_var may name. \
OSH_MCP_ALLOW_SHARE=1, OSH_MCP_SHARE_ROOT (separated like PATH) and OSH_MCP_REPO_VARS \
(comma-separated) in the server's environment override them.";

/// The answer to a line that is not UTF-8: a parse error with no id, as for bad JSON.
const NOT_UTF8: &str =
    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error: not UTF-8"}}"#;

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

pub fn run(args: &[String]) -> i32 {
    let gateway = match parse(args) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("osh: {e}\n{USAGE}");
            return 2;
        }
    };
    let dir = osh_core::assets::data_dir();
    let snap = osh_core::settings::load(&dir, &|k| std::env::var_os(k));
    for p in snap
        .problems
        .iter()
        .filter(|p| p.key.as_deref().is_none_or(|k| k.starts_with("mcp.")))
    {
        eprintln!("osh: mcp: {}", p.msg);
    }
    let (allow_share, root_entries, vars) = limits(&snap);
    let (roots, dropped) = share_roots(&root_entries);
    for line in dropped {
        eprintln!("{line}");
    }
    let ctx = Ctx {
        share_roots: Some(roots),
        ..server_ctx(gateway, allow_share, vars)
    };
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
