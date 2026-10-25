//! Thin wrappers around the `openshell`, `docker` and `just` CLIs.
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        mpsc::{Receiver, Sender},
        Arc, Mutex,
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

/// FNV-1a 64-bit of the Dockerfile contents; stable across Rust versions.
pub fn dockerfile_hash(file: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in fs::read(file).unwrap_or_default() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
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
    let label = Command::new("docker")
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
                    (p.extension()? == "yaml").then(|| p.file_stem()?.to_str().map(String::from))?
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn profile_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("providers").join(format!("{id}.yaml"))
}

/// Credential variables a profile expects. For alias lists such as
/// `[GITHUB_TOKEN, GH_TOKEN]` only the first name is prompted for.
/// `FORGEJO_HOST` is prepended when the profile is a template over it.
pub fn profile_env_keys(dir: &Path, id: &str) -> Vec<String> {
    let body = fs::read_to_string(profile_path(dir, id)).unwrap_or_default();
    let mut keys = Vec::new();
    if body.contains("${FORGEJO_HOST}") {
        keys.push("FORGEJO_HOST".to_string());
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
        for g in v.as_array().into_iter().flatten() {
            let name = text(&g["name"]);
            if gateway().is_empty() && g["active"].as_bool() == Some(true) {
                set_gateway(&name);
            }
            snap.gateways.push(Gateway {
                name,
                endpoint: text(&g["endpoint"]),
            });
        }
    }
    snap.gateway = gateway();
    for file in dockerfiles(dir) {
        let tag = dockerfile_tag(&file);
        let built = Command::new("docker")
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
        "docker" => {
            docker_limits(sandbox).unwrap_or_else(|| "unknown (container not found)".into())
        }
        "vm" => vm_limits().unwrap_or_else(|| "unknown".into()),
        _ => "unknown".into(),
    }
}

fn docker_limits(sandbox: &str) -> Option<String> {
    let names = run(
        "docker",
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
        "docker",
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
        if tx.send(collect(&dir, sel)).is_err() {
            break;
        }
        let _ = wake.recv_timeout(Duration::from_secs(2));
        while wake.try_recv().is_ok() {}
    });
}

pub fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
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

/// Import the profile if the gateway lacks it, or replace it with the repo copy.
pub fn sync_profile(dir: &Path, id: &str, host: Option<&str>) -> Result<String, String> {
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
    r.map(|_| format!("Updated profile {id}. Running sandboxes may need a restart to pick it up."))
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

/// Directory holding `sync-agents`: the config directory, or else the checkout this binary
/// was built in (`<repo>/tui/target/release/osh-tui`), for when `OSH_DIR` is not set.
fn script_dir(dir: &Path) -> PathBuf {
    if dir.join("sync-agents").is_file() {
        return dir.to_path_buf();
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| {
            e.ancestors()
                .find(|a| a.join("sync-agents").is_file())
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(|| dir.to_path_buf())
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

/// The profile (YAML) that grants exactly what the selected installs still lack.
pub fn sync_gen_profile(
    dir: &Path,
    sandbox: &str,
    agent: &str,
    ids: &[String],
    name: &str,
) -> Result<String, String> {
    let mut args: Vec<String> = [
        "--gen-profile",
        sandbox,
        "--name",
        name,
        "--agent",
        agent,
        "--only",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(ids.join(","));
    let gw = gateway();
    if !gw.is_empty() {
        args.extend(["--gateway".into(), gw]);
    }
    run(
        &script_dir(dir).join("sync-agents").to_string_lossy(),
        &args,
        &[],
    )
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
    let script = script_dir(dir).join("sync-agents");
    let args = [
        "--json".to_string(),
        "--agent".into(),
        agent.into(),
        "--home".into(),
        sandbox_home(sandbox),
    ];
    run(&script.to_string_lossy(), &args, &[]).and_then(|t| parse_manifest(&t))
}

/// Check the selected items against the sandbox's effective network policy.
/// The script exits 3 when something is blocked but still prints the report.
pub fn sync_check(
    dir: &Path,
    sandbox: &str,
    agent: &str,
    ids: &[String],
) -> Result<SyncCheck, String> {
    let mut args: Vec<String> = vec![
        "--preflight".into(),
        sandbox.into(),
        "--json".into(),
        "--agent".into(),
        agent.into(),
        "--only".into(),
        ids.join(","),
    ];
    let gw = gateway();
    if !gw.is_empty() {
        args.extend(["--gateway".into(), gw]);
    }
    let out = Command::new(script_dir(dir).join("sync-agents"))
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("sync-agents: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_check(&stdout).map_err(|_| String::from_utf8_lossy(&out.stderr).trim().to_string())
}
