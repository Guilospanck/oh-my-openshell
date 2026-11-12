//! Thin wrappers around the `openshell` and container runtime (`docker` or `podman`) CLIs.
use crate::agents;
use crate::ghrepo::{self, Forge, GhRepo, Tracked};
use crate::platform::{self, Runtime};
use saphyr::LoadableYamlNode;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        mpsc::{Receiver, Sender},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::Duration,
};

/// Gateway every `openshell` call targets; empty = the CLI's active gateway.
static GATEWAY: Mutex<String> = Mutex::new(String::new());

pub fn set_gateway(name: &str) {
    if let Ok(mut g) = GATEWAY.lock() {
        *g = name.to_string();
    }
}

pub fn gateway() -> String {
    GATEWAY.lock().map(|g| g.clone()).unwrap_or_default()
}

/// `args` for an `openshell` call, aimed at the selected gateway.
pub fn with_gateway(args: Vec<String>) -> Vec<String> {
    let gw = gateway();
    if gw.is_empty() {
        return args;
    }
    let mut v = vec!["-g".to_string(), gw];
    v.extend(args);
    v
}

pub fn exec(prog: &str, args: &[String], env: &[(String, String)]) -> Result<String, String> {
    let args = if prog == "openshell" {
        with_gateway(args.to_vec())
    } else {
        args.to_vec()
    };
    run(prog, &args, env)
}

pub fn run(prog: &str, args: &[String], env: &[(String, String)]) -> Result<String, String> {
    let out = Command::new(prog)
        .args(args)
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{prog}: {e}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if out.status.success() {
        Ok(text)
    } else {
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Err(text.trim().to_string())
    }
}

pub fn os(args: &[&str]) -> Result<String, String> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    exec("openshell", &args, &[])
}

/// Gateway-independent `openshell` call (registry queries).
fn os_global(args: &[&str]) -> Result<String, String> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    run("openshell", &args, &[])
}

pub fn have(bin: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {bin}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Clone, Default, Debug)]
pub struct Gateway {
    pub name: String,
    pub endpoint: String,
    /// The OpenShell CLI's default: what a bare `openshell` call (outside osh) talks to.
    pub active: bool,
}

/// Parses `openshell gateway list -o json`.
pub fn parse_gateways(v: &Value) -> Vec<Gateway> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|g| Gateway {
            name: text(&g["name"]),
            endpoint: text(&g["endpoint"]),
            active: g["active"].as_bool() == Some(true),
        })
        .collect()
}

/// Makes `name` the default gateway of the OpenShell CLI.
pub fn select_default_gateway(name: &str) -> Result<String, String> {
    os_global(&["gateway", "select", name])?;
    Ok(format!("{name} is now the default gateway"))
}

#[derive(Clone, Default, Debug)]
pub struct Cond {
    pub kind: String,
    pub status: String,
    pub message: String,
}

#[derive(Clone, Default, Debug)]
pub struct Sandbox {
    pub name: String,
    pub phase: String,
    pub created: String,
    pub conditions: Vec<Cond>,
}

#[derive(Clone, Default, Debug)]
pub struct Provider {
    pub name: String,
    pub ty: String,
    pub keys: Vec<String>,
}

#[derive(Clone, Default, Debug)]
pub struct Profile {
    pub id: String,
    pub version: u64,
}

#[derive(Clone, Default, Debug)]
pub struct Image {
    pub tag: String,
    pub file: PathBuf,
    pub built: bool,
    pub freshness: Freshness,
}

/// Whether a built image still matches its Dockerfile.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum Freshness {
    #[default]
    UpToDate,
    /// The Dockerfile changed since the image was built.
    Outdated,
    /// Built without a recorded Dockerfile hash (older build); rebuild to track.
    Unverified,
}

/// Image label holding the hash of the Dockerfile the image was built from.
pub const HASH_LABEL: &str = "osh.dockerfile-hash";

/// Hash of the Dockerfile contents, recorded on the image to tell when it is stale.
pub fn dockerfile_hash(file: &Path) -> String {
    crate::assets::hash(&fs::read(file).unwrap_or_default())
}

static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();

/// The container runtime to drive, found once: a ready Docker, else a ready Podman.
pub fn runtime() -> Option<Runtime> {
    *RUNTIME.get_or_init(|| platform::gather(&platform::RealHost).runtime())
}

/// Program for image builds and container inspection. Falls back to `docker` when no runtime
/// answers, so the failure reads like the familiar "command not found".
pub fn runtime_bin() -> &'static str {
    bin_for(runtime())
}

fn bin_for(rt: Option<Runtime>) -> &'static str {
    rt.map_or("docker", Runtime::bin)
}

/// `docker build` arguments that record the Dockerfile hash on the image.
pub fn build_args(dir: &Path, file: &Path, tag: &str) -> Vec<String> {
    vec![
        "build".into(),
        "-f".into(),
        file.to_string_lossy().into(),
        "-t".into(),
        tag.into(),
        "--label".into(),
        format!("{HASH_LABEL}={}", dockerfile_hash(file)),
        dir.join("images").to_string_lossy().into(),
    ]
}

fn image_freshness(tag: &str, file: &Path) -> Freshness {
    let label = Command::new(runtime_bin())
        .args([
            "image",
            "inspect",
            "-f",
            &format!("{{{{index .Config.Labels \"{HASH_LABEL}\"}}}}"),
            tag,
        ])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if label.is_empty() || label == "<no value>" {
        Freshness::Unverified
    } else if label == dockerfile_hash(file) {
        Freshness::UpToDate
    } else {
        Freshness::Outdated
    }
}

#[derive(Clone, Default, Debug)]
pub struct Snapshot {
    /// Name of the gateway this snapshot describes.
    pub gateway: String,
    /// Compute driver of that gateway (`vm`, `docker`, ...); empty when unknown.
    pub driver: String,
    pub gateways: Vec<Gateway>,
    pub gateway_ok: bool,
    pub version: String,
    pub status_text: String,
    pub sandboxes: Vec<Sandbox>,
    pub providers: Vec<Provider>,
    pub profiles: Vec<Profile>,
    pub images: Vec<Image>,
    /// Sandbox name and its recent log lines.
    pub logs: Option<(String, String)>,
    /// Sandbox name and a one-line description of its CPU/memory limits.
    pub limits: Option<(String, String)>,
    pub error: Option<String>,
}

/// Label tying a sandbox to the host folder it shares.
pub const SHARE_LABEL: &str = "osh.share";

/// Label value for a host folder. Values cannot hold `/`, so this is an FNV-1a hash of the path.
pub fn share_key(path: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Folders that identify the project `osh` started in: the current folder, then its git root.
/// Outside a git repository only the current folder counts.
pub fn project_dirs() -> Vec<String> {
    let mut dirs: Vec<String> = std::env::current_dir()
        .map(|d| d.display().to_string())
        .into_iter()
        .collect();
    if let Ok(root) = run("git", &["rev-parse".into(), "--show-toplevel".into()], &[]) {
        let root = root.trim().to_string();
        if !root.is_empty() && !dirs.contains(&root) {
            dirs.push(root);
        }
    }
    dirs
}

/// Where host folder `shared` appears in a sandbox, plus the spot inside it matching `cwd`.
/// Shared folders are mounted at `/sandbox/<folder name>`; most specific path first.
pub fn sandbox_paths(shared: &str, cwd: &str) -> Vec<String> {
    let shared = std::path::Path::new(shared);
    let Some(name) = shared.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let root = format!("/sandbox/{name}");
    let mut paths = Vec::new();
    if let Ok(rel) = std::path::Path::new(cwd).strip_prefix(shared) {
        if !rel.as_os_str().is_empty() {
            paths.push(format!("{root}/{}", rel.display()));
        }
    }
    paths.push(root);
    paths
}

/// Folders to try as the start directory of a shell in `sandbox`: empty unless the sandbox
/// shares the current folder or its git root.
pub fn start_dirs(sandbox: &str) -> Vec<String> {
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    for dir in project_dirs() {
        let selector = format!("{SHARE_LABEL}={}", share_key(&dir));
        let args = with_gateway(
            [
                "sandbox",
                "list",
                "--selector",
                &selector,
                "--output",
                "json",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        );
        let Ok(v) = run("openshell", &args, &[]).and_then(|t| parse(&t)) else {
            continue;
        };
        let shares = v["sandboxes"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| text(&s["name"]) == sandbox);
        if shares {
            return sandbox_paths(&dir, &cwd);
        }
    }
    Vec::new()
}

/// Ready sandboxes on any gateway sharing the current folder or, failing that, its git root,
/// as (gateway, sandbox). The active gateway comes first.
pub fn find_shared() -> Vec<(String, String)> {
    let mut gateways: Vec<(bool, String)> = os_global(&["gateway", "list", "-o", "json"])
        .and_then(|t| parse(&t))
        .map(|v| {
            v.as_array()
                .into_iter()
                .flatten()
                .map(|g| (g["active"].as_bool() != Some(true), text(&g["name"])))
                .collect()
        })
        .unwrap_or_default();
    gateways.sort();
    for dir in project_dirs() {
        let selector = format!("{SHARE_LABEL}={}", share_key(&dir));
        let mut hits = Vec::new();
        for (_, gw) in &gateways {
            let args = ["-g", gw, "sandbox", "list", "--selector", &selector];
            let args: Vec<String> = args
                .iter()
                .chain(&["--output", "json"])
                .map(|s| s.to_string())
                .collect();
            let Ok(v) = run("openshell", &args, &[]).and_then(|t| parse(&t)) else {
                continue;
            };
            for s in v["sandboxes"].as_array().into_iter().flatten() {
                if text(&s["phase"]) == "Ready" {
                    hits.push((gw.clone(), text(&s["name"])));
                }
            }
        }
        if !hits.is_empty() {
            return hits;
        }
    }
    Vec::new()
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

pub fn dockerfile_tag(file: &Path) -> String {
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    format!(
        "{}:latest",
        name.trim_start_matches("Dockerfile-").replace('_', "-")
    )
}

pub fn dockerfiles(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir.join("images"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("Dockerfile-"))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn profile_ids(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir.join("providers"))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let p = e.path();
                    let ext = p.extension()?;
                    (ext == "yaml" || ext == "yml")
                        .then(|| p.file_stem()?.to_str().map(String::from))?
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.dedup();
    v
}

/// The `id:` of a profile's text.
pub fn profile_id_of(text: &str) -> Option<String> {
    let docs = saphyr::Yaml::load_from_str(text).ok()?;
    docs.first()?
        .as_mapping_get("id")?
        .as_str()
        .map(String::from)
}

/// The profile file for `id`: `providers/ID.yaml`, or `ID.yml` when only that exists.
pub fn profile_path(dir: &Path, id: &str) -> PathBuf {
    let yaml = dir.join("providers").join(format!("{id}.yaml"));
    let yml = dir.join("providers").join(format!("{id}.yml"));
    if !yaml.exists() && yml.exists() {
        yml
    } else {
        yaml
    }
}

/// The editor's check for a provider profile: valid YAML with an `id`, then OpenShell's own lint
/// (read-only) when the CLI is there. "Already exists" is not a problem for a file being edited,
/// and a lint that cannot reach the gateway does not block the save.
pub fn check_profile(text: &str) -> Result<(), String> {
    let docs = saphyr::Yaml::load_from_str(text).map_err(|e| e.to_string())?;
    let doc = docs.first().ok_or("the profile is empty")?;
    if doc
        .as_mapping_get("id")
        .and_then(|v| v.as_str())
        .is_none_or(str::is_empty)
    {
        return Err("a profile needs an id: line (the name it is imported under)".into());
    }
    if cfg!(test) || !have("openshell") {
        return Ok(());
    }
    // OpenShell does not expand the template itself; lint what osh would import.
    let body = text.replace("${FORGEJO_HOST}", "forgejo.example");
    let tmp = std::env::temp_dir().join(format!("osh-lint-{}.yaml", std::process::id()));
    if fs::write(&tmp, body).is_err() {
        return Ok(());
    }
    let args = with_gateway(vec![
        "profile".into(),
        "lint".into(),
        "-f".into(),
        tmp.to_string_lossy().into(),
    ]);
    // The lint asks the gateway; a gateway that hangs must not freeze the editor's save.
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let out = run("openshell", &args, &[]);
        let _ = fs::remove_file(&tmp);
        let _ = tx.send(out);
    });
    match rx.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok(_)) | Err(_) => Ok(()),
        Ok(Err(e)) => lint_errors(&e).map_or(Ok(()), Err),
    }
}

