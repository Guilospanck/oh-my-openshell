//! Sandbox operations for a headless front end, each answering in JSON. `envelope` is the
//! output contract: the value and exit code 0, or `{"ok":false,"error":...}` and 1.

use crate::sandbox::{self, CreateCtx, SandboxSpec, Share};
use crate::{cli, ghrepo};
use serde_json::{json, Value};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What every operation runs against.
pub struct Ctx {
    /// The data dir (`sandbox-policy.yaml`, `toolchains.toml`, `images/`).
    pub dir: PathBuf,
    /// The gateway to target; `None` = the OpenShell CLI's active one.
    pub gateway: Option<String>,
    /// A spec may mount a host folder only when the caller opted in.
    pub allow_share: bool,
}

/// Aim every following `openshell` call at the context's gateway.
fn target(ctx: &Ctx) {
    cli::set_gateway(ctx.gateway.as_deref().unwrap_or(""));
}

fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("field `name` is required".into());
    }
    sandbox::validate_name(name)
}

/// The spec, checked before anything runs: a valid name, and a share only when allowed and only
/// of a folder that is really there.
fn checked_spec(ctx: &Ctx, spec: &Value) -> Result<SandboxSpec, String> {
    let s = SandboxSpec::from_json(spec)?;
    check_name(&s.name)?;
    if let Some(share) = &s.share {
        if !ctx.allow_share {
            return Err(
                "the spec shares a host folder (`share`), which needs the allow-share option"
                    .into(),
            );
        }
        check_share_path(share)?;
    }
    Ok(s)
}

fn check_share_path(share: &Share) -> Result<(), String> {
    let p = Path::new(&share.path);
    if !p.is_absolute() {
        return Err(format!(
            "share.path {:?} must be an absolute path",
            share.path
        ));
    }
    if !p.exists() {
        return Err(format!("share.path {:?} does not exist", share.path));
    }
    if !p.is_dir() {
        return Err(format!("share.path {:?} is not a directory", share.path));
    }
    Ok(())
}

/// The secret named by `repo.env_var`, read from this process's environment. Errors name the
/// variable only, never its value.
fn repo_secret(var: &str) -> Result<String, String> {
    if var.is_empty() || var.contains(['=', '\0']) {
        return Err(format!("repo.env_var {var:?} is not a variable name"));
    }
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => v
            .into_string()
            .map_err(|_| format!("environment variable {var} (repo.env_var) is not valid UTF-8")),
        _ => Err(format!(
            "environment variable {var} (repo.env_var) is unset or empty"
        )),
    }
}

/// The Dockerfile image the spec names, if any, with whether it is built: the only image
/// `plan_create` looks at, so only it is inspected.
fn images_for(dir: &Path, image: &str) -> Vec<cli::Image> {
    if image.is_empty() {
        return Vec::new();
    }
    cli::dockerfiles(dir)
        .into_iter()
        .filter(|f| cli::dockerfile_tag(f) == image)
        .map(|file| {
            let tag = cli::dockerfile_tag(&file);
            cli::Image {
                built: cli::image_built(&tag),
                tag,
                file,
                freshness: Default::default(),
            }
        })
        .collect()
}

/// `[{"name","phase","created"}]` for the sandboxes on the gateway.
pub fn sandbox_list(ctx: &Ctx) -> Result<Value, String> {
    target(ctx);
    let list = cli::sandboxes()?;
    Ok(Value::Array(
        list.into_iter()
            .map(|s| json!({"name": s.name, "phase": s.phase, "created": s.created}))
            .collect(),
    ))
}

