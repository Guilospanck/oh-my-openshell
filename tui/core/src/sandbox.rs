//! What creating a sandbox runs, without a terminal: the spec, the policy file and the argument
//! list. The dashboard and the headless front end both call `plan_create`, so they cannot drift.

use crate::{cli, ghrepo, preload};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Prepended to commands run inside a sandbox, so tools installed in `~/.local/bin` resolve.
pub const SANDBOX_PATH: &str = "export PATH=\"$HOME/.local/bin:$PATH\"; ";

const TOOLCHAINS_FILE: &str = "toolchains.toml";

/// A command run with the terminal handed back to it (build, create, setup-token).
pub struct Step {
    pub prog: String,
    pub args: Vec<String>,
}

impl Step {
    /// What the step does, for messages: the in-sandbox command without its PATH prefix.
    pub fn label(&self) -> String {
        match self.args.last() {
            Some(c) if self.args.iter().any(|a| a == "-c") => {
                c.strip_prefix(SANDBOX_PATH).unwrap_or(c).to_string()
            }
            _ => format!("{} {}", self.prog, self.args.join(" ")),
        }
    }
}

/// A host folder mounted at `/sandbox/<folder name>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Share {
    pub path: String,
    pub rw: bool,
}

/// Read/write access to one git repo. Holds the name of the environment variable that carries
/// the secret, never the secret itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[rustfmt::skip]
pub struct RepoGrant { pub url: String, pub env_var: String }

/// Everything a new sandbox is created from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxSpec {
    pub name: String,
    pub image: String,
    pub providers: Vec<String>,
    /// Kubernetes-style limits such as `500m`, `2`, `4Gi`; empty = gateway default.
    pub cpu: String,
    pub memory: String,
    /// Non-empty: run once via `sh -c` and delete the sandbox afterwards (`--no-keep`).
    pub command: String,
    /// Toolchain groups switched on; `None` = the file's own `enabled` flags.
    pub toolchains: Option<Vec<String>>,
    pub share: Option<Share>,
    pub repo: Option<RepoGrant>,
}

fn field_str(v: &Value, key: &str) -> Result<String, String> {
    v.as_str()
        .map(String::from)
        .ok_or_else(|| format!("field `{key}` must be a string"))
}

fn field_strs(v: &Value, key: &str) -> Result<Vec<String>, String> {
    let arr = v
        .as_array()
        .ok_or_else(|| format!("field `{key}` must be an array of strings"))?;
    arr.iter().map(|x| field_str(x, key)).collect()
}

fn object<'a>(
    v: &'a Value,
    key: &str,
    known: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let m = v
        .as_object()
        .ok_or_else(|| format!("field `{key}` must be an object"))?;
    match m.keys().find(|k| !known.contains(&k.as_str())) {
        Some(k) => Err(format!("unknown field `{k}` in `{key}`")),
        None => Ok(m),
    }
}

impl SandboxSpec {
    /// Parse a spec; an unknown field or a wrong type is an error naming the field. `name` is not
    /// required here (the dashboard lets the gateway pick one); the headless front end checks it.
    pub fn from_json(v: &Value) -> Result<SandboxSpec, String> {
        let m = object(
            v,
            "spec",
            &[
                "name",
                "image",
                "providers",
                "cpu",
                "memory",
                "command",
                "toolchains",
                "share",
                "repo",
            ],
        )?;
        let mut s = SandboxSpec::default();
        for (k, val) in m {
            match k.as_str() {
                "name" => s.name = field_str(val, k)?,
                "image" => s.image = field_str(val, k)?,
                "cpu" => s.cpu = field_str(val, k)?,
                "memory" => s.memory = field_str(val, k)?,
                "command" => s.command = field_str(val, k)?,
                "providers" => s.providers = field_strs(val, k)?,
                "toolchains" => s.toolchains = Some(field_strs(val, k)?),
                "share" => {
                    let o = object(val, k, &["path", "rw"])?;
                    let path = field_str(
                        o.get("path").ok_or("field `share.path` is required")?,
                        "share.path",
                    )?;
                    let rw = match o.get("rw") {
                        None => false,
                        Some(b) => b.as_bool().ok_or("field `share.rw` must be a boolean")?,
                    };
                    s.share = Some(Share { path, rw });
                }
                "repo" => {
                    let o = object(val, k, &["url", "env_var"])?;
                    let get = |f: &str| {
                        field_str(
                            o.get(f)
                                .ok_or_else(|| format!("field `repo.{f}` is required"))?,
                            &format!("repo.{f}"),
                        )
                    };
                    s.repo = Some(RepoGrant {
                        url: get("url")?,
                        env_var: get("env_var")?,
                    });
                }
                _ => unreachable!("checked by object()"),
            }
        }
        Ok(s)
    }
}

