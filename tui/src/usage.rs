//! Resource meter: the current CPU, memory and disk use of the sandbox in view, read from
//! `/proc` and `df` through `openshell sandbox exec` on a thread, like the port watcher
//! (`ports.rs`). Nothing has to be installed in the sandbox: the script needs only `sh`, `cat`
//! and `df`, which busybox images have too, and whatever cannot be read is left out.
//!
//! The totals are what the sandbox's kernel reports. On the vm gateway every sandbox is a microVM
//! of the gateway's one size; on docker and podman they are the host's (or its VM's). Neither is
//! a limit of this sandbox, so the screen never calls them that.
use std::{
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc,
    },
    thread,
    time::Duration,
};

/// How often the sandbox in view is read.
const EVERY: Duration = Duration::from_secs(5);
/// The longest wait after reads keep failing (stopped sandbox, busy gateway).
const MAX_BACKOFF: Duration = Duration::from_secs(40);
/// A new meter waits this long before its first read, so scrolling the sandbox list does not
/// start an exec per row.
const SETTLE: Duration = Duration::from_millis(400);

/// One usage exec at a time across every meter: a meter replaced while its exec runs keeps that
/// exec until it ends, and the next one waits for it instead of piling up.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// The read-only script run in the sandbox. The first read takes two CPU samples a second apart
/// so a figure shows at once; later reads take one and are diffed against the previous read.
pub fn script(first: bool) -> String {
    let cpu = "{ read -r l < /proc/stat && echo \"$l\"; } 2>/dev/null";
    let mut s = String::from(cpu);
    if first {
        s.push_str("; sleep 1; ");
        s.push_str(cpu);
    }
    s.push_str("; cat /proc/meminfo 2>/dev/null; df -Pk /sandbox / 2>/dev/null; true");
    s
}

/// The `openshell` arguments that run `script` in a sandbox.
pub fn meter_args(sandbox: &str, gateway: &str, first: bool) -> Vec<String> {
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
            "sh",
            "-c",
        ]
        .map(String::from),
    );
    a.push(script(first));
    a
}

/// Cumulative CPU time of the whole machine, in clock ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

/// The aggregate `cpu ` lines of `/proc/stat` text, in order. Kernels before 2.6 print only
/// user, nice, system and idle; iowait counts as idle when present, and guest time is left out
/// of the total because user time already includes it.
pub fn parse_cpu(text: &str) -> Vec<CpuTimes> {
    text.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("cpu ")?;
            let f: Vec<u64> = rest
                .split_whitespace()
                .map_while(|x| x.parse().ok())
                .collect();
            if f.len() < 4 {
                return None;
            }
            let total = f.iter().take(8).fold(0u64, |a, x| a.saturating_add(*x));
            let idle = f[3].saturating_add(f.get(4).copied().unwrap_or(0));
            Some(CpuTimes { total, idle })
        })
        .collect()
}

