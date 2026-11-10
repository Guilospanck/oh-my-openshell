//! Blocked-request watcher: follows the logs of each sandbox that has a shell open and
//! turns network policy denials into prompts that add an exact host:port rule.
use crate::cli;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
    time::{Duration, Instant},
};

/// A program the policy kept from reaching a named host; adding the host fixes it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Blocked {
    pub binary: String,
    pub host: String,
    pub port: u16,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Denial {
    Blocked(Blocked),
    /// A denial adding the host would not fix (an L7 rule, or an unknown reason).
    Notice {
        what: String,
        reason: String,
    },
}

impl Blocked {
    /// Program name without its directory, for prompts.
    pub fn program(&self) -> &str {
        self.binary.rsplit('/').next().unwrap_or(&self.binary)
    }

    pub fn target(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

pub fn is_ip(h: &str) -> bool {
    h.parse::<std::net::IpAddr>().is_ok() || h.starts_with('[')
}

/// `[key:value]` fields of an OCSF line, in order.
fn fields(s: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        let Some(j) = rest[i..].find(']') else { break };
        let inner = &rest[i + 1..i + j];
        if let Some((k, v)) = inner.split_once(':') {
            out.push((k, v));
        }
        rest = &rest[i + j + 1..];
    }
    out
}

/// The reason field runs to the last `]`: L7 reasons hold brackets and colons of their own.
fn reason(s: &str) -> String {
    match s.find("[reason:") {
        Some(i) => s[i + 8..].trim_end().trim_end_matches(']').to_string(),
        None => String::new(),
    }
}

/// A policy denial from one `openshell logs` line. `None` for everything else, for DNS
/// refusals (the connection attempt that follows names the program) and for bare addresses.
pub fn parse(line: &str) -> Option<Denial> {
    let at = line.find("OCSF")?;
    let rest = &line[at..];
    let d = rest.find(" DENIED ")?;
    let event = rest[..d]
        .split_whitespace()
        .find(|w| w.contains(':') && !w.starts_with('['))?;
    let body = rest[d + 8..].trim();
    let reason = reason(body);
    let engine = fields(body)
        .into_iter()
        .find(|(k, _)| *k == "engine")
        .map(|(_, v)| v);
    // A policy update closes the tunnels opened under the old one; clients just reconnect.
    if reason == "policy_dns_ineligible" || reason.contains("policy generation is stale") {
        return None;
    }
    let head = body.split(" [").next().unwrap_or_default().trim();
    if event.starts_with("HTTP:") || engine == Some("l7") {
        return Some(Denial::Notice {
            what: head.to_string(),
            reason,
        });
    }
    let (binary, target) = match head.split_once(" -> ") {
        Some((b, t)) => (Some(b.split('(').next().unwrap_or(b).trim()), t.trim()),
        None => (None, head),
    };
    let (host, port) = match target.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()),
        None => (target, None),
    };
    if host.is_empty() || is_ip(host) {
        return None;
    }
    match (binary, port) {
        (Some(b), Some(port)) if b.starts_with('/') && reason.ends_with("policy_denied") => {
            Some(Denial::Blocked(Blocked {
                binary: b.into(),
                host: host.into(),
                port,
                reason,
            }))
        }
        _ => Some(Denial::Notice {
            what: head.to_string(),
            reason,
        }),
    }
}

/// Programs the "web reads" shortcut covers: `curl` and Claude Code.
const WEB_PROGRAMS: &[&str] = &["curl", "claude.exe", "claude"];

/// A read of a web host (HTTP ports, a plain name) by one of `WEB_PROGRAMS`.
pub fn is_web_read(b: &Blocked) -> bool {
    matches!(b.port, 80 | 443)
        && WEB_PROGRAMS.contains(&b.program())
        && cli::valid_host(&b.host)
        && !is_ip(&b.host)
}

/// A read of a web host (HTTP ports, a plain name) by any program. What the persistent
/// "any program" setting covers; `allow` still needs an exact binary path.
pub fn is_any_read(b: &Blocked) -> bool {
    matches!(b.port, 80 | 443) && cli::valid_host(&b.host) && !is_ip(&b.host)
}