/// The lint's real problems (one per line), without "already exists"; `None` when the output has
/// no diagnostics (the gateway could not be reached, say).
pub fn lint_errors(out: &str) -> Option<String> {
    let errs: Vec<String> = out
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("error ") && !l.contains("already exists"))
        .map(|l| {
            // `error FILE profile=ID field=F message`: keep the field and the message.
            let rest = l.splitn(3, ' ').nth(2).unwrap_or(l);
            rest.split_once(' ')
                .filter(|(p, _)| p.starts_with("profile="))
                .map_or(rest, |(_, m)| m)
                .to_string()
        })
        .collect();
    (!errs.is_empty()).then(|| errs.join("\n"))
}

/// Repo profiles that need no credential and declare `host` as an endpoint: attaching one
/// gives a sandbox that host with the profile's exact settings.
pub fn profiles_for_host(dir: &Path, host: &str) -> Vec<String> {
    let want = format!("host: {host}");
    let mut all: Vec<String> = profile_ids(dir)
        .into_iter()
        .filter(|id| {
            profile_env_keys(dir, id).is_empty()
                && fs::read_to_string(profile_path(dir, id))
                    .unwrap_or_default()
                    .lines()
                    .any(|l| l.trim().trim_start_matches("- ").trim() == want)
        })
        .collect();
    // The opt-in profiles (language toolchains) are offered only where no ordinary profile
    // answers, and never for GitHub, which each of them lists for git dependencies.
    let opt = |id: &String| crate::assets::OPT_IN_PROFILES.contains(&id.as_str());
    let github = matches!(host, "github.com" | "api.github.com");
    if github || all.iter().any(|id| !opt(id)) {
        all.retain(|id| !opt(id));
    }
    all
}

/// Profile for one `host:port` and one binary, with the options `policy update
/// --add-endpoint` cannot set (`allow_encoded_slash`: scoped npm packages are `@scope%2fname`).
pub fn slash_profile(name: &str, host: &str, port: u16, binary: &str) -> String {
    format!(
        "id: {name}\ndisplay_name: {host} (encoded slashes)\ndescription: Read-only {host} with encoded slashes allowed\ncategory: data\nendpoints:\n  - host: {host}\n    port: {port}\n    protocol: rest\n    access: read-only\n    enforcement: enforce\n    allow_encoded_slash: true\nbinaries: [{binary}]\n"
    )
}

/// Name for a generated profile of `host`.
pub fn slash_name(host: &str) -> String {
    format!("slash-{}", host.replace('.', "-"))
}

/// Create the provider for a repo profile when the gateway lacks it, then attach it.
pub fn attach_profile(dir: &Path, sandbox: &str, id: &str) -> Result<String, String> {
    if os(&["provider", "get", id]).is_err() {
        create_provider(dir, id, &[])?;
    }
    os(&[
        "sandbox",
        "provider",
        "attach",
        sandbox,
        id,
        "--wait",
        "--timeout",
        "30",
    ])
    .map(|_| format!("Attached {id} to {sandbox}"))
}

/// The `origin` remote of git folder `dir`, on any host. Which provider hosts it is the
/// caller's question to settle (`Remote::repo`).
pub fn origin_remote(dir: &str) -> Option<ghrepo::Remote> {
    let args = ["-C", dir, "remote", "get-url", "origin"].map(String::from);
    ghrepo::parse_url(&run("git", &args, &[]).ok()?)
}

/// Open `url` in the host's browser. False when no opener could be started.
pub fn open_url(url: &str) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// What a read-only GET of the repo on its provider (`ghrepo::check_request`) with `token` says:
/// the HTTP status, and for GitHub the token's real expiry (epoch seconds) from the
/// `github-authentication-token-expiration` header. The token reaches curl in a private header
/// file, so it never appears in argv.
pub fn repo_probe(repo: &GhRepo, token: &str) -> Result<(u16, Option<u64>), String> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let (auth, accept, url) = ghrepo::check_request(repo);
    let file = std::env::temp_dir().join(format!(
        "osh-hdr-{}-{}",
        std::process::id(),
        ghrepo::now_secs()
    ));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
        .map_err(|e| e.to_string())?;
    let written = writeln!(f, "{auth}{token}");
    drop(f);
    let out = written.map_err(|e| e.to_string()).and_then(|_| {
        let args = [
            "-sS".to_string(),
            "--max-time".into(),
            "15".into(),
            "-o".into(),
            "/dev/null".into(),
            "-D".into(),
            "-".into(),
            "-H".into(),
            format!("@{}", file.display()),
            "-H".into(),
            accept.into(),
            "-H".into(),
            "User-Agent: osh".into(),
            url,
        ];
        run("curl", &args, &[])
    });
    let _ = fs::remove_file(&file);
    let headers = out?;
    let status = headers
        .lines()
        .rev()
        .find_map(|l| {
            l.strip_prefix("HTTP/")
                .and_then(|r| r.split_whitespace().nth(1))
        })
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("{} gave no HTTP status", host_name(repo)))?;
    let expires = headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case("github-authentication-token-expiration")
            .then(|| ghrepo::parse_token_expiry(v))
            .flatten()
    });
    Ok((status, expires))
}

/// HTTP status of the check (see `repo_probe`).
pub fn repo_status(repo: &GhRepo, token: &str) -> Result<u16, String> {
    repo_probe(repo, token).map(|(s, _)| s)
}

/// How the repo's host is named in messages: the provider for the fixed cloud hosts, the
/// hostname for self-hosted ones.
pub fn host_name(repo: &GhRepo) -> &str {
    match repo.forge {
        Forge::GitHub => "GitHub",
        Forge::BitbucketCloud => "Bitbucket",
        _ => &repo.host,
    }
}