/// Busy share of the time between two samples, 0 to 100. `None` when no time passed or a
/// counter went backwards (a wrap or a reset), rather than a nonsense figure.
pub fn cpu_percent(prev: CpuTimes, now: CpuTimes) -> Option<f64> {
    let dt = now.total.checked_sub(prev.total)?;
    let di = now.idle.checked_sub(prev.idle)?;
    if dt == 0 || di > dt {
        return None;
    }
    Some((dt - di) as f64 * 100.0 / dt as f64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mem {
    pub total_kb: u64,
    pub avail_kb: u64,
}

impl Mem {
    pub fn used_kb(&self) -> u64 {
        self.total_kb.saturating_sub(self.avail_kb)
    }
}

/// MemTotal and MemAvailable from `/proc/meminfo`. Kernels before 3.14 have no MemAvailable;
/// free plus buffers plus page cache stands in. `None` without a non-zero MemTotal.
pub fn parse_meminfo(text: &str) -> Option<Mem> {
    let get = |key: &str| {
        text.lines().find_map(|l| {
            let rest = l.strip_prefix(key)?.strip_prefix(':')?;
            rest.split_whitespace().next()?.parse::<u64>().ok()
        })
    };
    let total_kb = get("MemTotal").filter(|t| *t > 0)?;
    let avail_kb = get("MemAvailable")
        .or_else(|| {
            let free = get("MemFree")?;
            Some(free + get("Buffers").unwrap_or(0) + get("Cached").unwrap_or(0))
        })
        .unwrap_or(total_kb)
        .min(total_kb);
    Some(Mem { total_kb, avail_kb })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub fs: String,
    pub mount: String,
    pub used_kb: u64,
    pub avail_kb: u64,
    pub total_kb: u64,
}

impl Disk {
    /// Used share as `df` computes Capacity: of what the user can have (used plus available),
    /// since the blocks reserved for root are not the user's.
    pub fn percent(&self) -> Option<f64> {
        let room = self.used_kb.saturating_add(self.avail_kb);
        (room > 0).then(|| self.used_kb as f64 * 100.0 / room as f64)
    }
}

/// The filesystems of `df -Pk` output (GNU or busybox), one per mount, header skipped. Fields are
/// read from the right, so a device name with spaces still parses. A second line for the same
/// filesystem (`/sandbox` on the root filesystem) is dropped, and so are zero-size ones.
pub fn parse_df(text: &str) -> Vec<Disk> {
    let mut out: Vec<Disk> = Vec::new();
    for l in text.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 6 || !f[f.len() - 1].starts_with('/') {
            continue;
        }
        let n = f.len();
        let num = |i: usize| f[i].parse::<u64>().ok();
        let (Some(total_kb), Some(used_kb), Some(avail_kb)) = (num(n - 5), num(n - 4), num(n - 3))
        else {
            continue;
        };
        if total_kb == 0 {
            continue;
        }
        let d = Disk {
            fs: f[..n - 5].join(" "),
            mount: f[n - 1].into(),
            used_kb,
            avail_kb,
            total_kb,
        };
        if out
            .iter()
            .any(|o| o.mount == d.mount || (o.fs == d.fs && o.total_kb == d.total_kb))
        {
            continue;
        }
        out.push(d);
    }
    out
}

/// One reading of the sandbox in view.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub cpu: Option<f64>,
    pub mem: Option<Mem>,
    pub disks: Vec<Disk>,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.mem.is_none() && self.disks.is_empty()
    }
}

/// A reading from one exec's output and the last CPU sample before it. Returns the reading and
/// the sample to diff the next one against.
pub fn parse(text: &str, prev: Option<CpuTimes>) -> (Usage, Option<CpuTimes>) {
    let samples = parse_cpu(text);
    let last = samples.last().copied();
    let cpu = match samples.as_slice() {
        [.., a, b] => cpu_percent(*a, *b),
        [b] => prev.and_then(|a| cpu_percent(a, *b)),
        [] => None,
    };
    let u = Usage {
        cpu,
        mem: parse_meminfo(text),
        disks: parse_df(text),
    };
    (u, last.or(prev))
}

/// How full a figure reads: warned from 80%, alarming from 95%.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    High,
}

pub fn level(pct: f64) -> Level {
    if pct >= 95.0 {
        Level::High
    } else if pct >= 80.0 {
        Level::Warn
    } else {
        Level::Ok
    }
}

/// `used/total unit`, the unit picked by the total: `1.1/2.0 GiB`, `3.2/20 GiB`, `300/512 MiB`.
pub fn amount(used_kb: u64, total_kb: u64) -> String {
    const MIB: f64 = 1024.0;
    const GIB: f64 = 1024.0 * 1024.0;
    let (u, t) = (used_kb as f64, total_kb as f64);
    if t < GIB {
        return format!("{:.0}/{:.0} MiB", u / MIB, t / MIB);
    }
    let one = |x: f64| {
        if x < 10.0 {
            format!("{x:.1}")
        } else {
            format!("{x:.0}")
        }
    };
    format!("{}/{} GiB", one(u / GIB), one(t / GIB))
}

/// One figure of the meter in its long and short forms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub long: String,
    pub short: String,
    pub level: Level,
}

