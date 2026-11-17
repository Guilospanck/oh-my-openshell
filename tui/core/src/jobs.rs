//! Background jobs: a command the MCP server starts inside a sandbox and lets run after the
//! request that started it has been answered, even after the server itself restarts.
//!
//! The job lives in the sandbox (`/tmp/.osh-jobs/<id>/`: `out`, `err`, `pid`, `exit`); the server
//! keeps one small file per job in `<data>/mcp-jobs/` so any later process can find it again
//! and the dashboard can list it. Everything that reaches the sandbox is a fixed script whose
//! inputs are positional parameters: no id, path or argument is ever spliced into script text.

use crate::ops::{self, Ctx};
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
    static SAVES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir(data).join(format!(
        "{}.json.tmp-{}-{}",
        job.id,
        std::process::id(),
        SAVES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
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
    /// The script said the job's folder does not exist.
    Unknown,
    /// Not an answer to the question (output from somewhere else).
    Garbled,
}

/// What `STATUS` printed.
pub fn parse_status(out: &str) -> Status {
    let out = out.trim();
    match out.split_once(' ') {
        Some(("exited", code)) => code.trim().parse().map_or(Status::Garbled, Status::Exited),
        _ => match out {
            "running" => Status::Running,
            "cancelled" => Status::Cancelled,
            "lost" => Status::Lost,
            "unknown" => Status::Unknown,
            _ => Status::Garbled,
        },
    }
}

/// `$1` the job directory, `$2` the longest run in seconds (`0`: no limit), then the command.
/// Starts it detached (its own session when `setsid` exists) with output in `out` and `err`, and
/// prints `started` once its pid is recorded (`started nocap` when a limit was asked for and the
/// sandbox has no `timeout` to enforce it). The command runs as the script's own arguments: they
/// reach the inner shell as `"$@"`, never as text of the script. The `pid` file holds the pid and,
/// where `/proc` exists, the process's start time, so a pid handed to another process later is
/// not mistaken for the job. Job folders more than a week old are removed first.
pub const LAUNCH: &str = r#"set -u
d=$1; max=$2; shift 2
base=$(dirname "$d")
mkdir -p "$base" 2>/dev/null
find "$base" -mindepth 1 -maxdepth 1 -type d -mtime +7 -exec rm -rf {} + 2>/dev/null
mkdir -p "$d" || exit 1
: > "$d/out"; : > "$d/err"
inner='echo "$$ $(sed "s/^[^)]*) //" /proc/$$/stat 2>/dev/null | cut -d" " -f20)" > "$D/pid" || exit 1; if [ "$MAX" != 0 ] && command -v timeout >/dev/null 2>&1; then timeout -k 5 "$MAX" "$@"; else "$@"; fi; echo $? > "$D/exit"'
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
if [ ! -s "$d/pid" ]; then echo "the job did not start" >&2; exit 1; fi
if [ "$max" != 0 ] && ! command -v timeout >/dev/null 2>&1; then echo "started nocap"; else echo started; fi
"#;

/// `$1` the job directory. Prints one word: `exited N`, `cancelled`, `running`, `lost` (the
/// process is gone, a zombie, or its pid now belongs to another process, with no exit recorded)
/// or `unknown` (no such job).
pub const STATUS: &str = r#"set -u
d=$1
[ -d "$d" ] || { echo unknown; exit 0; }
# CANCEL writes this only while no exit is recorded, so if it is there the cancel came first,
# even when ending the command made the wrapper record an exit code a moment later.
if [ -e "$d/cancelled" ]; then echo cancelled; exit 0; fi
if [ -s "$d/exit" ]; then echo "exited $(cat "$d/exit")"; exit 0; fi
pid=; st=
if [ -s "$d/pid" ]; then read -r pid st < "$d/pid"; fi
if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
  if [ -r "/proc/$pid/stat" ]; then
    rest=$(sed 's/^[^)]*) //' "/proc/$pid/stat")
    cur=$(echo "$rest" | cut -d' ' -f20)
    if [ "${rest%% *}" = Z ] || { [ -n "$st" ] && [ "$cur" != "$st" ]; }; then echo lost; exit 0; fi
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

/// `$1` the job directory. Marks the job cancelled (unless it already exited) and ends its
/// processes: the wrapper, everything it started (found through parent links in `/proc`, else
/// `ps`) and everything in its session, each signalled by its own pid, with TERM, then after up
/// to 3 seconds KILL. A pid that now belongs to another process is left alone. Not a group kill:
/// in a real sandbox `kill -TERM -PID` is "Operation not permitted", and the image has neither
/// `ps` nor `pkill`.
pub const CANCEL: &str = r#"set -u
d=$1
[ -d "$d" ] || { echo unknown; exit 0; }
[ -s "$d/exit" ] || echo cancelled > "$d/cancelled"
pid=; st=
if [ -s "$d/pid" ]; then read -r pid st < "$d/pid"; fi
[ -n "$pid" ] || { echo done; exit 0; }
if [ -n "$st" ] && [ -r "/proc/$pid/stat" ]; then
  cur=$(sed 's/^[^)]*) //' "/proc/$pid/stat" | cut -d' ' -f20)
  if [ "$cur" != "$st" ]; then echo done; exit 0; fi
fi
# One line per process: pid, parent pid, session id (0 where unknown).
tree() {
  if [ -d /proc/self ]; then
    for p in /proc/[0-9]*; do
      r=$(sed 's/^[^)]*) //' "$p/stat" 2>/dev/null) || continue
      [ -n "$r" ] || continue
      set -- $r
      echo "${p#/proc/} $2 $4"
    done
  elif command -v ps >/dev/null 2>&1; then
    ps -A -o pid= -o ppid= | while read -r a b; do echo "$a $b 0"; done
  fi
}
# $fam: the job's wrapper, its descendants and everything in its session.
family() {
  tree > "$d/.tree"
  fam=" $pid "
  grow=1
  while [ "$grow" = 1 ]; do
    grow=0
    while read -r q pp sid; do
      case "$fam" in *" $q "*) continue ;; esac
      add=0
      if [ "$sid" = "$pid" ]; then add=1; else case "$fam" in *" $pp "*) add=1 ;; esac; fi
      if [ "$add" = 1 ]; then fam="$fam$q "; grow=1; fi
    done < "$d/.tree"
  done
  rm -f "$d/.tree"
}
alive() { for q in $fam; do if kill -0 "$q" 2>/dev/null; then return 0; fi; done; return 1; }
family
first=$fam
for q in $fam; do kill -TERM "$q" 2>/dev/null; done
i=0
while alive && [ "$i" -lt 30 ]; do sleep 0.1; i=$((i + 1)); done
if alive; then
  family
  for q in $first $fam; do kill -KILL "$q" 2>/dev/null; done