/// Lowercase letters, digits and `-`, 1 to 63 characters, starting with a letter or digit.
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok = (1..=63).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid sandbox name {name:?}: use 1 to 63 lowercase letters, digits or `-`, starting with a letter or digit"
        ))
    }
}

/// What `plan_create` needs from the host.
pub struct CreateCtx<'a> {
    /// The data dir: `sandbox-policy.yaml`, `toolchains.toml`, Dockerfiles; the generated policy
    /// is written here.
    pub dir: &'a Path,
    /// The gateway's compute driver (`vm`, `docker`, `podman`); keys the share mount.
    pub driver: &'a str,
    pub images: &'a [cli::Image],
    /// Build the image again before creating.
    pub rebuild: bool,
}

pub struct Created {
    pub steps: Vec<Step>,
    pub policy_note: Option<String>,
    /// The toolchain groups the policy opens; `None` when the hosts were left out.
    pub toolchain_groups: Option<Vec<String>>,
    /// The policy file written for this call alone (`plan_create`), which the caller removes
    /// once the create has run; `None` for the dashboard's shared generated file.
    pub policy_file: Option<PathBuf>,
}

/// The policy file for a new sandbox: `sandbox-policy.yaml` plus the toolchain hosts of
/// `toolchains.toml` (see `preload`), the note the user should know about, and the groups the
/// policy opens. A missing base policy is replaced by the shipped one (never a `--policy` path
/// that does not exist); an unusable list or block leaves the hosts out. The sandbox still
/// starts either way.
pub fn policy_for(
    dir: &Path,
    toolchains: Option<&[String]>,
) -> (PathBuf, Option<String>, Option<Vec<String>>) {
    policy_at(dir, toolchains, &dir.join(GENERATED_POLICY))
}

/// The dashboard's generated policy, rewritten on every create.
const GENERATED_POLICY: &str = ".sandbox-policy.generated.yaml";

/// A generated policy path no other `plan_create` call in any process uses, so concurrent
/// creates cannot swap each other's toolchain hosts.
fn unique_policy(dir: &Path, name: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(63)
        .collect();
    dir.join(format!(
        ".sandbox-policy.generated.{}-{}-{n}.yaml",
        if name.is_empty() { "unnamed" } else { &name },
        std::process::id()
    ))
}

/// `policy_for`, writing the generated policy to `generated`.
fn policy_at(
    dir: &Path,
    toolchains: Option<&[String]>,
    generated: &Path,
) -> (PathBuf, Option<String>, Option<Vec<String>>) {
    let base = dir.join("sandbox-policy.yaml");
    let generated = generated.to_path_buf();
    let mut note = None;
    let text = std::fs::read_to_string(&base).unwrap_or_else(|_| {
        note = Some(
            "sandbox-policy.yaml is missing, so the shipped default is used (Data files in the command palette restores the file)".to_string(),
        );
        crate::assets::embedded("sandbox-policy.yaml")
            .unwrap_or_default()
            .to_string()
    });
    let groups = match std::fs::read_to_string(dir.join(TOOLCHAINS_FILE)) {
        Ok(list) => preload::parse(&list).unwrap_or_else(|e| {
            note = Some(format!(
                "toolchains.toml is not valid TOML ({}), so no toolchain hosts are added",
                crate::text::error_summary(&e)
            ));
            Vec::new()
        }),
        Err(_) => {
            note = note.clone().or(Some(
                "toolchains.toml is missing, so no toolchain hosts are added".to_string(),
            ));
            Vec::new()
        }
    };
    let groups = match toolchains {
        Some(picks) => preload::select(&groups, picks),
        None => groups,
    };
    let mut opened: Option<Vec<String>> = Some(
        groups
            .iter()
            .filter(|g| g.on)
            .map(|g| g.name.clone())
            .collect(),
    );
    let policy = preload::creation_policy(&text, &groups).unwrap_or_else(|e| {
        note = Some(e);
        opened = None;
        preload::creation_policy(&text, &[]).unwrap_or_else(|_| text.clone())
    });
    match std::fs::write(&generated, policy) {
        Ok(()) => (generated, note, opened),
        Err(e) if base.is_file() => (
            base,
            Some(format!(
                "Could not write the generated policy ({e}), so the plain sandbox-policy.yaml is used without toolchain hosts"
            )),
            None,
        ),
        Err(e) => (
            generated,
            Some(format!("Could not write the sandbox policy: {e}")),
            None,
        ),
    }
}