/// Why a repo check failed, or `None` when `status` means the token can see the repo.
pub fn repo_status_problem(status: u16, repo: &GhRepo) -> Option<String> {
    let full = repo.full();
    let host = host_name(repo);
    let expired = "copy all of it, and check it has not expired";
    let msg = match (repo.forge, status) {
        (_, 200) => return None,
        (Forge::GitHub, 401) => format!("GitHub rejected the token (401): {expired}"),
        (Forge::GitHub, 403) => format!(
            "GitHub refused the token for {full} (403): the organization may need to approve it"
        ),
        (Forge::GitHub, 404) => format!(
            "The token cannot see {full} (404): set Resource owner to {} and select that repository",
            repo.owner
        ),
        (Forge::Forgejo, 401) => format!("{host} rejected the token (401): copy all of it"),
        (_, 401) => format!("{host} rejected the token (401): {expired}"),
        (Forge::GitLab, 403) => format!(
            "{host} refused the token for {full} (403): it needs the scope read_api"
        ),
        (Forge::GitLab, 404) => format!(
            "The token cannot see {full} on {host} (404): check the project path, and that the token was made in that project (or its user can access it)"
        ),
        (Forge::BitbucketCloud, 403) => format!(
            "Bitbucket refused the token for {full} (403): it needs Repositories Read"
        ),
        (Forge::BitbucketDc, 403) => format!(
            "{host} refused the token for {full} (403): it needs Repository read or write"
        ),
        (Forge::BitbucketCloud | Forge::BitbucketDc, 404) => format!(
            "The token cannot see {full} on {host} (404): create it in that repository's settings"
        ),
        (Forge::Forgejo, 403) => format!(
            "{host} refused the token for {full} (403): it needs the scope write:repository"
        ),
        (Forge::Forgejo, 404) => format!(
            "The token cannot see {full} on {host} (404): check the repository name, and that the token lists it under Specific repositories"
        ),
        (_, s) => format!("{host} answered {s} for {full}"),
    };
    Some(msg)
}

fn track_repo(provider: &str, repo: &GhRepo, token: &str) {
    // The user picks the lifetime on the provider's page, so ask GitHub when the token really
    // expires; the pre-filled default is only the fallback when the answer has none.
    let real = (repo.forge == ghrepo::Forge::GitHub)
        .then(|| repo_probe(repo, token).ok().and_then(|(_, e)| e))
        .flatten();
    let expiry = real.map_or_else(
        || ghrepo::expiry_ms(ghrepo::now_secs(), ghrepo::TOKEN_DAYS),
        |s| s * 1000,
    );
    let args = [
        "provider".to_string(),
        "update".into(),
        provider.into(),
        "--credential-expires-at".into(),
        format!("{}={}", ghrepo::token_env(repo), expiry),
    ];
    let _ = exec("openshell", &args, &[]);
    ghrepo::track(
        ghrepo::tracked_path().as_deref(),
        Tracked {
            gateway: gateway(),
            provider: provider.into(),
            expiry_ms: expiry,
            repo: repo.spec(),
        },
    );
}

/// Import the repo's push profile when the gateway lacks it, then create `provider` from it
/// with `token`. The token goes through the environment, never argv.
pub fn create_repo_provider(repo: &GhRepo, provider: &str, token: &str) -> Result<String, String> {
    let var = ghrepo::token_env(repo);
    let id = ghrepo::profile_id(repo);
    let imported = os(&["profile", "describe", &id]).is_err();
    if imported {
        let tmp = std::env::temp_dir().join(format!("osh-{id}.yaml"));
        fs::write(&tmp, ghrepo::push_profile(repo)).map_err(|e| e.to_string())?;
        let r = os(&["profile", "import", "--file", &tmp.to_string_lossy()]);
        let _ = fs::remove_file(&tmp);
        r?;
    }
    let args: Vec<String> = [
        "provider",
        "create",
        "--name",
        provider,
        "--type",
        &id,
        "--credential",
        var,
    ]
    .map(String::from)
    .to_vec();
    let env = [(var.to_string(), token.to_string())];
    if let Err(e) = exec("openshell", &args, &env) {
        if imported {
            let _ = os(&["profile", "delete", &id]);
        }
        return Err(e);
    }
    // Forgejo tokens never expire: nothing to track or warn about.
    if repo.forge.expires() {
        track_repo(provider, repo, token);
    }
    Ok(format!("Created provider {provider} for {}", repo.shown()))
}

/// Swap in a new token for a repo provider and restart its 30 day clock.
pub fn renew_repo_token(repo: &GhRepo, provider: &str, token: &str) -> Result<String, String> {
    update_credentials(provider, &[(ghrepo::token_env(repo).into(), token.into())])?;
    track_repo(provider, repo, token);
    Ok(format!(
        "Renewed the token of {provider} ({})",
        repo.shown()
    ))
}

/// Running sandboxes among `sandboxes` that have `provider` attached.
pub fn running_with(provider: &str, sandboxes: &[Sandbox]) -> Vec<String> {
    sandboxes
        .iter()
        .filter(|s| s.phase == "Ready")
        .filter(|s| attached_providers(&s.name).iter().any(|p| p == provider))
        .map(|s| s.name.clone())
        .collect()
}

/// Stop and start `sandbox`, so its programs start with the current provider credentials.
pub fn restart_sandbox(sandbox: &str) -> Result<(), String> {
    os(&["sandbox", "stop", sandbox])?;
    os(&["sandbox", "start", sandbox]).map(|_| ())
}

/// A repo provider removed with its sandbox, and the token behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dropped {
    pub provider: String,
    /// The token name osh asked for, to find it in the token list.
    pub token: String,
    /// Where a non-GitHub token is revoked, as (provider, page); `None` for GitHub, whose
    /// token list osh offers to open.
    pub page: Option<(String, String)>,
}

/// The Forgejo repo providers `sandbox` owned, among `providers`, as (provider, host). Forgejo
/// tokens are not tracked (they never expire), so they are told apart by shape: the profile
/// id is `fj-<host>-<owner>-<repo>` and the provider `forgejo-<sandbox>-<repo>`. `hosts` are
/// the hosts remembered as Forgejo, to read the host back out of the profile id.
fn forgejo_owned(sandbox: &str, providers: &[Provider], hosts: &[String]) -> Vec<(String, String)> {
    let prefix = ghrepo::sandbox_prefix(Forge::Forgejo, sandbox);
    providers
        .iter()
        .filter_map(|p| {
            let rest = p.name.strip_prefix(&prefix)?;
            let body = p.ty.strip_prefix("fj-")?;
            if rest.is_empty() || !body.ends_with(&format!("-{rest}")) {
                return None;
            }
            let host = hosts
                .iter()
                .find(|h| body.starts_with(&format!("{}-", ghrepo::slug(h))))?;
            Some((p.name.clone(), host.clone()))
        })
        .collect()
}

/// Remove the per-sandbox repo providers of a deleted sandbox, and their profiles once no
/// other provider uses them. Returns each provider removed with the name of the token behind
/// it, which stays valid on its host until the user deletes it there.
pub fn drop_repo_providers(sandbox: &str, providers: &[Provider]) -> Vec<Dropped> {
    let file = ghrepo::tracked_path();
    let gw = gateway();
    let mine: Vec<Tracked> = ghrepo::load_tracked(file.as_deref())
        .into_iter()
        .filter(|t| {
            t.gateway == gw
                && GhRepo::from_spec(&t.repo)
                    .is_some_and(|r| ghrepo::provider_name(sandbox, &r) == t.provider)
        })
        .collect();
    let hosts: Vec<String> = ghrepo::load_hosts(ghrepo::hosts_path().as_deref())
        .into_iter()
        .filter(|e| e.forge == Some(Forge::Forgejo))
        .map(|e| e.host)
        .collect();
    drop_owned(
        sandbox,
        providers,
        &mine,
        &forgejo_owned(sandbox, providers, &hosts),
        &mut |args| os(args).is_ok(),
        &mut |p| ghrepo::untrack(file.as_deref(), &gw, p),
    )
}

/// The removal itself, with the gateway call (`del`: true on success) and the untracking
/// passed in so it can be tested. `tracked` are the sandbox's tokens with an expiry (GitHub,
/// GitLab, Bitbucket); `forgejo` its Forgejo providers as (provider, host).
fn drop_owned(
    sandbox: &str,
    providers: &[Provider],
    tracked: &[Tracked],
    forgejo: &[(String, String)],
    del: &mut dyn FnMut(&[&str]) -> bool,
    untrack: &mut dyn FnMut(&str),
) -> Vec<Dropped> {
    let mut removed: Vec<Dropped> = Vec::new();
    for t in tracked {
        if del(&["provider", "delete", &t.provider]) {
            untrack(&t.provider);
            let repo = t.repo.rsplit('/').next().unwrap_or(&t.repo);
            removed.push(Dropped {
                provider: t.provider.clone(),
                token: ghrepo::token_name(&format!("{sandbox}-{repo}")),
                page: GhRepo::from_spec(&t.repo)
                    .filter(|r| !r.is_github())
                    .map(|r| (r.forge.label().to_string(), ghrepo::tokens_page(&r))),
            });
        }
    }
    for (name, host) in forgejo {
        if del(&["provider", "delete", name]) {
            let repo = name
                .strip_prefix(&ghrepo::sandbox_prefix(Forge::Forgejo, sandbox))
                .unwrap_or(name);
            removed.push(Dropped {
                provider: name.clone(),
                token: ghrepo::token_name(&format!("{sandbox}-{repo}")),
                page: Some((
                    Forge::Forgejo.label().to_string(),
                    ghrepo::forgejo_tokens_page(host),
                )),
            });
        }
    }
    let names: Vec<&String> = tracked
        .iter()
        .map(|t| &t.provider)
        .chain(forgejo.iter().map(|f| &f.0))
        .collect();
    for name in names {
        let Some(ty) = providers.iter().find(|p| &p.name == name).map(|p| &p.ty) else {
            continue;
        };
        let used = providers
            .iter()
            .any(|p| &p.ty == ty && !removed.iter().any(|r| r.provider == p.name));
        if !used {
            let _ = del(&["profile", "delete", ty]);
        }
    }
    removed
}

