//! Listening-port watcher: polls the TCP listeners of each sandbox that has a shell open and
//! offers to forward a newly opened one to the host (a web server, a served page).
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc,
    },
    thread,
    time::Duration,
};

/// Ports every sandbox listens on for its own sake: the policy proxy.
const NOISE: &[u16] = &[3128];

/// How often a sandbox is asked for its listeners.
const EVERY: Duration = Duration::from_secs(3);

/// Ports that accept TCP connections from `/proc/net/tcp` and `/proc/net/tcp6` text, bound to
/// loopback or to every address. Listeners bound to one other interface cannot be reached by a
/// forward (it dials loopback in the sandbox), so they are left out.
pub fn parse_listeners(text: &str) -> BTreeSet<u16> {
    const REACHABLE: &[&str] = &[
        "0100007F",
        "00000000",
        "00000000000000000000000001000000",
        "00000000000000000000000000000000",
        "0000000000000000FFFF00000100007F",
        "0000000000000000FFFF000000000000",
    ];
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(_sl), Some(local), Some(_remote), Some(state)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        // 0A is TCP_LISTEN.
        if state != "0A" {
            continue;
        }
        let Some((addr, port)) = local.split_once(':') else {
            continue;
        };
        if !REACHABLE.contains(&addr.to_ascii_uppercase().as_str()) {
            continue;
        }
        if let Ok(p) = u16::from_str_radix(port, 16) {
            if p != 0 && !NOISE.contains(&p) {
                out.insert(p);
            }
        }
    }
    out
}

/// Polls one sandbox on a thread; listener sets arrive on a channel the UI drains.
pub struct Scan {
    pub sandbox: String,
    pub gateway: String,
    stop: Arc<AtomicBool>,
    rx: Receiver<BTreeSet<u16>>,
}

/// The `openshell` arguments that print a sandbox's TCP tables.
fn scan_args(sandbox: &str, gateway: &str) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if !gateway.is_empty() {
        a.extend(["-g".into(), gateway.into()]);
    }
    a.extend(
        [
            "sandbox",
            "exec",
            "--name",
            sandbox,
            "--timeout",
            "10",
            "--no-tty",
            "--",
            "cat",
            "/proc/net/tcp",
            "/proc/net/tcp6",
        ]
        .map(String::from),
    );
    a
}

impl Scan {
    pub fn spawn(sandbox: String, gateway: String) -> Self {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (s, sb, gw) = (stop.clone(), sandbox.clone(), gateway.clone());
        thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                // `cat` exits non-zero when the IPv6 table is missing; stdout still holds IPv4.
                let out = Command::new("openshell")
                    .args(scan_args(&sb, &gw))
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output();
                if let Ok(o) = out {
                    let text = String::from_utf8_lossy(&o.stdout);
                    // No table at all means the exec failed (sandbox busy or stopped): skip
                    // the tick, so a failure never reads as "everything closed".
                    if text.contains("local_address") && tx.send(parse_listeners(&text)).is_err() {
                        break;
                    }
                }
                let mut waited = Duration::ZERO;
                while waited < EVERY && !s.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                    waited += Duration::from_millis(100);
                }
            }
        });
        Scan {
            sandbox,
            gateway,
            stop,
            rx,
        }
    }

    /// The newest listener set since the last call.
    pub fn latest(&mut self) -> Option<BTreeSet<u16>> {
        let mut last = None;
        loop {
            match self.rx.try_recv() {
                Ok(s) => last = Some(s),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return last,
            }
        }
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A port waiting for an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub sandbox: String,
    pub gateway: String,
    pub port: u16,
}

#[derive(Default)]
struct PerBox {
    /// Whether the first scan (the baseline) has been taken.
    seen_first: bool,
    /// Listening now and already asked about, or present when the shell opened.
    known: BTreeSet<u16>,
    /// Never ask again in this session.
    ignored: HashSet<u16>,
    /// Ports osh forwards for this sandbox.
    forwarded: BTreeSet<u16>,
    /// What listens as of the latest scan.
    listening: BTreeSet<u16>,
}

/// Per-sandbox listener state and the offers still to show.
#[derive(Default)]
pub struct Ports {
    boxes: HashMap<String, PerBox>,
    queue: VecDeque<Offer>,
    /// Forwards whose port stopped listening: `(sandbox, gateway, port)`, for the app to stop.
    gone: Vec<(String, String, u16)>,
}