/// The figures to show, in order: cpu, mem, then each filesystem (`disk` for the first; `root`
/// for `/` when `/sandbox` is a separate one).
pub fn parts(u: &Usage) -> Vec<Part> {
    let mut out = Vec::new();
    if let Some(c) = u.cpu {
        let s = format!("cpu {:.0}%", c);
        out.push(Part {
            long: s.clone(),
            short: s,
            level: level(c),
        });
    }
    if let Some(m) = u.mem {
        let pct = m.used_kb() as f64 * 100.0 / m.total_kb as f64;
        out.push(Part {
            long: format!("mem {}", amount(m.used_kb(), m.total_kb)),
            short: format!("mem {pct:.0}%"),
            level: level(pct),
        });
    }
    for (i, d) in u.disks.iter().enumerate() {
        let Some(pct) = d.percent() else { continue };
        let label = if i > 0 && d.mount == "/" {
            "root"
        } else {
            "disk"
        };
        out.push(Part {
            long: format!("{label} {}", amount(d.used_kb, d.total_kb)),
            short: format!("{label} {pct:.0}%"),
            level: level(pct),
        });
    }
    out
}

/// Separator between figures.
pub const GAP: &str = "  ";

/// The figures that fit in `width` columns, each as `(text, level)`: every figure in its long
/// form if they fit, else in the short form, else as many leading short figures as fit.
pub fn fit(parts: &[Part], width: usize) -> Vec<(String, Level)> {
    let w = |xs: &[&String]| {
        xs.iter().map(|s| s.chars().count()).sum::<usize>() + GAP.len() * xs.len().saturating_sub(1)
    };
    for k in (1..=parts.len()).rev() {
        for long in [true, false] {
            if long && k < parts.len() {
                continue;
            }
            let pick: Vec<&String> = parts[..k]
                .iter()
                .map(|p| if long { &p.long } else { &p.short })
                .collect();
            if w(&pick) <= width {
                return parts[..k]
                    .iter()
                    .zip(pick)
                    .map(|(p, s)| (s.clone(), p.level))
                    .collect();
            }
        }
    }
    Vec::new()
}

/// Reads one sandbox on a thread; each successful read arrives on a channel, and `None` says
/// reads keep failing (the old figures are stale).
pub struct Meter {
    pub sandbox: String,
    pub gateway: String,
    stop: Arc<AtomicBool>,
    rx: Receiver<Option<Usage>>,
}

impl Meter {
    pub fn spawn(sandbox: String, gateway: String) -> Self {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (s, sb, gw) = (stop.clone(), sandbox.clone(), gateway.clone());
        thread::spawn(move || {
            let wait = |d: Duration| {
                let mut waited = Duration::ZERO;
                while waited < d && !s.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                    waited += Duration::from_millis(100);
                }
            };
            wait(SETTLE);
            let mut prev: Option<CpuTimes> = None;
            let mut fails = 0u32;
            // Unit tests drive the app without a gateway: never exec into a real sandbox.
            while !cfg!(test) && !s.load(Ordering::Relaxed) {
                if IN_FLIGHT
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_err()
                {
                    wait(Duration::from_millis(200));
                    continue;
                }
                let out = Command::new("openshell")
                    .args(meter_args(&sb, &gw, prev.is_none()))
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output();
                IN_FLIGHT.store(false, Ordering::Release);
                let reading = out.ok().map(|o| {
                    let (u, p) = parse(&String::from_utf8_lossy(&o.stdout), prev);
                    prev = p;
                    u
                });
                let msg = match reading.filter(|u| !u.is_empty()) {
                    Some(u) => {
                        fails = 0;
                        Some(Some(u))
                    }
                    None => {
                        fails += 1;
                        // One miss keeps the figures; a second one clears them.
                        (fails == 2).then_some(None)
                    }
                };
                if let Some(m) = msg {
                    if tx.send(m).is_err() {
                        break;
                    }
                }
                wait(backoff(fails));
            }
        });
        Meter {
            sandbox,
            gateway,
            stop,
            rx,
        }
    }

    /// The newest message since the last call: `Some(Some(u))` a reading, `Some(None)` reads
    /// are failing, `None` nothing new.
    pub fn latest(&mut self) -> Option<Option<Usage>> {
        let mut last = None;
        loop {
            match self.rx.try_recv() {
                Ok(m) => last = Some(m),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return last,
            }
        }
    }
}