/// `(sandbox, binary, host, port)`: what a decision applies to.
pub type Key = (String, String, String, u16);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Pending,
    /// Allowed, read-only or all methods.
    Allowed(bool),
    IgnoredOnce,
    Ignored,
    /// Refused for good in this sandbox, whichever binary asks.
    Never,
    Notice,
}

#[derive(Clone, Debug)]
pub struct Seen {
    pub sandbox: String,
    pub denial: Denial,
    pub state: State,
    pub count: u32,
}

/// What to put in front of the user next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    Blocked {
        sandbox: String,
        gateway: String,
        b: Blocked,
        count: u32,
    },
    Notice {
        sandbox: String,
        what: String,
        reason: String,
    },
}

impl Prompt {
    pub fn sandbox(&self) -> &str {
        match self {
            Prompt::Blocked { sandbox, .. } | Prompt::Notice { sandbox, .. } => sandbox,
        }
    }
}

/// Decisions, the denials seen so far, and the prompts still to show (one at a time).
#[derive(Default)]
pub struct Watch {
    pub seen: Vec<Seen>,
    queue: VecDeque<Prompt>,
    ignored: HashSet<Key>,
    allowed_at: HashMap<Key, Instant>,
    /// `(sandbox, host, port)` the user never wants to allow; kept in `never_file`.
    never: HashSet<(String, String, u16)>,
    never_file: Option<PathBuf>,
    /// Sandboxes where web reads by `curl` and Claude Code are allowed without asking, for the
    /// session. OpenShell refuses wildcard hosts, so this is how "any website" is done: each
    /// denial is answered with the same read-only allow a person would have picked.
    auto_web: HashSet<String>,
    /// Sandboxes where web reads by any program are allowed without asking. Unlike `auto_web`
    /// it outlives the session: kept in `auto_any_file`, one sandbox name per line.
    auto_any: HashSet<String>,
    auto_any_file: Option<PathBuf>,
    /// Allows decided without a prompt, waiting for the app to apply them.
    auto: Vec<(String, String, Blocked)>,
}

/// After an allow, repeats of the same denial are in flight, not a new request.
const SETTLE: Duration = Duration::from_secs(60);

fn key(sandbox: &str, b: &Blocked) -> Key {
    (sandbox.into(), b.binary.clone(), b.host.clone(), b.port)
}

/// Where the never-allow list lives: `$XDG_STATE_HOME/osh/never`, else `~/.local/state/osh/never`.
pub fn never_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("osh/never"))
}

/// Where the "any program" sandboxes are listed: next to `never`, in `osh/auto_any`.
pub fn auto_any_path() -> Option<PathBuf> {
    never_path().map(|p| p.with_file_name("auto_any"))
}