impl Ports {
    /// Take one scan. The first scan of a sandbox is the baseline: what already listens when the
    /// shell opens is not offered. Later, each port that was not listening before is queued,
    /// and a forwarded port that stopped listening is reported by `take_gone`.
    pub fn observe(&mut self, sandbox: &str, gateway: &str, now: &BTreeSet<u16>) {
        let b = self.boxes.entry(sandbox.into()).or_default();
        b.listening = now.clone();
        if !b.seen_first {
            b.seen_first = true;
            b.known = now.clone();
            return;
        }
        for p in b.known.clone() {
            if !now.contains(&p) {
                b.known.remove(&p);
                if b.forwarded.remove(&p) {
                    self.gone.push((sandbox.into(), gateway.into(), p));
                }
                self.queue
                    .retain(|o| !(o.sandbox == sandbox && o.port == p));
            }
        }
        for &p in now {
            if b.known.insert(p) && !b.ignored.contains(&p) {
                self.queue.push_back(Offer {
                    sandbox: sandbox.into(),
                    gateway: gateway.into(),
                    port: p,
                });
            }
        }
    }

    pub fn has_queued(&self, only: Option<&str>) -> bool {
        self.queue
            .iter()
            .any(|o| only.is_none_or(|s| o.sandbox == s))
    }

    /// The next offer for the sandbox whose shell is open (`None`: any), like `Watch::next`.
    pub fn next(&mut self, only: Option<&str>) -> Option<Offer> {
        let i = self
            .queue
            .iter()
            .position(|o| only.is_none_or(|s| o.sandbox == s))?;
        self.queue.remove(i)
    }

    pub fn forwarded(&mut self, sandbox: &str, port: u16) {
        self.boxes
            .entry(sandbox.into())
            .or_default()
            .forwarded
            .insert(port);
    }

    /// Ports osh forwards for `sandbox`.
    pub fn forwards(&self, sandbox: &str) -> Vec<u16> {
        self.boxes
            .get(sandbox)
            .map(|b| b.forwarded.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Ports listening in `sandbox` as of the latest scan that are not forwarded.
    pub fn unforwarded(&self, sandbox: &str) -> Vec<u16> {
        self.boxes
            .get(sandbox)
            .map(|b| b.listening.difference(&b.forwarded).copied().collect())
            .unwrap_or_default()
    }

    /// Forget a forward the user stopped.
    pub fn unforward(&mut self, sandbox: &str, port: u16) {
        if let Some(b) = self.boxes.get_mut(sandbox) {
            b.forwarded.remove(&port);
        }
    }

    pub fn ignore(&mut self, sandbox: &str, port: u16) {
        self.boxes
            .entry(sandbox.into())
            .or_default()
            .ignored
            .insert(port);
    }

    pub fn take_gone(&mut self) -> Vec<(String, String, u16)> {
        std::mem::take(&mut self.gone)
    }

    /// The shell closed: forget the sandbox. Returns the ports osh had forwarded, to stop.
    pub fn end_session(&mut self, sandbox: &str) -> Vec<u16> {
        self.queue.retain(|o| o.sandbox != sandbox);
        self.gone.retain(|g| g.0 != sandbox);
        self.boxes
            .remove(sandbox)
            .map(|b| b.forwarded.into_iter().collect())
            .unwrap_or_default()
    }
}

/// `openshell forward start`/`stop` arguments for one port.
pub fn forward_args(verb: &str, sandbox: &str, gateway: &str, port: u16) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if !gateway.is_empty() {
        a.extend(["-g".into(), gateway.into()]);
    }
    a.extend(["forward".into(), verb.into()]);
    if verb == "start" {
        a.push("-d".into());
    }
    a.extend([port.to_string(), sandbox.into()]);
    a
}

/// Whether the host can bind `port` on loopback, which a forward needs.
pub fn host_port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11 1 0 100 0 0 10 0
   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12 1 0 100 0 0 10 0
   2: 0100007F:0C38 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 13 1 0 100 0 0 10 0
   3: 0100007F:2328 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 14 1 0 100 0 0 10 0
   4: 0A00020F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 15 1 0 100 0 0 10 0
";

    fn set(p: &[u16]) -> BTreeSet<u16> {
        p.iter().copied().collect()
    }

    #[test]
    fn only_reachable_listeners_count() {
        // 8000 on loopback and 22 on every address; 3128 is the proxy, 9000 is a connection (state
        // 01), 8080 is bound to one other interface.
        assert_eq!(parse_listeners(TCP), set(&[22, 8000]));
    }

