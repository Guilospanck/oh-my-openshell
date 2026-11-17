//! Background jobs: a command the MCP server starts inside a sandbox and lets run after the
//! request that started it has been answered, even after the server itself restarts.
//!
//! The job lives in the sandbox (`/tmp/.osh-jobs/<id>/`: `out`, `err`, `pid`, `exit`); the server
//! keeps one small file per job in `<data>/mcp-jobs/` so any later process can find it again
//! and the dashboard can list it. Everything that reaches the sandbox is a fixed script whose
//! inputs are positional parameters: no id, path or argument is ever spliced into script text.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Finished jobs are kept in the index this long; any job older than the second span goes too.
const KEEP_FINISHED_SECS: u64 = 7 * 86_400;
const KEEP_ANY_SECS: u64 = 30 * 86_400;
const COMMAND_MAX: usize = 80;

/// Where a job's files live in the sandbox.
pub const SANDBOX_DIR: &str = "/tmp/.osh-jobs";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    pub gateway: String,
    pub sandbox: String,
    /// The program's name, `argv[0]`, and nothing of the arguments: they may carry secrets and
    /// the index is readable by anything running as this user.
    pub command: String,
    /// Unix seconds.
    pub started: u64,
    /// `running`, `exited`, `cancelled` or `lost`.
    pub state: String,
    pub exit_code: Option<i64>,
    /// When the state was last asked of the sandbox.
    pub checked: u64,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Job {
    pub fn new(gateway: &str, sandbox: &str, argv: &[String]) -> Job {
        let t = now();
        Job {
            id: new_id(),
            gateway: gateway.into(),
            sandbox: sandbox.into(),
            command: argv
                .first()
                .map_or(String::new(), |a| a.chars().take(COMMAND_MAX).collect()),
            started: t,
            state: "running".into(),
            exit_code: None,
            checked: t,
        }
    }
}

/// `j` and 16 lowercase hex digits from the system's random source.
pub fn new_id() -> String {
    let mut b = [0u8; 8];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_ok();
    if !ok {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        b = (t ^ (N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) << 48)).to_le_bytes();
    }
    format!(
        "j{}",
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    )
}