impl Watch {
    /// Read the "any program" sandboxes from, and save them to, `file`.
    pub fn with_auto_any_file(mut self, file: Option<PathBuf>) -> Self {
        self.auto_any = file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| {
                t.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        self.auto_any_file = file;
        self
    }

    fn save_auto_any(&self) {
        let Some(f) = &self.auto_any_file else { return };
        let mut lines: Vec<&str> = self.auto_any.iter().map(String::as_str).collect();
        lines.sort();
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(f, lines.join("\n") + "\n");
    }

    /// A watcher whose never-allow list is read from, and saved to, `file`.
    pub fn with_never_file(file: Option<PathBuf>) -> Self {
        let never = file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| {
                t.lines()
                    .filter_map(|l| {
                        let mut f = l.split('\t');
                        let (sb, host, port) = (f.next()?, f.next()?, f.next()?);
                        Some((sb.to_string(), host.to_string(), port.parse().ok()?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            never,
            never_file: file,
            ..Self::default()
        }
    }

    fn save_never(&self) {
        let Some(f) = &self.never_file else { return };
        let mut lines: Vec<String> = self
            .never
            .iter()
            .map(|(s, h, p)| format!("{s}\t{h}\t{p}"))
            .collect();
        lines.sort();
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(f, lines.join("\n") + "\n");
    }

    fn seen_mut(&mut self, sandbox: &str, d: &Denial) -> Option<&mut Seen> {
        self.seen
            .iter_mut()
            .find(|s| s.sandbox == sandbox && same(&s.denial, d))
    }

    /// Record a denial. A blocked one is queued unless ignored, already queued, or just
    /// allowed (repeats while the new policy loads); a notice is shown once.
    pub fn push(&mut self, sandbox: &str, gateway: &str, d: Denial) {
        let prev = match self.seen_mut(sandbox, &d) {
            Some(s) => {
                s.count += 1;
                Some(s.state.clone())
            }
            None => {
                self.seen.push(Seen {
                    sandbox: sandbox.into(),
                    denial: d.clone(),
                    state: State::Pending,
                    count: 1,
                });
                None
            }
        };
        match d {
            Denial::Blocked(b) => {
                let k = key(sandbox, &b);
                if self
                    .never
                    .contains(&(sandbox.to_string(), b.host.clone(), b.port))
                {
                    self.set(sandbox, &Denial::Blocked(b), State::Never);
                    return;
                }
                if self.ignored.contains(&k)
                    || self
                        .allowed_at
                        .get(&k)
                        .is_some_and(|t| t.elapsed() < SETTLE)
                {
                    return;
                }
                for p in &mut self.queue {
                    if let Prompt::Blocked {
                        sandbox: s,
                        b: q,
                        count,
                        ..
                    } = p
                    {
                        if key(s, q) == k {
                            *count += 1;
                            return;
                        }
                    }
                }
                if (self.auto_web.contains(sandbox) && is_web_read(&b))
                    || (self.auto_any.contains(sandbox) && is_any_read(&b))
                {
                    self.allowed_at.insert(k, Instant::now());
                    self.set(sandbox, &Denial::Blocked(b.clone()), State::Allowed(false));
                    self.auto.push((sandbox.into(), gateway.into(), b));
                    return;
                }
                self.set(sandbox, &Denial::Blocked(b.clone()), State::Pending);
                self.queue.push_back(Prompt::Blocked {
                    sandbox: sandbox.into(),
                    gateway: gateway.into(),
                    b,
                    count: 1,
                });
            }
            Denial::Notice { what, reason } => {
                if prev.is_none() {
                    self.set(
                        sandbox,
                        &Denial::Notice {
                            what: what.clone(),
                            reason: reason.clone(),
                        },
                        State::Notice,
                    );
                    self.queue.push_back(Prompt::Notice {
                        sandbox: sandbox.into(),
                        what,
                        reason,
                    });
                }
            }
        }
    }

    fn set(&mut self, sandbox: &str, d: &Denial, state: State) {
        if let Some(s) = self.seen_mut(sandbox, d) {
            s.state = state;
        }
    }

    /// The next prompt to show; it leaves the queue, so repeats while it is open queue again
    /// only once answered (`decide`) unless they coalesce into a later entry.
    /// `only` names the sandbox whose shell is open: prompts of other sandboxes stay queued until
    /// their own shell has focus, so one sandbox never interrupts another. `None` (dashboard)
    /// takes any.
    pub fn next(&mut self, only: Option<&str>) -> Option<Prompt> {
        let i = self
            .queue
            .iter()
            .position(|p| only.is_none_or(|o| p.sandbox() == o))?;
        self.queue.remove(i)
    }

    pub fn has_queued(&self, only: Option<&str>) -> bool {
        self.queue
            .iter()
            .any(|p| only.is_none_or(|o| p.sandbox() == o))
    }

    /// Record the answer to a blocked prompt.
    pub fn decide(&mut self, sandbox: &str, b: &Blocked, state: State) {
        let k = key(sandbox, b);
        if state == State::Ignored {
            self.ignored.insert(k.clone());
        } else {
            self.ignored.remove(&k);
        }
        if state == State::Never {
            self.never.insert((sandbox.into(), b.host.clone(), b.port));
            self.save_never();
        }
        if matches!(state, State::Allowed(_)) {
            self.allowed_at.insert(k.clone(), Instant::now());
        }
        self.queue
            .retain(|p| !matches!(p, Prompt::Blocked { sandbox: s, b: q, .. } if key(s, q) == k));
        self.set(sandbox, &Denial::Blocked(b.clone()), state);
    }

    /// From now until the shell closes, allow web reads by `curl` and Claude Code in `sandbox`
    /// without asking; prompts already queued for it that qualify are answered the same way.
    pub fn enable_auto_web(&mut self, sandbox: &str) {
        self.auto_web.insert(sandbox.into());
        self.answer_queued(sandbox, is_web_read);
    }

    /// Allow web reads by any program in `sandbox` without asking, also in later sessions,
    /// until `disable_auto_any`. Queued prompts that qualify are answered the same way.
    pub fn enable_auto_any(&mut self, sandbox: &str) {
        self.auto_any.insert(sandbox.into());
        self.save_auto_any();
        self.answer_queued(sandbox, is_any_read);
    }

    pub fn disable_auto_any(&mut self, sandbox: &str) {
        if self.auto_any.remove(sandbox) {
            self.save_auto_any();
        }
    }

    pub fn auto_any_on(&self, sandbox: &str) -> bool {
        self.auto_any.contains(sandbox)
    }

    fn answer_queued(&mut self, sandbox: &str, covered: fn(&Blocked) -> bool) {
        let queued: Vec<Prompt> = self.queue.drain(..).collect();
        for p in queued {
            match p {
                Prompt::Blocked {
                    sandbox: s,
                    gateway,
                    b,
                    ..
                } if s == sandbox && covered(&b) => {
                    self.allowed_at.insert(key(&s, &b), Instant::now());
                    self.set(&s, &Denial::Blocked(b.clone()), State::Allowed(false));
                    self.auto.push((s, gateway, b));
                }
                other => self.queue.push_back(other),
            }
        }
    }

    /// Apply `b` through the same one-at-a-time path as the automatic allows.
    pub fn auto_allow(&mut self, sandbox: &str, gateway: &str, b: Blocked) {
        self.auto.push((sandbox.into(), gateway.into(), b));
    }

    pub fn auto_web_on(&self, sandbox: &str) -> bool {
        self.auto_web.contains(sandbox)
    }

    /// The allows decided without a prompt since the last call.
    pub fn take_auto(&mut self) -> Vec<(String, String, Blocked)> {
        std::mem::take(&mut self.auto)
    }

    /// Ask about `b` again now, whatever was decided before.
    pub fn reprompt(&mut self, sandbox: &str, gateway: &str, b: Blocked) {
        let k = key(sandbox, &b);
        if self
            .never
            .remove(&(sandbox.to_string(), b.host.clone(), b.port))
        {
            self.save_never();
        }
        self.ignored.remove(&k);
        self.allowed_at.remove(&k);
        self.queue
            .retain(|p| !matches!(p, Prompt::Blocked { sandbox: s, b: q, .. } if key(s, q) == k));
        self.set(sandbox, &Denial::Blocked(b.clone()), State::Pending);
        self.queue.push_front(Prompt::Blocked {
            sandbox: sandbox.into(),
            gateway: gateway.into(),
            b,
            count: 1,
        });
    }

    /// The shell in `sandbox` closed: forget its ignores and queued prompts.
    pub fn end_session(&mut self, sandbox: &str) {
        self.auto_web.remove(sandbox);
        self.auto.retain(|a| a.0 != sandbox);
        self.ignored.retain(|k| k.0 != sandbox);
        self.allowed_at.retain(|k, _| k.0 != sandbox);
        self.queue.retain(|p| match p {
            Prompt::Blocked { sandbox: s, .. } | Prompt::Notice { sandbox: s, .. } => s != sandbox,
        });
        self.seen.retain(|s| s.sandbox != sandbox);
    }

    pub fn for_sandbox(&self, sandbox: &str) -> Vec<&Seen> {
        self.seen.iter().filter(|s| s.sandbox == sandbox).collect()
    }
}

/// Same denial, ignoring the reason text of blocked ones (it varies by path).
fn same(a: &Denial, b: &Denial) -> bool {
    match (a, b) {
        (Denial::Blocked(x), Denial::Blocked(y)) => {
            (&x.binary, &x.host, x.port) == (&y.binary, &y.host, y.port)
        }
        (
            Denial::Notice {
                what: w1,
                reason: r1,
            },
            Denial::Notice {
                what: w2,
                reason: r2,
            },
        ) => w1 == w2 && r1 == r2,
        _ => false,
    }
}

/// `openshell logs --tail` for one sandbox; lines arrive on a channel the UI drains.
pub struct Follow {
    pub sandbox: String,
    pub gateway: String,
    child: Option<Child>,
    rx: Receiver<String>,
}

fn pump(r: impl Read + Send + 'static, tx: Sender<String>) {
    thread::spawn(move || {
        for line in BufReader::new(r).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

impl Follow {
    /// Only new lines: `--since 1s` skips the backlog `--tail` would replay.
    pub fn spawn(sandbox: String, gateway: String) -> Result<Self, String> {
        let mut args: Vec<String> = Vec::new();
        if !gateway.is_empty() {
            args.extend(["-g".into(), gateway.clone()]);
        }
        args.extend(
            [
                "logs", &sandbox, "--tail", "--since", "1s", "--source", "sandbox",
            ]
            .map(String::from),
        );
        let mut child = Command::new("openshell")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("openshell logs: {e}"))?;
        let (tx, rx) = mpsc::channel();
        if let Some(o) = child.stdout.take() {
            pump(o, tx.clone());
        }
        if let Some(e) = child.stderr.take() {
            pump(e, tx);
        }
        Ok(Follow {
            sandbox,
            gateway,
            child: Some(child),
            rx,
        })
    }

    /// Lines so far; `Err` with the last line seen once the stream has ended.
    pub fn drain(&mut self) -> Result<Vec<String>, String> {
        let mut v = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(l) => v.push(l),
                Err(TryRecvError::Empty) => return Ok(v),
                Err(TryRecvError::Disconnected) => {
                    if let Some(mut c) = self.child.take() {
                        let _ = c.wait();
                    }
                    return Err(v.pop().unwrap_or_default());
                }
            }
        }
    }
}

impl Drop for Follow {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Endpoint spec for `policy update --add-endpoint`: read-only needs REST inspection to be
/// enforced (without it every method passes); `full` allows all traffic.
pub fn endpoint(b: &Blocked, all: bool) -> String {
    if all {
        format!("{}:full", b.target())
    } else {
        format!("{}:read-only:rest:enforce", b.target())
    }
}

/// The registrable domain of `host` by the public suffix list (`dl.google.com` gives
/// `google.com`, `a.b.github.io` gives `b.github.io`). `None` for a bare suffix or an unknown one.
pub fn registrable_domain(host: &str) -> Option<String> {
    psl::domain_str(host).map(str::to_string)
}

/// The update for `allow_domain`: the domain and every subdomain (`**.D` does not match `D`
/// itself), read-only, for any program. One update, so one policy reload.
pub fn domain_update_args(gateway: &str, sandbox: &str, port: u16, domain: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if !gateway.is_empty() {
        args.extend(["-g".into(), gateway.into()]);
    }
    args.extend(["policy".into(), "update".into(), sandbox.into()]);
    for host in [domain.to_string(), format!("**.{domain}")] {
        args.extend([
            "--add-endpoint".into(),
            format!("{host}:{port}:read-only:rest:enforce"),
        ]);
    }
    args.extend(["--binary", "/**", "--wait", "--timeout", "60"].map(String::from));
    args
}

/// Allow read-only web access for any program to the whole registrable domain of `b.host`, so
/// the next host under it (a CDN, a download redirect) does not fail on its first request.
/// Err when the host has no registrable domain or OpenShell refuses the update (an overlap with
/// an endpoint of different settings); the caller then falls back to `allow`.
pub fn allow_domain(gateway: &str, sandbox: &str, b: &Blocked) -> Result<String, String> {
    if !cli::valid_host(&b.host) || is_ip(&b.host) {
        return Err(format!("not a plain host name: {}", b.host));
    }
    let domain = registrable_domain(&b.host)
        .ok_or_else(|| format!("{} has no registrable domain", b.host))?;
    cli::run(
        "openshell",
        &domain_update_args(gateway, sandbox, b.port, &domain),
        &[],
    )?;
    Ok(format!(
        "Allowed any program to read {domain} and its subdomains in {sandbox}; it is live, ask again"
    ))
}

/// Add exactly `host:port` for exactly one binary to the running sandbox.
pub fn allow(gateway: &str, sandbox: &str, b: &Blocked, all: bool) -> Result<String, String> {
    if !cli::valid_host(&b.host) || is_ip(&b.host) {
        return Err(format!(
            "refusing to allow {}: not a plain host name",
            b.host
        ));
    }
    if !b.binary.starts_with('/') || b.binary.contains(|c: char| c.is_whitespace() || c == '*') {
        return Err(format!(
            "refusing to allow {}: not an exact binary path",
            b.binary
        ));
    }
    let ep = endpoint(b, all);
    let mut args: Vec<String> = Vec::new();
    if !gateway.is_empty() {
        args.extend(["-g".into(), gateway.into()]);
    }
    args.extend(
        [
            "policy",
            "update",
            sandbox,
            "--add-endpoint",
            &ep,
            "--binary",
            &b.binary,
            "--wait",
            "--timeout",
            "60",
        ]
        .map(String::from),
    );
    cli::run("openshell", &args, &[])?;
    let what = if all { "all methods" } else { "read-only" };
    Ok(format!(
        "Allowed {} to reach {} ({what}) in {sandbox}; it is live, ask again",
        b.program(),
        b.target()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURL: &str = "[1792770856.781] [sandbox] [OCSF ] [ocsf] NET:OPEN [MED] DENIED /usr/bin/curl(0) -> example.com:443 [reason:transparent_tcp_policy_denied]";
    const CURL_ISO: &str = "2026-10-23T15:54:16.781Z OCSF NET:OPEN [MED] DENIED /usr/bin/curl(0) -> example.com:443 [reason:transparent_tcp_policy_denied]";
    const DNS: &str = "[1792770856.779] [sandbox] [OCSF ] [ocsf] NET:REFUSE [MED] DENIED example.com [reason:policy_dns_ineligible]";
    const L7: &str = "[1] [sandbox] [OCSF ] [ocsf] NET:OPEN [MED] DENIED registry.npmjs.org:443 [policy:_provider_npm engine:l7] [reason:HTTP request-target rejected: request-target contains an encoded '/' ...]";
    const HTTP: &str = "[1792770946.183] [sandbox] [OCSF ] [ocsf] HTTP:POST [MED] DENIED POST http://example.com:443/ [policy:allow_example_com_443 engine:l7] [reason:L7_REQUEST deny POST example.com:443/ reason=POST / not permitted by policy]";
    const IP: &str = "[1] [sandbox] [OCSF ] [ocsf] NET:OPEN [MED] DENIED /usr/local/bin/node(0) -> 198.18.0.2:443 [reason:transparent_tcp_mapping_denied]";
    const CHATTER: [&str; 6] = [
        "[1792770856.779] [sandbox] [OCSF ] [ocsf] CONFIG:OBSERVATION [INFO] Policy DNS staged unapproved name example.com synthetic=198.18.0.2 for TCP policy review",
        "[1792770856.781] [sandbox] [WARN ] [openshell_supervisor_network::proxy] Denied staged transparent connection destination=198.18.0.2:443 reason=network connections not allowed by policy",
        "[1792771053.674] [sandbox] [OCSF ] [ocsf] SSH:OPEN [INFO] ALLOWED",
        "[1792770946.000] [sandbox] [OCSF ] [ocsf] HTTP:GET [INFO] ALLOWED GET http://example.com:443/ [policy:allow_example_com_443 engine:l7]",
        "[1792770846.129] [sandbox] [OCSF ] [ocsf] NET:LISTEN [INFO] 127.0.0.1:3128",
        "2026-10-23T15:55:46.150Z OCSF NET:OPEN [INFO] ALLOWED /usr/bin/curl(0) -> example.com:443 [policy:allow_example_com_443 engine:opa]",
    ];

    fn curl() -> Blocked {
        Blocked {
            binary: "/usr/bin/curl".into(),
            host: "example.com".into(),
            port: 443,
            reason: "transparent_tcp_policy_denied".into(),
        }
    }

    #[test]
    fn parser_offers_only_fixable_denials() {
        let stale = "[1] [sandbox] [OCSF ] [ocsf] NET:OPEN [MED] DENIED api.anthropic.com:443 [reason:L7 tunnel closed before inspection because policy changed: policy generation is stale [captured_generation:1 current_generation:2]]";
        assert_eq!(parse(stale), None, "a policy reload is not a denial");
        assert_eq!(parse(CURL), Some(Denial::Blocked(curl())));
        assert_eq!(parse(CURL_ISO), Some(Denial::Blocked(curl())));
        assert_eq!(parse(DNS), None);
        assert_eq!(parse(IP), None);
        for l in CHATTER {
            assert_eq!(parse(l), None, "{l}");
        }
        match parse(L7) {
            Some(Denial::Notice { what, reason }) => {
                assert_eq!(what, "registry.npmjs.org:443");
                assert!(reason.starts_with("HTTP request-target rejected"));
            }
            d => panic!("{d:?}"),
        }
        match parse(HTTP) {
            Some(Denial::Notice { what, reason }) => {
                assert_eq!(what, "POST http://example.com:443/");
                assert!(reason.ends_with("not permitted by policy"), "{reason}");
            }
            d => panic!("{d:?}"),
        }
        let odd = CURL.replace("transparent_tcp_policy_denied", "something_new");
        assert!(matches!(parse(&odd), Some(Denial::Notice { .. })));
        assert_eq!(curl().program(), "curl");
        assert_eq!(
            endpoint(&curl(), false),
            "example.com:443:read-only:rest:enforce"
        );
        assert_eq!(endpoint(&curl(), true), "example.com:443:full");
    }

    #[test]
    fn web_reads_by_curl_are_allowed_without_a_prompt_once_enabled() {
        let mut w = Watch::default();
        w.push("a", "gw", Denial::Blocked(curl()));
        w.enable_auto_web("a");
        assert!(!w.has_queued(None), "the queued curl prompt was answered");
        assert_eq!(w.take_auto().len(), 1);
        let mut other = curl();
        other.host = "docs.rs".into();
        w.push("a", "gw", Denial::Blocked(other));
        assert!(!w.has_queued(None));
        assert_eq!(w.take_auto()[0].2.host, "docs.rs");
        let mut node = curl();
        node.binary = "/usr/local/bin/node".into();
        node.host = "evil.example".into();
        w.push("a", "gw", Denial::Blocked(node));
        assert!(w.has_queued(None), "only curl and claude are covered");
        let mut ssh = curl();
        ssh.port = 22;
        ssh.host = "git.example".into();
        w.push("a", "gw", Denial::Blocked(ssh));
        assert!(w.take_auto().is_empty(), "only HTTP ports");
        w.push("b", "gw", Denial::Blocked(curl()));
        assert!(w.take_auto().is_empty(), "another sandbox still asks");
        w.end_session("a");
        assert!(!w.auto_web_on("a"));
    }

    #[test]
    fn any_program_reads_are_allowed_for_the_sandbox_that_opted_in_and_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("osh-auto-any-{}", std::process::id()));
        let file = dir.join("auto_any");
        let _ = std::fs::remove_dir_all(&dir);
        let mut w = Watch::default().with_auto_any_file(Some(file.clone()));
        let mut node = curl();
        node.binary = "/usr/local/bin/node".into();
        node.host = "registry.example".into();
        w.push("a", "gw", Denial::Blocked(node.clone()));
        assert!(w.has_queued(None), "asks until the sandbox opts in");
        w.enable_auto_any("a");
        assert!(!w.has_queued(None), "the queued prompt was answered");
        assert_eq!(w.take_auto().len(), 1);
        node.host = "ziglang.org".into();
        w.push("a", "gw", Denial::Blocked(node.clone()));
        assert_eq!(w.take_auto()[0].2.host, "ziglang.org");
        let mut ssh = node.clone();
        ssh.port = 22;
        w.push("a", "gw", Denial::Blocked(ssh));
        assert!(w.take_auto().is_empty(), "only HTTP ports");
        w.push("b", "gw", Denial::Blocked(node.clone()));
        assert!(w.take_auto().is_empty(), "another sandbox still asks");
        w.end_session("a");
        assert!(w.auto_any_on("a"), "outlives the session");
        let mut again = Watch::default().with_auto_any_file(Some(file.clone()));
        assert!(again.auto_any_on("a") && !again.auto_any_on("b"));
        again.disable_auto_any("a");
        let last = Watch::default().with_auto_any_file(Some(file));
        assert!(!last.auto_any_on("a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_registrable_domain_follows_the_public_suffix_list() {
        let d = |h| registrable_domain(h);
        assert_eq!(d("dl.google.com").as_deref(), Some("google.com"));
        assert_eq!(d("google.com").as_deref(), Some("google.com"));
        assert_eq!(d("a.b.example.co.uk").as_deref(), Some("example.co.uk"));
        assert_eq!(d("com"), None, "a bare suffix has no registrable domain");
        let args = domain_update_args("gw", "sb", 443, "google.com").join(" ");
        assert_eq!(
            args,
            "-g gw policy update sb --add-endpoint google.com:443:read-only:rest:enforce --add-endpoint **.google.com:443:read-only:rest:enforce --binary /** --wait --timeout 60"
        );
    }

    #[test]
    fn a_prompt_waits_for_its_own_sandbox_shell() {
        let mut w = Watch::default();
        w.push("a", "gw", Denial::Blocked(curl()));
        w.push("b", "gw", Denial::Blocked(curl()));
        assert!(w.next(Some("c")).is_none());
        assert!(
            matches!(w.next(Some("b")), Some(Prompt::Blocked { sandbox, .. }) if sandbox == "b")
        );
        assert!(w.has_queued(Some("a")) && !w.has_queued(Some("b")));
    }

    #[test]
    fn bursts_coalesce_and_ignores_last_for_the_session() {
        let mut w = Watch::default();
        let d = Denial::Blocked(curl());
        for _ in 0..3 {
            w.push("sb", "gw", d.clone());
        }
        let other = Blocked {
            host: "example.org".into(),
            ..curl()
        };
        w.push("sb", "gw", Denial::Blocked(other.clone()));
        match w.next(None) {
            Some(Prompt::Blocked { b, count, .. }) => assert_eq!((b, count), (curl(), 3)),
            p => panic!("{p:?}"),
        }
        // A repeat while the prompt is open queues once more; answering drops it.
        w.push("sb", "gw", d.clone());
        w.decide("sb", &curl(), State::Ignored);
        assert!(matches!(w.next(None), Some(Prompt::Blocked { b, .. }) if b == other));
        assert!(w.next(None).is_none());
        w.push("sb", "gw", d.clone());
        assert!(!w.has_queued(None), "ignored until the session ends");
        assert_eq!(w.for_sandbox("sb")[0].state, State::Ignored);
        // Once only: the next denial asks again.
        w.decide("sb", &other, State::IgnoredOnce);
        w.push("sb", "gw", Denial::Blocked(other.clone()));
        assert!(w.has_queued(None));
        // Notices show once.
        let n = parse(L7).unwrap();
        w.push("sb", "gw", n.clone());
        w.push("sb", "gw", n);
        assert!(matches!(w.next(None), Some(Prompt::Blocked { .. })));
        assert!(matches!(w.next(None), Some(Prompt::Notice { .. })));
        assert!(w.next(None).is_none());
        // Never: refused for good, even after the session ends, until asked again.
        let never = Blocked {
            host: "example.net".into(),
            binary: "/usr/bin/wget".into(),
            ..curl()
        };
        w.push("sb", "gw", Denial::Blocked(never.clone()));
        assert!(matches!(w.next(None), Some(Prompt::Blocked { b, .. }) if b == never));
        w.decide("sb", &never, State::Never);
        let same_host = Blocked {
            binary: "/usr/bin/curl".into(),
            ..never.clone()
        };
        w.push("sb", "gw", Denial::Blocked(same_host.clone()));
        assert!(!w.has_queued(None), "never covers every binary");
        w.end_session("sb");
        w.push("sb", "gw", Denial::Blocked(same_host.clone()));
        assert!(!w.has_queued(None), "never outlives the session");
        w.reprompt("sb", "gw", same_host);
        assert!(w.has_queued(None), "asking again lifts it");
        w.next(None);
        w.next(None);
        w.end_session("sb");
        w.push("sb", "gw", d);
        assert!(w.has_queued(None), "ignores end with the session");
    }
}