    #[test]
    fn ipv6_listeners_count_and_the_header_is_skipped() {
        let v6 = "  sl  local_address rem_address st\n   0: 00000000000000000000000001000000:0BB8 00000000000000000000000000000000:0000 0A x\n   1: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A x\n";
        assert_eq!(parse_listeners(v6), set(&[3000, 8080]));
        assert!(parse_listeners("").is_empty());
    }

    #[test]
    fn the_first_scan_is_a_baseline_and_a_new_port_is_offered_once() {
        let mut p = Ports::default();
        p.observe("sb", "gw", &set(&[22]));
        assert!(!p.has_queued(None), "what listened already is not offered");
        p.observe("sb", "gw", &set(&[22, 8000]));
        p.observe("sb", "gw", &set(&[22, 8000]));
        let o = p.next(Some("sb")).unwrap();
        assert_eq!((o.port, o.gateway.as_str()), (8000, "gw"));
        assert!(
            p.next(None).is_none(),
            "asked once while it keeps listening"
        );
    }

    #[test]
    fn a_port_that_closes_and_opens_again_is_offered_again() {
        let mut p = Ports::default();
        p.observe("sb", "gw", &set(&[]));
        p.observe("sb", "gw", &set(&[8000]));
        p.next(None).unwrap();
        p.observe("sb", "gw", &set(&[]));
        p.observe("sb", "gw", &set(&[8000]));
        assert!(p.next(None).is_some());
    }

    #[test]
    fn an_ignored_port_is_not_offered_again() {
        let mut p = Ports::default();
        p.observe("sb", "gw", &set(&[]));
        p.ignore("sb", 8000);
        p.observe("sb", "gw", &set(&[8000]));
        assert!(!p.has_queued(None));
    }

    #[test]
    fn offers_wait_for_their_own_sandbox() {
        let mut p = Ports::default();
        for sb in ["a", "b"] {
            p.observe(sb, "gw", &set(&[]));
        }
        p.observe("b", "gw", &set(&[8000]));
        assert!(!p.has_queued(Some("a")));
        assert!(p.next(Some("a")).is_none());
        assert!(p.has_queued(None) && p.next(Some("b")).is_some());
    }

    #[test]
    fn a_forward_whose_port_closed_is_reported_and_ending_the_session_lists_the_rest() {
        let mut p = Ports::default();
        p.observe("sb", "gw", &set(&[]));
        p.observe("sb", "gw", &set(&[8000, 9000]));
        p.forwarded("sb", 8000);
        p.forwarded("sb", 9000);
        p.observe("sb", "gw", &set(&[9000]));
        assert_eq!(p.take_gone(), vec![("sb".into(), "gw".into(), 8000)]);
        assert_eq!(p.end_session("sb"), vec![9000]);
        assert!(!p.has_queued(None));
    }

    #[test]
    fn the_menu_lists_forwards_and_the_listeners_not_yet_forwarded() {
        let mut p = Ports::default();
        p.observe("sb", "gw", &set(&[22]));
        p.observe("sb", "gw", &set(&[22, 8000, 9000]));
        p.forwarded("sb", 8000);
        assert_eq!(p.forwards("sb"), vec![8000]);
        assert_eq!(p.unforwarded("sb"), vec![22, 9000]);
        p.unforward("sb", 8000);
        assert!(p.forwards("sb").is_empty());
        assert_eq!(p.unforwarded("sb"), vec![22, 8000, 9000]);
        assert!(p.forwards("other").is_empty() && p.unforwarded("other").is_empty());
    }

    #[test]
    fn forward_arguments_name_the_gateway_port_and_sandbox() {
        assert_eq!(
            forward_args("start", "sb", "gw", 8000),
            ["-g", "gw", "forward", "start", "-d", "8000", "sb"]
        );
        assert_eq!(
            forward_args("stop", "sb", "", 8000),
            ["forward", "stop", "8000", "sb"]
        );
    }

    #[test]
    fn the_scan_reads_both_tcp_tables_without_a_terminal() {
        let a = scan_args("sb", "gw");
        assert_eq!(&a[..2], ["-g", "gw"]);
        assert!(a.contains(&"--no-tty".to_string()));
        assert!(a.ends_with(&[
            "cat".into(),
            "/proc/net/tcp".into(),
            "/proc/net/tcp6".into()
        ]));
    }
}
