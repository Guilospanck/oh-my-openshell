//! `osh mcp [--gateway G]`: the stdio loop around `osh_core::mcp::handle`. Stdout carries
//! protocol lines only; diagnostics go to stderr and never include request bodies.

use osh_core::mcp;
use osh_core::ops::Ctx;
use std::io::{BufRead, Write};

const USAGE: &str = "usage: osh mcp [--gateway G]\n\
Serves MCP over stdio (newline-delimited JSON-RPC). Set OSH_MCP_ALLOW_SHARE=1 in the \
server's environment to let specs mount host folders, and OSH_MCP_REPO_VARS to the \
comma-separated names of the variables a spec's repo.env_var may name.";

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

/// Only the operator's environment can enable sharing; no flag or tool argument can.
fn share_allowed(var: Option<&str>) -> bool {
    var == Some("1")
}

/// The variables a spec may name in `repo.env_var`, from `OSH_MCP_REPO_VARS`: comma-separated,
/// trimmed, empty entries dropped. Unset or empty gives an empty list, which refuses every
/// repo grant.
fn repo_vars(var: Option<&str>) -> Vec<String> {
    var.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
        .collect()
}

/// The server's context from the operator's two variables. An agent writes the specs, so a
/// risky share folder is refused, not only noted. The version is this binary's (the one
/// `osh --version` prints and the release bumps), not osh-core's.
fn server_ctx(gateway: Option<String>, share: Option<&str>, vars: Option<&str>) -> Ctx {
    Ctx {
        dir: osh_core::assets::data_dir(),
        gateway,
        allow_share: share_allowed(share),
        refuse_risky_share: true,
        repo_vars: Some(repo_vars(vars)),
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
    let ctx = server_ctx(
        gateway,
        std::env::var("OSH_MCP_ALLOW_SHARE").ok().as_deref(),
        std::env::var("OSH_MCP_REPO_VARS").ok().as_deref(),
    );
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

    #[test]
    fn share_only_from_env_value_one() {
        assert!(share_allowed(Some("1")));
        for v in [None, Some(""), Some("0"), Some("true")] {
            assert!(!share_allowed(v));
        }
    }

    #[test]
    fn repo_vars_from_the_operators_list() {
        assert!(repo_vars(None).is_empty());
        assert!(repo_vars(Some("")).is_empty());
        assert!(repo_vars(Some(" , ,")).is_empty());
        assert_eq!(repo_vars(Some(" A_KEY, B ,,")), ["A_KEY", "B"]);
    }

    #[test]
    fn the_server_context_is_strict_and_carries_the_binarys_version() {
        let c = server_ctx(None, None, None);
        assert!(c.refuse_risky_share && !c.allow_share);
        assert_eq!(c.repo_vars, Some(Vec::new()));
        assert_eq!(c.version, env!("CARGO_PKG_VERSION"));
        let c = server_ctx(Some("g".into()), Some("1"), Some("A"));
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