/// `--driver-config-json` value mounting the host folder, keyed by the active driver. Takes the
/// driver as given and is `None` for a path without a folder name, as the dashboard always did.
fn share_config(share: &Share, driver: &str) -> Option<String> {
    let name = Path::new(&share.path).file_name()?.to_str()?;
    let mount = serde_json::json!({
        "type": "bind",
        "source": share.path,
        "target": format!("/sandbox/{name}"),
        "read_only": !share.rw,
    });
    Some(serde_json::json!({ driver: { "mounts": [mount] } }).to_string())
}

/// The headless entry checks what the dashboard cannot get wrong: a share needs a known driver
/// and a path with a folder name.
fn check_share(share: &Share, driver: &str) -> Result<(), String> {
    if !matches!(driver, "vm" | "docker" | "podman") {
        return Err(if driver.is_empty() {
            "cannot share a folder: the gateway has not said which driver it runs (driver is empty)"
                .to_string()
        } else {
            format!("cannot share a folder on the {driver:?} driver")
        });
    }
    if share_config(share, driver).is_none() {
        return Err(format!("share path {:?} has no folder name", share.path));
    }
    Ok(())
}

/// The forge a repo URL names, from the host alone (the well-known public ones).
pub fn grant_repo(url: &str) -> Result<ghrepo::GhRepo, String> {
    let remote = ghrepo::parse_url(url).ok_or_else(|| format!("not a git remote URL: {url:?}"))?;
    let forge = match remote.host.as_str() {
        "github.com" => ghrepo::Forge::GitHub,
        "gitlab.com" => ghrepo::Forge::GitLab,
        "bitbucket.org" => ghrepo::Forge::BitbucketCloud,
        h => {
            return Err(format!(
                "cannot tell which forge {h:?} is; use github.com, gitlab.com or bitbucket.org"
            ))
        }
    };
    remote
        .repo(forge)
        .ok_or_else(|| format!("{url:?} does not name a repository"))
}

/// The steps that create a sandbox: an image build when it is outdated, then `sandbox create`.
/// The argument order is what the dashboard has always run.
pub fn plan_create(spec: &SandboxSpec, ctx: &CreateCtx) -> Result<Created, String> {
    if let Some(share) = spec.share.as_ref().filter(|s| !s.path.is_empty()) {
        check_share(share, ctx.driver)?;
    }
    let repo = spec.repo.as_ref().map(|g| grant_repo(&g.url)).transpose()?;
    let own = unique_policy(ctx.dir, &spec.name);
    let mut c = plan_steps(spec, ctx, repo.as_ref(), &own);
    // Only a policy written to `own` is this call's to remove (never the base policy).
    c.policy_file = c
        .steps
        .last()
        .is_some_and(|s| s.args.iter().any(|a| Path::new(a) == own))
        .then_some(own);
    Ok(c)
}

/// `plan_create` with the granted repo already resolved (the dashboard knows its forge and host
/// exactly); `spec.repo` is ignored. Never fails and checks nothing about the driver: a share
/// goes under whatever driver string it is given, as before the extraction.
pub fn plan_create_with(
    spec: &SandboxSpec,
    ctx: &CreateCtx,
    repo: Option<&ghrepo::GhRepo>,
) -> Created {
    plan_steps(spec, ctx, repo, &ctx.dir.join(GENERATED_POLICY))
}

