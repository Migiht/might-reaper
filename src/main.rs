//! claude-reaper — stops idle Claude Desktop session workers.
//!
//! Usage: claude-reaper [--interval DUR] [--idle DUR] [--sample DUR] [--cpu PCT] [--grace DUR]
//!                      [--only SUBSTR] [--dry-run] [--list]
//!
//! Claude Desktop keeps one `claude` CLI process (a Bun binary, 100–400 MB private memory) alive for
//! every session it has opened, until the session is archived or the app quits. Stopping the
//! process loses nothing: the transcript is on disk and the app starts a new worker with `--resume`
//! as soon as the session gets its next message.
//!
//! The daemon samples every worker each `--sample` (5m). A sample counts as ACTIVITY when the
//! worker used more than `--cpu` percent (1%) of one core since the previous sample — an idle
//! worker sits at ~0.4% (Bun's GC and timers) — or when it has a child process started more than
//! `--grace` (120s) after itself: a Bash tool call or a background task. Children started with the
//! worker are its MCP servers and do not count. Every `--interval` (1h) the workers with no
//! activity for `--idle` (default: the interval) get SIGTERM.
//!
//! Only desktop workers are touched (`claude --output-format stream-json --input-format …`);
//! interactive `claude` sessions in a terminal are left alone. Newly seen workers start as active,
//! so a restarted daemon never kills anything before a full idle period has passed.

use std::collections::HashMap;
use std::fs;
use std::thread;
use std::time::{Duration, Instant};

const SIGTERM: i32 = 15;
const SC_CLK_TCK: i32 = 2;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn sysconf(name: i32) -> i64;
}

fn log(msg: &str) {
    eprintln!("[claude-reaper] {msg}");
}

struct Opts {
    interval: Duration,
    idle: Duration,
    sample: Duration,
    cpu_pct: f64,
    grace: Duration,
    only: Option<String>,
    dry_run: bool,
    list: bool,
}

fn parse_duration(s: &str) -> Option<Duration> {
    let (num, mul) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        _ => (s, 1),
    };
    let n: f64 = num.parse().ok().filter(|n: &f64| *n > 0.0)?;
    Some(Duration::from_secs_f64(n * mul as f64))
}

fn usage() -> ! {
    eprintln!(
        "usage: claude-reaper [--interval DUR] [--idle DUR] [--sample DUR] [--cpu PCT] [--grace DUR] \
         [--only SUBSTR] [--dry-run] [--list]\n  DUR: 90, 30s, 5m, 1h, 1d\n  defaults: --interval 1h --idle <interval> \
         --sample 5m --cpu 1 --grace 120s"
    );
    std::process::exit(2);
}

fn parse_args() -> Opts {
    let mut o = Opts {
        interval: Duration::from_secs(3600),
        idle: Duration::ZERO,
        sample: Duration::from_secs(300),
        cpu_pct: 1.0,
        grace: Duration::from_secs(120),
        only: None,
        dry_run: false,
        list: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut dur = || args.next().and_then(|v| parse_duration(&v)).unwrap_or_else(|| usage());
        match a.as_str() {
            "--interval" => o.interval = dur(),
            "--idle" => o.idle = dur(),
            "--sample" => o.sample = dur(),
            "--grace" => o.grace = dur(),
            "--cpu" => {
                o.cpu_pct = args.next().and_then(|v| v.parse().ok()).filter(|p: &f64| *p >= 0.0).unwrap_or_else(|| usage())
            }
            "--only" => o.only = Some(args.next().unwrap_or_else(|| usage())),
            "--dry-run" => o.dry_run = true,
            "--list" => o.list = true,
            _ => usage(),
        }
    }
    if o.idle.is_zero() {
        o.idle = o.interval;
    }
    o.sample = o.sample.min(o.interval);
    o
}

/// The fields of /proc/<pid>/stat we need.
struct Stat {
    ppid: i32,
    cpu_ticks: u64,
    start_ticks: u64,
}

fn read_stat(pid: i32) -> Option<Stat> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) may contain spaces and parens; the rest starts after the last ')'.
    let f: Vec<&str> = s[s.rfind(')')? + 2..].split(' ').collect();
    Some(Stat {
        ppid: f.get(1)?.parse().ok()?,
        cpu_ticks: f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?,
        start_ticks: f.get(19)?.parse().ok()?,
    })
}

fn pids() -> Vec<i32> {
    let Ok(dir) = fs::read_dir("/proc") else { return vec![] };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect()
}

fn is_desktop_worker(pid: i32) -> bool {
    if fs::read_to_string(format!("/proc/{pid}/comm")).map(|c| c.trim_end() != "claude").unwrap_or(true) {
        return false;
    }
    let Ok(cmd) = fs::read(format!("/proc/{pid}/cmdline")) else { return false };
    let args: Vec<&[u8]> = cmd.split(|&b| b == 0).collect();
    args.windows(2).any(|w| w[0] == b"--output-format" && w[1] == b"stream-json")
        && args.contains(&&b"--input-format"[..])
}

fn session_name(pid: i32) -> String {
    fs::read(format!("/proc/{pid}/environ"))
        .ok()
        .and_then(|env| {
            env.split(|&b| b == 0)
                .find_map(|kv| kv.strip_prefix(b"CLAUDE_CODE_SESSION_NAME=").map(|v| String::from_utf8_lossy(v).into_owned()))
        })
        .unwrap_or_default()
}

