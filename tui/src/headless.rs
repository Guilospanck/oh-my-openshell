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

/// Where a spec comes from; read after parsing so a bad file is a runtime error, not usage.
#[derive(Debug, PartialEq)]
enum SpecSrc {
    File(String),
    Stdin,
}

impl SpecSrc {
    fn of(arg: &str) -> Self {
        if arg == "-" {
            SpecSrc::Stdin
        } else {
            SpecSrc::File(arg.into())
        }
    }

    fn label(&self) -> &str {
        match self {
            SpecSrc::File(p) => p,
            SpecSrc::Stdin => "stdin",
        }
    }

    /// Read and parse the spec; errors name the source and never echo its contents.
    fn load(&self, stdin: &dyn Fn() -> Result<String, String>) -> Result<Value, String> {
        let text = match self {
            SpecSrc::Stdin => stdin()?,
            SpecSrc::File(p) => {
                std::fs::read_to_string(p).map_err(|e| format!("cannot read {p}: {e}"))?
            }
        };
        if text.trim().is_empty() {
            return Err(format!("{} is empty: expected a JSON spec", self.label()));
        }
        serde_json::from_str(&text).map_err(|e| format!("invalid JSON in {}: {e}", self.label()))
    }
}

#[derive(Debug, PartialEq)]
enum Command {
    List {
        gateway: Option<String>,
    },
    Create {
        spec: SpecSrc,
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
        spec: SpecSrc,
        gateway: Option<String>,
        allow_share: bool,
    },
}

fn value<'a>(it: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a String, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}

