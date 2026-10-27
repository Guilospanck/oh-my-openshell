//! Describes how the host's agent setup (Claude Code and pi) would be reproduced in a sandbox:
//! skills, plugins and packages to install, files to upload, and the network each install needs.
//! Reads install records and stages copies of the uploads; installs and uploads nothing itself.
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const AGENTS: [&str; 2] = ["claude-code", "pi"];
/// Skills a package installer writes itself: not copied, not "unclassified".
const PACKAGE_SKILLS: &[&str] = &["graphify"];
const SKIP_DIRS: &[&str] = &["synced"];
/// Installed on the host but not wanted in the sandbox.
const EXCLUDED_SKILLS: &[&str] = &[
    "openshell-cli",
    "debug-inference",
    "debug-openshell-cluster",
    "generate-sandbox-policy",
];
/// Plugins that need language-server binaries the images do not ship.
const HOST_ONLY_PLUGINS: &[&str] = &["clangd-lsp", "gopls-lsp", "rust-analyzer-lsp", "swift-lsp"];
/// The `skills` CLI that `npx` runs, pinned so a sandbox gets what the host tested with.
const SKILLS_CLI: &str = "skills@1.7.4";
/// Claude plugin -> the equivalent install command for pi, where one exists.
const PI_PLUGINS: &[(&str, &str)] =
    &[("superpowers", "pi install git:github.com/obra/superpowers")];

const BIN_NODE: &str = "/usr/local/bin/node";
const BIN_GIT: &str = "/usr/bin/git";
const BIN_UV: &str = "/usr/local/bin/uv";
const BIN_CLAUDE: &str = "/usr/local/bin/claude";

/// Network each install step needs: (host, binary).
fn needs_of(kind: &str) -> Vec<(String, String)> {
    let p = |h: &str, b: &str| (h.to_string(), b.to_string());
    match kind {
        "npx" => vec![p("registry.npmjs.org", BIN_NODE), p("github.com", BIN_GIT)],
        "claude-plugin" => vec![p("github.com", BIN_CLAUDE)],
        "pi-plugin" => vec![p("github.com", BIN_GIT), p("registry.npmjs.org", BIN_NODE)],
        "uv" => vec![p("pypi.org", BIN_UV), p("files.pythonhosted.org", BIN_UV)],
        _ => vec![],
    }
}

/// Where everything is read from; `detect` fills it from the process environment.
#[derive(Clone, Debug)]
pub struct Env {
    pub claude: PathBuf,
    pub lock: PathBuf,
    pub providers_dir: PathBuf,
    pub gitleaks_config: PathBuf,
    /// The project whose Claude memory is offered for upload.
    pub memory_repo: PathBuf,
    /// graphify's version when already known; otherwise `graphify --version` is run.
    pub graphify_version: Option<String>,
    /// OpenShell sets HOME inside sandboxes (`/workspace` on docker, `/sandbox` on vm).
    pub sandbox_home: String,
}

impl Env {
    pub fn detect(data_dir: &Path, sandbox_home: &str) -> Env {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let memory_repo = std::env::var_os("OSH_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        Env {
            claude: std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
            lock: home.join(".agents/.skill-lock.json"),
            providers_dir: data_dir.join("providers"),
            gitleaks_config: data_dir.join(".gitleaks.toml"),
            memory_repo,
            graphify_version: None,
            sandbox_home: sandbox_home.trim_end_matches('/').to_string(),
        }
    }

    fn plugins(&self) -> PathBuf {
        self.claude.join("plugins/installed_plugins.json")
    }