/// Replace the ad-hoc rule for `host:port` with a generated provider that allows encoded slashes.
pub fn allow_slashes(sandbox: &str, host: &str, port: u16, binary: &str) -> Result<String, String> {
    let name = slash_name(host);
    let _ = os(&[
        "policy",
        "update",
        sandbox,
        "--remove-endpoint",
        &format!("{host}:{port}"),
        "--wait",
        "--timeout",
        "60",
    ]);
    import_generated(&slash_profile(&name, host, port, binary), &name, sandbox)?;
    Ok(format!(
        "Allowed encoded slashes for {host} in {sandbox} ({name})"
    ))
}

/// Credential variables a profile expects. For alias lists such as
/// `[GITHUB_TOKEN, GH_TOKEN]` only the first name is prompted for.
/// `FORGEJO_HOST` is prepended when the profile is a template over it.
pub fn profile_env_keys(dir: &Path, id: &str) -> Vec<String> {
    env_keys_of(&fs::read_to_string(profile_path(dir, id)).unwrap_or_default())
}

/// `profile_env_keys` for a profile's text. Read as YAML, so a block list (`env_vars:` then
/// `- NAME` lines) counts as much as `[NAME]`; text that is not YAML falls back to line matching.
pub fn env_keys_of(body: &str) -> Vec<String> {
    let mut keys = Vec::new();
    if body.contains("${FORGEJO_HOST}") {
        keys.push("FORGEJO_HOST".to_string());
    }
    if let Ok(docs) = saphyr::Yaml::load_from_str(body) {
        if let Some(creds) = docs
            .first()
            .and_then(|d| d.as_mapping_get("credentials"))
            .and_then(|c| c.as_sequence())
        {
            for c in creds {
                let first = match c.as_mapping_get("env_vars") {
                    Some(saphyr::Yaml::Sequence(a)) => a.first().and_then(|v| v.as_str()),
                    Some(v) => v.as_str(),
                    None => None,
                };
                if let Some(k) = first.filter(|k| !k.is_empty()) {
                    keys.push(k.to_string());
                }
            }
        }
        return keys;
    }
    for line in body.lines() {
        if let Some(rest) = line.trim().strip_prefix("env_vars:") {
            let first = rest
                .trim()
                .trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .next()
                .unwrap_or_default()
                .trim();
            if !first.is_empty() {
                keys.push(first.to_string());
            }
        }
    }
    keys
}

pub fn collect(dir: &Path, selected: Option<String>) -> Snapshot {
    let mut snap = Snapshot {
        version: os(&["--version"]).unwrap_or_default().trim().to_string(),
        ..Default::default()
    };
    if let Ok(v) = os_global(&["gateway", "list", "-o", "json"]).and_then(|t| parse(&t)) {
        for g in parse_gateways(&v) {
            if gateway().is_empty() && g.active {
                set_gateway(&g.name);
            }
            snap.gateways.push(g);
        }
    }
    snap.gateway = gateway();
    for file in dockerfiles(dir) {
        let tag = dockerfile_tag(&file);
        let built = Command::new(runtime_bin())
            .args(["image", "inspect", &tag])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let freshness = if built {
            image_freshness(&tag, &file)
        } else {
            Freshness::UpToDate
        };
        snap.images.push(Image {
            tag,
            file,
            built,
            freshness,
        });
    }
    match os(&["status"]) {
        Ok(t) => {
            snap.gateway_ok = t.contains("Connected");
            snap.status_text = t;
        }
        Err(e) => {
            snap.status_text = e;
            return snap;
        }
    }
    if !snap.gateway_ok {
        return snap;
    }
    snap.driver = os(&["gateway", "info", "-o", "json"])
        .and_then(|t| parse(&t))
        .map(|v| text(&v["compute_drivers"][0]["name"]))
        .unwrap_or_default();
    match os(&["sandbox", "list", "--output", "json"]).and_then(|t| parse(&t)) {
        Ok(v) => {
            for s in v["sandboxes"].as_array().into_iter().flatten() {
                snap.sandboxes.push(Sandbox {
                    name: text(&s["name"]),
                    phase: text(&s["phase"]),
                    created: text(&s["created_at"]),
                    conditions: s["conditions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|c| Cond {
                            kind: text(&c["type"]),
                            status: text(&c["status"]),
                            message: text(&c["message"]),
                        })
                        .collect(),
                });
            }
        }
        Err(e) => snap.error = Some(format!("sandbox list: {e}")),
    }
    if let Ok(v) = os(&["provider", "list", "--output", "json"]).and_then(|t| parse(&t)) {
        for p in v["providers"].as_array().into_iter().flatten() {
            snap.providers.push(Provider {
                name: text(&p["name"]),
                ty: text(&p["type"]),
                keys: p["credential_keys"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(text)
                    .collect(),
            });
        }
    }
    if let Ok(v) = os(&["profile", "list", "--output", "json"]).and_then(|t| parse(&t)) {
        for p in v.as_array().into_iter().flatten() {
            snap.profiles.push(Profile {
                id: text(&p["id"]),
                version: p["resource_version"].as_u64().unwrap_or(0),
            });
        }
    }
    if let Some(name) = selected.filter(|n| snap.sandboxes.iter().any(|s| &s.name == n)) {
        let logs = os(&["logs", &name, "-n", "40"]).unwrap_or_else(|e| e);
        snap.limits = Some((name.clone(), limits(&snap.driver, &name)));
        snap.logs = Some((name, logs));
    }
    snap
}

/// Applied CPU/memory for a sandbox: read from the Docker container's host
/// config, or, for the vm driver, from the gateway-wide size on its process line.
fn limits(driver: &str, sandbox: &str) -> String {
    match driver {
        "docker" | "podman" => {
            docker_limits(sandbox).unwrap_or_else(|| "unknown (container not found)".into())
        }
        "vm" => vm_limits().unwrap_or_else(|| "unknown".into()),
        _ => "unknown".into(),
    }
}

fn docker_limits(sandbox: &str) -> Option<String> {
    let names = run(
        runtime_bin(),
        &[
            "ps".into(),
            "-a".into(),
            "--format".into(),
            "{{.Names}}".into(),
        ],
        &[],
    )
    .ok()?;
    let needle = format!("--{sandbox}-");
    let container = names.lines().find(|n| {
        n.starts_with("openshell-") && n.contains(&needle) && !n.ends_with("-supervisor")
    })?;
    let out = run(
        runtime_bin(),
        &[
            "inspect".into(),
            container.into(),
            "--format".into(),
            "{{.HostConfig.NanoCpus}} {{.HostConfig.Memory}}".into(),
        ],
        &[],
    )
    .ok()?;
    let mut it = out.split_whitespace();
    let nano: f64 = it.next()?.parse().ok()?;
    let mem: u64 = it.next()?.parse().ok()?;
    let cpu = if nano == 0.0 {
        "unlimited".to_string()
    } else {
        format!("{} CPU", nano / 1e9)
    };
    let mem = if mem == 0 {
        "unlimited".to_string()
    } else {
        format!("{} MiB", mem / (1024 * 1024))
    };
    Some(format!("{cpu}, {mem} memory"))
}

fn vm_limits() -> Option<String> {
    let ps = run("ps", &["-axo".into(), "command".into()], &[]).ok()?;
    let line = ps
        .lines()
        .find(|l| l.contains("openshell-driver-vm") && l.contains("--vcpus"))?;
    let arg = |flag: &str| {
        let mut it = line.split_whitespace().skip_while(|w| *w != flag);
        it.nth(1).map(String::from)
    };
    Some(format!(
        "{} vCPUs, {} MiB memory (gateway-wide, fixed)",
        arg("--vcpus")?,
        arg("--mem-mib")?
    ))
}

fn parse(t: &str) -> Result<Value, String> {
    serde_json::from_str(t).map_err(|e| e.to_string())
}

/// Poll the gateway every two seconds, or immediately when woken.
pub fn spawn_poller(
    dir: PathBuf,
    tx: Sender<Snapshot>,
    selected: Arc<Mutex<Option<String>>>,
    wake: Receiver<()>,
) {
    thread::spawn(move || loop {
        let sel = selected.lock().ok().and_then(|g| g.clone());
        let gw = gateway();
        let snap = collect(&dir, sel);
        // The user switched gateways while this ran: its calls mixed two gateways. Start over.
        if !gw.is_empty() && gateway() != gw {
            continue;
        }
        if tx.send(snap).is_err() {
            break;
        }
        let _ = wake.recv_timeout(Duration::from_secs(2));
        while wake.try_recv().is_ok() {}
    });
}

pub fn valid_host(h: &str) -> bool {
    ghrepo::valid_host(h)
}

/// Write the repo profile to a temp file, rendering `${FORGEJO_HOST}`, with an
/// optional `resource_version` appended for updates.
fn stage_profile(
    dir: &Path,
    id: &str,
    host: Option<&str>,
    resource_version: Option<u64>,
) -> Result<PathBuf, String> {
    let path = profile_path(dir, id);
    let mut body = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if body.contains("${FORGEJO_HOST}") {
        let host = host.ok_or("FORGEJO_HOST is required for this profile")?;
        if !valid_host(host) {
            return Err("FORGEJO_HOST must be a bare hostname (no scheme, port or path)".into());
        }
        body = body.replace("${FORGEJO_HOST}", host);
    }
    if let Some(v) = resource_version {
        body.push_str(&format!("\nresource_version: {v}\n"));
    }
    let out = std::env::temp_dir().join(format!("osh-{}-{id}.yaml", std::process::id()));
    fs::write(&out, body).map_err(|e| e.to_string())?;
    Ok(out)
}