fn plan_steps(
    spec: &SandboxSpec,
    ctx: &CreateCtx,
    repo: Option<&ghrepo::GhRepo>,
    generated: &Path,
) -> Created {
    let mut steps = Vec::new();
    if let Some(img) = ctx
        .images
        .iter()
        .find(|i| i.tag == spec.image && (!i.built || ctx.rebuild))
    {
        steps.push(Step {
            prog: cli::runtime_bin().into(),
            args: cli::build_args(ctx.dir, &img.file, &img.tag),
        });
    }
    let (policy, policy_note, toolchain_groups) =
        policy_at(ctx.dir, spec.toolchains.as_deref(), generated);
    let mut args: Vec<String> = vec!["sandbox".into(), "create".into()];
    // The default policy denies opening a PTY, which tmux needs in the shell.
    args.extend(["--policy".into(), policy.to_string_lossy().into()]);
    if !spec.name.is_empty() {
        args.extend(["--name".into(), spec.name.clone()]);
    }
    if !spec.image.is_empty() {
        args.extend(["--from".into(), spec.image.clone()]);
    }
    if !spec.cpu.is_empty() {
        args.extend(["--cpu".into(), spec.cpu.clone()]);
    }
    if !spec.memory.is_empty() {
        args.extend(["--memory".into(), spec.memory.clone()]);
    }
    if let Some(share) = spec.share.as_ref().filter(|s| !s.path.is_empty()) {
        if let Some(json) = share_config(share, ctx.driver) {
            args.extend(["--driver-config-json".into(), json]);
            args.extend([
                "--label".into(),
                format!("{}={}", cli::SHARE_LABEL, cli::share_key(&share.path)),
            ]);
        }
    }
    for p in &spec.providers {
        args.extend(["--provider".into(), p.clone()]);
    }
    if let Some(r) = repo {
        args.extend(["--provider".into(), ghrepo::provider_name(&spec.name, r)]);
    }
    if spec.command.is_empty() {
        // The detached shell stays the main process; sessions come from `exec`.
        args.push("--detach".into());
    } else {
        args.extend([
            "--no-keep".into(),
            "--".into(),
            "sh".into(),
            "-c".into(),
            spec.command.clone(),
        ]);
    }
    steps.push(Step {
        prog: "openshell".into(),
        args: cli::with_gateway(args),
    });
    Created {
        steps,
        policy_note,
        toolchain_groups,
        policy_file: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli;
    use serde_json::json;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("osh-core-sandbox-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seeded(tag: &str) -> PathBuf {
        let d = temp_dir(tag);
        for f in ["sandbox-policy.yaml", "toolchains.toml"] {
            std::fs::write(d.join(f), crate::assets::embedded(f).unwrap()).unwrap();
        }
        d
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            name: "w1".into(),
            image: "img:1".into(),
            providers: vec!["npm".into()],
            cpu: "2".into(),
            memory: "4Gi".into(),
            ..SandboxSpec::default()
        }
    }

    fn ctx<'a>(dir: &'a Path, driver: &'a str, images: &'a [cli::Image]) -> CreateCtx<'a> {
        CreateCtx {
            dir,
            driver,
            images,
            rebuild: false,
        }
    }

    #[test]
    fn from_json_rejects_unknown_field() {
        let e = SandboxSpec::from_json(&json!({"name":"a","cpus":2})).unwrap_err();
        assert!(e.contains("cpus"), "{e}");
    }

    #[test]
    fn from_json_rejects_wrong_type() {
        let e = SandboxSpec::from_json(&json!({"cpu":2})).unwrap_err();
        assert!(e.contains("cpu"), "{e}");
    }

    #[test]
    fn from_json_reads_every_field() {
        let s = SandboxSpec::from_json(&json!({
            "name":"a","image":"i","providers":["p"],"cpu":"1","memory":"2Gi","command":"ls",
            "toolchains":["rust"],"share":{"path":"/x/y","rw":true},
            "repo":{"url":"https://h/o/r","env_var":"MY_VAR"}
        }))
        .unwrap();
        assert_eq!(s.providers, vec!["p"]);
        assert_eq!(s.toolchains, Some(vec!["rust".to_string()]));
        assert!(s.share.as_ref().unwrap().rw);
        assert_eq!(s.share.unwrap().path, "/x/y");
        let r = s.repo.unwrap();
        assert_eq!(
            (r.url.as_str(), r.env_var.as_str()),
            ("https://h/o/r", "MY_VAR")
        );
    }

    #[test]
    fn validate_name_rejects_path_and_long() {
        for bad in ["", "a b", "../x", &"a".repeat(64), "-a", "A"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_name("factory-1").is_ok());
        assert!(validate_name(&"a".repeat(63)).is_ok());
    }

    #[test]
    fn plan_matches_dashboard_args() {
        let d = seeded("args");
        let c = plan_create(&spec(), &ctx(&d, "docker", &[])).unwrap();
        let last = c.steps.last().unwrap();
        assert_eq!(last.prog, "openshell");
        let file = c.policy_file.clone().unwrap();
        let want = format!(
            "sandbox create --policy {} --name w1 --from img:1 --cpu 2 --memory 4Gi --provider npm --detach",
            file.display()
        );
        assert!(
            last.args.join(" ").ends_with(&want),
            "{}",
            last.args.join(" ")
        );
        assert!(file.is_file());
    }

    #[test]
    fn plan_create_writes_a_policy_file_per_call() {
        let d = seeded("percall");
        let other = SandboxSpec {
            name: "w2".into(),
            toolchains: Some(vec![]),
            ..spec()
        };
        let a = plan_create(&spec(), &ctx(&d, "docker", &[])).unwrap();
        let b = plan_create(&other, &ctx(&d, "docker", &[])).unwrap();
        let again = plan_create(&spec(), &ctx(&d, "docker", &[])).unwrap();
        let policy = |c: &Created| {
            let a = &c.steps.last().unwrap().args;
            a[a.iter().position(|x| x == "--policy").unwrap() + 1].clone()
        };
        assert_ne!(policy(&a), policy(&b));
        assert_ne!(policy(&a), policy(&again));
        for c in [&a, &b, &again] {
            assert_eq!(c.policy_file.as_ref().unwrap().to_string_lossy(), policy(c));
            assert_ne!(
                c.policy_file.as_ref().unwrap(),
                &d.join(".sandbox-policy.generated.yaml")
            );
        }
    }

    #[test]
    fn plan_create_with_keeps_the_shared_generated_file() {
        let d = seeded("shared");
        let c = plan_create_with(&spec(), &ctx(&d, "docker", &[]), None);
        let a = c.steps.last().unwrap().args.join(" ");
        let want = format!("--policy {}/.sandbox-policy.generated.yaml ", d.display());
        assert!(a.contains(&want), "{a}");
        assert!(c.policy_file.is_none());
    }

    #[test]
    fn command_spec_is_one_shot() {
        let d = seeded("cmd");
        let s = SandboxSpec {
            command: "echo hi".into(),
            ..spec()
        };
        let c = plan_create(&s, &ctx(&d, "docker", &[])).unwrap();
        let a = c.steps.last().unwrap().args.join(" ");
        assert!(a.ends_with("--no-keep -- sh -c echo hi"), "{a}");
        assert!(!a.contains("--detach"));
    }

    #[test]
    fn share_without_driver_errors() {
        let d = seeded("nodrv");
        let s = SandboxSpec {
            share: Some(Share {
                path: "/a/b".into(),
                rw: false,
            }),
            ..spec()
        };
        let e = plan_create(&s, &ctx(&d, "", &[])).err().unwrap();
        assert!(e.contains("driver"), "{e}");
        assert!(plan_create(&s, &ctx(&d, "lxc", &[])).is_err());
    }

    #[test]
    fn plan_create_with_keeps_the_dashboards_share_handling() {
        let d = seeded("parity");
        let s = SandboxSpec {
            share: Some(Share {
                path: "/a/b".into(),
                rw: true,
            }),
            ..spec()
        };
        let c = plan_create_with(&s, &ctx(&d, "", &[]), None);
        let a = c.steps.last().unwrap().args.join(" ");
        assert!(a.contains("--driver-config-json {\"\":{"), "{a}");
        assert!(a.contains("--label"), "{a}");
        let none = SandboxSpec {
            share: Some(Share {
                path: "/".into(),
                rw: true,
            }),
            ..spec()
        };
        let c = plan_create_with(&none, &ctx(&d, "docker", &[]), None);
        assert!(!c.steps.last().unwrap().args.join(" ").contains("--label"));
    }

    #[test]
    fn share_uses_driver_key_and_label() {
        let d = seeded("share");
        let s = SandboxSpec {
            share: Some(Share {
                path: "/a/b".into(),
                rw: false,
            }),
            ..spec()
        };
        let c = plan_create(&s, &ctx(&d, "docker", &[])).unwrap();
        let args = &c.steps.last().unwrap().args;
        let i = args
            .iter()
            .position(|a| a == "--driver-config-json")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
        let m = &v["docker"]["mounts"][0];
        assert_eq!(m["type"], "bind");
        assert_eq!(m["read_only"], true);
        assert_eq!(m["target"], "/sandbox/b");
        let label = format!("{}={}", cli::SHARE_LABEL, cli::share_key("/a/b"));
        let j = args.iter().position(|a| a == "--label").unwrap();
        assert_eq!(args[j + 1], label);
    }

    #[test]
    fn rebuild_step_first_when_image_outdated() {
        let d = seeded("rebuild");
        let img = cli::Image {
            tag: "img:1".into(),
            file: d.join("Dockerfile-x"),
            built: false,
            freshness: Default::default(),
        };
        let imgs = [img];
        let c = plan_create(&spec(), &ctx(&d, "docker", &imgs)).unwrap();
        assert_eq!(c.steps.len(), 2);
        assert_eq!(c.steps[0].prog, cli::runtime_bin());
        assert_eq!(c.steps[1].prog, "openshell");
    }

    #[test]
    fn repo_adds_only_the_provider_arg() {
        let d = seeded("repo");
        let s = SandboxSpec {
            repo: Some(RepoGrant {
                url: "https://github.com/o/r".into(),
                env_var: "X".into(),
            }),
            ..spec()
        };
        let c = plan_create(&s, &ctx(&d, "docker", &[])).unwrap();
        let a = c.steps.last().unwrap().args.join(" ");
        assert!(a.contains("--provider github-w1-r"), "{a}");
    }
}