/// Ids name paths in the data folder and in the sandbox, so only the exact shape is accepted.
pub fn valid_id(id: &str) -> bool {
    id.len() == 17
        && id.starts_with('j')
        && id[1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn dir(data: &Path) -> PathBuf {
    data.join("mcp-jobs")
}

fn file(data: &Path, id: &str) -> PathBuf {
    dir(data).join(format!("{id}.json"))
}

pub fn to_json(j: &Job) -> serde_json::Value {
    serde_json::json!({
        "id": j.id, "gateway": j.gateway, "sandbox": j.sandbox, "command": j.command,
        "started": j.started, "state": j.state, "exit_code": j.exit_code, "checked": j.checked,
    })
}

fn from_json(v: &serde_json::Value) -> Option<Job> {
    let s = |k: &str| v[k].as_str().map(String::from);
    Some(Job {
        id: s("id")?,
        gateway: s("gateway")?,
        sandbox: s("sandbox")?,
        command: s("command")?,
        started: v["started"].as_u64()?,
        state: s("state")?,
        exit_code: v["exit_code"].as_i64(),
        checked: v["checked"].as_u64()?,
    })
}

/// Written beside the target with its final mode, then renamed: a reader never sees half of it.
pub fn save(data: &Path, job: &Job) -> std::io::Result<()> {
    if !valid_id(&job.id) {
        return Err(std::io::Error::other("invalid job id"));
    }
    std::fs::create_dir_all(dir(data))?;
    let tmp = dir(data).join(format!("{}.json.tmp-{}", job.id, std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(serde_json::to_string_pretty(&to_json(job))?.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    std::fs::rename(&tmp, file(data, &job.id))
}

/// `None` for an invalid id, a missing or broken file, or one that names another id.
pub fn load(data: &Path, id: &str) -> Option<Job> {
    if !valid_id(id) {
        return None;
    }
    let text = std::fs::read_to_string(file(data, id)).ok()?;
    let job = from_json(&serde_json::from_str(&text).ok()?)?;
    (job.id == id).then_some(job)
}

/// Every job in the index, newest first.
pub fn list(data: &Path) -> Vec<Job> {
    let Ok(rd) = std::fs::read_dir(dir(data)) else {
        return Vec::new();
    };
    let mut jobs: Vec<Job> = rd
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            load(data, name.strip_suffix(".json")?)
        })
        .collect();
    jobs.sort_by(|a, b| b.started.cmp(&a.started).then(a.id.cmp(&b.id)));
    jobs
}

/// Forget finished jobs after a week and any job after a month; returns how many went.
pub fn prune(data: &Path, now: u64) -> usize {
    let mut n = 0;
    for j in list(data) {
        let age = now.saturating_sub(j.started);
        let old = age > KEEP_ANY_SECS || (j.state != "running" && age > KEEP_FINISHED_SECS);
        if old && std::fs::remove_file(file(data, &j.id)).is_ok() {
            n += 1;
        }
    }
    n
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Exited(i64),
    Cancelled,
    Lost,
    Unknown,
}

/// What `STATUS` printed.
pub fn parse_status(out: &str) -> Status {
    let out = out.trim();
    match out.split_once(' ') {
        Some(("exited", code)) => code.trim().parse().map_or(Status::Unknown, Status::Exited),
        _ => match out {
            "running" => Status::Running,
            "cancelled" => Status::Cancelled,
            "lost" => Status::Lost,
            _ => Status::Unknown,
        },
    }
}

/// `$1` the job directory, `$2` the longest run in seconds (`0`: no limit), then the command.
/// Starts it detached (its own session when `setsid` exists) with output in `out` and `err`, and
/// prints `started` once its pid is recorded. The command runs as the script's own arguments:
/// they reach the inner shell as `"$@"`, never as text of the script.
pub const LAUNCH: &str = r#"set -u
d=$1; max=$2; shift 2
mkdir -p "$d" || exit 1
: > "$d/out"; : > "$d/err"
inner='echo $$ > "$D/pid"; if [ "$MAX" != 0 ] && command -v timeout >/dev/null 2>&1; then timeout -k 5 "$MAX" "$@"; else "$@"; fi; echo $? > "$D/exit"'
if command -v setsid >/dev/null 2>&1; then
  D="$d" MAX="$max" setsid sh -c "$inner" sh "$@" >"$d/out" 2>"$d/err" </dev/null &
elif command -v nohup >/dev/null 2>&1; then
  D="$d" MAX="$max" nohup sh -c "$inner" sh "$@" >"$d/out" 2>"$d/err" </dev/null &
else
  echo "neither setsid nor nohup is available in this sandbox" >&2
  exit 1
fi
i=0
while [ ! -s "$d/pid" ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
if [ -s "$d/pid" ]; then echo started; else echo "the job did not start" >&2; exit 1; fi
"#;

/// `$1` the job directory. Prints one word: `exited N`, `cancelled`, `running`, `lost` (the
/// process is gone or a zombie, with no exit recorded) or `unknown` (no such job).
pub const STATUS: &str = r#"set -u
d=$1
[ -d "$d" ] || { echo unknown; exit 0; }
if [ -s "$d/exit" ]; then echo "exited $(cat "$d/exit")"; exit 0; fi
if [ -e "$d/cancelled" ]; then echo cancelled; exit 0; fi
pid=$(cat "$d/pid" 2>/dev/null)
if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
  if [ -r "/proc/$pid/stat" ]; then
    st=$(sed 's/^[^)]*) //' "/proc/$pid/stat" | cut -c1)
    if [ "$st" = Z ]; then echo lost; exit 0; fi
  fi
  echo running
else
  echo lost
fi
"#;

/// `$1` a file, `$2` the offset, `$3` the most bytes to print. First line: the file's size.
pub const READ: &str = r#"set -u
f=$1; o=$2; l=$3
s=$(wc -c < "$f" 2>/dev/null || echo 0)
echo "$s" | tr -d ' '
tail -c +$((o + 1)) "$f" 2>/dev/null | head -c "$l"
"#;

/// `$1` the job directory. Marks the job cancelled (unless it already exited), ends its process
/// group (else the process), waits up to 3 seconds, then kills.
pub const CANCEL: &str = r#"set -u
d=$1
[ -d "$d" ] || { echo unknown; exit 0; }
[ -s "$d/exit" ] || echo cancelled > "$d/cancelled"
pid=$(cat "$d/pid" 2>/dev/null)
[ -n "$pid" ] || { echo done; exit 0; }
kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null
i=0
while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 30 ]; do sleep 0.1; i=$((i + 1)); done
if kill -0 "$pid" 2>/dev/null; then kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null; fi
echo done
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("osh-jobs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    fn job(id: &str, started: u64, state: &str) -> Job {
        Job {
            id: id.into(),
            gateway: "openshell-docker".into(),
            sandbox: "w1".into(),
            command: "sleep".into(),
            started,
            state: state.into(),
            exit_code: None,
            checked: started,
        }
    }

    #[test]
    fn new_id_is_valid_and_unique() {
        let a = new_id();
        let b = new_id();
        assert!(valid_id(&a) && valid_id(&b), "{a} {b}");
        assert_ne!(a, b);
        assert_eq!(a.len(), 17);
    }

    #[test]
    fn valid_id_refuses_paths_and_odd_text() {
        for bad in [
            "",
            "j",
            "../x",
            "j0123456789abcdeg",
            "j0123456789abcdef/",
            "j0123456789abcdef0",
            "j0123456789ABCDEF",
            "k0123456789abcdef",
            "j0123456789abcde\n",
            "j../../etc/passwd",
        ] {
            assert!(!valid_id(bad), "{bad:?}");
        }
        assert!(valid_id("j0123456789abcdef"));
    }

    #[test]
    fn a_job_round_trips_through_its_file_with_mode_0600() {
        let d = tmp("round");
        let mut j = job("j0123456789abcdef", 100, "running");
        j.exit_code = Some(3);
        save(&d.0, &j).unwrap();
        let p = dir(&d.0).join("j0123456789abcdef.json");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(load(&d.0, "j0123456789abcdef"), Some(j));
        let names: Vec<String> = std::fs::read_dir(dir(&d.0))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["j0123456789abcdef.json".to_string()],
            "no temp file left"
        );
    }

    #[test]
    fn load_refuses_an_invalid_id_and_a_broken_file() {
        let d = tmp("load");
        assert_eq!(load(&d.0, "../x"), None);
        assert_eq!(load(&d.0, "j0123456789abcdef"), None, "no file");
        std::fs::create_dir_all(dir(&d.0)).unwrap();
        std::fs::write(dir(&d.0).join("j0123456789abcdef.json"), "{nope").unwrap();
        assert_eq!(load(&d.0, "j0123456789abcdef"), None, "broken file");
        // A file that names a different id than its own name is not trusted.
        let other = job("j1111111111111111", 5, "running");
        save(&d.0, &other).unwrap();
        std::fs::rename(
            dir(&d.0).join("j1111111111111111.json"),
            dir(&d.0).join("j2222222222222222.json"),
        )
        .unwrap();
        assert_eq!(load(&d.0, "j2222222222222222"), None);
    }

    #[test]
    fn list_is_newest_first_and_skips_junk_files() {
        let d = tmp("list");
        save(&d.0, &job("j00000000000000a1", 10, "running")).unwrap();
        save(&d.0, &job("j00000000000000a2", 30, "exited")).unwrap();
        save(&d.0, &job("j00000000000000a3", 20, "running")).unwrap();
        std::fs::write(dir(&d.0).join("notes.txt"), "x").unwrap();
        std::fs::write(dir(&d.0).join("..hidden.json"), "{}").unwrap();
        let ids: Vec<String> = list(&d.0).into_iter().map(|j| j.id).collect();
        assert_eq!(
            ids,
            [
                "j00000000000000a2",
                "j00000000000000a3",
                "j00000000000000a1"
            ]
        );
    }

    #[test]
    fn prune_removes_old_finished_jobs_and_very_old_running_ones() {
        let d = tmp("prune");
        let day = 86_400;
        let now = 100 * day;
        save(&d.0, &job("j000000000000000a", now - 6 * day, "exited")).unwrap();
        save(&d.0, &job("j000000000000000b", now - 20 * day, "running")).unwrap();
        save(&d.0, &job("j000000000000000c", now - 8 * day, "exited")).unwrap();
        save(&d.0, &job("j000000000000000d", now - 31 * day, "running")).unwrap();
        save(&d.0, &job("j000000000000000e", now - 8 * day, "cancelled")).unwrap();
        assert_eq!(prune(&d.0, now), 3);
        let mut left: Vec<String> = list(&d.0).into_iter().map(|j| j.id).collect();
        left.sort();
        assert_eq!(left, ["j000000000000000a", "j000000000000000b"]);
    }

    #[test]
    fn the_command_is_the_first_word_only() {
        let argv: Vec<String> = ["curl", "-H", "Authorization: Bearer hunter2", "http://x"]
            .map(String::from)
            .to_vec();
        let j = Job::new("gw", "sb", &argv);
        assert_eq!(j.command, "curl");
        let text = serde_json::to_string(&to_json(&j)).unwrap();
        assert!(
            !text.contains("hunter2") && !text.contains("Bearer"),
            "{text}"
        );
        assert!(valid_id(&j.id));
        assert_eq!((j.state.as_str(), j.exit_code), ("running", None));
        let long = Job::new("gw", "sb", &["x".repeat(500)]);
        assert_eq!(long.command.len(), 80, "a long name is cut");
    }

    #[test]
    fn parse_status_reads_every_word() {
        assert_eq!(parse_status("running\n"), Status::Running);
        assert_eq!(parse_status("exited 3\n"), Status::Exited(3));
        assert_eq!(parse_status("exited 137"), Status::Exited(137));
        assert_eq!(parse_status("cancelled"), Status::Cancelled);
        assert_eq!(parse_status("lost"), Status::Lost);
        assert_eq!(parse_status("unknown"), Status::Unknown);
        assert_eq!(parse_status("exited x"), Status::Unknown);
        assert_eq!(parse_status("garbage"), Status::Unknown);
        assert_eq!(parse_status(""), Status::Unknown);
    }

    #[test]
    fn the_scripts_are_fixed_text_that_never_splices_its_inputs() {
        for (name, s) in [
            ("launch", LAUNCH),
            ("status", STATUS),
            ("read", READ),
            ("cancel", CANCEL),
        ] {
            assert!(s.starts_with("set -u\n"), "{name}");
            assert!(!s.contains("eval"), "{name}");
        }
        assert!(LAUNCH.contains("setsid") && LAUNCH.contains("nohup"));
        // The command arrives as the script's own arguments, reaching the inner shell as "$@".
        assert!(LAUNCH.contains("sh -c \"$inner\" sh \"$@\""), "{LAUNCH}");
        assert!(!LAUNCH.contains("$*"));
    }
}