fn gateway_profile_version(id: &str) -> Option<u64> {
    let v = parse(&os(&["profile", "list", "--output", "json"]).ok()?).ok()?;
    v.as_array()?
        .iter()
        .find(|p| p["id"].as_str() == Some(id))
        .map(|p| p["resource_version"].as_u64().unwrap_or(0))
}

/// The gateway's copy of a profile as YAML, without the `resource_version` bookkeeping line, so it
/// can be saved as a file or compared with one.
pub fn export_profile(id: &str) -> Result<String, String> {
    let text = os(&["profile", "export", id, "--output", "yaml"])?;
    let mut out: String = text
        .lines()
        .filter(|l| !l.starts_with("resource_version:"))
        .collect::<Vec<_>>()
        .join("\n");
    out.push('\n');
    Ok(out)
}

/// Import the profile if the gateway lacks it, or replace it with the repo copy.
pub fn sync_profile(dir: &Path, id: &str, host: Option<&str>) -> Result<String, String> {
    // Lint first (read-only), so a broken file is explained, not half-imported.
    let path = profile_path(dir, id);
    if let Ok(text) = fs::read_to_string(&path) {
        check_profile(&text).map_err(|e| {
            format!(
                "{} does not pass the profile check: {}",
                path.display(),
                e.lines().next().unwrap_or_default()
            )
        })?;
    }
    if gateway_profile_version(id).is_none() {
        let tmp = stage_profile(dir, id, host, None)?;
        let r = os(&["profile", "import", "--file", &tmp.to_string_lossy()]);
        let _ = fs::remove_file(&tmp);
        return r.map(|_| format!("Imported profile {id}"));
    }
    let exported = os(&["profile", "export", id, "--output", "yaml"])?;
    let version = exported
        .lines()
        .find_map(|l| l.strip_prefix("resource_version:"))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .ok_or("could not read resource_version from the exported profile")?;
    let tmp = stage_profile(dir, id, host, Some(version))?;
    let r = os(&["profile", "update", id, "--file", &tmp.to_string_lossy()]);
    let _ = fs::remove_file(&tmp);
    r.map(|_| format!("Updated profile {id}"))
}

/// Create a provider, importing its profile first when needed. Credentials are
/// passed through the environment so they never appear in argv.
pub fn create_provider(
    dir: &Path,
    name: &str,
    values: &[(String, String)],
) -> Result<String, String> {
    let host = values
        .iter()
        .find(|(k, _)| k == "FORGEJO_HOST")
        .map(|(_, v)| v.as_str());
    if gateway_profile_version(name).is_none() {
        sync_profile(dir, name, host)?;
    }
    let mut args: Vec<String> = ["provider", "create", "--name", name, "--type", name]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for (k, _) in values.iter().filter(|(k, _)| k != "FORGEJO_HOST") {
        args.push("--credential".into());
        args.push(k.clone());
    }
    exec("openshell", &args, values).map(|_| format!("Created provider {name}"))
}

pub fn update_credentials(name: &str, values: &[(String, String)]) -> Result<String, String> {
    for (k, _) in values {
        let args: Vec<String> = ["provider", "update", name, "--credential", k]
            .iter()
            .map(|s| s.to_string())
            .collect();
        exec("openshell", &args, values)?;
    }
    Ok(format!(
        "Updated credentials for {name}. Start a new client process to pick them up."
    ))
}

/// One thing `sync-agents` can put into a sandbox.
#[derive(Clone, Debug, Default)]
pub struct SyncItem {
    pub id: String,
    pub group: String,
    pub name: String,
    pub default: bool,
    /// `run` (commands), `upload` (staged files) or `host-only`.
    pub kind: String,
    /// Per agent: an array of commands, or an object with `src`, `dest`, `result`.
    pub agents: Vec<(String, Value)>,
}

#[derive(Clone, Debug, Default)]
pub struct SyncManifest {
    /// Directory holding the files to upload; the caller removes it.
    pub stage_dir: String,
    pub items: Vec<SyncItem>,
    /// Secret-scan findings in the staged files; `None` when gitleaks is missing.
    pub findings: Option<usize>,
    /// Each finding: file (relative to `stage_dir`), gitleaks rule id, line.
    pub flagged: Vec<(String, String, u64)>,
}

impl SyncManifest {
    /// Ids of the upload items whose staged files hold a finding.
    pub fn flagged_ids(&self) -> Vec<String> {
        let stage = Path::new(&self.stage_dir);
        let mut ids: Vec<String> = Vec::new();
        for (file, _, _) in &self.flagged {
            let at = stage.join(file);
            for i in &self.items {
                let hit = i.agents.iter().any(|(_, spec)| {
                    spec["src"]
                        .as_str()
                        .is_some_and(|src| at.starts_with(stage.join(src)))
                });
                if hit && !ids.contains(&i.id) {
                    ids.push(i.id.clone());
                }
            }
        }
        ids
    }

    /// The findings as lines for a viewer: which file, which rule, which line (values redacted).
    pub fn findings_text(&self) -> String {
        let mut out = String::from(
            "The secret scan (gitleaks, values redacted) flagged these staged files:\n\n",
        );
        for (file, rule, line) in &self.flagged {
            out.push_str(&format!("  {file}:{line}  {rule}\n"));
        }
        out.push_str("\nNothing has been uploaded. The files are copies of yours, staged in:\n  ");
        out.push_str(&self.stage_dir);
        out.push('\n');
        out
    }
}

#[derive(Clone, Debug, Default)]
pub struct SyncCheck {
    pub ok: bool,
    /// `(host, binary)` pairs the sandbox policy does not allow.
    pub missing: Vec<(String, String)>,
    /// Ids of selected items that cannot run under the policy.
    pub blocked: Vec<String>,
    /// Anonymous provider profiles that grant the missing access.
    pub fixes: Vec<String>,
    /// Profiles that grant the access but need a secret: `(profile, env keys)`.
    pub secret_fixes: Vec<(String, Vec<String>)>,
    /// Missing pairs no anonymous profile grants.
    pub unfixed: Vec<(String, String)>,
}

pub fn parse_manifest(text: &str) -> Result<SyncManifest, String> {
    let v = parse(text)?;
    let items = v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| SyncItem {
            id: text_of(&i["id"]),
            group: text_of(&i["group"]),
            name: text_of(&i["name"]),
            default: i["default"].as_bool().unwrap_or(false),
            kind: text_of(&i["kind"]),
            agents: i["agents"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        })
        .collect();
    Ok(SyncManifest {
        stage_dir: text_of(&v["stage_dir"]),
        items,
        findings: v["secret_findings"].as_array().map(|a| a.len()),
        flagged: v["secret_findings"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                (
                    text_of(&f["file"]),
                    text_of(&f["rule"]),
                    f["line"].as_u64().unwrap_or(0),
                )
            })
            .collect(),
    })
}

pub fn parse_check(text: &str) -> Result<SyncCheck, String> {
    let v = parse(text)?;
    let pair = |m: &Value| (text_of(&m["host"]), text_of(&m["binary"]));
    Ok(SyncCheck {
        ok: v["ok"].as_bool().unwrap_or(false),
        missing: v["missing"]
            .as_array()
            .into_iter()
            .flatten()
            .map(pair)
            .collect(),
        blocked: v["items"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, needs)| needs.as_array().is_some_and(|a| !a.is_empty()))
            .map(|(id, _)| id.clone())
            .collect(),
        fixes: v["fixes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text_of)
            .collect(),
        secret_fixes: v["secret_fixes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                let keys = f["keys"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(text_of)
                    .collect();
                (text_of(&f["profile"]), keys)
            })
            .collect(),
        unfixed: v["unfixed"]
            .as_array()
            .into_iter()
            .flatten()
            .map(pair)
            .collect(),
    })
}

/// Whether `key` is a GitHub token variable that `gh auth token` can supply.
pub fn is_github_key(key: &str) -> bool {
    matches!(key, "GITHUB_TOKEN" | "GH_TOKEN")
}

