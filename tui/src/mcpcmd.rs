//! `osh mcp [--gateway G]`: the stdio loop around `osh_core::mcp::handle`. Stdout carries
//! protocol lines only; diagnostics go to stderr and never include request bodies.

use osh_core::mcp;
use osh_core::ops::Ctx;
use std::io::{BufRead, Write};

const USAGE: &str = "usage: osh mcp [--gateway G]\n\
Serves MCP over stdio (newline-delimited JSON-RPC). Set OSH_MCP_ALLOW_SHARE=1 in the \
server's environment to let specs mount host folders.";

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

/// Answer each line until EOF. A line that is not UTF-8 is a parse error, not the end.
pub fn serve(input: impl BufRead, mut out: impl Write, ctx: &Ctx) -> std::io::Result<()> {
    for raw in input.split(b'\n') {
        let raw = raw?;
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(resp) = mcp::handle(line, ctx) {
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
    let ctx = Ctx {
        dir: osh_core::assets::data_dir(),
        gateway,
        allow_share: share_allowed(std::env::var("OSH_MCP_ALLOW_SHARE").ok().as_deref()),
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
    fn serve_answers_requests_skips_notifications_and_survives_garbage() {
        let ctx = Ctx {
            dir: "/nonexistent".into(),
            gateway: None,
            allow_share: false,
        };
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\
{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\xff\xfe\n\n\
{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}";
        let mut out = Vec::new();
        serve(&input[..], &mut out, &ctx).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].contains("\"id\":1"));
        assert!(lines[1].contains("-32700"));
        assert!(lines[2].contains("\"id\":2"));
    }
}