/// Private memory plus swap, in MiB: what stopping the worker gives back.
fn private_mib(pid: i32) -> u64 {
    let Ok(s) = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) else { return 0 };
    s.lines()
        .filter(|l| l.starts_with("Private_Clean:") || l.starts_with("Private_Dirty:") || l.starts_with("Swap:"))
        .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        .sum::<u64>()
        / 1024
}

struct Snapshot {
    /// pid -> stat for every desktop worker.
    workers: HashMap<i32, Stat>,
    /// pid -> number of children started more than the grace period after it.
    late_children: HashMap<i32, usize>,
}

fn snapshot(grace_ticks: u64) -> Snapshot {
    let mut stats: HashMap<i32, Stat> = pids().into_iter().filter_map(|p| Some((p, read_stat(p)?))).collect();
    let workers: HashMap<i32, Stat> = stats
        .keys()
        .copied()
        .filter(|&p| is_desktop_worker(p))
        .collect::<Vec<_>>()
        .into_iter()
        .filter_map(|p| Some((p, stats.remove(&p)?)))
        .collect();
    let mut late_children = HashMap::new();
    for st in stats.values() {
        if let Some(w) = workers.get(&st.ppid)
            && st.start_ticks.saturating_sub(w.start_ticks) > grace_ticks
        {
            *late_children.entry(st.ppid).or_insert(0) += 1;
        }
    }
    Snapshot { workers, late_children }
}

struct Tracked {
    cpu_ticks: u64,
    sampled: Instant,
    last_active: Instant,
}

fn fmt_dur(d: Duration) -> String {
    let (s, m) = (d.as_secs(), d.as_secs() / 60);
    if m >= 60 {
        format!("{}h{:02}m", m / 60, m % 60)
    } else if s >= 60 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

fn list(hz: u64, grace_ticks: u64) {
    let wait = Duration::from_secs(3);
    let before = snapshot(grace_ticks);
    thread::sleep(wait);
    let after = snapshot(grace_ticks);
    let mut pids: Vec<_> = after.workers.keys().copied().collect();
    pids.sort();
    println!("{:>7} {:>8} {:>6} {:>5}  session", "pid", "private", "cpu%", "kids");
    for pid in pids {
        let st = &after.workers[&pid];
        let cpu = match before.workers.get(&pid) {
            Some(b) if b.start_ticks == st.start_ticks => {
                format!("{:.1}", (st.cpu_ticks - b.cpu_ticks) as f64 / hz as f64 / wait.as_secs_f64() * 100.0)
            }
            _ => "-".into(),
        };
        let kids = after.late_children.get(&pid).copied().unwrap_or(0);
        println!("{pid:>7} {:>6}Mi {cpu:>6} {kids:>5}  {}", private_mib(pid), session_name(pid));
    }
}

fn main() {
    let o = parse_args();
    let hz = match unsafe { sysconf(SC_CLK_TCK) } {
        n if n > 0 => n as u64,
        _ => 100,
    };
    let grace_ticks = o.grace.as_secs() * hz;
    if o.list {
        return list(hz, grace_ticks);
    }
    log(&format!(
        "check every {}, stop after {} idle (sample {}, cpu > {}% or a Bash child counts as activity){}",
        fmt_dur(o.interval),
        fmt_dur(o.idle),
        fmt_dur(o.sample),
        o.cpu_pct,
        if o.dry_run { ", dry run" } else { "" }
    ));

    // Keyed by (pid, start time) so a recycled pid is a new worker.
    let mut tracked: HashMap<(i32, u64), Tracked> = HashMap::new();
    let mut next_check = Instant::now() + o.interval;
    loop {
        let now = Instant::now();
        let snap = snapshot(grace_ticks);
        tracked.retain(|(pid, start), _| snap.workers.get(pid).is_some_and(|s| s.start_ticks == *start));
        for (&pid, st) in &snap.workers {
            let t = tracked.entry((pid, st.start_ticks)).or_insert(Tracked {
                cpu_ticks: st.cpu_ticks,
                sampled: now,
                last_active: now,
            });
            let limit = o.cpu_pct / 100.0 * now.duration_since(t.sampled).as_secs_f64() * hz as f64;
            let busy = st.cpu_ticks.saturating_sub(t.cpu_ticks) as f64 > limit || snap.late_children.contains_key(&pid);
            if busy {
                t.last_active = now;
            }
            t.cpu_ticks = st.cpu_ticks;
            t.sampled = now;
        }

        if now >= next_check {
            next_check = now + o.interval;
            let mut freed = 0;
            for ((pid, start), t) in &tracked {
                let idle = now.duration_since(t.last_active);
                if idle < o.idle {
                    continue;
                }
                // Re-check identity right before the signal: the pid may have been recycled.
                if !read_stat(*pid).is_some_and(|s| s.start_ticks == *start) {
                    continue;
                }
                let name = session_name(*pid);
                if o.only.as_ref().is_some_and(|only| !name.contains(only.as_str())) {
                    continue;
                }
                let mib = private_mib(*pid);
                if o.dry_run || unsafe { kill(*pid, SIGTERM) } == 0 {
                    freed += mib;
                    log(&format!(
                        "{}stopped pid {pid} ({mib} MiB, idle {}): {name}",
                        if o.dry_run { "[dry run] would have " } else { "" },
                        fmt_dur(idle)
                    ));
                }
            }
            if freed > 0 {
                log(&format!("freed ~{freed} MiB"));
            }
        }

        thread::sleep(o.sample.min(next_check.saturating_duration_since(Instant::now())).max(Duration::from_secs(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("0.5h"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration("x"), None);
    }

    #[test]
    fn own_stat_parses() {
        let st = read_stat(std::process::id() as i32).unwrap();
        assert!(st.start_ticks > 0);
    }
}