/// The host's GitHub CLI token, if `gh` is installed and logged in. Read from the
/// command's output, so it never appears on a command line.
pub fn gh_token() -> Option<String> {
    let out = Command::new("gh")
        .args(["auth", "token"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !t.is_empty()).then_some(t)
}

fn text_of(v: &Value) -> String {
    text(v)
}

/// The sandbox user's HOME, which differs by driver (`/workspace` on docker, `/sandbox` on vm)
/// and is the only place the default policy lets uploads land.
fn sandbox_home(sandbox: &str) -> String {
    let args: Vec<String> = [
        "sandbox",
        "exec",
        "-n",
        sandbox,
        "--timeout",
        "30",
        "--",
        "sh",
        "-c",
        "printf %s \"$HOME\"",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    exec("openshell", &args, &[])
        .map(|t| t.trim().to_string())
        .ok()
        .filter(|h| h.starts_with('/'))
        .unwrap_or_else(|| "/workspace".into())
}

/// Name of the archive a transfer stages in the sandboxes.
const STAGE: &str = ".osh-transfer.tar";

/// Run a shell script in `sandbox` as its user.
fn run_in(sandbox: &str, script: &str) -> Result<String, String> {
    os(&[
        "sandbox",
        "exec",
        "-n",
        sandbox,
        "--timeout",
        "600",
        "--",
        "sh",
        "-c",
        script,
    ])
}

/// Script for the source sandbox: archive `$HOME` into the workspace and print the number of
/// top-level entries and the archive's directory. `openshell sandbox download` only reads inside
/// the workspace, and HOME is outside it on some drivers (`/home/node` against `/workspace`),
/// so the home itself cannot be downloaded. Exit status 1 from tar (a file changed while it was
/// read) still leaves a usable archive.
fn pack_script() -> String {
    format!(
        "ws=$(pwd -P) && rm -f \"$ws/{STAGE}\" && {{ tar -C \"$HOME\" --exclude=./{STAGE} -cf \"$ws/{STAGE}\" . || [ $? -eq 1 ]; }} && ls -A \"$HOME\" | wc -l && pwd -P"
    )
}

/// Script for the target sandbox: unpack the uploaded archive over `$HOME` and delete it.
fn unpack_script(home: &str) -> String {
    let stage = format!("{}/{STAGE}", home.trim_end_matches('/'));
    format!("tar -C \"$HOME\" -xf \"{stage}\" ; rc=$? ; rm -f \"{stage}\" ; exit $rc")
}

/// Copy the home directory of sandbox `from` (workspace, dotfiles, tool caches) into the home of
/// `to` as one archive, through a temporary folder on the host that is removed afterwards. Files
/// that exist in both are overwritten; files only in `to` stay. Installed system packages (`apt`)
/// cannot move: the new sandbox's image has to carry them.
pub fn transfer_state(from: &str, to: &str) -> Result<String, String> {
    if from == to {
        return Err("Pick a different sandbox as the target".into());
    }
    let (from_home, to_home) = (sandbox_home(from), sandbox_home(to));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("osh-transfer-{}-{stamp}", std::process::id()));
    let result = copy_home(from, to, &to_home, &tmp);
    let _ = std::fs::remove_dir_all(&tmp);
    result.map(|n| format!("Transferred {n} entries from {from} ({from_home}) to {to} ({to_home})"))
}

fn copy_home(from: &str, to: &str, to_home: &str, tmp: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(tmp).map_err(|e| format!("Error: {}: {e}", tmp.display()))?;
    let out = run_in(from, &pack_script())
        .map_err(|e| format!("Error: archiving the home of {from} failed: {e}"))?;
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    let n: usize = lines.next().and_then(|l| l.parse().ok()).unwrap_or(0);
    let ws = lines.next().unwrap_or("/workspace").to_string();
    let remote = format!("{}/{STAGE}", ws.trim_end_matches('/'));
    let tmp_s = tmp.to_string_lossy().to_string();
    let result = (|| {
        os(&["sandbox", "download", from, &remote, &tmp_s])
            .map_err(|e| format!("Error: download from {from} failed: {e}"))?;
        let local = find_file(tmp, STAGE)
            .ok_or_else(|| format!("Error: the archive from {from} did not arrive"))?;
        let dest = format!("{}/", to_home.trim_end_matches('/'));
        os(&[
            "sandbox",
            "upload",
            to,
            &local.to_string_lossy(),
            &dest,
            "--no-git-ignore",
        ])
        .map_err(|e| format!("Error: upload to {to} failed: {e}"))?;
        run_in(to, &unpack_script(to_home))
            .map_err(|e| format!("Error: unpacking in {to} failed: {e}"))?;
        Ok(n)
    })();
    // Best effort: the archive is a full copy of a home, so it must not stay behind.
    let _ = run_in(from, &format!("rm -f \"{remote}\""));
    result
}

/// A file called `name` anywhere under `dir`.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n == name) {
            return Some(p);
        }
    }
    None
}

/// Put the host's tmux config and a filtered copy of its global git config (allowlist in
/// `agents::filter_git_config`) in a sandbox just created, so its tmux has the host's prefix and
/// bindings and its git the host's aliases and colours from the first shell. `name` is empty when
/// OpenShell generated it: the sandbox is then the first one not in `before`. One wait for the
/// sandbox serves both. Each copy is skipped (with the reason) when the host has none, the secret
/// scan flags it, or the image lacks the tool; a skip never fails the creation.
pub fn install_sandbox_confs(dir: &Path, name: &str, before: &[String]) -> Result<String, String> {
    let has_tmux = agents::Env::detect(dir, "").tmux_conf.is_some();
    let git = agents::gitconfig_for_new_sandbox(dir);
    if !has_tmux && !matches!(git, Ok(Some(_))) {
        let git_note = match &git {
            Err(e) => format!("git config not copied: {e}"),
            _ => "no host git config to copy".into(),
        };
        return Ok(format!(
            "Sandbox created (no host tmux config to copy; {git_note})"
        ));
    }
    let result = upload_sandbox_confs(dir, name, before, has_tmux, &git);
    if let Ok(Some(f)) = &git {
        if let Some(stage) = f.parent() {
            let _ = std::fs::remove_dir_all(stage);
        }
    }
    result
}