fi
echo done
"#;

// ---- running jobs through `openshell sandbox exec` ----

/// How long `exec` waits for a command by default, and the longest it may be asked to.
pub const DEFAULT_WAIT: u64 = 30;
pub const MAX_WAIT: u64 = 900;
/// The most one `output` call returns.
pub const CHUNK: usize = 64 * 1024;
/// `exec` reads at most this much of each stream when it hands back a finished command; the rest
/// is for `job_output` (the job id comes with the reply).
const EXEC_READ_CAP: usize = 1024 * 1024;
/// The longest run a caller can ask for (the setting's own ceiling).
const MAX_RUN_SECS: u64 = 604_800;
/// Offsets are refused above this: the shell adds one to them and must not wrap.
pub const MAX_OFFSET: u64 = 1 << 62;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    /// Where to read from next time.
    pub next: u64,
    /// More output exists beyond `next`.
    pub more: bool,
}

/// Where a job's files live in the sandbox. Tests point it at a folder of their own
/// (`OSH_JOBS_SANDBOX_DIR`), since the fake `openshell` runs the scripts on this machine.
pub fn sandbox_dir() -> String {
    if crate::testmode::under_test() {
        if let Ok(d) = std::env::var("OSH_JOBS_SANDBOX_DIR") {
            return d;
        }
    }
    SANDBOX_DIR.to_string()
}

fn unknown(id: &str) -> String {
    format!("unknown job {id:?}: it was never started here, or it has been forgotten")
}

fn job_dir(id: &str) -> String {
    format!("{}/{id}", sandbox_dir())
}

/// Run one of the fixed scripts in the job's sandbox, on the gateway the job was started on,
/// without the login shell (its profile files could print into the answer being parsed). `Err`
/// when the call did not get through or the script failed.
fn run_script(
    ctx: &Ctx,
    job: &Job,
    body: &str,
    args: &[&str],
    timeout_secs: u64,
) -> Result<Vec<u8>, String> {
    let ctx = Ctx {
        gateway: (!job.gateway.is_empty()).then(|| job.gateway.clone()),
        ..ctx.clone()
    };
    let mut argv: Vec<String> = ["sh", "-c", body, "sh"].map(String::from).to_vec();
    argv.extend(args.iter().map(|a| a.to_string()));
    let (code, out, err) = ops::run_in_sandbox(&ctx, &job.sandbox, &argv, timeout_secs, true)?;
    if code == 0 {
        Ok(out)
    } else {
        let msg = String::from_utf8_lossy(&err).trim().to_string();
        Err(if msg.is_empty() {
            format!("the sandbox command failed with exit code {code}")
        } else {
            msg
        })
    }
}

/// Start `argv` in the sandbox as a job that runs for at most `max_secs` (`0`: no limit).
pub fn start(ctx: &Ctx, name: &str, argv: &[String], max_secs: u64) -> Result<Job, String> {
    start_noted(ctx, name, argv, max_secs).map(|(job, _)| job)
}

/// `start`, and a note when the sandbox cannot enforce the time limit that was asked for.
fn start_noted(
    ctx: &Ctx,
    name: &str,
    argv: &[String],
    max_secs: u64,
) -> Result<(Job, Option<String>), String> {
    ops::check_name(name)?;
    let Some(first) = argv.first() else {
        return Err("no command to run".into());
    };
    if first.starts_with('-') {
        return Err(format!(
            "the command cannot start with `-` ({first:?}): give the program first"
        ));
    }
    // A job names the gateway it runs on, the default one included: the CLI's default can change
    // later, and the job must not follow it.
    let gateway = match ctx.gateway.as_deref().filter(|g| !g.is_empty()) {
        Some(g) => g.to_string(),
        None => ops::active_gateway().ok().flatten().unwrap_or_default(),
    };
    let job = Job::new(&gateway, name, argv);
    let mut args = vec![job_dir(&job.id), max_secs.to_string()];
    args.extend(argv.iter().cloned());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let started = run_script(ctx, &job, LAUNCH, &refs, 60);
    let out = match started {
        Ok(out) if String::from_utf8_lossy(&out).contains("started") => out,
        other => {
            // Whatever is half-started goes, so nothing is left running unrecorded.
            let _ = run_script(ctx, &job, CANCEL, &[&job_dir(&job.id)], 60);
            return Err(match other {
                Err(e) => format!("could not start the job in sandbox {name}: {e}"),
                Ok(_) => format!("could not start the job in sandbox {name}"),
            });
        }
    };
    let note = String::from_utf8_lossy(&out)
        .contains("nocap")
        .then(|| "max_secs is not enforced: this sandbox has no `timeout` command".to_string());
    if let Err(e) = save(&ctx.dir, &job) {
        // Running with no record nobody could follow: stop it rather than leave it behind.
        let _ = run_script(ctx, &job, CANCEL, &[&job_dir(&job.id)], 60);
        return Err(format!("could not record the job: {e}"));
    }
    Ok((job, note))
}

