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

fn is_ip(h: &str) -> bool {
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

impl Watch {
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
    pub fn next(&mut self) -> Option<Prompt> {
        self.queue.pop_front()
    }

    pub fn has_queued(&self) -> bool {
        !self.queue.is_empty()
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
        "Allowed {} to reach {} ({what}) in {sandbox}",
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
        match w.next() {
            Some(Prompt::Blocked { b, count, .. }) => assert_eq!((b, count), (curl(), 3)),
            p => panic!("{p:?}"),
        }
        // A repeat while the prompt is open queues once more; answering drops it.
        w.push("sb", "gw", d.clone());
        w.decide("sb", &curl(), State::Ignored);
        assert!(matches!(w.next(), Some(Prompt::Blocked { b, .. }) if b == other));
        assert!(w.next().is_none());
        w.push("sb", "gw", d.clone());
        assert!(!w.has_queued(), "ignored until the session ends");
        assert_eq!(w.for_sandbox("sb")[0].state, State::Ignored);
        // Once only: the next denial asks again.
        w.decide("sb", &other, State::IgnoredOnce);
        w.push("sb", "gw", Denial::Blocked(other.clone()));
        assert!(w.has_queued());
        // Notices show once.
        let n = parse(L7).unwrap();
        w.push("sb", "gw", n.clone());
        w.push("sb", "gw", n);
        assert!(matches!(w.next(), Some(Prompt::Blocked { .. })));
        assert!(matches!(w.next(), Some(Prompt::Notice { .. })));
        assert!(w.next().is_none());
        // Never: refused for good, even after the session ends, until asked again.
        let never = Blocked {
            host: "example.net".into(),
            binary: "/usr/bin/wget".into(),
            ..curl()
        };
        w.push("sb", "gw", Denial::Blocked(never.clone()));
        assert!(matches!(w.next(), Some(Prompt::Blocked { b, .. }) if b == never));
        w.decide("sb", &never, State::Never);
        let same_host = Blocked {
            binary: "/usr/bin/curl".into(),
            ..never.clone()
        };
        w.push("sb", "gw", Denial::Blocked(same_host.clone()));
        assert!(!w.has_queued(), "never covers every binary");
        w.end_session("sb");
        w.push("sb", "gw", Denial::Blocked(same_host.clone()));
        assert!(!w.has_queued(), "never outlives the session");
        w.reprompt("sb", "gw", same_host);
        assert!(w.has_queued(), "asking again lifts it");
        w.next();
        w.next();
        w.end_session("sb");
        w.push("sb", "gw", d);
        assert!(w.has_queued(), "ignores end with the session");
    }
}