/// The wait before the next read: the interval, doubled per failed read up to `MAX_BACKOFF`.
pub fn backoff(fails: u32) -> Duration {
    (EVERY * 2u32.saturating_pow(fails.min(4))).min(MAX_BACKOFF)
}

impl Drop for Meter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Output of the first read in a busybox (Alpine) sandbox: two `cpu` lines, a meminfo, and
    /// busybox `df -Pk` with `/sandbox` on the root filesystem.
    const BUSYBOX: &str = "cpu  4705 0 1325 236812 207 0 61 0 0 0
cpu  4730 0 1330 237187 207 0 62 0 0 0
MemTotal:        2030612 kB
MemFree:          912340 kB
MemAvailable:    1478220 kB
Buffers:           20212 kB
Cached:           600000 kB
Filesystem           1024-blocks    Used Available Capacity Mounted on
overlay               20511312   3355448  16091060  17% /
overlay               20511312   3355448  16091060  17% /sandbox
";

    #[test]
    fn a_busybox_first_read_gives_every_figure() {
        let (u, prev) = parse(BUSYBOX, None);
        // 406 ticks passed, 375 of them idle.
        assert_eq!(u.cpu.map(|c| c.round()), Some(8.0));
        assert_eq!(
            u.mem,
            Some(Mem {
                total_kb: 2030612,
                avail_kb: 1478220
            })
        );
        assert_eq!(u.disks.len(), 1, "/sandbox on / is the same filesystem");
        assert_eq!(prev.unwrap().total, 4730 + 1330 + 237187 + 207 + 62);
        let shown: Vec<String> = fit(&parts(&u), 80).into_iter().map(|p| p.0).collect();
        assert_eq!(shown, ["cpu 8%", "mem 0.5/1.9 GiB", "disk 3.2/20 GiB"]);
    }

    #[test]
    fn a_later_read_is_diffed_against_the_previous_one() {
        let prev = CpuTimes {
            total: 1000,
            idle: 900,
        };
        let (u, p) = parse("cpu  100 0 50 900 50\n", Some(prev));
        assert_eq!(u.cpu, Some(50.0));
        assert_eq!(
            p,
            Some(CpuTimes {
                total: 1100,
                idle: 950
            })
        );
        // The first read without a previous sample and only one line has no CPU figure.
        assert_eq!(parse("cpu  1 2 3 4\n", None).0.cpu, None);
    }

    #[test]
    fn counters_that_go_backwards_or_stand_still_give_no_cpu_figure() {
        let a = CpuTimes {
            total: u64::MAX - 5,
            idle: 10,
        };
        let wrapped = CpuTimes { total: 3, idle: 12 };
        assert_eq!(cpu_percent(a, wrapped), None);
        assert_eq!(cpu_percent(a, a), None, "zero time passed");
        let odd = CpuTimes {
            total: a.total,
            idle: 5,
        };
        assert_eq!(cpu_percent(a, odd), None, "idle went backwards");
        // Huge counters do not overflow the sum.
        assert_eq!(
            parse_cpu(&format!("cpu  {} {} 0 0\n", u64::MAX, u64::MAX))[0].total,
            u64::MAX
        );
    }

    #[test]
    fn an_old_kernel_without_iowait_or_memavailable_still_reads() {
        let c = parse_cpu("cpu  10 0 10 80\ncpu0 10 0 10 80\nintr 5\n");
        assert_eq!(
            c,
            vec![CpuTimes {
                total: 100,
                idle: 80
            }]
        );
        assert!(parse_cpu("cpu  1 2\n").is_empty(), "too few fields");
        let m =
            parse_meminfo("MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 250 kB\n");
        assert_eq!(m.unwrap().avail_kb, 400);
    }

    #[test]
    fn missing_or_zero_totals_are_left_out_not_shown_as_errors() {
        assert_eq!(parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\n"), None);
        assert_eq!(
            parse_meminfo("cat: /proc/meminfo: Permission denied\n"),
            None
        );
        assert!(parse_df(
            "Filesystem 1024-blocks Used Available Capacity Mounted on\nnone 0 0 0 - /\n"
        )
        .is_empty());
        let (u, _) = parse("sh: /proc/stat: Permission denied\n", None);
        assert!(u.is_empty() && parts(&u).is_empty());
        // Only memory could be read: only memory shows.
        let (u, _) = parse("MemTotal: 2097152 kB\nMemAvailable: 1048576 kB\n", None);
        let shown: Vec<String> = fit(&parts(&u), 80).into_iter().map(|p| p.0).collect();
        assert_eq!(shown, ["mem 1.0/2.0 GiB"]);
    }

    #[test]
    fn a_separate_sandbox_filesystem_shows_beside_root() {
        let df = "Filesystem     1024-blocks    Used Available Capacity Mounted on
/dev/vdb          10255636 9900000    355636      97% /sandbox
my disk            4096000  100000   3996000       3% /
";
        let d = parse_df(df);
        assert_eq!(d.len(), 2);
        assert_eq!(d[1].fs, "my disk");
        let u = Usage {
            disks: d,
            ..Default::default()
        };
        let p = parts(&u);
        assert_eq!(p[0].long, "disk 9.4/9.8 GiB");
        assert_eq!(p[0].level, Level::High);
        assert_eq!(p[1].long, "root 0.1/3.9 GiB");
        assert_eq!(p[1].level, Level::Ok);
    }

    #[test]
    fn levels_warn_at_80_and_alarm_at_95() {
        assert_eq!(level(79.9), Level::Ok);
        assert_eq!(level(80.0), Level::Warn);
        assert_eq!(level(95.0), Level::High);
        assert_eq!(amount(512 * 1024, 900 * 1024), "512/900 MiB");
    }

    #[test]
    fn tiny_widths_drop_to_percentages_then_to_fewer_figures() {
        let (u, _) = parse(BUSYBOX, None);
        let p = parts(&u);
        let text = |w: usize| -> String {
            fit(&p, w)
                .into_iter()
                .map(|x| x.0)
                .collect::<Vec<_>>()
                .join(GAP)
        };
        assert_eq!(text(40), "cpu 8%  mem 0.5/1.9 GiB  disk 3.2/20 GiB");
        assert_eq!(text(30), "cpu 8%  mem 27%  disk 17%");
        assert_eq!(text(15), "cpu 8%  mem 27%");
        assert_eq!(text(6), "cpu 8%");
        assert_eq!(text(3), "");
        assert_eq!(text(0), "");
        for w in 0..60 {
            assert!(text(w).chars().count() <= w, "width {w}");
        }
    }

    #[test]
    fn the_command_is_one_read_only_sh_and_the_first_takes_two_samples() {
        let a = meter_args("sb", "gw", true);
        assert_eq!(&a[..4], ["-g", "gw", "sandbox", "exec"]);
        let s = a.last().unwrap();
        assert_eq!(s.matches("/proc/stat").count(), 2);
        assert!(s.contains("df -Pk /sandbox /") && s.contains("/proc/meminfo"));
        assert_eq!(script(false).matches("/proc/stat").count(), 1);
        assert!(!script(false).contains("sleep"));
        assert_eq!(meter_args("sb", "", false)[0], "sandbox");
    }

    #[test]
    fn the_sandbox_check_runs_the_script_osh_runs() {
        let checks = include_str!("../../tests/sandbox/checks.sh");
        assert!(
            checks.contains(&format!("local meter='{}'", script(true))),
            "tests/sandbox/checks.sh must run usage::script(true) verbatim"
        );
    }

    #[test]
    fn failed_reads_back_off_up_to_a_ceiling() {
        assert_eq!(backoff(0), EVERY);
        assert_eq!(backoff(1), EVERY * 2);
        assert_eq!(backoff(9), MAX_BACKOFF);
    }
}