    fn marketplaces(&self) -> PathBuf {
        self.claude.join("plugins/known_marketplaces.json")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Spec {
    Run(Vec<String>),
    Upload {
        src: String,
        dest: String,
        result: String,
    },
}

#[derive(Clone, Debug)]
pub struct Item {
    pub group: &'static str,
    pub name: String,
    pub detail: String,
    /// `run`, `upload` or `host-only`.
    pub kind: &'static str,
    pub default: bool,
    pub agents: Vec<(&'static str, Spec)>,
    pub needs: Vec<(String, String)>,
    pub note: String,
}

impl Item {
    pub fn id(&self) -> String {
        format!("{}:{}", self.group, self.name)
    }

    fn to_json(&self) -> Value {
        let agents: Map<String, Value> = self
            .agents
            .iter()
            .map(|(a, s)| {
                let v = match s {
                    Spec::Run(c) => json!(c),
                    Spec::Upload { src, dest, result } => {
                        json!({"src": src, "dest": dest, "result": result})
                    }
                };
                (a.to_string(), v)
            })
            .collect();
        json!({
            "group": self.group, "id": self.id(), "name": self.name, "detail": self.detail,
            "kind": self.kind, "default": self.default, "agents": agents,
            "needs": self.needs.iter().map(|(h, b)| json!([h, b])).collect::<Vec<_>>(),
            "note": self.note,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn item(
    group: &'static str,
    name: &str,
    detail: String,
    agents: Vec<(&'static str, Spec)>,
    kind: &'static str,
    default: bool,
    mut needs: Vec<(String, String)>,
    note: &str,
) -> Item {
    needs.sort();
    needs.dedup();
    Item {
        group,
        name: name.into(),
        detail,
        kind,
        default: default && kind != "host-only",
        agents,
        needs,
        note: note.into(),
    }
}

fn load(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn run_out(prog: &str, args: &[&str]) -> String {
    Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

fn npx_skills(env: &Env) -> Vec<Item> {
    let lock = load(&env.lock);
    let skills = lock
        .as_ref()
        .and_then(|l| l.get("skills"))
        .and_then(Value::as_object);
    let mut out = Vec::new();
    for (name, v) in skills.into_iter().flatten() {
        if EXCLUDED_SKILLS.contains(&name.as_str()) {
            continue;
        }
        let Some(source) = v.get("source").and_then(Value::as_str) else {
            continue;
        };
        let hash = v
            .get("skillFolderHash")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let hash: String = hash.chars().take(7).collect();
        // -a is required: without it the CLI writes into every agent it knows (dozens of dot-directories).
        // claude-code lands in ~/.claude/skills, pi in ~/.agents/skills; both are real copies (--copy).
        let cmd = |agent: &str| {
            Spec::Run(vec![format!(
                "npx --yes {SKILLS_CLI} add {source} -g -a {agent} -s {name} -y --copy"
            )])
        };
        out.push(item(
            "npx-skills",
            name,
            format!("{source} @ {hash}"),
            AGENTS.iter().map(|a| (*a, cmd(a))).collect(),
            "run",
            true,
            needs_of("npx"),
            "",
        ));
    }
    out
}

/// The command that registers a marketplace in the sandbox, or None when its source is not GitHub.
/// A fresh sandbox knows no marketplaces, so `plugin install` alone fails with "not found".
fn marketplace_add(env: &Env, market: &str) -> Option<String> {
    let all = load(&env.marketplaces())?;
    let src = all.get(market)?.get("source")?;
    if src.get("source")?.as_str()? != "github" {
        return None;
    }
    let repo = src.get("repo")?.as_str().filter(|r| !r.is_empty())?;
    Some(format!("claude plugin marketplace add {repo}"))
}

fn is_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn plugins(env: &Env) -> Vec<Item> {
    let data = load(&env.plugins());
    let plugins = data
        .as_ref()
        .and_then(|d| d.get("plugins"))
        .and_then(Value::as_object);
    let mut out = Vec::new();
    for (key, entries) in plugins.into_iter().flatten() {
        let name = key.split('@').next().unwrap_or("");
        let first = entries.get(0).cloned().unwrap_or(Value::Null);
        let version = first.get("version").and_then(Value::as_str).unwrap_or("?");
        let detail = format!("{key} v{version}");
        if HOST_ONLY_PLUGINS.contains(&name) {
            out.push(item(
                "plugins",
                name,
                detail,
                vec![(
                    "claude-code",
                    Spec::Run(vec![format!("claude plugin install {key}")]),
                )],
                "host-only",
                true,
                vec![],
                "needs the language-server binary in the image; no official pi equivalent",
            ));
            continue;
        }
        let market = key.rsplit('@').next().unwrap_or("");
        let mut cc: Vec<String> = marketplace_add(env, market).into_iter().collect();
        cc.push(format!("claude plugin install {key}"));
        let mut agents = vec![("claude-code", Spec::Run(cc))];
        let mut needs = needs_of("claude-plugin");
        if let Some((_, pi)) = PI_PLUGINS.iter().find(|(n, _)| *n == name) {
            let sha = first
                .get("gitCommitSha")
                .and_then(Value::as_str)
                .unwrap_or("");
            let cmd = if is_sha(sha) {
                format!("{pi}@{sha}")
            } else {
                pi.to_string()
            };
            agents.push(("pi", Spec::Run(vec![cmd])));
            needs.extend(needs_of("pi-plugin"));
        }
        let note = if agents.len() > 1 {
            ""
        } else {
            "no pi equivalent known"
        };
        out.push(item(
            "plugins", name, detail, agents, "run", true, needs, note,
        ));
    }
    out
}

fn packages(env: &Env) -> Vec<Item> {
    if !env.claude.join("skills/graphify").exists() {
        return vec![];
    }
    let version = env.graphify_version.clone().unwrap_or_else(|| {
        let out = run_out("graphify", &["--version"]);
        out.split_whitespace().last().unwrap_or("?").to_string()
    });
    let install = format!("uv tool install graphifyy=={version}");
    vec![item(
        "packages",
        "graphify",
        format!("graphifyy {version} (PyPI; needs python 3.10+ and uv in the image)"),
        vec![
            (
                "claude-code",
                Spec::Run(vec![install.clone(), "graphify install".into()]),
            ),
            (
                "pi",
                Spec::Run(vec![install, "graphify install --platform pi".into()]),
            ),
        ],
        "run",
        true,
        needs_of("uv"),
        "",
    )]
}

/// Skills no installer can fetch: everything left in ~/.claude/skills.
fn own_skills(env: &Env, locked: &HashSet<String>) -> (Vec<PathBuf>, Vec<String>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(env.claude.join("skills"))
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    entries.sort();
    let (mut own, mut unclassified) = (Vec::new(), Vec::new());
    for p in entries {
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if locked.contains(&name)
            || EXCLUDED_SKILLS.contains(&name.as_str())
            || PACKAGE_SKILLS.contains(&name.as_str())
            || SKIP_DIRS.contains(&name.as_str())
        {
            continue;
        }
        if p.join("SKILL.md").is_file() {
            own.push(p);
        } else {
            unclassified.push(name);
        }
    }
    (own, unclassified)
}

fn memory_dir(env: &Env) -> PathBuf {
    let slug: String = env
        .memory_repo
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    env.claude.join("projects").join(slug).join("memory")
}

/// Copies a tree, following symlinks and skipping dangling ones and `.DS_Store`.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        if e.file_name() == ".DS_Store" {
            continue;
        }
        let src = e.path();
        let Ok(meta) = fs::metadata(&src) else {
            continue; // dangling symlink
        };
        let dst = to.join(e.file_name());
        if meta.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Copies what would be uploaded into `stage` and returns its upload items.
fn stage_and_describe(env: &Env, own: &[PathBuf], stage: &Path) -> io::Result<Vec<Item>> {
    let h = &env.sandbox_home;
    let mut items = Vec::new();
    for p in own {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let dst = stage.join("skills").join(name);
        copy_tree(p, &dst)?;
        let src = dst.to_string_lossy().into_owned();
        let kind = if p.is_symlink() {
            "symlink, dereferenced"
        } else {
            "directory"
        };
        // `openshell sandbox upload DIR DEST` creates DEST/<basename of DIR>, so DEST is the parent.
        let up = |root: &str| Spec::Upload {
            src: src.clone(),
            dest: format!("{h}/{root}"),
            result: format!("{h}/{root}/{name}"),
        };
        items.push(item(
            "own-skills",
            name,
            kind.into(),
            vec![
                ("claude-code", up(".claude/skills")),
                ("pi", up(".agents/skills")),
            ],
            "upload",
            true,
            vec![],
            "",
        ));
    }
    let claude_md = env.claude.join("CLAUDE.md");
    if claude_md.is_file() {
        let dst = stage.join("CLAUDE.md");
        fs::copy(&claude_md, &dst)?;
        let src = dst.to_string_lossy().into_owned();
        let up = |path: &str| Spec::Upload {
            src: src.clone(),
            dest: format!("{h}/{path}"),
            result: format!("{h}/{path}"),
        };
        items.push(item(
            "context",
            "CLAUDE.md",
            "global instructions".into(),
            vec![
                ("claude-code", up(".claude/CLAUDE.md")),
                ("pi", up(".pi/agent/AGENTS.md")),
            ],
            "upload",
            true,
            vec![],
            "",
        ));
    }
    let mem = memory_dir(env);
    if mem.is_dir() && fs::read_dir(&mem).is_ok_and(|mut r| r.next().is_some()) {
        let dst = stage.join("memory");
        copy_tree(&mem, &dst)?;
        items.push(item(
            "context",
            "memory",
            "this repo's Claude memory".into(),
            vec![(
                "claude-code",
                Spec::Upload {
                    src: dst.to_string_lossy().into_owned(),
                    dest: format!("{h}/.claude/projects/-workspace"),
                    result: format!("{h}/.claude/projects/-workspace/memory"),
                },
            )],
            "upload",
            true,
            vec![],
            "",
        ));
    }
    Ok(items)
}

/// Secret-scan findings in the staged files; `None` when gitleaks is missing.
fn scan(env: &Env, stage: &Path) -> Option<Vec<Value>> {
    if !which("gitleaks") {
        return None;
    }
    let report = stage.join(".gitleaks-report.json");
    let mut c = Command::new("gitleaks");
    c.arg("dir").arg(stage);
    if env.gitleaks_config.is_file() {
        c.arg("--config").arg(&env.gitleaks_config);
    }
    let _ = c
        .args([
            "--no-banner",
            "--no-color",
            "--log-level",
            "error",
            "--redact",
        ])
        .args(["--report-format", "json", "--report-path"])
        .arg(&report)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let found = load(&report)
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|f| {
            let file = f["File"].as_str().unwrap_or("");
            let rel = Path::new(file)
                .strip_prefix(stage)
                .unwrap_or(Path::new(file));
            json!({"rule": f["RuleID"], "file": rel.to_string_lossy(), "line": f["StartLine"]})
        })
        .collect();
    let _ = fs::remove_file(&report);
    Some(found)
}

/// Python's `fnmatch.fnmatchcase`: `*` (any run, `/` included), `?`, `[seq]`, `[!seq]`.
pub fn fnmatch(pat: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some('[') => {
                let Some(close) = p.iter().skip(2).position(|c| *c == ']').map(|i| i + 2) else {
                    return !n.is_empty() && n[0] == '[' && go(&p[1..], &n[1..]);
                };
                if n.is_empty() {
                    return false;
                }
                let (neg, set) = match p[1] {
                    '!' => (true, &p[2..close]),
                    _ => (false, &p[1..close]),
                };
                let mut hit = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        hit |= set[i] <= n[0] && n[0] <= set[i + 2];
                        i += 3;
                    } else {
                        hit |= set[i] == n[0];
                        i += 1;
                    }
                }
                hit != neg && go(&p[close + 1..], &n[1..])
            }
            Some(c) => !n.is_empty() && n[0] == *c && go(&p[1..], &n[1..]),
        }
    }
    let (p, n): (Vec<char>, Vec<char>) = (pat.chars().collect(), name.chars().collect());
    go(&p, &n)
}

/// The effective policy's network rules of a sandbox.
fn network_rules(sandbox: &str, gateway: &str) -> Result<Vec<Value>, String> {
    let mut c = Command::new("openshell");
    if !gateway.is_empty() {
        c.args(["-g", gateway]);
    }
    let out = c
        .args(["policy", "get", sandbox, "--full", "-o", "json"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let mut m = String::from_utf8_lossy(&out.stdout).into_owned();
        m.push_str(&String::from_utf8_lossy(&out.stderr));
        return Err(m.trim().to_string());
    }
    parse_rules(&String::from_utf8_lossy(&out.stdout))
}

fn parse_rules(text: &str) -> Result<Vec<Value>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("unreadable policy: {e}"))?;
    let policy = v.get("policy").ok_or("unreadable policy: no policy")?;
    Ok(policy
        .get("network_policies")
        .and_then(Value::as_object)
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default())
}

/// True if some rule lets `binary` reach host:port. Hosts and binaries may be globs.
pub fn allows(rules: &[Value], host: &str, binary: &str, port: i64) -> bool {
    rules.iter().any(|rule| {
        let bin_ok = rule["binaries"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|b| fnmatch(b["path"].as_str().unwrap_or(""), binary));
        bin_ok
            && rule["endpoints"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|ep| {
                    let ports: Vec<i64> = match ep["ports"].as_array().filter(|a| !a.is_empty()) {
                        Some(a) => a.iter().filter_map(Value::as_i64).collect(),
                        None => ep["port"].as_i64().into_iter().collect(),
                    };
                    ports.contains(&port) && fnmatch(ep["host"].as_str().unwrap_or(""), host)
                })
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub id: String,
    /// (host, port, binary globs)
    pub eps: Vec<(String, i64, Vec<String>)>,
    pub keys: Vec<String>,
}

fn bracket_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect()
}

/// Provider profiles in `providers/`: those needing no credential (`anonymous`) or those needing
/// one. Read with a small line parser: the files are flat and a YAML parser is not a dependency.
fn profile_table(env: &Env, anonymous: bool) -> Vec<Profile> {
    let mut files: Vec<PathBuf> = fs::read_dir(&env.providers_dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.retain(|p| p.extension().is_some_and(|e| e == "yaml"));
    files.sort();
    let mut found = Vec::new();
    for f in files {
        let Ok(text) = fs::read_to_string(&f) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        if lines.iter().any(|l| l.starts_with("credentials:")) == anonymous {
            continue;
        }
        let Some(id) = lines.iter().find_map(|l| {
            l.strip_prefix("id:")
                .and_then(|r| r.split_whitespace().next())
        }) else {
            continue;
        };
        let mut binaries: Vec<String> = Vec::new();
        if let Some(i) = lines.iter().position(|l| *l == "binaries:") {
            binaries = lines[i + 1..]
                .iter()
                .filter_map(|l| {
                    let t = l.trim_end();
                    let rest = t.trim_start().strip_prefix('-')?;
                    if !rest.starts_with(char::is_whitespace) {
                        return None;
                    }
                    let b = rest.trim_start();
                    let indented = t.len() != t.trim_start().len();
                    (indented && b.starts_with('/') && !b.contains(char::is_whitespace))
                        .then(|| b.to_string())
                })
                .collect();
        }
        if let Some(inline) = lines.iter().find_map(|l| {
            l.strip_prefix("binaries:")
                .and_then(|r| r.trim_start().strip_prefix('['))
                .and_then(|r| r.split_once(']'))
        }) {
            binaries = bracket_list(inline.0);
        }
        let mut eps = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            let t = l.trim_start();
            let Some(rest) = t
                .strip_prefix('-')
                .filter(|r| r.starts_with(char::is_whitespace))
                .map(str::trim_start)
                .and_then(|r| r.strip_prefix("host:"))
            else {
                continue;
            };
            if t.len() == l.len() {
                continue; // `-` must be indented
            }
            let Some(host) = rest.split_whitespace().next() else {
                continue;
            };
            let port = lines
                .get(i + 1)
                .filter(|n| n.starts_with([' ', '\t']))
                .and_then(|n| n.trim_start().strip_prefix("port:"))
                .and_then(|r| r.split_whitespace().next())
                .and_then(|d| d.parse().ok())
                .unwrap_or(443);
            eps.push((host.to_string(), port, binaries.clone()));
        }
        let keys = text
            .find("env_vars:")
            .map(|i| &text[i + "env_vars:".len()..])
            .and_then(|r| r.trim_start().strip_prefix('['))
            .and_then(|r| r.split_once(']'))
            .map(|(inner, _)| bracket_list(inner))
            .unwrap_or_default();
        found.push(Profile {
            id: id.to_string(),
            eps,
            keys,
        });
    }
    found
}

fn covering<'a>(profiles: &'a [Profile], host: &str, binary: &str) -> Option<&'a Profile> {
    profiles.iter().find(|p| {
        p.eps
            .iter()
            .any(|(h, _, bins)| fnmatch(h, host) && bins.iter().any(|b| fnmatch(b, binary)))
    })
}

type Pair = (String, String);

/// Profiles that grant the missing (host, binary) pairs: anonymous ones first, then ones that
/// need a secret (`id -> env keys`); plus the pairs no profile grants.
/// Profile id and the env keys its credential needs.
type SecretFix = (String, Vec<String>);

fn fixes_for(env: &Env, missing: &[Pair]) -> (Vec<String>, Vec<SecretFix>, Vec<Pair>) {
    let (anon, secret) = (profile_table(env, true), profile_table(env, false));
    let (mut chosen, mut needs_secret, mut unfixed) =
        (Vec::new(), Vec::<SecretFix>::new(), Vec::new());
    for (host, binary) in missing {
        if let Some(p) = covering(&anon, host, binary) {
            if !chosen.contains(&p.id) {
                chosen.push(p.id.clone());
            }
        } else if let Some(p) = covering(&secret, host, binary) {
            match needs_secret.iter_mut().find(|(id, _)| *id == p.id) {
                Some(e) => e.1 = p.keys.clone(),
                None => needs_secret.push((p.id.clone(), p.keys.clone())),
            }
        } else {
            unfixed.push((host.clone(), binary.clone()));
        }
    }
    (chosen, needs_secret, unfixed)
}

/// Endpoint shapes for hosts whose traffic is better restricted than opened wholesale.
fn endpoint_yaml(host: &str) -> String {
    match host {
        "github.com" => "  - host: github.com
    port: 443
    protocol: rest
    enforcement: enforce
    rules:
      - allow: { method: GET, path: \"**\" }
      - allow: { method: HEAD, path: \"**\" }
      - allow: { method: OPTIONS, path: \"**\" }
      - allow: { method: POST, path: \"/**/git-upload-pack\" }
"
        .into(),
        "registry.npmjs.org" => "  - host: registry.npmjs.org
    port: 443
    protocol: rest
    access: read-only
    enforcement: enforce
    # Scoped packages (@scope/name) are requested as @scope%2fname.
    allow_encoded_slash: true
"
        .into(),
        h => format!("  - host: {h}\n    port: 443\n"),
    }
}

/// Other paths the same program is installed at, so one profile works on every image.
fn binary_aliases(b: &str) -> &'static [&'static str] {
    match b {
        BIN_NODE => &["/usr/bin/node"],
        BIN_GIT => &["/usr/local/bin/git"],
        BIN_CLAUDE => &[
            "/usr/bin/claude",
            "/usr/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe",
            "/usr/local/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe",
        ],
        _ => &[],
    }
}

/// A provider profile (YAML) granting exactly the missing (host, binary) pairs, no credential.
pub fn generated_profile(name: &str, missing: &[Pair], sandbox: &str) -> String {
    let mut hosts: Vec<&str> = missing.iter().map(|(h, _)| h.as_str()).collect();
    hosts.sort();
    hosts.dedup();
    let mut sorted = missing.to_vec();
    sorted.sort();
    let mut binaries: Vec<String> = Vec::new();
    for (_, b) in &sorted {
        for path in std::iter::once(b.as_str()).chain(binary_aliases(b).iter().copied()) {
            if !binaries.iter().any(|x| x == path) {
                binaries.push(path.to_string());
            }
        }
    }
    let endpoints: String = hosts.iter().map(|h| endpoint_yaml(h)).collect();
    let bins: String = binaries.iter().map(|b| format!("  - {b}\n")).collect();
    format!(
        "# Generated by sync-agents for the install steps of sandbox {sandbox}. No credential.\n\
         # Grants only: {}. Detach it once the installs are done.\n\
         id: {name}\ndisplay_name: Install access for {sandbox}\n\
         description: Network the skill and plugin installs need, generated for {sandbox}\n\
         category: agent\nendpoints:\n{endpoints}binaries:\n{bins}",
        hosts.join(", ")
    )
}

/// Maps each selected item id to the (host, binary) needs the policy does not allow. Selected =
/// the ids in `only`, or every default item when `only` is None.
fn preflight(
    items: &[Item],
    rules: &[Value],
    only: Option<&HashSet<String>>,
) -> BTreeMap<String, Vec<Pair>> {
    items
        .iter()
        .filter(|i| only.map_or(i.default, |o| o.contains(&i.id())))
        .filter(|i| !i.needs.is_empty())
        .map(|i| {
            let blocked = i
                .needs
                .iter()
                .filter(|(h, b)| !allows(rules, h, b, 443))
                .cloned()
                .collect();
            (i.id(), blocked)
        })
        .collect()
}

/// Keeps only the selected agents; drops items that apply to none of them.
fn for_agents(it: &Item, agents: &[&str]) -> Option<Item> {
    let kept: Vec<_> = it
        .agents
        .iter()
        .filter(|(a, _)| agents.contains(a))
        .cloned()
        .collect();
    if kept.is_empty() {
        return None;
    }
    // The claude binary is only needed when claude-code is a target.
    let claude = kept.iter().any(|(a, _)| *a == "claude-code");
    let needs = it
        .needs
        .iter()
        .filter(|(_, b)| b != BIN_CLAUDE || claude)
        .cloned()
        .collect();
    Some(Item {
        agents: kept,
        needs,
        ..it.clone()
    })
}

pub fn agent_list(agent: &str) -> Result<Vec<&'static str>, String> {
    match agent {
        "both" => Ok(AGENTS.to_vec()),
        a => AGENTS
            .iter()
            .find(|x| **x == a)
            .map(|x| vec![*x])
            .ok_or_else(|| format!("--agent takes claude-code, pi or both, not {a}")),
    }
}

struct Built {
    items: Vec<Item>,
    unclassified: Vec<String>,
}

fn build(env: &Env, agents: &[&str], stage: &Path) -> Result<Built, String> {
    let npx = npx_skills(env);
    let locked: HashSet<String> = npx.iter().map(|i| i.name.clone()).collect();
    let (own, unclassified) = own_skills(env, &locked);
    let mut all = npx;
    all.extend(plugins(env));
    all.extend(packages(env));
    all.extend(stage_and_describe(env, &own, stage).map_err(|e| format!("staging: {e}"))?);
    Ok(Built {
        items: all.iter().filter_map(|i| for_agents(i, agents)).collect(),
        unclassified,
    })
}

fn mkdtemp() -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("sync-agents-{}-{nanos}", std::process::id()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// The manifest as JSON, with the staged uploads left in `stage_dir` for the caller to remove.
pub fn manifest(env: &Env, agent: &str) -> Result<String, String> {
    let agents = agent_list(agent)?;
    let stage = mkdtemp()?;
    let built = build(env, &agents, &stage).inspect_err(|_| {
        let _ = fs::remove_dir_all(&stage);
    })?;
    let findings = scan(env, &stage);
    Ok(json!({
        "agents": agents, "stage_dir": stage.to_string_lossy(),
        "items": built.items.iter().map(Item::to_json).collect::<Vec<_>>(),
        "unclassified": built.unclassified, "secret_findings": findings,
    })
    .to_string())
}

fn with_stage<T>(
    env: &Env,
    agent: &str,
    f: impl FnOnce(&Built) -> Result<T, String>,
) -> Result<T, String> {
    let agents = agent_list(agent)?;
    let stage = mkdtemp()?;
    let built = build(env, &agents, &stage);
    let out = built.and_then(|b| f(&b));
    let _ = fs::remove_dir_all(&stage);
    out
}

/// The ids picked with `--only`; `None` (no flag) means every default item.
fn selection(only: Option<&[String]>) -> Option<HashSet<String>> {
    only.map(|o| o.iter().filter(|s| !s.is_empty()).cloned().collect())
}

/// 3 when the check found access the policy blocks, as the Python original did; else 0.
fn check_exit_code(json: &str) -> i32 {
    let ok = serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|v| v["ok"].as_bool())
        .unwrap_or(false);
    if ok {
        0
    } else {
        3
    }
}

fn pair_json(pairs: &[Pair]) -> Vec<Value> {
    pairs
        .iter()
        .map(|(h, b)| json!({"host": h, "binary": b}))
        .collect()
}

/// Checks the selected items against the sandbox policy; the JSON the dashboard reads.
pub fn check(
    env: &Env,
    agent: &str,
    sandbox: &str,
    gateway: &str,
    only: Option<&[String]>,
) -> Result<String, String> {
    let rules = network_rules(sandbox, gateway)
        .map_err(|e| format!("cannot read the policy of {sandbox}: {e}"))?;
    with_stage(env, agent, |b| {
        let result = preflight(&b.items, &rules, selection(only).as_ref());
        let mut missing: Vec<Pair> = result.values().flatten().cloned().collect();
        missing.sort();
        missing.dedup();
        let (fixes, secret, unfixed) = fixes_for(env, &missing);
        Ok(json!({
            "sandbox": sandbox, "ok": missing.is_empty(), "items": result,
            "missing": pair_json(&missing), "fixes": fixes,
            "secret_fixes": secret.iter().map(|(p, k)| json!({"profile": p, "keys": k})).collect::<Vec<_>>(),
            "unfixed": pair_json(&unfixed),
        })
        .to_string())
    })
}

/// The profile (YAML) that grants exactly what the selected installs still lack.
pub fn gen_profile(
    env: &Env,
    agent: &str,
    sandbox: &str,
    gateway: &str,
    only: Option<&[String]>,
    name: &str,
) -> Result<String, String> {
    let ok_name = !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok_name {
        return Err("--name must be lowercase letters, digits and dashes".into());
    }
    let rules = network_rules(sandbox, gateway)
        .map_err(|e| format!("cannot read the policy of {sandbox}: {e}"))?;
    with_stage(env, agent, |b| {
        let mut missing: Vec<Pair> = preflight(&b.items, &rules, selection(only).as_ref())
            .values()
            .flatten()
            .cloned()
            .collect();
        missing.sort();
        missing.dedup();
        if missing.is_empty() {
            return Err(
                "nothing is missing: the policy already allows every selected install".into(),
            );
        }
        Ok(generated_profile(name, &missing, sandbox))
    })
}

fn show(it: &Item) -> String {
    let mark = if it.default { " " } else { "-" };
    let mut out = format!(" {mark} {:<26} {}\n", it.name, it.detail);
    for (agent, spec) in &it.agents {
        match spec {
            Spec::Upload { result, .. } => {
                out.push_str(&format!("      {agent:<11} upload -> {result}\n"))
            }
            Spec::Run(cmds) => {
                for c in cmds {
                    out.push_str(&format!("      {agent:<11} $ {c}\n"));
                }
            }
        }
    }
    if !it.note.is_empty() {
        out.push_str(&format!("      note: {}\n", it.note));
    }
    out
}

/// The human-readable dry run.
pub fn dry_run(env: &Env, agent: &str) -> Result<String, String> {
    let agents = agent_list(agent)?;
    let stage = mkdtemp()?;
    let built = build(env, &agents, &stage);
    let findings = built.as_ref().ok().and_then(|_| scan(env, &stage));
    let _ = fs::remove_dir_all(&stage);
    let b = built?;
    let mut out = String::from("Dry run: nothing is installed, copied or uploaded.\n");
    out.push_str(&format!(
        "Agents: {}   Sandbox HOME: {}   ('-' = not selected by default)\n",
        agents.join(", "),
        env.sandbox_home
    ));
    for group in ["npx-skills", "plugins", "packages", "own-skills", "context"] {
        let rows: Vec<_> = b.items.iter().filter(|i| i.group == group).collect();
        out.push_str(&format!("\n{group} ({})\n", rows.len()));
        for r in &rows {
            out.push_str(&show(r));
        }
        if rows.is_empty() {
            out.push_str("  none\n");
        }
    }
    if !b.unclassified.is_empty() {
        out.push_str(&format!(
            "\nUnclassified, skipped (no SKILL.md): {}\n",
            b.unclassified.join(", ")
        ));
    }
    let mut needs: Vec<&Pair> = b
        .items
        .iter()
        .filter(|i| i.default)
        .flat_map(|i| &i.needs)
        .collect();
    needs.sort();
    needs.dedup();
    out.push_str("\nNetwork the selected installs need (host, binary)\n");
    for (host, binary) in needs {
        out.push_str(&format!("  {host:<28} {binary}\n"));
    }
    out.push_str("\nSecret scan of staged uploads\n");
    match findings {
        None => out.push_str("  gitleaks not installed: skipped\n"),
        Some(f) => {
            for x in &f {
                out.push_str(&format!(
                    "  {}  {}:{}\n",
                    x["rule"].as_str().unwrap_or(""),
                    x["file"].as_str().unwrap_or(""),
                    x["line"]
                ));
            }
            if f.is_empty() {
                out.push_str("  clean\n");
            } else {
                out.push_str(&format!(
                    "  {} finding(s): review before any real sync\n",
                    f.len()
                ));
            }
        }
    }
    out.push_str("\nExcluded always: settings.json, hooks, ~/.claude.json, skills/synced\n");
    Ok(out)
}

const USAGE: &str = "usage: osh sync-agents (--dry-run | --json | --preflight SANDBOX [--gateway NAME] | --gen-profile SANDBOX --name ID [--gateway NAME]) [--agent claude-code|pi|both] [--only ID,ID] [--home DIR]";

/// `osh sync-agents ...`; returns the exit code.
pub fn cli_main(args: &[String], data_dir: &Path) -> i32 {
    let (mut mode, mut sandbox, mut name, mut agent) = (
        String::new(),
        String::new(),
        String::new(),
        "both".to_string(),
    );
    let (mut gateway, mut home, mut only) =
        (String::new(), "/workspace".to_string(), None::<Vec<String>>);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut next = || it.next().cloned().unwrap_or_default();
        match a.as_str() {
            "--dry-run" | "--json" if mode.is_empty() => mode = a.clone(),
            "--preflight" | "--gen-profile" => {
                mode = a.clone();
                sandbox = next();
            }
            "--name" => name = next(),
            "--agent" => agent = next(),
            "--only" => only = Some(next().split(',').map(String::from).collect()),
            // The Python took `--json` after a mode; the checks always print JSON.
            "--json" => {}
            "--gateway" => gateway = next(),
            "--home" => home = next(),
            _ => {
                eprintln!("{USAGE}");
                return 2;
            }
        }
    }
    if mode.is_empty() || (mode.starts_with("--p") || mode == "--gen-profile") && sandbox.is_empty()
    {
        eprintln!("{USAGE}");
        return 2;
    }
    let env = Env::detect(data_dir, &home);
    let only = only.as_deref();
    let res = match mode.as_str() {
        "--dry-run" => dry_run(&env, &agent),
        "--json" => manifest(&env, &agent),
        "--preflight" => check(&env, &agent, &sandbox, &gateway, only),
        _ => gen_profile(&env, &agent, &sandbox, &gateway, only, &name),
    };
    match res {
        Ok(t) => {
            print!("{t}");
            if mode != "--dry-run" && mode != "--gen-profile" {
                println!();
            }
            if mode == "--preflight" {
                check_exit_code(&t)
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn env_with(claude: &Path) -> Env {
        Env {
            claude: claude.to_path_buf(),
            lock: claude.join("../.agents/.skill-lock.json"),
            providers_dir: repo().join("providers"),
            gitleaks_config: repo().join(".gitleaks.toml"),
            memory_repo: repo(),
            graphify_version: Some("1.2.3".into()),
            sandbox_home: "/workspace".into(),
        }
    }

    fn pair(h: &str, b: &str) -> Pair {
        (h.into(), b.into())
    }

    #[test]
    fn fnmatch_matches_pythons_rules() {
        assert!(fnmatch("/usr/*/node", "/usr/bin/node"));
        assert!(fnmatch("a?c", "abc"));
        assert!(fnmatch("[!a]x", "bx"));
        assert!(!fnmatch("[a-c]x", "dx"));
        assert!(fnmatch("*.github.com", "api.github.com"));
        assert!(fnmatch(
            "/sandbox/.uv/python/**",
            "/sandbox/.uv/python/3.12/bin/python"
        ));
        assert!(!fnmatch("github.com", "api.github.com"));
    }

    #[test]
    fn allows_checks_host_port_and_binary_globs() {
        let rules = vec![json!({
            "binaries": [{"path": "/usr/bin/*"}],
            "endpoints": [{"host": "*.github.com", "ports": [443, 8443]}, {"host": "pypi.org", "port": 443}],
        })];
        assert!(allows(&rules, "api.github.com", "/usr/bin/git", 443));
        assert!(allows(&rules, "api.github.com", "/usr/bin/git", 8443));
        assert!(allows(&rules, "pypi.org", "/usr/bin/uv", 443));
        assert!(!allows(&rules, "pypi.org", "/usr/local/bin/uv", 443));
        assert!(!allows(&rules, "pypi.org", "/usr/bin/uv", 80));
        assert!(!allows(&rules, "evil.org", "/usr/bin/git", 443));
    }

    #[test]
    fn parse_rules_reads_the_effective_policy() {
        let t = r#"{"policy":{"network_policies":{"a":{"binaries":[],"endpoints":[]}}}}"#;
        assert_eq!(parse_rules(t).unwrap().len(), 1);
        assert!(parse_rules("{}").unwrap_err().contains("unreadable policy"));
        assert!(parse_rules("nope").is_err());
    }

    #[test]
    fn profile_table_splits_anonymous_from_credentialed() {
        let env = env_with(Path::new("/nonexistent"));
        let anon: Vec<_> = profile_table(&env, true)
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(anon, ["claude-marketplace", "npm", "pypi"]);
        let secret = profile_table(&env, false);
        let ids: Vec<_> = secret.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["claude-code", "forgejo", "github"]);
        assert_eq!(secret[0].keys, ["CLAUDE_CODE_OAUTH_TOKEN"]);
        assert_eq!(secret[2].keys, ["GITHUB_TOKEN", "GH_TOKEN"]);
        let npm = &profile_table(&env, true)[1];
        assert_eq!(
            npm.eps,
            [(
                "registry.npmjs.org".to_string(),
                443,
                vec![
                    "/usr/bin/node".to_string(),
                    "/usr/local/bin/node".to_string()
                ]
            )]
        );
    }

    #[test]
    fn fixes_for_prefers_anonymous_profiles_and_reports_the_rest() {
        let env = env_with(Path::new("/nonexistent"));
        let mut miss = vec![
            pair("github.com", "/usr/local/bin/claude"),
            pair("registry.npmjs.org", "/usr/local/bin/node"),
            pair("github.com", "/usr/bin/git"),
            pair("pypi.org", "/usr/local/bin/uv"),
            pair("example.org", "/usr/bin/curl"),
        ];
        miss.sort();
        let (fixes, secret, unfixed) = fixes_for(&env, &miss);
        assert_eq!(fixes, ["claude-marketplace", "pypi", "npm"]);
        assert!(secret.is_empty());
        assert_eq!(unfixed, [pair("example.org", "/usr/bin/curl")]);
        // Only a credentialed profile covers api.anthropic.com.
        let (fixes, secret, _) = fixes_for(&env, &[pair("api.anthropic.com", "/usr/bin/claude")]);
        assert!(fixes.is_empty());
        assert_eq!(
            secret,
            [(
                "claude-code".to_string(),
                vec!["CLAUDE_CODE_OAUTH_TOKEN".to_string()]
            )]
        );
    }

    #[test]
    fn generated_profile_grants_only_what_is_missing() {
        let miss = vec![
            pair("example.org", "/usr/bin/curl"),
            pair("github.com", "/usr/bin/git"),
            pair("github.com", "/usr/local/bin/claude"),
        ];
        let got = generated_profile("fix-x", &miss, "sb");
        let want = "# Generated by sync-agents for the install steps of sandbox sb. No credential.\n# Grants only: example.org, github.com. Detach it once the installs are done.\nid: fix-x\ndisplay_name: Install access for sb\ndescription: Network the skill and plugin installs need, generated for sb\ncategory: agent\nendpoints:\n  - host: example.org\n    port: 443\n  - host: github.com\n    port: 443\n    protocol: rest\n    enforcement: enforce\n    rules:\n      - allow: { method: GET, path: \"**\" }\n      - allow: { method: HEAD, path: \"**\" }\n      - allow: { method: OPTIONS, path: \"**\" }\n      - allow: { method: POST, path: \"/**/git-upload-pack\" }\nbinaries:\n  - /usr/bin/curl\n  - /usr/bin/git\n  - /usr/local/bin/git\n  - /usr/local/bin/claude\n  - /usr/bin/claude\n  - /usr/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe\n  - /usr/local/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe\n";
        assert_eq!(got, want);
    }

    #[test]
    fn no_only_flag_selects_every_default_item_not_nothing() {
        assert_eq!(selection(None), None);
        assert_eq!(selection(Some(&[])), Some(HashSet::new()));
        let ids = ["a:b".to_string(), String::new()];
        assert_eq!(selection(Some(&ids)), Some(["a:b".to_string()].into()));
    }

    #[test]
    fn a_blocked_check_exits_3() {
        assert_eq!(check_exit_code(r#"{"ok":false}"#), 3);
        assert_eq!(check_exit_code(r#"{"ok":true}"#), 0);
    }

    #[test]
    fn gen_profile_rejects_a_bad_name_before_touching_the_gateway() {
        let env = env_with(Path::new("/nonexistent"));
        assert!(gen_profile(&env, "both", "sb", "", None, "Bad Name")
            .unwrap_err()
            .contains("--name"));
    }

    #[test]
    fn for_agents_drops_the_claude_binary_need_without_claude_code() {
        let it = item(
            "plugins",
            "x",
            "d".into(),
            vec![
                ("claude-code", Spec::Run(vec![])),
                ("pi", Spec::Run(vec![])),
            ],
            "run",
            true,
            vec![pair("github.com", BIN_CLAUDE), pair("github.com", BIN_GIT)],
            "",
        );
        assert_eq!(
            for_agents(&it, &["pi"]).unwrap().needs,
            [pair("github.com", BIN_GIT)]
        );
        assert_eq!(for_agents(&it, &["claude-code"]).unwrap().needs.len(), 2);
        assert!(for_agents(
            &item(
                "g",
                "y",
                "".into(),
                vec![("pi", Spec::Run(vec![]))],
                "run",
                true,
                vec![],
                ""
            ),
            &["claude-code"]
        )
        .is_none());
    }

    #[test]
    fn preflight_marks_blocked_needs_for_selected_items_only() {
        let a = item(
            "npx-skills",
            "a",
            "".into(),
            vec![("pi", Spec::Run(vec![]))],
            "run",
            true,
            needs_of("npx"),
            "",
        );
        let b = item(
            "npx-skills",
            "b",
            "".into(),
            vec![("pi", Spec::Run(vec![]))],
            "run",
            false,
            needs_of("npx"),
            "",
        );
        let rules = vec![
            json!({"binaries":[{"path":BIN_NODE}],"endpoints":[{"host":"registry.npmjs.org","port":443}]}),
        ];
        let r = preflight(&[a.clone(), b.clone()], &rules, None);
        assert_eq!(r.keys().collect::<Vec<_>>(), ["npx-skills:a"]);
        assert_eq!(r["npx-skills:a"], [pair("github.com", BIN_GIT)]);
        let only: HashSet<String> = ["npx-skills:b".to_string()].into();
        assert!(preflight(&[a, b], &rules, Some(&only)).contains_key("npx-skills:b"));
    }

    /// Builds a fake HOME; returns (home, repo-memory dir).
    fn fixture(root: &Path) -> PathBuf {
        let w = |p: PathBuf, c: &str| {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        };
        let claude = root.join(".claude");
        w(claude.join("skills/own-one/SKILL.md"), "# one\n");
        w(claude.join("skills/own-one/scripts/run.sh"), "echo hi\n");
        w(claude.join("skills/noskill/readme.txt"), "x");
        w(claude.join("skills/synced/x"), "x");
        w(claude.join("skills/openshell-cli/SKILL.md"), "x");
        w(claude.join("skills/graphify/SKILL.md"), "x");
        w(claude.join("skills/locked-skill/SKILL.md"), "x");
        w(claude.join("CLAUDE.md"), "# global\n");
        w(
            root.join(".agents/.skill-lock.json"),
            r#"{"skills":{"locked-skill":{"source":"acme/skills","skillFolderHash":"abcdef123456"},"openshell-cli":{"source":"x"}}}"#,
        );
        w(
            claude.join("plugins/installed_plugins.json"),
            r#"{"plugins":{"superpowers@official":[{"version":"1.2","gitCommitSha":"0123456789abcdef0123456789abcdef01234567"}],"gopls-lsp@official":[{"version":"1"}],"other@market":[{}]}}"#,
        );
        w(
            claude.join("plugins/known_marketplaces.json"),
            r#"{"official":{"source":{"source":"github","repo":"acme/plugins"}}}"#,
        );
        let slug: String = repo()
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        w(
            claude.join("projects").join(slug).join("memory/note.md"),
            "remember\n",
        );
        claude
    }

    /// The manifest JSON with the volatile staging path replaced.
    fn normalized(text: &str) -> Value {
        let mut v: Value = serde_json::from_str(text).unwrap();
        let stage = v["stage_dir"].as_str().unwrap().to_string();
        v.as_object_mut().unwrap().remove("stage_dir");
        let _ = fs::remove_dir_all(&stage);
        serde_json::from_str(&v.to_string().replace(&stage, "<STAGE>")).unwrap()
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("osh-agents-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn manifest_matches_the_recorded_python_output_on_the_fixture() {
        let root = tmp("golden");
        let claude = fixture(&root);
        let got = normalized(&manifest(&env_with(&claude), "both").unwrap());
        let want: Value =
            serde_json::from_str(include_str!("../fixtures/agents-golden.json")).unwrap();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(got, want);
    }

    #[test]
    fn manifest_for_one_agent_keeps_only_that_agent() {
        let root = tmp("one");
        let claude = fixture(&root);
        let v = normalized(&manifest(&env_with(&claude), "pi").unwrap());
        let _ = fs::remove_dir_all(&root);
        for i in v["items"].as_array().unwrap() {
            assert_eq!(
                i["agents"].as_object().unwrap().keys().collect::<Vec<_>>(),
                ["pi"]
            );
        }
        assert!(manifest(&env_with(Path::new("/x")), "bogus").is_err());
    }
}