fn upload_sandbox_confs(
    dir: &Path,
    name: &str,
    before: &[String],
    has_tmux: bool,
    git: &Result<Option<PathBuf>, String>,
) -> Result<String, String> {
    let mut name = name.to_string();
    let mut probe = String::new();
    // A detached create can return before the sandbox accepts commands.
    for _ in 0..30 {
        if name.is_empty() {
            if let Ok(v) = os(&["sandbox", "list", "--output", "json"]).and_then(|t| parse(&t)) {
                name = v["sandboxes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| text(&s["name"]))
                    .find(|n| !before.contains(n))
                    .unwrap_or_default();
            }
        }
        if !name.is_empty() {
            let script = "printf '%s\\n' \"$HOME\"; \
                command -v tmux >/dev/null 2>&1 && echo tmux; \
                command -v git >/dev/null 2>&1 && echo git; true";
            if let Ok(t) = exec("openshell", &sandbox_sh(&name, script), &[]) {
                probe = t;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    let mut lines = probe.lines();
    let home = lines.next().unwrap_or("").trim().to_string();
    if name.is_empty() || !home.starts_with('/') {
        return Err("Sandbox created, host configs not copied: sandbox not ready".into());
    }
    let tools: Vec<&str> = lines.collect();
    let tmux = if !has_tmux {
        "no host tmux config to copy".to_string()
    } else if !tools.contains(&"tmux") {
        format!("{name}'s image has no tmux")
    } else {
        match upload_tmux_conf(dir, &name, &home) {
            Ok(m) => m,
            Err(e) => format!("tmux config not copied: {e}"),
        }
    };
    let gitmsg = match git {
        Err(e) => format!("git config not copied: {e}"),
        Ok(None) => "no host git config to copy".into(),
        Ok(Some(_)) if !tools.contains(&"git") => format!("{name}'s image has no git"),
        Ok(Some(f)) => match upload_git_conf(&name, &home, f) {
            Ok(m) => m,
            Err(e) => format!("git config not copied: {e}"),
        },
    };
    Ok(format!("Sandbox created ({tmux}; {gitmsg})"))
}

/// Upload the filtered git config and make the sandbox's `~/.gitconfig` include it.
fn upload_git_conf(name: &str, home: &str, file: &Path) -> Result<String, String> {
    exec(
        "openshell",
        &sandbox_sh(name, "mkdir -p \"$HOME/.config/osh\""),
        &[],
    )?;
    let args: Vec<String> = [
        "sandbox",
        "upload",
        name,
        &file.to_string_lossy(),
        &format!("{home}/.config/osh/gitconfig"),
        "--no-git-ignore",
    ]
    .map(String::from)
    .to_vec();
    exec("openshell", &args, &[])?;
    exec(
        "openshell",
        &sandbox_sh(name, &agents::git_include_script()),
        &[],
    )?;
    Ok("host git config copied".into())
}

fn upload_tmux_conf(dir: &Path, name: &str, home: &str) -> Result<String, String> {
    // The image's tmux can be older than the host's: leave out the options it does not have.
    let known = exec(
        "openshell",
        &sandbox_sh(
            name,
            "tmux -L oshchk -f /dev/null new-session -d 'sleep 30' >/dev/null 2>&1; \
             for f in -g -gw -gs; do tmux -L oshchk show-options $f; done 2>/dev/null; \
             tmux -L oshchk kill-server >/dev/null 2>&1; true",
        ),
        &[],
    )
    .ok()
    .map(|t| {
        t.lines()
            .filter_map(|l| l.split_whitespace().next())
            .map(|o| o.split('[').next().unwrap_or(o).to_string())
            .collect::<std::collections::HashSet<_>>()
    })
    .filter(|k| !k.is_empty());
    let conf = match agents::tmux_conf_for_new_sandbox(dir, known.as_ref()) {
        Ok(Some(c)) => c,
        Ok(None) => return Ok("no host tmux config to copy".into()),
        Err(e) => return Err(e),
    };
    let args: Vec<String> = [
        "sandbox",
        "upload",
        name,
        &conf.to_string_lossy(),
        &format!("{home}/.tmux.conf"),
        "--no-git-ignore",
    ]
    .map(String::from)
    .to_vec();
    let copied = if have("gitleaks") {
        "host tmux config copied (secret scan clean)"
    } else {
        "host tmux config copied unscanned: gitleaks is missing (Install gitleaks in the command palette)"
    };
    let result = exec("openshell", &args, &[]).map(|_| copied.to_string());
    if let Some(stage) = conf.parent() {
        let _ = std::fs::remove_dir_all(stage);
    }
    result
}

/// `openshell sandbox exec` arguments that run `script` under `sh -c` in `sandbox`.
fn sandbox_sh(sandbox: &str, script: &str) -> Vec<String> {
    [
        "sandbox",
        "exec",
        "-n",
        sandbox,
        "--timeout",
        "30",
        "--",
        "sh",
        "-c",
        script,
    ]
    .map(String::from)
    .to_vec()
}

/// The profile (YAML) that grants exactly what the selected installs still lack.
pub fn sync_gen_profile(
    dir: &Path,
    sandbox: &str,
    agent: &str,
    ids: &[String],
    name: &str,
) -> Result<String, String> {
    let env = agents::Env::detect(dir, &sandbox_home(sandbox));
    agents::gen_profile(&env, agent, sandbox, &gateway(), Some(ids), name)
}

/// What the gateway says when it will not merge a provider into the sandbox's policy,
/// e.g. github's credentialed github.com next to a profile that allows it uninspected.
pub const ATTACH_CONFLICT: &str = "not in a state required";

/// Names of the providers attached to a sandbox.
pub fn attached_providers(sandbox: &str) -> Vec<String> {
    os(&["sandbox", "provider", "list", sandbox, "--output", "json"])
        .ok()
        .and_then(|t| parse(&t).ok())
        .map(|v| {
            v["providers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// One network endpoint a provider profile opens.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct EndpointPerm {
    pub host: String,
    pub port: u64,
    pub protocol: String,
    /// `read-only`, `read-write`, `full`, or empty when `rules` spell it out.
    pub access: String,
    pub enforcement: String,
    /// `METHOD path` allow rules.
    pub rules: Vec<String>,
}

/// What an attached provider's profile grants a sandbox.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct ProviderPerm {
    pub name: String,
    pub ty: String,
    pub category: String,
    pub description: String,
    /// `ENV_VAR (auth style, required|optional)` per credential.
    pub credentials: Vec<String>,
    pub endpoints: Vec<EndpointPerm>,
    pub binaries: Vec<String>,
    /// Set when the profile could not be found.
    pub error: Option<String>,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().map(text).collect()
}

/// Read one entry of `openshell profile list --output json`.
fn parse_perm(name: &str, ty: &str, p: &Value) -> ProviderPerm {
    let credentials = p["credentials"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let vars = strings(&c["env_vars"]);
            let who = if vars.is_empty() {
                text(&c["name"])
            } else {
                vars.join(", ")
            };
            let req = if c["required"].as_bool().unwrap_or(false) {
                "required"
            } else {
                "optional"
            };
            format!("{who} ({}, {req})", text(&c["auth_style"]))
        })
        .collect();
    let endpoints = p["endpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| EndpointPerm {
            host: text(&e["host"]),
            port: e["port"].as_u64().unwrap_or(0),
            protocol: text(&e["protocol"]),
            access: text(&e["access"]),
            enforcement: text(&e["enforcement"]),
            rules: e["rules"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| {
                    format!(
                        "{} {}",
                        text(&r["allow"]["method"]),
                        text(&r["allow"]["path"])
                    )
                })
                .collect(),
        })
        .collect();
    ProviderPerm {
        name: name.into(),
        ty: ty.into(),
        category: text(&p["category"]),
        description: text(&p["description"]),
        credentials,
        endpoints,
        binaries: strings(&p["binaries"]),
        error: None,
    }
}

/// What each provider attached to `sandbox` allows, from the gateway's profiles.
pub fn provider_permissions(sandbox: &str, providers: &[Provider]) -> Vec<ProviderPerm> {
    let profiles = os(&["profile", "list", "--output", "json"])
        .and_then(|t| parse(&t))
        .unwrap_or_default();
    attached_providers(sandbox)
        .into_iter()
        .map(|name| {
            let ty = providers
                .iter()
                .find(|p| p.name == name)
                .map_or(name.clone(), |p| p.ty.clone());
            match profiles
                .as_array()
                .and_then(|a| a.iter().find(|p| p["id"].as_str() == Some(&ty)))
            {
                Some(p) => parse_perm(&name, &ty, p),
                None => ProviderPerm {
                    name,
                    ty: ty.clone(),
                    error: Some(format!("profile {ty} not found on the gateway")),
                    ..Default::default()
                },
            }
        })
        .collect()
}

/// Import a generated profile, create its provider (no credential) and attach it.
pub fn import_generated(yaml: &str, name: &str, sandbox: &str) -> Result<(), String> {
    let tmp = std::env::temp_dir().join(format!("osh-{name}.yaml"));
    fs::write(&tmp, yaml).map_err(|e| e.to_string())?;
    let imported = os(&["profile", "import", "--file", &tmp.to_string_lossy()]);
    let _ = fs::remove_file(&tmp);
    imported?;
    // Nothing is left behind when a step fails: the caller forgets the name on error.
    if let Err(e) = os(&["provider", "create", "--name", name, "--type", name]) {
        let _ = os(&["profile", "delete", name]);
        return Err(e);
    }
    if let Err(e) = os(&[
        "sandbox",
        "provider",
        "attach",
        sandbox,
        name,
        "--wait",
        "--timeout",
        "30",
    ]) {
        let _ = os(&["provider", "delete", name]);
        let _ = os(&["profile", "delete", name]);
        return Err(e);
    }
    Ok(())
}

/// Detach a generated provider and remove it and its profile.
pub fn drop_generated(sandbox: &str, name: &str) -> Result<String, String> {
    os(&[
        "sandbox",
        "provider",
        "detach",
        sandbox,
        name,
        "--wait",
        "--timeout",
        "30",
    ])?;
    os(&["provider", "delete", name])?;
    os(&["profile", "delete", name])?;
    Ok(format!("Removed {name} from {sandbox}"))
}

/// `agent` is `claude-code`, `pi` or `both`.
pub fn sync_manifest(dir: &Path, sandbox: &str, agent: &str) -> Result<SyncManifest, String> {
    let env = agents::Env::detect(dir, &sandbox_home(sandbox));
    agents::manifest(&env, agent).and_then(|t| parse_manifest(&t))
}

/// Check the selected items against the sandbox's effective network policy.
pub fn sync_check(
    dir: &Path,
    sandbox: &str,
    agent: &str,
    ids: &[String],
) -> Result<SyncCheck, String> {
    let env = agents::Env::detect(dir, "/workspace");
    agents::check(&env, agent, sandbox, &gateway(), Some(ids)).and_then(|t| parse_check(&t))
}

#[cfg(test)]
mod tests {
    #[test]
    fn credential_keys_come_from_inline_and_block_lists_and_yml_files_count() {
        let inline = "id: a\ncredentials:\n  - name: t\n    env_vars: [GITHUB_TOKEN, GH_TOKEN]\n";
        assert_eq!(super::env_keys_of(inline), vec!["GITHUB_TOKEN"]);
        let block = "id: a\ncredentials:\n  - name: t\n    env_vars:\n      - FOO_TOKEN\n      - FOO_ALT\n  - name: u\n    env_vars: BAR\n";
        assert_eq!(super::env_keys_of(block), vec!["FOO_TOKEN", "BAR"]);
        let tpl = "id: f\nendpoints:\n  - host: ${FORGEJO_HOST}\ncredentials:\n  - env_vars: [FORGEJO_TOKEN]\n";
        assert_eq!(
            super::env_keys_of(tpl),
            vec!["FORGEJO_HOST", "FORGEJO_TOKEN"]
        );
        let d = std::env::temp_dir().join(format!("osh-yml-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("providers")).unwrap();
        std::fs::write(d.join("providers/mine.yml"), block).unwrap();
        std::fs::write(d.join("providers/x.yaml"), inline).unwrap();
        assert_eq!(super::profile_ids(&d), vec!["mine", "x"]);
        assert_eq!(
            super::profile_path(&d, "mine"),
            d.join("providers/mine.yml")
        );
        assert_eq!(
            super::profile_env_keys(&d, "mine"),
            vec!["FOO_TOKEN", "BAR"]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_profile_check_wants_yaml_with_an_id_and_lint_output_is_reduced_to_its_errors() {
        assert!(super::check_profile("id: x\nendpoints: []\n").is_ok());
        assert!(super::check_profile("endpoints: []\n")
            .unwrap_err()
            .contains("id"));
        let e = super::check_profile("id: x\nendpoints: [\n").unwrap_err();
        assert!(e.contains("line"), "{e}");
        let out = "Provider profile diagnostics:\n  error a.yaml profile=npm field=id custom provider profile 'npm' already exists\n  error a.yaml profile=npm field=endpoints host must not be empty\nError:   x provider profile lint failed";
        assert_eq!(
            super::lint_errors(out).as_deref(),
            Some("field=endpoints host must not be empty")
        );
        assert_eq!(super::lint_errors("connection refused"), None);
    }

    #[test]
    fn the_gateway_list_marks_the_cli_default() {
        let v: Value = serde_json::from_str(
            r#"[{"name":"a","endpoint":"e1","active":false},{"name":"b","endpoint":"e2","active":true}]"#,
        )
        .unwrap();
        let g = parse_gateways(&v);
        assert_eq!(g.len(), 2);
        assert!(!g[0].active && g[1].active && g[1].name == "b");
    }

    use super::*;

    #[test]
    fn container_programs_follow_the_selected_runtime() {
        assert_eq!(bin_for(Some(Runtime::Podman)), "podman");
        assert_eq!(bin_for(Some(Runtime::Docker)), "docker");
    }

    #[test]
    fn without_a_runtime_the_familiar_docker_error_shows() {
        assert_eq!(bin_for(None), "docker");
    }

    #[test]
    fn profile_json_becomes_permissions() {
        let v: Value = serde_json::from_str(
            r#"{"category":"agent","description":"d","binaries":["/usr/bin/x"],
            "credentials":[{"name":"t","env_vars":["TOK"],"required":true,"auth_style":"bearer"}],
            "endpoints":[{"host":"h.io","port":443,"protocol":"rest","access":"read-only","enforcement":"enforce"},
              {"host":"g.com","port":443,"protocol":"rest","enforcement":"enforce",
               "rules":[{"allow":{"method":"POST","path":"/a/git-receive-pack"}}]}]}"#,
        )
        .unwrap();
        let p = parse_perm("n", "ty", &v);
        assert_eq!(p.credentials, ["TOK (bearer, required)"]);
        assert_eq!(p.binaries, ["/usr/bin/x"]);
        assert_eq!(p.endpoints[0].access, "read-only");
        assert_eq!(p.endpoints[1].rules, ["POST /a/git-receive-pack"]);
        let text: String = crate::ui::perm_lines(&[p])
            .iter()
            .map(|l| l.to_string() + "\n")
            .collect();
        assert!(text.contains("h.io:443") && text.contains("custom rules"));
        assert!(text.contains("POST /a/git-receive-pack") && text.contains("/usr/bin/x"));
    }

    fn r(forge: Forge, host: &str, owner: &str) -> GhRepo {
        GhRepo {
            forge,
            host: host.into(),
            owner: owner.into(),
            repo: "app".into(),
        }
    }

    #[test]
    fn repo_status_problems_tell_the_user_what_to_fix() {
        let r = r(Forge::GitHub, "github.com", "acme");
        assert_eq!(repo_status_problem(200, &r), None);
        assert!(repo_status_problem(401, &r).unwrap().contains("expired"));
        assert!(repo_status_problem(403, &r).unwrap().contains("approve"));
        let nf = repo_status_problem(404, &r).unwrap();
        assert!(nf.contains("acme/app") && nf.contains("Resource owner to acme"));
        assert!(repo_status_problem(500, &r).unwrap().contains("500"));
    }

    #[test]
    fn forgejo_status_problems_use_forgejo_words() {
        let r = r(Forge::Forgejo, "example.org", "acme");
        assert_eq!(repo_status_problem(200, &r), None);
        let p401 = repo_status_problem(401, &r).unwrap();
        assert!(p401.contains("example.org") && !p401.contains("expired"));
        assert!(repo_status_problem(403, &r)
            .unwrap()
            .contains("write:repository"));
        let nf = repo_status_problem(404, &r).unwrap();
        assert!(
            nf.contains("acme/app") && !nf.contains("GitHub") && !nf.contains("Resource owner")
        );
        assert!(nf.contains("Specific repositories"));
        assert!(repo_status_problem(500, &r).unwrap().contains("500"));
    }

    #[test]
    fn gitlab_and_bitbucket_status_problems_name_what_to_fix() {
        let gl = r(Forge::GitLab, "gitlab.com", "grp/sub");
        let p = |s, r: &GhRepo| repo_status_problem(s, r).unwrap();
        assert!(p(401, &gl).contains("gitlab.com") && p(401, &gl).contains("expired"));
        assert!(p(403, &gl).contains("read_api"));
        assert!(p(404, &gl).contains("grp/sub/app") && p(404, &gl).contains("project path"));
        let bc = r(Forge::BitbucketCloud, "bitbucket.org", "ws");
        assert!(p(401, &bc).starts_with("Bitbucket rejected"));
        assert!(p(403, &bc).contains("Repositories Read"));
        assert!(p(404, &bc).contains("ws/app"));
        let dc = r(Forge::BitbucketDc, "example.net", "PROJ");
        assert!(p(403, &dc).contains("example.net") && p(403, &dc).contains("Repository read"));
        assert!(p(502, &dc).contains("502"));
        assert_eq!(repo_status_problem(200, &dc), None);
        assert_eq!(host_name(&dc), "example.net");
        assert_eq!(host_name(&bc), "Bitbucket");
    }

    fn prov(name: &str, ty: &str) -> Provider {
        Provider {
            name: name.into(),
            ty: ty.into(),
            keys: vec![],
        }
    }

    #[test]
    fn a_deleted_sandbox_takes_its_forgejo_providers_and_unused_profiles() {
        let providers = vec![
            prov("forgejo-dev-app", "fj-example-org-acme-app"),
            // shared token: not the sandbox's own, shares the profile
            prov("forgejo-app", "fj-example-org-acme-app"),
            prov("forgejo-dev-lib", "fj-example-org-acme-lib"),
            prov("forgejo-other-app", "fj-example-org-acme-app"),
            prov("forgejo", "forgejo"),
            prov("github-dev-app", "gh-acme-app"),
        ];
        let hosts = vec!["example.org".to_string()];
        let owned = forgejo_owned("dev", &providers, &hosts);
        assert_eq!(
            owned,
            [
                ("forgejo-dev-app".to_string(), "example.org".to_string()),
                ("forgejo-dev-lib".to_string(), "example.org".to_string()),
            ]
        );
        assert!(forgejo_owned("dev", &providers, &[]).is_empty());
        let mut calls: Vec<String> = Vec::new();
        let gone = drop_owned(
            "dev",
            &providers,
            &[],
            &owned,
            &mut |a| {
                calls.push(a.join(" "));
                true
            },
            &mut |_| {},
        );
        assert_eq!(gone.len(), 2);
        assert_eq!(
            gone[0].page,
            Some((
                "Forgejo".to_string(),
                "https://example.org/user/settings/applications".to_string()
            ))
        );
        assert_eq!(gone[0].token, "osh-dev-app");
        // the app profile is still used by two other providers; the lib one is not
        assert!(calls.contains(&"profile delete fj-example-org-acme-lib".to_string()));
        assert!(!calls
            .iter()
            .any(|c| c == "profile delete fj-example-org-acme-app"));
    }

    #[test]
    fn a_deleted_sandbox_takes_its_tracked_gitlab_and_github_tokens() {
        let gl = r(Forge::GitLab, "gitlab.com", "grp/sub");
        let t = |p: &str, repo: String| Tracked {
            gateway: "vm".into(),
            provider: p.into(),
            expiry_ms: 1,
            repo,
        };
        let tracked = [
            t("gitlab-dev-app", gl.spec()),
            t("github-dev-app", "acme/app".into()),
        ];
        let providers = vec![
            prov("gitlab-dev-app", "gl-gitlab-com-grp-sub-app"),
            prov("github-dev-app", "gh-acme-app"),
            prov("github-app", "gh-acme-app"),
        ];
        let mut calls: Vec<String> = Vec::new();
        let mut untracked: Vec<String> = Vec::new();
        let gone = drop_owned(
            "dev",
            &providers,
            &tracked,
            &[],
            &mut |a| {
                calls.push(a.join(" "));
                true
            },
            &mut |p| untracked.push(p.into()),
        );
        assert_eq!(untracked, ["gitlab-dev-app", "github-dev-app"]);
        assert_eq!(gone[0].token, "osh-dev-app");
        assert_eq!(
            gone[0].page,
            Some((
                "GitLab".to_string(),
                "https://gitlab.com/grp/sub/app/-/settings/access_tokens".to_string()
            ))
        );
        assert_eq!(gone[1].page, None);
        assert!(calls.contains(&"profile delete gl-gitlab-com-grp-sub-app".to_string()));
        assert!(!calls.contains(&"profile delete gh-acme-app".to_string()));
    }

    #[test]
    fn repo_profiles_answer_for_their_hosts_without_credentials() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        assert_eq!(profiles_for_host(&dir, "registry.npmjs.org"), ["npm"]);
        let git = profiles_for_host(&dir, "github.com");
        assert!(git.contains(&"claude-marketplace".to_string()));
        assert!(!git.contains(&"github".to_string()), "github needs a token");
        assert!(profiles_for_host(&dir, "example.com").is_empty());
        assert_eq!(profiles_for_host(&dir, "static.crates.io"), ["rust"]);
        assert_eq!(profiles_for_host(&dir, "proxy.golang.org"), ["go"]);
        assert!(
            git.iter()
                .all(|id| !["go", "js", "rust"].contains(&id.as_str())),
            "language profiles are not offered for github.com: {git:?}"
        );
    }

    #[test]
    fn every_opt_in_profile_exists_and_is_anonymous() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        for id in crate::assets::OPT_IN_PROFILES {
            assert!(profile_ids(&dir).contains(&id.to_string()), "{id}");
            assert!(
                profile_env_keys(&dir, id).is_empty(),
                "{id} needs no secret"
            );
        }
    }

    #[test]
    fn generated_slash_profile_names_one_host_and_binary() {
        let y = slash_profile(
            &slash_name("registry.npmjs.org"),
            "registry.npmjs.org",
            443,
            "/usr/local/bin/node",
        );
        assert!(y.starts_with("id: slash-registry-npmjs-org\n"));
        assert!(y.contains("    allow_encoded_slash: true\n"));
        assert!(y.ends_with("binaries: [/usr/local/bin/node]\n"));
    }

    #[test]
    fn transfer_stages_one_archive_through_the_workspace() {
        let pack = pack_script();
        assert!(pack.contains("tar -C \"$HOME\"") && pack.contains("$ws/.osh-transfer.tar"));
        assert!(
            pack.contains("--exclude=./.osh-transfer.tar"),
            "never archives itself"
        );
        let un = unpack_script("/home/node/");
        assert!(un.contains("-xf \"/home/node/.osh-transfer.tar\""), "{un}");
        assert!(un.contains("rm -f") && un.contains("exit $rc"));
        assert!(transfer_state("a", "a").unwrap_err().contains("different"));
    }

    #[test]
    fn find_file_looks_in_subfolders() {
        let d = std::env::temp_dir().join(format!("osh-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/b/x.tar"), "t").unwrap();
        assert!(find_file(&d, "x.tar").is_some() && find_file(&d, "y.tar").is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