/// Create a sandbox from a JSON spec. With `dry_run`, only the plan: `{"name","steps"}`
/// (secrets travel through the environment, so the argv shown holds none). Otherwise the repo
/// provider first (when the spec grants a repo), then the plan's steps: `{"name","phase"}`.
pub fn sandbox_create(ctx: &Ctx, spec: &Value, dry_run: bool) -> Result<Value, String> {
    let s = checked_spec(ctx, spec)?;
    let grant = match &s.repo {
        Some(g) => Some((sandbox::grant_repo(&g.url)?, repo_secret(&g.env_var)?)),
        None => None,
    };
    target(ctx);
    let images = images_for(&ctx.dir, &s.image);
    let driver = if s.share.is_some() {
        cli::gateway_driver()
    } else {
        String::new()
    };
    let plan = sandbox::plan_create(
        &s,
        &CreateCtx {
            dir: &ctx.dir,
            driver: &driver,
            images: &images,
            rebuild: false,
        },
    )?;
    if dry_run {
        let steps: Vec<Value> = plan
            .steps
            .iter()
            .map(|st| json!({"prog": st.prog, "args": st.args}))
            .collect();
        return Ok(json!({"name": s.name, "steps": steps}));
    }
    if let Some((repo, secret)) = &grant {
        let provider = ghrepo::provider_name(&s.name, repo);
        cli::create_repo_provider(repo, &provider, secret)?;
    }
    for st in &plan.steps {
        // The plan's `openshell` args already carry the gateway, so `run`, not `exec`.
        cli::run(&st.prog, &st.args, &[]).map_err(|e| format!("{}: {e}", st.label()))?;
    }
    let phase = cli::sandboxes()
        .ok()
        .and_then(|l| l.into_iter().find(|x| x.name == s.name))
        .map(|x| x.phase)
        .unwrap_or_default();
    Ok(json!({"name": s.name, "phase": phase}))
}

/// Delete a sandbox and, as the dashboard does, the per-sandbox repo providers osh made for it.
pub fn sandbox_delete(ctx: &Ctx, name: &str) -> Result<Value, String> {
    check_name(name)?;
    target(ctx);
    let providers = cli::providers().unwrap_or_default();
    cli::os(&["sandbox", "delete", name])?;
    cli::drop_repo_providers(name, &providers);
    Ok(json!({"name": name, "deleted": true}))
}

/// Run `argv` in a sandbox. The command's own exit code, non-zero included, is `Ok`; only
/// failing to run `openshell` is `Err`.
pub fn sandbox_exec(ctx: &Ctx, name: &str, argv: &[String]) -> Result<Value, String> {
    check_name(name)?;
    if argv.is_empty() {
        return Err("no command to run".into());
    }
    target(ctx);
    let mut args: Vec<String> = ["sandbox", "exec", "-n", name, "--timeout", "600", "--"]
        .map(String::from)
        .to_vec();
    args.extend(argv.iter().cloned());
    let out = Command::new("openshell")
        .args(cli::with_gateway(args))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("openshell: {e}"))?;
    let code = out
        .status
        .code()
        .or_else(|| out.status.signal().map(|s| 128 + s))
        .unwrap_or(-1);
    Ok(json!({
        "exit_code": code,
        "stdout": String::from_utf8_lossy(&out.stdout),
        "stderr": String::from_utf8_lossy(&out.stderr),
    }))
}

/// Create the sandbox unless one of that name exists; an existing one is left as it is.
pub fn apply(ctx: &Ctx, spec: &Value) -> Result<Value, String> {
    let s = checked_spec(ctx, spec)?;
    target(ctx);
    if cli::sandboxes()?.iter().any(|x| x.name == s.name) {
        return Ok(json!({"action": "unchanged", "name": s.name}));
    }
    sandbox_create(ctx, spec, false)?;
    Ok(json!({"action": "created", "name": s.name}))
}