fn load_job(ctx: &Ctx, id: &str) -> Result<Job, String> {
    load(&ctx.dir, id).ok_or_else(|| unknown(id))
}

/// Ask the sandbox what the job is doing and remember the answer.
fn refresh(ctx: &Ctx, mut job: Job) -> Result<Job, String> {
    if job.state != "running" {
        return Ok(job);
    }
    let asked = run_script(ctx, &job, STATUS, &[&job_dir(&job.id)], 30);
    match asked {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out).into_owned();
            match parse_status(&text) {
                Status::Running => {}
                Status::Exited(n) => {
                    job.state = "exited".into();
                    job.exit_code = Some(n);
                }
                Status::Cancelled => job.state = "cancelled".into(),
                Status::Lost | Status::Unknown => job.state = "lost".into(),
                // Not an answer to the question: no verdict, and nothing written down.
                Status::Garbled => {
                    let shown: String = text.trim().chars().take(80).collect();
                    return Err(format!("unexpected answer from the sandbox: {shown:?}"));
                }
            }
        }
        Err(e) => {
            // A call that did not get through is not a verdict. Only a sandbox that is no
            // longer listed on the job's own gateway means the job is gone; a job whose
            // gateway was never recorded cannot be judged at all.
            if job.gateway.is_empty() {
                return Err(format!(
                    "cannot reach the job's sandbox, and its gateway was not recorded: {e}"
                ));
            }
            let gateway = Ctx {
                gateway: Some(job.gateway.clone()),
                ..ctx.clone()
            };
            if ops::sandbox_exists(&gateway, &job.sandbox) != Ok(false) {
                return Err(format!("cannot reach the job's sandbox: {e}"));
            }
            job.state = "lost".into();
        }
    }
    job.checked = now();
    let _ = save(&ctx.dir, &job);
    Ok(job)
}

pub fn status(ctx: &Ctx, id: &str) -> Result<Job, String> {
    refresh(ctx, load_job(ctx, id)?)
}

/// New output of one stream from byte `offset`, at most `limit` bytes (kept between 16 bytes and
/// `CHUNK`). A multibyte character cut by the limit is left for the next call.
pub fn output(
    ctx: &Ctx,
    id: &str,
    stream: Stream,
    offset: u64,
    limit: usize,
) -> Result<Chunk, String> {
    if offset > MAX_OFFSET {
        return Err(format!("the offset must be at most {MAX_OFFSET}"));
    }
    let job = load_job(ctx, id)?;
    let limit = limit.clamp(16, CHUNK);
    let name = match stream {
        Stream::Out => "out",
        Stream::Err => "err",
    };
    let path = format!("{}/{name}", job_dir(id));
    let raw = run_script(
        ctx,
        &job,
        READ,
        &[&path, &offset.to_string(), &limit.to_string()],
        30,
    )?;
    let split = raw.iter().position(|b| *b == b'\n').unwrap_or(raw.len());
    let size: u64 = String::from_utf8_lossy(&raw[..split])
        .trim()
        .parse()
        .map_err(|_| "the sandbox gave no size for the output".to_string())?;
    let data = raw.get(split + 1..).unwrap_or(&[]);
    if offset >= size {
        return Ok(Chunk {
            text: String::new(),
            next: size,
            more: false,
        });
    }
    let mut keep = data.len();
    if let Err(e) = std::str::from_utf8(data) {
        let at_end = offset.saturating_add(data.len() as u64) >= size;
        // A character cut by the limit, or by a job still writing, waits for the next call;
        // at the end of a finished job's output it is all there will ever be.
        if e.error_len().is_none() && !(at_end && job.state != "running") {
            keep = e.valid_up_to();
        }
    }
    let next = offset.saturating_add(keep as u64);
    Ok(Chunk {
        text: String::from_utf8_lossy(&data[..keep]).into_owned(),
        next,
        more: size > next,
    })
}

/// End the job (unless it already exited) and report what became of it.
pub fn cancel(ctx: &Ctx, id: &str) -> Result<Job, String> {
    let job = refresh(ctx, load_job(ctx, id)?)?;
    if job.state != "running" {
        return Ok(job);
    }
    run_script(ctx, &job, CANCEL, &[&job_dir(id)], 90)?;
    refresh(ctx, job)
}

/// How long a job may run: the operator's cap, which the caller may lower and never raise. `0`
/// is no limit, which only an operator who set no cap can allow; a caller's figure is held to the
/// setting's own ceiling.
pub fn effective_cap(operator: u64, asked: Option<u64>) -> u64 {
    match asked.map(|a| a.min(MAX_RUN_SECS)) {
        None => operator,
        Some(a) if operator == 0 => a,
        Some(0) => operator,
        Some(a) => a.min(operator),
    }
}

