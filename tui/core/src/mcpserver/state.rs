//! What the server tells the dashboard: `<data>/mcp-server.json`, written atomically by the
//! server and read by anything that wants to show who is connected. A file whose process is
//! gone reads as "off". It never holds the token.

use super::http::Observer;
use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const FILE: &str = "mcp-server.json";
/// More sessions than this are not listed: a client that starts a new session for every call
/// must not make the file grow without bound.
const MAX_CLIENTS: usize = 200;

pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    pub session: String,
    pub peer: String,
    pub name: String,
    pub version: String,
    /// Unix seconds.
    pub first_seen: u64,
    pub last_seen: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub pid: u32,
    pub listen: String,
    pub started: u64,
    pub version: String,
    pub clients: Vec<Client>,
}

impl State {
    fn to_json(&self) -> String {
        let clients: Vec<Value> = self
            .clients
            .iter()
            .map(|c| {
                json!({"session": c.session, "peer": c.peer, "name": c.name,
                       "version": c.version, "first_seen": c.first_seen, "last_seen": c.last_seen})
            })
            .collect();
        let v = json!({"pid": self.pid, "listen": self.listen, "started": self.started,
                       "version": self.version, "clients": clients});
        serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
    }

    fn from_json(text: &str) -> Option<State> {
        let v: Value = serde_json::from_str(text).ok()?;
        let s = |x: &Value| x.as_str().map(String::from);
        let clients = v["clients"]
            .as_array()?
            .iter()
            .map(|c| {
                Some(Client {
                    session: s(&c["session"])?,
                    peer: s(&c["peer"])?,
                    name: s(&c["name"])?,
                    version: s(&c["version"])?,
                    first_seen: c["first_seen"].as_u64()?,
                    last_seen: c["last_seen"].as_u64()?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(State {
            pid: u32::try_from(v["pid"].as_u64()?).ok()?,
            listen: s(&v["listen"])?,
            started: v["started"].as_u64()?,
            version: s(&v["version"])?,
            clients,
        })
    }
}

/// Whether a process with this id exists. `/proc` where there is one, else `kill -0` (sends
/// nothing; only asks whether the process could be signalled).
pub fn pid_alive(pid: u32) -> bool {
    if Path::new("/proc/self").exists() {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Same-folder temporary file with the final mode, then a rename: a reader never sees half of
/// it and nobody else can read it, not even for an instant.
fn write_atomic(dir: &Path, text: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{FILE}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path(dir))
}

/// The state of the running server, or `None` when there is no file, it is not ours to read,
/// or the process that wrote it is gone.
pub fn read(dir: &Path) -> Option<State> {
    let st = State::from_json(&std::fs::read_to_string(path(dir)).ok()?)?;
    pid_alive(st.pid).then_some(st)
}

/// Take the right to run: refuse when another live server owns the file, else record this
/// process as the owner (replacing the leftover of a crashed one).
pub fn claim(dir: &Path, listen: &str) -> Result<(), String> {
    if let Some(other) = read(dir) {
        if other.pid != std::process::id() {
            return Err(format!(
                "another osh MCP server is already running (pid {}, listening on {})",
                other.pid, other.listen
            ));
        }
    }
    let mine = State {
        pid: std::process::id(),
        listen: listen.into(),
        started: system_now(),
        version: String::new(),
        clients: Vec::new(),
    };
    write_atomic(dir, &mine.to_json()).map_err(|e| format!("cannot write {FILE}: {e}"))
}

struct Inner {
    clients: Vec<Client>,
    dirty: bool,
}

/// The connected clients, kept in memory and written to the state file by `flush`.
pub struct Registry {
    dir: PathBuf,
    listen: String,
    version: String,
    started: u64,
    idle: Box<dyn Fn() -> Duration + Send + Sync>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    inner: Mutex<Inner>,
}

impl Registry {
    pub fn new(
        dir: PathBuf,
        listen: String,
        version: String,
        idle: impl Fn() -> Duration + Send + Sync + 'static,
    ) -> Registry {
        Registry {
            dir,
            listen,
            version,
            started: system_now(),
            idle: Box::new(idle),
            clock: Box::new(system_now),
            inner: Mutex::new(Inner {
                clients: Vec::new(),
                dirty: true,
            }),
        }
    }

    /// Replace the clock (Unix seconds); for tests.
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Registry {
        self.clock = Box::new(clock);
        self.started = (self.clock)();
        self
    }

    fn snapshot(&self, clients: &[Client]) -> State {
        State {
            pid: std::process::id(),
            listen: self.listen.clone(),
            started: self.started,
            version: self.version.clone(),
            clients: clients.to_vec(),
        }
    }

    /// Drop idle clients and write the file if anything changed since the last write.
    pub fn flush(&self) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let now = (self.clock)();
        let idle = (self.idle)().as_secs();
        let before = inner.clients.len();
        inner
            .clients
            .retain(|c| now.saturating_sub(c.last_seen) <= idle);
        if inner.clients.len() != before {
            inner.dirty = true;
        }
        if !inner.dirty {
            return;
        }
        let text = self.snapshot(&inner.clients).to_json();
        if write_atomic(&self.dir, &text).is_ok() {
            inner.dirty = false;
        }
    }

    /// Delete the file: this server is stopping.
    pub fn remove(&self) {
        let _ = std::fs::remove_file(path(&self.dir));
    }
}

impl Observer for Registry {
    fn seen(&self, session: &str, peer: &str, client: Option<(String, String)>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let now = (self.clock)();
        match inner.clients.iter_mut().find(|c| c.session == session) {
            Some(c) => {
                c.last_seen = now;
                c.peer = peer.to_string();
                if let Some((name, version)) = client {
                    c.name = name;
                    c.version = version;
                }
            }
            None => {
                let (name, version) = client.unwrap_or_default();
                inner.clients.push(Client {
                    session: session.to_string(),
                    peer: peer.to_string(),
                    name,
                    version,
                    first_seen: now,
                    last_seen: now,
                });
                if inner.clients.len() > MAX_CLIENTS {
                    if let Some(oldest) =
                        (0..inner.clients.len()).min_by_key(|i| inner.clients[*i].last_seen)
                    {
                        inner.clients.remove(oldest);
                    }
                }
            }
        }
        inner.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("osh-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    /// A registry on a clock the test moves by hand.
    fn registry(dir: &Path, idle_secs: u64) -> (Registry, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(1_000));
        let n = now.clone();
        let r = Registry::new(
            dir.to_path_buf(),
            "127.0.0.1:17800".into(),
            "9.9.9".into(),
            move || Duration::from_secs(idle_secs),
        )
        .with_clock(move || n.load(Ordering::SeqCst));
        (r, now)
    }

    fn dead_pid() -> u32 {
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    #[test]
    fn a_new_session_is_listed_and_a_repeat_updates_last_seen() {
        let d = tmp("repeat");
        let (r, now) = registry(&d.0, 600);
        r.seen("s1", "10.0.0.5", None);
        now.store(1_030, Ordering::SeqCst);
        r.seen("s1", "10.0.0.5", None);
        r.seen("s2", "10.0.0.6", None);
        r.flush();
        let st = read(&d.0).expect("state");
        assert_eq!(st.pid, std::process::id());
        assert_eq!(st.listen, "127.0.0.1:17800");
        assert_eq!(st.version, "9.9.9");
        assert_eq!(st.clients.len(), 2);
        let s1 = st.clients.iter().find(|c| c.session == "s1").unwrap();
        assert_eq!((s1.first_seen, s1.last_seen), (1_000, 1_030));
        assert_eq!(s1.peer, "10.0.0.5");
    }

    #[test]
    fn initialize_name_and_version_are_recorded_and_kept_by_later_calls() {
        let d = tmp("name");
        let (r, _) = registry(&d.0, 600);
        r.seen("s1", "p", Some(("factory".into(), "0.3".into())));
        r.seen("s1", "p", None);
        r.flush();
        let c = &read(&d.0).unwrap().clients[0];
        assert_eq!((c.name.as_str(), c.version.as_str()), ("factory", "0.3"));
    }

    #[test]
    fn idle_clients_are_dropped() {
        let d = tmp("idle");
        let (r, now) = registry(&d.0, 100);
        r.seen("old", "p", None);
        now.store(1_090, Ordering::SeqCst);
        r.seen("fresh", "p", None);
        now.store(1_150, Ordering::SeqCst);
        r.flush();
        let names: Vec<String> = read(&d.0)
            .unwrap()
            .clients
            .into_iter()
            .map(|c| c.session)
            .collect();
        assert_eq!(names, vec!["fresh".to_string()]);
    }

    #[test]
    fn the_list_is_capped_so_a_client_that_always_initializes_cannot_grow_it() {
        let d = tmp("cap");
        let (r, now) = registry(&d.0, 100_000);
        for i in 0..500 {
            now.store(1_000 + i, Ordering::SeqCst);
            r.seen(&format!("s{i}"), "p", None);
        }
        r.flush();
        let st = read(&d.0).unwrap();
        assert!(st.clients.len() <= 200, "{}", st.clients.len());
        assert!(
            st.clients.iter().any(|c| c.session == "s499"),
            "newest kept"
        );
    }

    #[test]
    fn the_state_file_is_0600_and_never_holds_the_token() {
        let d = tmp("mode");
        std::fs::write(super::super::token::path(&d.0), "super-secret-value\n").unwrap();
        let (r, _) = registry(&d.0, 600);
        r.seen("s1", "p", Some(("n".into(), "v".into())));
        r.flush();
        let p = path(&d.0);
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!std::fs::read_to_string(&p)
            .unwrap()
            .contains("super-secret"));
    }

    #[test]
    fn a_dead_pid_or_a_broken_file_reads_as_off() {
        let d = tmp("dead");
        assert!(read(&d.0).is_none(), "no file");
        let st =
            json!({"pid": dead_pid(), "listen": "x", "started": 1, "version": "v", "clients": []});
        std::fs::write(path(&d.0), st.to_string()).unwrap();
        assert!(read(&d.0).is_none(), "dead pid");
        std::fs::write(path(&d.0), "{not json").unwrap();
        assert!(read(&d.0).is_none(), "broken file");
    }

    #[test]
    fn claim_refuses_a_live_server_and_takes_over_a_stale_file() {
        let d = tmp("claim");
        let other = std::process::Command::new("sleep").arg("30").spawn();
        let mut other = other.unwrap();
        let st = json!({"pid": other.id(), "listen": "127.0.0.1:1", "started": 1, "version": "v", "clients": []});
        std::fs::write(path(&d.0), st.to_string()).unwrap();
        let e = claim(&d.0, "127.0.0.1:17800").unwrap_err();
        assert!(
            e.contains(&other.id().to_string()) && e.contains("127.0.0.1:1"),
            "{e}"
        );
        assert_eq!(read(&d.0).unwrap().listen, "127.0.0.1:1", "untouched");
        let _ = other.kill();
        let _ = other.wait();
        claim(&d.0, "127.0.0.1:17800").unwrap();
        let mine = read(&d.0).unwrap();
        assert_eq!(
            (mine.pid, mine.listen.as_str()),
            (std::process::id(), "127.0.0.1:17800")
        );
    }

    #[test]
    fn flush_leaves_no_partial_or_temporary_file() {
        let d = tmp("atomic");
        let (r, _) = registry(&d.0, 600);
        for i in 0..20 {
            r.seen(&format!("s{i}"), "p", None);
            r.flush();
            let names: Vec<String> = std::fs::read_dir(&d.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, vec!["mcp-server.json".to_string()], "{names:?}");
            assert!(read(&d.0).is_some());
        }
    }

    #[test]
    fn remove_deletes_the_file() {
        let d = tmp("remove");
        let (r, _) = registry(&d.0, 600);
        r.flush();
        assert!(path(&d.0).exists());
        r.remove();
        assert!(!path(&d.0).exists());
    }
}