/// The output contract: `Ok(v)` prints `v` and exits 0; `Err(e)` prints
/// `{"ok":false,"error":e}` and exits 1.
pub fn envelope(r: Result<Value, String>) -> (Value, i32) {
    match r {
        Ok(v) => (v, 0),
        Err(e) => (json!({"ok": false, "error": e}), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ghrepo;
    use serde_json::json;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    /// Tests here change PATH and process-wide variables, so they run one at a time.
    static ENV: Mutex<()> = Mutex::new(());

    const FAKE_VARS: &[&str] = &[
        "OSH_FAKE_SANDBOXES",
        "OSH_FAKE_PROVIDERS",
        "OSH_FAKE_GATEWAYS",
        "OSH_FAKE_GATEWAY_INFO",
        "OSH_FAKE_FAIL",
        "OSH_FAKE_EXEC_EXIT",
        "OSH_FAKE_EXEC_STDOUT",
        "OSH_FAKE_EXEC_STDERR",
        "OSH_FAKE_ENV_KEYS",
    ];

    /// The stand-in `openshell` (and a failing `curl`, so a repo grant never reaches the
    /// network) first on PATH, a seeded data dir, and the call log.
    struct Fake {
        dir: PathBuf,
        log: PathBuf,
        path: Option<OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl Fake {
        fn new(tag: &str) -> Fake {
            let lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
            let root =
                std::env::temp_dir().join(format!("osh-core-ops-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let bin = root.join("bin");
            let dir = root.join("data");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            for f in ["sandbox-policy.yaml", "toolchains.toml"] {
                std::fs::write(dir.join(f), crate::assets::embedded(f).unwrap()).unwrap();
            }
            let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-openshell.sh");
            let fake = bin.join("openshell");
            std::fs::copy(&script, &fake).unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            let curl = bin.join("curl");
            std::fs::write(&curl, "#!/bin/sh\nexit 7\n").unwrap();
            std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
            let log = root.join("calls.log");
            std::fs::write(&log, "").unwrap();
            let path = std::env::var_os("PATH");
            let mut dirs = vec![bin];
            dirs.extend(std::env::split_paths(path.as_deref().unwrap_or_default()));
            std::env::set_var("PATH", std::env::join_paths(dirs).unwrap());
            std::env::set_var("OSH_FAKE_LOG", &log);
            for v in FAKE_VARS {
                std::env::remove_var(v);
            }
            crate::cli::set_gateway("");
            Fake {
                dir,
                log,
                path,
                _lock: lock,
            }
        }

        fn ctx(&self) -> Ctx {
            Ctx {
                dir: self.dir.clone(),
                gateway: None,
                allow_share: false,
            }
        }

        fn calls(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            match &self.path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
            std::env::remove_var("OSH_FAKE_LOG");
            for v in FAKE_VARS {
                std::env::remove_var(v);
            }
            crate::cli::set_gateway("");
        }
    }

    fn spec() -> Value {
        json!({"name":"w1","image":"img:1","providers":["npm"],"cpu":"2","memory":"4Gi"})
    }

    #[test]
    fn create_requires_valid_name() {
        let f = Fake::new("name");
        for bad in [json!({}), json!({"name":""}), json!({"name":"../x"})] {
            let e = sandbox_create(&f.ctx(), &bad, false).unwrap_err();
            assert!(e.contains("name"), "{e}");
        }
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn create_rejects_bad_spec_without_calls() {
        let f = Fake::new("badspec");
        let e = sandbox_create(&f.ctx(), &json!({"name":"w1","cpus":2}), false).unwrap_err();
        assert!(e.contains("cpus"), "{e}");
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn create_dry_run_does_not_call_openshell_write() {
        let f = Fake::new("dry");
        let v = sandbox_create(&f.ctx(), &spec(), true).unwrap();
        assert_eq!(v["name"], "w1");
        let steps = v["steps"].as_array().unwrap();
        let last = steps.last().unwrap();
        assert_eq!(last["prog"], "openshell");
        let args: Vec<&str> = last["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(&args[..2], ["sandbox", "create"]);
        assert!(args.contains(&"--detach"), "{args:?}");
        let calls = f.calls();
        assert!(!calls.contains("create"), "{calls}");
        assert!(!calls.contains("delete"), "{calls}");
    }

    #[test]
    fn create_runs_plan_steps_in_order() {
        let f = Fake::new("run");
        let v = sandbox_create(&f.ctx(), &spec(), false).unwrap();
        assert_eq!(v["name"], "w1");
        assert!(v.get("phase").is_some(), "{v}");
        let want = format!(
            "sandbox create --policy {}/.sandbox-policy.generated.yaml --name w1 --from img:1 --cpu 2 --memory 4Gi --provider npm --detach",
            f.dir.display()
        );
        let calls = f.calls();
        assert!(calls.lines().any(|l| l == want), "{calls}");
    }

    #[test]
    fn create_reports_phase_from_the_listing() {
        let f = Fake::new("phase");
        std::env::set_var(
            "OSH_FAKE_SANDBOXES",
            r#"{"sandboxes":[{"name":"w1","phase":"Ready","created_at":"t"}]}"#,
        );
        let v = sandbox_create(&f.ctx(), &spec(), false).unwrap();
        assert_eq!(v, json!({"name":"w1","phase":"Ready"}));
    }

    #[test]
    fn create_failure_is_err() {
        let f = Fake::new("fail");
        std::env::set_var("OSH_FAKE_FAIL", "sandbox create");
        let e = sandbox_create(&f.ctx(), &spec(), false).unwrap_err();
        assert!(e.contains("fake failure"), "{e}");
    }

    #[test]
    fn gateway_from_ctx_targets_every_call() {
        let f = Fake::new("gw");
        let ctx = Ctx {
            gateway: Some("gw1".into()),
            ..f.ctx()
        };
        sandbox_create(&ctx, &spec(), false).unwrap();
        let calls = f.calls();
        assert!(!calls.is_empty());
        assert!(calls.lines().all(|l| l.starts_with("-g gw1 ")), "{calls}");
        assert!(!calls.contains("-g gw1 -g"), "{calls}");
    }

    fn with_share(path: &Path) -> Value {
        let mut s = spec();
        s["share"] = json!({"path": path.to_string_lossy(), "rw": false});
        s
    }

    #[test]
    fn share_rejected_unless_allowed() {
        let f = Fake::new("share-no");
        let e = sandbox_create(&f.ctx(), &with_share(&f.dir), false).unwrap_err();
        assert!(e.contains("share"), "{e}");
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn share_allowed_with_flag() {
        let f = Fake::new("share-yes");
        let ctx = Ctx {
            allow_share: true,
            ..f.ctx()
        };
        sandbox_create(&ctx, &with_share(&f.dir), false).unwrap();
        let calls = f.calls();
        assert!(calls.contains("gateway info"), "{calls}");
        let create = calls
            .lines()
            .find(|l| l.starts_with("sandbox create"))
            .unwrap();
        assert!(
            create.contains("--driver-config-json {\"docker\":"),
            "{create}"
        );
        assert!(create.contains("\"target\":\"/sandbox/data\""), "{create}");
    }

    #[test]
    fn share_path_must_be_an_existing_absolute_dir() {
        let f = Fake::new("share-path");
        let ctx = Ctx {
            allow_share: true,
            ..f.ctx()
        };
        let file = f.dir.join("sandbox-policy.yaml");
        let missing = f.dir.join("nope");
        for bad in [Path::new("rel/dir"), Path::new(""), &missing, &file] {
            let e = sandbox_create(&ctx, &with_share(bad), false).unwrap_err();
            assert!(e.contains("share.path"), "{bad:?}: {e}");
        }
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn share_on_unknown_driver_is_err() {
        let f = Fake::new("share-drv");
        std::env::set_var("OSH_FAKE_GATEWAY_INFO", "{}");
        let ctx = Ctx {
            allow_share: true,
            ..f.ctx()
        };
        let e = sandbox_create(&ctx, &with_share(&f.dir), false).unwrap_err();
        assert!(e.contains("driver"), "{e}");
        assert!(!f.calls().contains("sandbox create"));
    }

    fn with_repo(var: &str) -> Value {
        let mut s = spec();
        s["repo"] = json!({"url": "https://github.com/o/r", "env_var": var});
        s
    }

    fn secret_value() -> String {
        format!("tok-{}", "x".repeat(24))
    }

    #[test]
    fn env_var_unset_is_error_and_value_never_leaks() {
        let f = Fake::new("repo-unset");
        std::env::remove_var("OSH_TEST_UNSET");
        let e = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_UNSET"), false).unwrap_err();
        assert!(e.contains("OSH_TEST_UNSET"), "{e}");
        std::env::set_var("OSH_TEST_EMPTY", "");
        let e = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_EMPTY"), false).unwrap_err();
        assert!(e.contains("OSH_TEST_EMPTY"), "{e}");
        std::env::remove_var("OSH_TEST_EMPTY");
        assert_eq!(f.calls(), "");

        // Set, but the provider cannot be created: the error and argv carry no secret.
        let value = secret_value();
        std::env::set_var("OSH_TEST_TOK", &value);
        std::env::set_var("OSH_FAKE_FAIL", "provider create");
        let e = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_TOK"), false).unwrap_err();
        std::env::remove_var("OSH_TEST_TOK");
        assert!(!e.contains(&value), "{e}");
        let calls = f.calls();
        assert!(calls.contains("provider create"), "{calls}");
        assert!(!calls.contains(&value), "{calls}");
        assert!(!calls.contains("sandbox create"), "{calls}");
    }

    #[test]
    fn repo_provider_is_created_before_the_sandbox_through_the_environment() {
        let f = Fake::new("repo-ok");
        let repo = ghrepo::parse_url("https://github.com/o/r")
            .unwrap()
            .repo(ghrepo::Forge::GitHub)
            .unwrap();
        let var = ghrepo::token_env(&repo);
        std::env::set_var("OSH_FAKE_ENV_KEYS", var);
        let value = secret_value();
        std::env::set_var("OSH_TEST_TOK", &value);
        let v = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_TOK"), false);
        std::env::remove_var("OSH_TEST_TOK");
        v.unwrap();
        let calls = f.calls();
        assert!(!calls.contains(&value), "{calls}");
        let lines: Vec<&str> = calls.lines().collect();
        let prov = lines
            .iter()
            .position(|l| l.starts_with("provider create --name github-w1-r"))
            .unwrap_or_else(|| panic!("{calls}"));
        assert_eq!(lines[prov + 1], format!("env {var} set"), "{calls}");
        let create = lines
            .iter()
            .position(|l| l.starts_with("sandbox create"))
            .unwrap();
        assert!(prov < create, "{calls}");
        assert!(lines[create].contains("--provider github-w1-r"), "{calls}");
    }

    #[test]
    fn dry_run_with_repo_creates_no_provider() {
        let f = Fake::new("repo-dry");
        std::env::set_var("OSH_TEST_TOK", secret_value());
        let v = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_TOK"), true);
        std::env::remove_var("OSH_TEST_TOK");
        let v = v.unwrap();
        assert!(!v.to_string().contains(&secret_value()));
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn list_gives_name_phase_created() {
        let f = Fake::new("list");
        std::env::set_var(
            "OSH_FAKE_SANDBOXES",
            r#"{"sandboxes":[{"name":"w1","phase":"Ready","created_at":"2026-01-01","conditions":[]}]}"#,
        );
        let v = sandbox_list(&f.ctx()).unwrap();
        assert_eq!(
            v,
            json!([{"name":"w1","phase":"Ready","created":"2026-01-01"}])
        );
    }

    #[test]
    fn list_failure_is_err() {
        let f = Fake::new("list-fail");
        std::env::set_var("OSH_FAKE_FAIL", "sandbox list");
        assert!(sandbox_list(&f.ctx()).is_err());
    }

    #[test]
    fn delete_runs_sandbox_delete() {
        let f = Fake::new("delete");
        let v = sandbox_delete(&f.ctx(), "w1").unwrap();
        assert_eq!(v, json!({"name":"w1","deleted":true}));
        assert!(f.calls().lines().any(|l| l == "sandbox delete w1"));
    }

    #[test]
    fn delete_and_exec_validate_the_name() {
        let f = Fake::new("badname");
        assert!(sandbox_delete(&f.ctx(), "../x").is_err());
        assert!(sandbox_exec(&f.ctx(), "-n", &["ls".into()]).is_err());
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn exec_nonzero_is_ok_with_exit_code() {
        let f = Fake::new("exec");
        std::env::set_var("OSH_FAKE_EXEC_EXIT", "3");
        std::env::set_var("OSH_FAKE_EXEC_STDOUT", "out");
        std::env::set_var("OSH_FAKE_EXEC_STDERR", "err");
        let argv = vec!["sh".to_string(), "-c".into(), "exit 3".into()];
        let v = sandbox_exec(&f.ctx(), "w1", &argv).unwrap();
        assert_eq!(v, json!({"exit_code":3,"stdout":"out","stderr":"err"}));
        let calls = f.calls();
        let line = calls.lines().next().unwrap();
        assert!(line.starts_with("sandbox exec -n w1 "), "{line}");
        assert!(line.ends_with("-- sh -c exit 3"), "{line}");
    }

    #[test]
    fn exec_needs_a_command() {
        let f = Fake::new("exec-empty");
        assert!(sandbox_exec(&f.ctx(), "w1", &[]).is_err());
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn apply_unchanged_when_exists() {
        let f = Fake::new("apply-same");
        std::env::set_var(
            "OSH_FAKE_SANDBOXES",
            r#"{"sandboxes":[{"name":"w1","phase":"Ready","created_at":"t"}]}"#,
        );
        let v = apply(&f.ctx(), &spec()).unwrap();
        assert_eq!(v, json!({"action":"unchanged","name":"w1"}));
        assert!(!f.calls().contains("create"), "{}", f.calls());
    }

    #[test]
    fn apply_creates_when_absent() {
        let f = Fake::new("apply-new");
        let v = apply(&f.ctx(), &spec()).unwrap();
        assert_eq!(v, json!({"action":"created","name":"w1"}));
        assert!(f.calls().lines().any(|l| l.starts_with("sandbox create")));
    }

    #[test]
    fn apply_validates_before_listing() {
        let f = Fake::new("apply-bad");
        assert!(apply(&f.ctx(), &json!({"name":"../x"})).is_err());
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn envelope_maps_err_to_ok_false_exit_1() {
        assert_eq!(envelope(Ok(json!({"a":1}))), (json!({"a":1}), 0));
        assert_eq!(
            envelope(Err("boom".into())),
            (json!({"ok":false,"error":"boom"}), 1)
        );
    }
}