/// Run a command as a job and wait up to `wait_secs` for it. Finished: the result `ops` gives
/// for a command (`exit_code`, `stdout`, `stderr`). Not finished: `{"job": id, "status":
/// "running"}` while it keeps running. `max_secs` is how long the caller lets it run (`None`: as
/// long as the operator allows, `ctx.job_max_secs`). Once the job has started this never fails:
/// whatever goes wrong afterwards comes back with the job id and a `note`, so the caller follows
/// the job instead of running the command again.
pub fn exec(
    ctx: &Ctx,
    name: &str,
    argv: &[String],
    wait_secs: u64,
    max_secs: Option<u64>,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let (mut job, start_note) =
        start_noted(ctx, name, argv, effective_cap(ctx.job_max_secs, max_secs))?;
    let reply = |j: &Job, more: Option<String>| {
        let mut v = json!({"job": j.id, "status": j.state});
        let notes: Vec<String> = start_note.iter().cloned().chain(more).collect();
        if !notes.is_empty() {
            v["note"] = json!(notes.join("; "));
        }
        v
    };
    let wait = std::time::Duration::from_secs(wait_secs.min(MAX_WAIT));
    if wait.is_zero() {
        return Ok(reply(&job, None));
    }
    let started = std::time::Instant::now();
    let mut pause = 200;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(pause));
        pause = 500;
        match refresh(ctx, job.clone()) {
            Ok(j) => job = j,
            Err(e) => {
                return Ok(reply(
                    &job,
                    Some(format!(
                        "its state could not be read just now ({e}): ask again with job_status"
                    )),
                ))
            }
        }
        if job.state != "running" {
            break;
        }
        if started.elapsed() >= wait {
            return Ok(reply(&job, None));
        }
    }
    if job.state != "exited" {
        return Ok(reply(&job, None));
    }
    let mut out = json!({"exit_code": job.exit_code});
    let mut truncated = false;
    for (key, stream) in [("stdout", Stream::Out), ("stderr", Stream::Err)] {
        let (mut text, mut at) = (String::new(), 0);
        loop {
            let c = match output(ctx, &job.id, stream, at, CHUNK) {
                Ok(c) => c,
                Err(e) => {
                    let mut v = reply(&job, Some(format!("it finished, but its output could not be read just now ({e}): read it with job_output")));
                    v["exit_code"] = json!(job.exit_code);
                    return Ok(v);
                }
            };
            text.push_str(&c.text);
            at = c.next;
            if !c.more {
                break;
            }
            if text.len() >= EXEC_READ_CAP {
                truncated = true;
                break;
            }
        }
        out[key] = json!(text);
    }
    if let Some(n) = &start_note {
        out["note"] = json!(n);
    }
    if truncated {
        out["truncated"] = json!(true);
        out["job"] = json!(job.id);
        let cut = "the output is cut here: read the rest with job_output";
        out["note"] = json!(match &start_note {
            Some(n) => format!("{n}; {cut}"),
            None => cut.to_string(),
        });
    }
    Ok(out)
}

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
        // Anything else is not an answer (a profile file printing into the output, say).
        assert_eq!(parse_status("exited x"), Status::Garbled);
        assert_eq!(parse_status("garbage"), Status::Garbled);
        assert_eq!(parse_status(""), Status::Garbled);
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

    // ---- running jobs (a fake `openshell` that really runs the command on this machine) ----

    use crate::ops::tests::Fake;
    use std::time::{Duration, Instant};

    /// Removes the job folders a test made under the sandbox directory.
    struct Ids(std::cell::RefCell<Vec<String>>);
    impl Drop for Ids {
        fn drop(&mut self) {
            for id in self.0.borrow().iter() {
                let _ = std::fs::remove_dir_all(format!("{}/{id}", sandbox_dir()));
            }
        }
    }

    fn running(tag: &str) -> (Fake, Ids) {
        let f = Fake::new(tag);
        std::env::set_var("OSH_FAKE_EXEC_RUN", "1");
        (f, Ids(Default::default()))
    }

    /// What `openshell gateway list -o json` prints when `openshell-docker` is the default.
    const ACTIVE: &str = r#"[{"name":"openshell-docker","endpoint":"e","active":true}]"#;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    fn until_finished(ctx: &Ctx, id: &str) -> Job {
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            let j = status(ctx, id).unwrap();
            if j.state != "running" || Instant::now() > end {
                return j;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn all(ctx: &Ctx, id: &str, stream: Stream) -> String {
        let mut text = String::new();
        let mut at = 0;
        loop {
            let c = output(ctx, id, stream, at, CHUNK).unwrap();
            text.push_str(&c.text);
            at = c.next;
            if !c.more {
                return text;
            }
        }
    }

    #[test]
    fn effective_cap_never_exceeds_the_operators() {
        assert_eq!(effective_cap(100, None), 100);
        assert_eq!(effective_cap(100, Some(50)), 50);
        assert_eq!(effective_cap(100, Some(500)), 100);
        assert_eq!(
            effective_cap(100, Some(0)),
            100,
            "no limit is not the caller's to ask for"
        );
        assert_eq!(effective_cap(0, None), 0);
        assert_eq!(effective_cap(0, Some(500)), 500);
        assert_eq!(effective_cap(0, Some(0)), 0);
        assert_eq!(
            effective_cap(0, Some(u64::MAX)),
            604_800,
            "held to the setting's ceiling"
        );
        assert_eq!(effective_cap(100, Some(u64::MAX)), 100);
    }

    #[test]
    fn start_returns_a_job_and_records_it_in_the_index() {
        let (f, ids) = running("jobs-start");
        let ctx = f.ctx();
        let j = start(&ctx, "w1", &sh("echo hi; sleep 1"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        assert!(valid_id(&j.id));
        assert_eq!(
            (j.command.as_str(), j.sandbox.as_str(), j.gateway.as_str()),
            ("sh", "w1", "")
        );
        assert_eq!(load(&f.dir, &j.id), Some(j.clone()));
        let index = std::fs::read_to_string(dir(&f.dir).join(format!("{}.json", j.id))).unwrap();
        assert!(
            !index.contains("echo hi"),
            "no arguments in the index: {index}"
        );
        assert!(f.calls().contains("sandbox exec -n w1 "), "{}", f.calls());
    }

    #[test]
    fn a_command_that_cannot_be_given_is_refused_before_anything_runs() {
        let (f, _ids) = running("jobs-refuse");
        let ctx = f.ctx();
        let e = start(&ctx, "w1", &[], 0).unwrap_err();
        assert!(e.contains("no command"), "{e}");
        let e = start(&ctx, "w1", &["-rf".into(), "x".into()], 0).unwrap_err();
        assert!(e.contains("cannot start with"), "{e}");
        let e = start(&ctx, "../x", &sh("true"), 0).unwrap_err();
        assert!(!e.is_empty());
        assert_eq!(f.calls(), "", "nothing reached openshell");
        assert!(list(&f.dir).is_empty());
    }

    #[test]
    fn an_argv_with_odd_text_runs_as_given() {
        let (f, ids) = running("jobs-odd");
        let ctx = f.ctx();
        let j = start(
            &ctx,
            "w1",
            &sh("printf '%s' \"a b\"; printf 'l1\\nl2'; echo '$HOME `x`' >&2"),
            0,
        )
        .unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        assert_eq!(until_finished(&ctx, &j.id).state, "exited");
        assert_eq!(all(&ctx, &j.id, Stream::Out), "a bl1\nl2");
        assert_eq!(all(&ctx, &j.id, Stream::Err), "$HOME `x`\n");
    }

    #[test]
    fn status_follows_a_short_command_to_exited_with_its_code() {
        let (f, ids) = running("jobs-status");
        let ctx = f.ctx();
        let j = start(&ctx, "w1", &sh("exit 3"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        let done = until_finished(&ctx, &j.id);
        assert_eq!((done.state.as_str(), done.exit_code), ("exited", Some(3)));
        assert_eq!(load(&f.dir, &j.id).unwrap().exit_code, Some(3), "saved");
    }

    #[test]
    fn output_returns_new_text_and_the_next_offset_in_chunks() {
        let (f, ids) = running("jobs-chunks");
        let ctx = f.ctx();
        let j = start(&ctx, "w1", &sh("head -c 150000 /dev/zero | tr '\\0' a"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        until_finished(&ctx, &j.id);
        let a = output(&ctx, &j.id, Stream::Out, 0, CHUNK).unwrap();
        assert_eq!((a.text.len(), a.next, a.more), (65_536, 65_536, true));
        let b = output(&ctx, &j.id, Stream::Out, a.next, CHUNK).unwrap();
        assert_eq!((b.text.len(), b.next, b.more), (65_536, 131_072, true));
        let c = output(&ctx, &j.id, Stream::Out, b.next, CHUNK).unwrap();
        assert_eq!((c.text.len(), c.next, c.more), (18_928, 150_000, false));
        let end = output(&ctx, &j.id, Stream::Out, 150_000, CHUNK).unwrap();
        assert_eq!(
            (end.text.as_str(), end.next, end.more),
            ("", 150_000, false)
        );
        let past = output(&ctx, &j.id, Stream::Out, 1_000_000, CHUNK).unwrap();
        assert_eq!(
            (past.text.as_str(), past.next, past.more),
            ("", 150_000, false)
        );
        let err = output(&ctx, &j.id, Stream::Err, 0, CHUNK).unwrap();
        assert_eq!((err.text.as_str(), err.next, err.more), ("", 0, false));
    }

    #[test]
    fn a_multibyte_character_is_not_split_at_a_chunk_edge() {
        let (f, ids) = running("jobs-utf8");
        let ctx = f.ctx();
        // 15 ASCII bytes then a two-byte character: a 16 byte chunk would end inside it.
        let j = start(&ctx, "w1", &sh("printf 'aaaaaaaaaaaaaaa\\303\\251z'"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        until_finished(&ctx, &j.id);
        let a = output(&ctx, &j.id, Stream::Out, 0, 16).unwrap();
        assert_eq!(
            (a.text.as_str(), a.next, a.more),
            ("aaaaaaaaaaaaaaa", 15, true)
        );
        let b = output(&ctx, &j.id, Stream::Out, a.next, 16).unwrap();
        assert_eq!((b.text.as_str(), b.next, b.more), ("éz", 18, false));
    }

    #[test]
    fn a_fresh_process_finds_the_job_by_id() {
        let (f, ids) = running("jobs-fresh");
        let j = start(&f.ctx(), "w1", &sh("echo kept; sleep 1"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        // Nothing but the data folder and the id: what a restarted server has.
        let fresh = Ctx {
            dir: f.dir.clone(),
            ..f.ctx()
        };
        assert_eq!(until_finished(&fresh, &j.id).state, "exited");
        assert_eq!(all(&fresh, &j.id, Stream::Out), "kept\n");
    }

    #[test]
    fn an_unknown_or_invalid_id_is_a_plain_error() {
        let (f, _ids) = running("jobs-unknown");
        let ctx = f.ctx();
        for bad in ["../x", "", "j0000000000000000", "J0000000000000000"] {
            let e = status(&ctx, bad).unwrap_err();
            assert!(e.contains("unknown job"), "{bad:?}: {e}");
            assert!(output(&ctx, bad, Stream::Out, 0, 16).is_err());
            assert!(cancel(&ctx, bad).is_err());
        }
        assert_eq!(f.calls(), "", "nothing reached openshell");
    }

    #[test]
    fn cancel_ends_a_running_job_and_after_an_exit_keeps_exited() {
        let (f, ids) = running("jobs-cancel");
        let ctx = f.ctx();
        let slow = start(&ctx, "w1", &sh("sleep 30"), 0).unwrap();
        ids.0.borrow_mut().push(slow.id.clone());
        let c = cancel(&ctx, &slow.id).unwrap();
        assert_eq!(c.state, "cancelled");
        assert_eq!(
            cancel(&ctx, &slow.id).unwrap().state,
            "cancelled",
            "twice is harmless"
        );
        let pid = std::fs::read_to_string(format!("{}/{}/pid", sandbox_dir(), slow.id)).unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the job's process is gone");
        let quick = start(&ctx, "w1", &sh("true"), 0).unwrap();
        ids.0.borrow_mut().push(quick.id.clone());
        assert_eq!(until_finished(&ctx, &quick.id).state, "exited");
        assert_eq!(cancel(&ctx, &quick.id).unwrap().state, "exited");
        assert_eq!(cancel(&ctx, &quick.id).unwrap().state, "exited");
    }

    #[test]
    fn exec_returns_the_result_when_the_command_finishes_within_the_wait() {
        let (f, ids) = running("jobs-exec");
        let v = exec(
            &f.ctx(),
            "w1",
            &sh("echo out; echo err >&2; exit 4"),
            10,
            None,
        )
        .unwrap();
        for j in list(&f.dir) {
            ids.0.borrow_mut().push(j.id);
        }
        assert_eq!(
            v,
            serde_json::json!({"exit_code": 4, "stdout": "out\n", "stderr": "err\n"})
        );
    }

    #[test]
    fn exec_with_wait_zero_returns_a_job_at_once_and_when_the_wait_runs_out() {
        let (f, ids) = running("jobs-wait");
        let ctx = f.ctx();
        let t = Instant::now();
        let v = exec(&ctx, "w1", &sh("sleep 3"), 0, None).unwrap();
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert_eq!(v["status"], "running");
        let id = v["job"].as_str().unwrap().to_string();
        assert!(valid_id(&id));
        ids.0.borrow_mut().push(id);
        let t = Instant::now();
        let v = exec(&ctx, "w1", &sh("sleep 3"), 1, None).unwrap();
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert_eq!(v["status"], "running");
        ids.0
            .borrow_mut()
            .push(v["job"].as_str().unwrap().to_string());
        let done = until_finished(&ctx, v["job"].as_str().unwrap());
        assert_eq!((done.state.as_str(), done.exit_code), ("exited", Some(0)));
    }

    #[test]
    fn a_deleted_sandbox_reports_lost_without_hanging() {
        let (f, ids) = running("jobs-lost");
        std::env::set_var("OSH_FAKE_GATEWAYS", ACTIVE);
        let ctx = f.ctx();
        let j = start(&ctx, "w1", &sh("sleep 3"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        // The sandbox is gone: `exec` fails and the sandbox list no longer has it.
        std::env::set_var("OSH_FAKE_FAIL", "sandbox exec");
        let t = Instant::now();
        let after = status(&ctx, &j.id).unwrap();
        assert!(t.elapsed() < Duration::from_secs(5));
        assert_eq!(after.state, "lost");
        assert_eq!(load(&f.dir, &j.id).unwrap().state, "lost", "remembered");
    }

    #[test]
    fn an_unreachable_gateway_is_an_error_not_a_lost_job() {
        let (f, ids) = running("jobs-unreach");
        std::env::set_var("OSH_FAKE_GATEWAYS", ACTIVE);
        let ctx = f.ctx();
        let j = start(&ctx, "w1", &sh("sleep 3"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        // The sandbox still exists (the list shows it) but this call cannot get through.
        std::env::set_var(
            "OSH_FAKE_SANDBOXES",
            r#"{"sandboxes":[{"name":"w1","phase":"Ready","created":"t"}]}"#,
        );
        std::env::set_var("OSH_FAKE_FAIL", "sandbox exec");
        assert!(status(&ctx, &j.id).is_err());
        assert_eq!(
            load(&f.dir, &j.id).unwrap().state,
            "running",
            "not written off"
        );
    }

    #[test]
    fn cancel_signals_each_process_by_its_pid_not_a_group() {
        // A real sandbox refuses `kill -TERM -PID` ("Operation not permitted") and has no ps or
        // pkill: the script must find the job's processes itself and signal them one by one.
        assert!(
            !CANCEL.contains("kill -TERM -\"") && !CANCEL.contains("kill -TERM --"),
            "{CANCEL}"
        );
        assert!(!CANCEL.contains("pkill"), "{CANCEL}");
        assert!(
            CANCEL.contains("/proc/[0-9]*") && CANCEL.contains("kill -TERM \"$q\""),
            "{CANCEL}"
        );
        assert!(CANCEL.contains("kill -KILL \"$q\""));
    }

    /// Run `CANCEL` in `shell` against a job whose wrapper has two children (the shape of a real
    /// job), and say how many of the three processes are left. The wrapper is not a group
    /// leader here, as in a sandbox where signalling a group is refused.
    fn cancel_leaves(shell: &str, a: &str, b: &str) -> usize {
        let t = tmp(&format!("cancelfam-{shell}-{a}"));
        let wrapper = std::process::Command::new("sh")
            .args(["-c", &format!("sleep {a} & sleep {b} & wait")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let jobdir = t.0.join("job");
        std::fs::create_dir_all(&jobdir).unwrap();
        std::fs::write(jobdir.join("pid"), format!("{} \n", wrapper.id())).unwrap();
        let out = std::process::Command::new(shell)
            .args(["-c", CANCEL, "sh"])
            .arg(&jobdir)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("done"),
            "{out:?}"
        );
        assert!(jobdir.join("cancelled").exists());
        std::thread::sleep(std::time::Duration::from_millis(300));
        let left = std::process::Command::new("pgrep")
            .args(["-f", &format!("sleep ({a}|{b})")])
            .output()
            .unwrap();
        let n = String::from_utf8_lossy(&left.stdout)
            .split_whitespace()
            .count();
        // Whatever the script left must not outlive the test.
        for pid in String::from_utf8_lossy(&left.stdout).split_whitespace() {
            let _ = std::process::Command::new("kill")
                .args(["-9", pid])
                .status();
        }
        let mut w = wrapper;
        let _ = w.wait();
        n
    }

    #[test]
    fn cancel_ends_the_wrapper_and_the_children_it_started() {
        assert_eq!(cancel_leaves("sh", "40117", "40118"), 0);
    }

    #[test]
    fn cancel_does_the_same_under_dash_the_sandbox_shell() {
        if std::process::Command::new("dash")
            .args(["-c", "exit 0"])
            .status()
            .is_ok_and(|s| s.success())
        {
            assert_eq!(cancel_leaves("dash", "40127", "40128"), 0);
        }
    }

    #[test]
    fn the_launcher_records_the_pid_with_its_start_time_and_cleans_old_folders() {
        assert!(LAUNCH.contains("-mtime +7"), "week-old job folders go");
        assert!(LAUNCH.contains("|| exit 1;"), "no pid file, no command");
        assert!(
            STATUS.contains("cut -d' ' -f20") && CANCEL.contains("cut -d' ' -f20"),
            "pid reuse check"
        );
        assert!(LAUNCH.contains("started nocap"));
    }

    #[test]
    fn jobs_are_started_and_asked_about_without_the_login_shell() {
        let (f, ids) = running("jobs-nologin");
        let ctx = f.ctx();
        let j = start(&ctx, "w1", &sh("sleep 1"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        status(&ctx, &j.id).unwrap();
        for line in f.calls().lines().filter(|l| l.contains("sandbox exec")) {
            assert!(line.contains("--no-login-shell"), "{line}");
        }
    }

    #[test]
    fn a_job_on_the_default_gateway_records_that_gateways_name() {
        let (f, ids) = running("jobs-active");
        std::env::set_var("OSH_FAKE_GATEWAYS", ACTIVE);
        let ctx = f.ctx();
        let j = start(&ctx, "w1", &sh("sleep 1"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        assert_eq!(j.gateway, "openshell-docker");
        assert_eq!(load(&f.dir, &j.id).unwrap().gateway, "openshell-docker");
        // The CLI's default moves; the job's calls still go to the gateway it runs on.
        std::env::set_var(
            "OSH_FAKE_GATEWAYS",
            r#"[{"name":"openshell-vm","endpoint":"e","active":true}]"#,
        );
        std::fs::write(&f.log, "").unwrap();
        status(&ctx, &j.id).unwrap();
        assert!(
            f.calls()
                .lines()
                .next()
                .unwrap()
                .starts_with("-g openshell-docker sandbox exec"),
            "{}",
            f.calls()
        );
    }

    #[test]
    fn a_job_whose_gateway_was_never_recorded_is_not_written_off_as_lost() {
        let (f, ids) = running("jobs-unrecorded");
        let ctx = f.ctx(); // no active gateway to name: the fake lists none
        let j = start(&ctx, "w1", &sh("sleep 1"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        assert_eq!(j.gateway, "");
        std::env::set_var("OSH_FAKE_FAIL", "sandbox exec");
        let e = status(&ctx, &j.id).unwrap_err();
        assert!(e.contains("not recorded"), "{e}");
        assert_eq!(load(&f.dir, &j.id).unwrap().state, "running");
    }

    #[test]
    fn exec_never_loses_the_job_id_once_the_job_has_started() {
        // The state cannot be read: the reply still names the job.
        let (f, ids) = running("jobs-keepid");
        std::env::set_var("OSH_FAKE_GATEWAYS", ACTIVE);
        std::env::set_var(
            "OSH_FAKE_SANDBOXES",
            r#"{"sandboxes":[{"name":"w1","phase":"Ready","created_at":"t"}]}"#,
        );
        let ctx = f.ctx();
        // Only the status script contains this text.
        std::env::set_var("OSH_FAKE_FAIL", "echo lost");
        let v = exec(&ctx, "w1", &sh("sleep 1"), 5, None).unwrap();
        for j in list(&f.dir) {
            ids.0.borrow_mut().push(j.id);
        }
        assert_eq!(v["status"], "running", "{v}");
        assert!(valid_id(v["job"].as_str().unwrap()), "{v}");
        assert!(v["note"].as_str().unwrap().contains("job_status"), "{v}");
    }

    #[test]
    fn exec_keeps_the_job_id_and_the_exit_code_when_the_output_cannot_be_read() {
        let (f, ids) = running("jobs-keepout");
        std::env::set_var("OSH_FAKE_GATEWAYS", ACTIVE);
        let ctx = f.ctx();
        // Only the read script contains this text.
        std::env::set_var("OSH_FAKE_FAIL", "wc -c");
        let v = exec(&ctx, "w1", &sh("echo x; exit 6"), 10, None).unwrap();
        for j in list(&f.dir) {
            ids.0.borrow_mut().push(j.id);
        }
        assert_eq!(
            (v["status"].as_str(), v["exit_code"].as_i64()),
            (Some("exited"), Some(6)),
            "{v}"
        );
        assert!(valid_id(v["job"].as_str().unwrap()), "{v}");
        assert!(v["note"].as_str().unwrap().contains("job_output"), "{v}");
    }

    #[test]
    fn a_long_output_is_cut_inline_with_the_job_id_to_read_the_rest() {
        let (f, ids) = running("jobs-trunc");
        let ctx = f.ctx();
        let v = exec(
            &ctx,
            "w1",
            &sh("head -c 1500000 /dev/zero | tr '\\0' b"),
            20,
            None,
        )
        .unwrap();
        for j in list(&f.dir) {
            ids.0.borrow_mut().push(j.id);
        }
        assert_eq!(
            v["truncated"],
            true,
            "{}",
            v.to_string().chars().take(200).collect::<String>()
        );
        assert!(valid_id(v["job"].as_str().unwrap()));
        let len = v["stdout"].as_str().unwrap().len();
        assert!((1_048_576..1_200_000).contains(&len), "{len}");
        assert_eq!(v["exit_code"], 0);
    }

    #[test]
    fn a_time_limit_the_sandbox_cannot_enforce_comes_with_a_note() {
        let (f, ids) = running("jobs-nocap");
        let has_timeout = std::process::Command::new("sh")
            .args(["-c", "command -v timeout"])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        let v = exec(&f.ctx(), "w1", &sh("echo capped"), 10, Some(30)).unwrap();
        for j in list(&f.dir) {
            ids.0.borrow_mut().push(j.id);
        }
        assert_eq!(v["stdout"], "capped\n", "{v}");
        assert_eq!(v.get("note").is_some(), !has_timeout, "{v}");
        if !has_timeout {
            assert!(v["note"].as_str().unwrap().contains("not enforced"));
        }
    }

    #[test]
    fn a_huge_offset_is_refused_not_wrapped() {
        let (f, ids) = running("jobs-offset");
        let ctx = f.ctx();
        let j = start(&ctx, "w1", &sh("echo hi"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        until_finished(&ctx, &j.id);
        for bad in [u64::MAX, i64::MAX as u64, MAX_OFFSET + 1] {
            let e = output(&ctx, &j.id, Stream::Out, bad, 16).unwrap_err();
            assert!(e.contains("at most"), "{bad}: {e}");
        }
        let c = output(&ctx, &j.id, Stream::Out, MAX_OFFSET, 16).unwrap();
        assert_eq!((c.text.as_str(), c.next, c.more), ("", 3, false));
    }

    #[test]
    fn a_launch_that_fails_leaves_nothing_recorded_or_running() {
        let (f, _ids) = running("jobs-badlaunch");
        // A sandbox folder that cannot be made: its parent is a file.
        let blocker = f.dir.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        std::env::set_var("OSH_JOBS_SANDBOX_DIR", blocker.join("jobs"));
        let e = start(&f.ctx(), "w1", &sh("sleep 5"), 0).unwrap_err();
        assert!(e.contains("could not start the job"), "{e}");
        assert!(list(&f.dir).is_empty(), "no record of a job that never ran");
    }

    #[test]
    fn jobs_in_tests_never_touch_the_real_tmp_folder() {
        let (f, ids) = running("jobs-isolated");
        let j = start(&f.ctx(), "w1", &sh("sleep 1"), 0).unwrap();
        ids.0.borrow_mut().push(j.id.clone());
        assert!(std::path::Path::new(&sandbox_dir())
            .join(&j.id)
            .join("pid")
            .exists());
        assert!(!std::path::Path::new("/tmp/.osh-jobs").join(&j.id).exists());
        assert_ne!(sandbox_dir(), "/tmp/.osh-jobs");
    }

    #[test]
    fn concurrent_saves_of_one_job_never_leave_a_broken_record() {
        let d = tmp("racesave");
        let id = "j00000000000000ab";
        save(&d.0, &job(id, 5, "running")).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|w| {
                let dir = d.0.clone();
                std::thread::spawn(move || {
                    for n in 0..40u64 {
                        let mut j = job(id, 5, "running");
                        j.checked = w * 100 + n;
                        save(&dir, &j).unwrap();
                        assert!(load(&dir, id).is_some(), "a reader never sees half a file");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let names: Vec<String> = std::fs::read_dir(super::dir(&d.0))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![format!("{id}.json")], "no temporary file left");
    }

    #[test]
    fn exec_in_a_test_runs_the_scripts_where_the_fake_says() {
        let (f, ids) = running("jobs-dirs");
        let v = exec(&f.ctx(), "w1", &sh("echo ok"), 10, None).unwrap();
        for j in list(&f.dir) {
            ids.0.borrow_mut().push(j.id);
        }
        assert_eq!(v["stdout"], "ok\n");
    }
}