/// Parse argv; every error here is a usage mistake (stderr, exit 2).
fn parse(args: &[String]) -> Result<Command, String> {
    let (verb, rest) = match args.first().map(String::as_str) {
        Some("sandbox") => match args.get(1) {
            Some(v) => (v.as_str(), &args[2..]),
            None => return Err("sandbox needs list, create, delete or exec".into()),
        },
        Some("apply") => ("apply", &args[1..]),
        _ => return Err("expected sandbox or apply".into()),
    };
    let mut gateway = None;
    let mut spec: Option<SpecSrc> = None;
    let (mut dry_run, mut allow_share) = (false, false);
    let mut name: Option<String> = None;
    let mut argv: Option<Vec<String>> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--gateway" => gateway = Some(value(&mut it, "--gateway")?.clone()),
            "--spec" if verb == "create" => spec = Some(SpecSrc::of(value(&mut it, "--spec")?)),
            "-f" if verb == "apply" => spec = Some(SpecSrc::of(value(&mut it, "-f")?)),
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
        "create" => Ok(Command::Create {
            spec: spec.ok_or("create needs --spec FILE|-")?,
            gateway,
            dry_run,
            allow_share,
        }),
        "apply" => Ok(Command::Apply {
            spec: spec.ok_or("apply needs -f FILE|-")?,
            gateway,
            allow_share,
        }),
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

/// The process exit code: `exec` reports the command's own code when the call ran (1 if the
/// reply carries none); every other operation, and a failed call, uses the envelope's code.
fn exit_for(exec: bool, out: &Value, code: i32) -> i32 {
    if exec && code == 0 {
        return out["exit_code"].as_i64().map_or(1, |c| c as i32);
    }
    code
}

fn real_stdin() -> Result<String, String> {
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    Ok(s)
}

/// Run the headless command; 0 ok, 1 the operation failed (JSON on stdout), 2 usage error
/// (message on stderr). For `exec` the process exit code is the command's own when the call ran.
pub fn headless(args: &[String]) -> i32 {
    let cmd = match parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("osh: {e}\n{USAGE}");
            return 2;
        }
    };
    let dir = osh_core::assets::data_dir();
    // The user names share folders and repo variables here, so a risky folder is only noted
    // and any variable may be named.
    let ctx = |gateway: Option<String>, allow_share: bool| Ctx {
        dir: dir.clone(),
        gateway,
        allow_share,
        refuse_risky_share: false,
        repo_vars: None,
        share_roots: None,
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let mut exec = false;
    let result = match cmd {
        Command::List { gateway } => ops::sandbox_list(&ctx(gateway, false)),
        Command::Create {
            spec,
            gateway,
            dry_run,
            allow_share,
        } => spec
            .load(&real_stdin)
            .and_then(|v| ops::sandbox_create(&ctx(gateway, allow_share), &v, dry_run)),
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
        } => spec
            .load(&real_stdin)
            .and_then(|v| ops::apply(&ctx(gateway, allow_share), &v)),
    };
    let (out, code) = ops::envelope(result);
    println!("{out}");
    exit_for(exec, &out, code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn a(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }
    fn p(s: &str) -> Result<Command, String> {
        parse(&a(s))
    }
    fn err(s: &str) -> String {
        p(s).unwrap_err()
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
        assert!(err("sandbox delete").contains("needs a NAME"));
    }

    #[test]
    fn create_flags_and_spec_sources() {
        assert_eq!(
            p("sandbox create --spec - --dry-run --allow-share --gateway g"),
            Ok(Command::Create {
                spec: SpecSrc::Stdin,
                gateway: Some("g".into()),
                dry_run: true,
                allow_share: true
            })
        );
        assert_eq!(
            p("sandbox create --spec a.json"),
            Ok(Command::Create {
                spec: SpecSrc::File("a.json".into()),
                gateway: None,
                dry_run: false,
                allow_share: false
            })
        );
        assert!(err("sandbox create --spec").contains("--spec needs a value"));
        assert!(err("sandbox create").contains("needs --spec"));
        assert!(err("sandbox create --spec - --bogus").contains("unknown option --bogus"));
    }

    #[test]
    fn apply_forms() {
        assert_eq!(
            p("apply -f - --allow-share"),
            Ok(Command::Apply {
                spec: SpecSrc::Stdin,
                gateway: None,
                allow_share: true
            })
        );
        assert!(err("apply").contains("needs -f"));
        assert!(err("apply --dry-run -f -").contains("unknown option --dry-run"));
    }

    #[test]
    fn exec_needs_double_dash_and_keeps_later_flags() {
        assert_eq!(
            p("sandbox exec w1 -- ls -la --color"),
            Ok(Command::Exec {
                name: "w1".into(),
                gateway: None,
                argv: a("ls -la --color")
            })
        );
        assert_eq!(
            p("sandbox exec w1 -- cmd --gateway x"),
            Ok(Command::Exec {
                name: "w1".into(),
                gateway: None,
                argv: a("cmd --gateway x")
            })
        );
        assert!(err("sandbox exec w1 ls").contains("unexpected argument ls"));
        assert!(err("sandbox exec w1 --").contains("needs `-- CMD...`"));
        assert!(err("sandbox exec -- ls").contains("needs a NAME"));
    }

    #[test]
    fn usage_errors_for_verbs() {
        assert!(err("sandbox frob").contains("unknown sandbox command frob"));
        assert!(err("sandbox").contains("sandbox needs"));
    }

    #[test]
    fn spec_read_failures_are_runtime_errors_naming_the_source() {
        let no = || Ok(String::new());
        let e = SpecSrc::File("/nonexistent/zz.json".into())
            .load(&no)
            .unwrap_err();
        assert!(e.starts_with("cannot read /nonexistent/zz.json"), "{e}");
        let e = SpecSrc::Stdin
            .load(&|| Ok("not json LEAKCHECK".into()))
            .unwrap_err();
        assert!(e.starts_with("invalid JSON in stdin"), "{e}");
        assert!(!e.contains("LEAKCHECK"), "{e}");
        let e = SpecSrc::Stdin.load(&no).unwrap_err();
        assert_eq!(e, "stdin is empty: expected a JSON spec");
        assert_eq!(
            SpecSrc::Stdin.load(&|| Ok(r#"{"name":"x"}"#.into())),
            Ok(json!({"name":"x"}))
        );
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_for(true, &json!({"exit_code":0}), 0), 0);
        assert_eq!(exit_for(true, &json!({"exit_code":7}), 0), 7);
        assert_eq!(exit_for(true, &json!({"stdout":""}), 0), 1);
        assert_eq!(exit_for(true, &json!({"ok":false,"error":"x"}), 1), 1);
        assert_eq!(exit_for(false, &json!({"exit_code":7}), 0), 0);
    }
}
