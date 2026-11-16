//! `osh sandbox ...` and `osh apply`: argument parsing for the JSON front end. All logic lives
//! in `osh_core::ops`; this file only turns argv into a `Command` and prints the envelope.

use osh_core::ops::{self, Ctx};
use serde_json::Value;
use std::io::Read;

const USAGE: &str = "\
usage:
  osh sandbox list [--gateway G]
  osh sandbox create --spec FILE|- [--gateway G] [--dry-run] [--allow-share]
  osh sandbox delete NAME [--gateway G]
  osh sandbox exec NAME [--gateway G] -- CMD...
  osh apply -f FILE|- [--gateway G] [--allow-share]
A spec is JSON, e.g. {\"name\":\"w1\",\"repo\":{\"url\":\"https://github.com/o/r\",\"env_var\":\"GH_TOKEN_NAME\"}}.
Output is one JSON line on stdout; failures print {\"ok\":false,\"error\":...} and exit 1.";

#[derive(Debug, PartialEq)]
enum Command {
    List {
        gateway: Option<String>,
    },
    Create {
        spec: Value,
        gateway: Option<String>,
        dry_run: bool,
        allow_share: bool,
    },
    Delete {
        name: String,
        gateway: Option<String>,
    },
    Exec {
        name: String,
        gateway: Option<String>,
        argv: Vec<String>,
    },
    Apply {
        spec: Value,
        gateway: Option<String>,
        allow_share: bool,
    },
}

/// Read a spec from a file, or from `stdin` for `-`.
fn read_spec(path: &str, stdin: &dyn Fn() -> Result<String, String>) -> Result<Value, String> {
    let text = if path == "-" {
        stdin()?
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?
    };
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON in {path}: {e}"))
}

fn value<'a>(it: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a String, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn parse(args: &[String]) -> Result<Command, String> {
    parse_with(args, &|| {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        Ok(s)
    })
}

fn parse_with(
    args: &[String],
    stdin: &dyn Fn() -> Result<String, String>,
) -> Result<Command, String> {
    let (verb, rest) = match args.first().map(String::as_str) {
        Some("sandbox") => match args.get(1) {
            Some(v) => (v.as_str(), &args[2..]),
            None => return Err("sandbox needs list, create, delete or exec".into()),
        },
        Some("apply") => ("apply", &args[1..]),
        _ => return Err("expected sandbox or apply".into()),
    };
    let mut gateway = None;
    let mut spec_path: Option<String> = None;
    let (mut dry_run, mut allow_share) = (false, false);
    let mut name: Option<String> = None;
    let mut argv: Option<Vec<String>> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--gateway" => gateway = Some(value(&mut it, "--gateway")?.clone()),
            "--spec" if verb == "create" => spec_path = Some(value(&mut it, "--spec")?.clone()),
            "-f" if verb == "apply" => spec_path = Some(value(&mut it, "-f")?.clone()),
            "--dry-run" if verb == "create" => dry_run = true,
            "--allow-share" if verb == "create" || verb == "apply" => allow_share = true,
            "--" if verb == "exec" => argv = Some(it.by_ref().cloned().collect()),
            f if f.starts_with('-') && f != "-" => return Err(format!("unknown option {f}")),
            n if matches!(verb, "delete" | "exec") && name.is_none() => name = Some(n.into()),
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    match verb {
        "list" => Ok(Command::List { gateway }),
        "create" => {
            let p = spec_path.ok_or("create needs --spec FILE|-")?;
            Ok(Command::Create {
                spec: read_spec(&p, stdin)?,
                gateway,
                dry_run,
                allow_share,
            })
        }
        "apply" => {
            let p = spec_path.ok_or("apply needs -f FILE|-")?;
            Ok(Command::Apply {
                spec: read_spec(&p, stdin)?,
                gateway,
                allow_share,
            })
        }
        "delete" => Ok(Command::Delete {
            name: name.ok_or("delete needs a NAME")?,
            gateway,
        }),
        "exec" => {
            let name = name.ok_or("exec needs a NAME")?;
            match argv {
                Some(argv) if !argv.is_empty() => Ok(Command::Exec {
                    name,
                    gateway,
                    argv,
                }),
                _ => Err("exec needs `-- CMD...` after the name".into()),
            }
        }
        v => Err(format!("unknown sandbox command {v}")),
    }
}

/// Run the headless command; 0 ok, 1 the operation failed, 2 usage error. For `exec` the
/// process exit code is the command's own when the call ran.
pub fn headless(args: &[String]) -> i32 {
    let cmd = match parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("osh: {e}\n{USAGE}");
            return 2;
        }
    };
    let dir = osh_core::assets::data_dir();
    let ctx = |gateway: Option<String>, allow_share: bool| Ctx {
        dir: dir.clone(),
        gateway,
        allow_share,
    };
    let mut exec = false;
    let result = match cmd {
        Command::List { gateway } => ops::sandbox_list(&ctx(gateway, false)),
        Command::Create {
            spec,
            gateway,
            dry_run,
            allow_share,
        } => ops::sandbox_create(&ctx(gateway, allow_share), &spec, dry_run),
        Command::Delete { name, gateway } => ops::sandbox_delete(&ctx(gateway, false), &name),
        Command::Exec {
            name,
            gateway,
            argv,
        } => {
            exec = true;
            ops::sandbox_exec(&ctx(gateway, false), &name, &argv)
        }
        Command::Apply {
            spec,
            gateway,
            allow_share,
        } => ops::apply(&ctx(gateway, allow_share), &spec),
    };
    let (out, code) = ops::envelope(result);
    println!("{out}");
    if exec && code == 0 {
        return out["exit_code"].as_i64().map_or(1, |c| c as i32);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }
    fn p(s: &str) -> Result<Command, String> {
        parse_with(&a(s), &|| Ok(r#"{"name":"x"}"#.to_string()))
    }

    #[test]
    fn list_and_delete() {
        assert_eq!(p("sandbox list"), Ok(Command::List { gateway: None }));
        assert_eq!(
            p("sandbox list --gateway g1"),
            Ok(Command::List {
                gateway: Some("g1".into())
            })
        );
        assert_eq!(
            p("sandbox delete w1 --gateway g"),
            Ok(Command::Delete {
                name: "w1".into(),
                gateway: Some("g".into())
            })
        );
        assert!(p("sandbox delete").is_err());
    }

    #[test]
    fn create_reads_stdin_and_flags() {
        let c = p("sandbox create --spec - --dry-run --allow-share --gateway g").unwrap();
        assert_eq!(
            c,
            Command::Create {
                spec: serde_json::json!({"name":"x"}),
                gateway: Some("g".into()),
                dry_run: true,
                allow_share: true
            }
        );
        assert!(p("sandbox create --spec").is_err());
        assert!(p("sandbox create").is_err());
        assert!(p("sandbox create --spec - --bogus").is_err());
    }

    #[test]
    fn apply_forms() {
        assert_eq!(
            p("apply -f - --allow-share"),
            Ok(Command::Apply {
                spec: serde_json::json!({"name":"x"}),
                gateway: None,
                allow_share: true
            })
        );
        assert!(p("apply").is_err());
        assert!(p("apply --dry-run -f -").is_err());
    }

    #[test]
    fn exec_needs_double_dash() {
        assert_eq!(
            p("sandbox exec w1 -- ls -la --color"),
            Ok(Command::Exec {
                name: "w1".into(),
                gateway: None,
                argv: a("ls -la --color")
            })
        );
        assert!(p("sandbox exec w1 ls").is_err());
        assert!(p("sandbox exec w1 --").is_err());
        assert!(p("sandbox exec -- ls").is_err());
    }

    #[test]
    fn unknown_verb_and_bad_spec() {
        assert!(p("sandbox frob").is_err());
        assert!(p("sandbox").is_err());
        assert!(
            parse_with(&a("sandbox create --spec /nonexistent/zz.json"), &|| Ok(
                String::new()
            ))
            .is_err()
        );
        assert!(parse_with(&a("apply -f -"), &|| Ok("not json".into())).is_err());
    }
}
