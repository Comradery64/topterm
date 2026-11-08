//! Snapshot collection and session grouping.
//!
//! Data source (macOS, unprivileged, via shelling out):
//!   ps -axo pid=,ppid=,pgid=,tty=,rss=,%cpu=,etime=,command=   (rss in KB)
//!   lsof -a -d cwd -p pid1,pid2,...   (batched; parse stdout even on non-zero exit)
//!   sysctl -n hw.memsize
//!
//! `collect()` shells out; everything else is pure and unit-testable
//! against canned `ps` output.

use std::collections::{HashMap, HashSet};
use std::io;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// One process as reported by `ps`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// Controlling terminal, e.g. "ttys003". `None` when ps reports "??".
    pub tty: Option<String>,
    /// Memory in KB: physical footprint when readable (includes compressed
    /// and swapped pages), else resident set size from ps.
    pub mem_kb: u64,
    pub cpu_pct: f64,
    /// Elapsed time string from ps, e.g. "01:23" or "2-03:45:12".
    pub etime: String,
    /// Full command line.
    pub command: String,
}

/// One terminal session: all processes sharing a controlling tty
/// (plus tty-less descendants attributed by walking ancestry to a
/// ttys* ancestor).
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// e.g. "ttys003". For detached subtrees this is the root pid, e.g. "pid:4242".
    pub tty: String,
    /// Pid of the interactive shell (the child of `login -pfl <user>`,
    /// or the session root itself if no login wrapper).
    pub shell_pid: i32,
    /// Basename of the shell, e.g. "zsh".
    pub shell_name: String,
    /// Most interesting non-shell command running in the session, if any.
    pub recent_command: Option<String>,
    /// Shell's cwd from batched lsof; "?" when unavailable.
    pub cwd: String,
    /// Sum of mem_kb over `procs` (double-counts shared pages).
    pub total_mem_kb: u64,
    /// Change in total_mem_kb vs the previous sample (0 on first sample).
    pub delta_mem_kb: i64,
    pub total_cpu_pct: f64,
    pub proc_count: usize,
    /// Distinct process-group ids in this session (kill targets).
    pub pgids: Vec<i32>,
    /// Processes in tree order (parent before children); pair with
    /// `depths` for indentation. Same length as `depths`.
    pub procs: Vec<ProcInfo>,
    /// Parent-child depth for each entry of `procs` (session root = 0).
    pub depths: Vec<usize>,
    /// True when total_mem_kb grew >= GROWING_THRESHOLD_KB across recent samples.
    pub growing: bool,
    /// The Claude Code instance running in this tab, if any.
    pub claude: Option<crate::claude::Live>,
    /// Memory of that Claude process plus everything under it (MCP servers,
    /// helpers) — what exiting it would free.
    pub claude_mem_kb: u64,
    /// Memory held by MCP server processes in this tab.
    pub mcp_mem_kb: u64,
    /// Characters of scrollback Terminal.app holds for this tab, when measured.
    pub scrollback_chars: Option<u64>,
}

/// One MCP server, by name, across the whole machine.
#[derive(Debug, Clone, PartialEq)]
pub struct McpGroup {
    pub name: String,
    /// Number of processes (an `npm exec` wrapper and its node child count as two).
    pub procs: usize,
    /// Number of distinct tabs running a copy.
    pub tabs: usize,
    pub mem_kb: u64,
    /// Processes reparented to launchd: their Claude session is gone, so
    /// nothing will ever talk to them again.
    pub orphaned: usize,
    pub orphaned_mem_kb: u64,
}

/// Machine-wide memory-pressure numbers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct System {
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
    /// Bytes written to the internal SSD since boot, when readable.
    pub ssd_written_bytes: Option<u64>,
    pub uptime_secs: u64,
}

impl System {
    /// Average internal-SSD writes per day since boot.
    pub fn ssd_bytes_per_day(&self) -> Option<u64> {
        let w = self.ssd_written_bytes?;
        (self.uptime_secs > 0).then(|| (w as f64 * 86_400.0 / self.uptime_secs as f64) as u64)
    }
}

/// One full sample of the system.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Tty-owned sessions, sorted by total_mem_kb descending.
    pub sessions: Vec<Session>,
    /// Dev-tool subtrees reparented to launchd (pid 1) with subtree RSS
    /// over DETACHED_MIN_RSS_KB — no tab owns them; keyed by root pid.
    pub detached: Vec<Session>,
    /// Terminal emulator app processes (Terminal.app, iTerm2, kitty,
    /// Alacritty, WezTerm, Ghostty), matched by command name.
    pub emulators: Vec<ProcInfo>,
    /// Sum of mem_kb over all listed processes (double-counts shared pages).
    pub total_mem_kb: u64,
    /// Physical memory from `sysctl -n hw.memsize`.
    pub phys_mem_bytes: u64,
    /// Total number of processes seen in the ps output.
    pub proc_count: usize,
    /// tty -> terminal tab title, from the emulator's AppleScript API.
    /// Empty when no scriptable emulator is running or the query failed.
    pub tab_titles: HashMap<String, String>,
    /// One-line explanation when tab titles could not be read (e.g. the
    /// Automation permission has not been granted).
    pub tab_title_hint: Option<String>,
    pub taken_at: Instant,
    pub system: System,
    /// MCP servers grouped by name, biggest first.
    pub mcp: Vec<McpGroup>,
}

/// Session RSS growth flag threshold (50 MB in KB).
pub const GROWING_THRESHOLD_KB: u64 = 50 * 1024;
/// Minimum subtree RSS for a launchd-reparented dev-tool subtree to be
/// listed in the detached section (50 MB in KB).
pub const DETACHED_MIN_RSS_KB: u64 = 50 * 1024;

/// Timeout for the batched lsof call.
const LSOF_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for one batched tab-title AppleScript call. Terminal.app answers
/// in ~3s with 100+ tabs on a machine under memory pressure, so 2s silently
/// dropped every title.
const OSASCRIPT_TIMEOUT: Duration = Duration::from_secs(10);
/// Tab titles change rarely and osascript is slow, so reuse the last answer
/// across samples for this long.
const TAB_TITLE_TTL: Duration = Duration::from_secs(60);

/// Cached (taken_at, tty -> title, hint) from the last successful query.
#[allow(clippy::type_complexity)]
static TAB_TITLE_CACHE: Mutex<Option<(Instant, HashMap<String, String>, Option<String>)>> =
    Mutex::new(None);

/// Terminal.app exposes `tty` per tab and `custom title` as the tab's label.
/// Note: inside `tell application "Terminal"` the word `tab` names Terminal's
/// tab *class*, so the field separator has to be built outside the tell block.
/// Some windows (Settings, Inspector) have no `tabs`, hence the inner `try`.
const TERMINAL_SCPT: &[&str] = &[
    "set d to (ASCII character 9)",
    "set out to \"\"",
    "tell application \"Terminal\"",
    "repeat with w in windows",
    "try",
    "repeat with t in tabs of w",
    "set ct to \"\"",
    "try",
    "set ct to custom title of t",
    "end try",
    "set out to out & (tty of t) & d & ct & linefeed",
    "end repeat",
    "end try",
    "end repeat",
    "end tell",
    "return out",
];

/// iTerm2 exposes tty per *session* (a split pane); its `name` is the label
/// shown on the tab.
const ITERM_SCPT: &[&str] = &[
    "set d to (ASCII character 9)",
    "set out to \"\"",
    "tell application \"iTerm2\"",
    "repeat with w in windows",
    "try",
    "repeat with t in tabs of w",
    "repeat with s in sessions of t",
    "try",
    "set out to out & (tty of s) & d & (name of s) & linefeed",
    "end try",
    "end repeat",
    "end repeat",
    "end try",
    "end repeat",
    "end tell",
    "return out",
];

/// Titles the emulators report for a tab the user never named — noise, not a label.
const PLACEHOLDER_TITLES: &[&str] = &["Terminal", "iTerm2"];

/// Terminal emulator command-line markers (matched by substring, like the
/// Python reference).
const EMULATOR_MARKERS: &[&str] = &[
    "Terminal.app/Contents/MacOS/Terminal",
    "iTerm.app/Contents/MacOS/iTerm2",
    "kitty.app/Contents/MacOS/kitty",
    "Alacritty.app/Contents/MacOS/alacritty",
    "WezTerm.app/Contents/MacOS/wezterm-gui",
    "Ghostty.app/Contents/MacOS/ghostty",
];

/// Dev-tool basenames whose launchd-reparented subtrees are worth listing.
const ORPHAN_HINTS: &[&str] = &[
    "node", "claude", "python", "python3", "ruby", "java", "deno", "bun",
    "go", "cargo", "rustc", "esbuild", "tsserver", "vite", "next", "docker",
    "com.docker.backend", "dockerd", "npm", "pnpm", "yarn",
];

/// Shell basenames (leading '-' of login shells is stripped before matching).
const SHELLS: &[&str] = &[
    "bash", "zsh", "sh", "fish", "tcsh", "csh", "ksh", "dash", "login",
];

/// Take one full snapshot: run ps, group into sessions, resolve shell
/// cwds with one batched lsof (parse stdout even if lsof exits non-zero;
/// degrade to "?" on timeout/failure), and read hw.memsize.
///
/// This is the only function here that executes commands.
pub fn collect() -> io::Result<Snapshot> {
    collect_opts(false)
}

/// `collect()`, optionally also measuring per-tab scrollback (slow: ~10s with
/// 100 tabs, so it is cached for SCROLLBACK_TTL).
pub fn collect_opts(scrollback: bool) -> io::Result<Snapshot> {
    let ps_out = Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,tty=,rss=,%cpu=,etime=,command="])
        .stderr(Stdio::null())
        .output()?;
    let mut procs = parse_ps(&String::from_utf8_lossy(&ps_out.stdout));
    // RSS only counts pages currently in RAM; on a machine that is swapping
    // it hides most of the real usage. Prefer the physical footprint
    // (Activity Monitor's "Memory"), which includes compressed and swapped
    // pages. Other users' processes refuse the call and keep their RSS.
    for p in &mut procs {
        if let Some(kb) = footprint_kb(p.pid) {
            p.mem_kb = kb;
        }
    }
    let proc_count = procs.len();
    let total_mem_kb: u64 = procs.iter().map(|p| p.mem_kb).sum();

    let all_procs = procs.clone();
    let (mut sessions, detached, emulators) = group_sessions(procs);

    let variants = crate::claude::variants(&crate::claude::home());
    for s in &mut sessions {
        attach_claude(s, &variants);
    }
    let mcp = mcp_groups(&sessions, &all_procs);
    if scrollback {
        let chars = cached_scrollback();
        for s in &mut sessions {
            s.scrollback_chars = chars.get(&s.tty).copied();
        }
    }

    // One batched lsof for all shell pids; parse whatever stdout was
    // produced even when lsof exits non-zero (some pids — e.g. the
    // root-owned login wrapper — are unreadable while others resolve).
    let shell_pids: Vec<i32> = sessions.iter().map(|s| s.shell_pid).collect();
    if !shell_pids.is_empty() {
        let list = shell_pids
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut cmd = Command::new("lsof");
        cmd.args(["-a", "-d", "cwd", "-p", &list]);
        if let Some((out, _)) = run_with_timeout(cmd, LSOF_TIMEOUT) {
            let cwds: HashMap<i32, String> = parse_lsof_cwd(&out).into_iter().collect();
            for s in &mut sessions {
                if let Some(cwd) = cwds.get(&s.shell_pid) {
                    s.cwd = cwd.clone();
                }
            }
        }
    }

    let (tab_titles, tab_title_hint) = cached_tab_titles(&emulators);

    let phys_mem_bytes = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
        .unwrap_or(0);

    Ok(Snapshot {
        sessions,
        detached,
        emulators,
        total_mem_kb,
        phys_mem_bytes,
        proc_count,
        tab_titles,
        tab_title_hint,
        taken_at: Instant::now(),
        system: read_system(),
        mcp,
    })
}

/// Fill `claude`, `claude_mem_kb` and `mcp_mem_kb` from the session's tree.
fn attach_claude(s: &mut Session, variants: &[crate::claude::Variant]) {
    s.mcp_mem_kb = s.procs.iter().filter(|p| mcp_name(&p.command).is_some()).map(|p| p.mem_kb).sum();
    let Some((i, live)) = s
        .procs
        .iter()
        .enumerate()
        .find_map(|(i, p)| crate::claude::live(p.pid, variants).map(|l| (i, l)))
    else {
        return;
    };
    s.claude_mem_kb = subtree_mem(&s.procs, &s.depths, i);
    s.claude = Some(live);
}

/// Memory of `procs[i]` plus its descendants, given tree-ordered procs and
/// depths (descendants are the following entries deeper than it). Pure.
pub fn subtree_mem(procs: &[ProcInfo], depths: &[usize], i: usize) -> u64 {
    let d = depths[i];
    procs[i].mem_kb
        + procs[i + 1..]
            .iter()
            .zip(&depths[i + 1..])
            .take_while(|(_, &dd)| dd > d)
            .map(|(p, _)| p.mem_kb)
            .sum::<u64>()
}

/// A readable MCP server name for a process command line, or `None` if it
/// isn't one. `npm exec @acme/weather-mcp` -> "weather-mcp";
/// `python3 ~/.local/bin/notes-tool mcp` -> "notes-tool mcp". Pure.
pub fn mcp_name(command: &str) -> Option<String> {
    let toks: Vec<&str> = command.split_whitespace().collect();
    let base = |t: &str| t.rsplit('/').next().unwrap_or(t).to_string();
    // Someone searching for MCP processes is not one.
    if matches!(toks.first().map(|t| base(t)).as_deref(), Some("grep" | "pgrep" | "rg" | "egrep" | "awk" | "ps")) {
        return None;
    }
    for (i, t) in toks.iter().enumerate() {
        let lower = t.to_ascii_lowercase();
        if lower.starts_with('-') || lower.contains(['^', '$', '|', '*', '\\', '(', ')', ':'])
            || !(lower.contains("mcp") || lower.contains("modelcontextprotocol")) {
            continue;
        }
        if lower == "mcp" {
            return Some(match i.checked_sub(1).map(|j| toks[j]) {
                Some(prev) if !prev.starts_with('-') => format!("{} mcp", base(prev)),
                _ => "mcp".to_string(),
            });
        }
        // `uvx --with mcp[cli]<2 blender-mcp`: a dependency spec, not the server.
        if lower.contains('[') || lower.contains('<') || lower.contains('=') {
            continue;
        }
        return Some(base(t));
    }
    None
}

/// Group every MCP process on the machine by server name. Pure.
pub fn mcp_groups(sessions: &[Session], all: &[ProcInfo]) -> Vec<McpGroup> {
    let mut groups: HashMap<String, McpGroup> = HashMap::new();
    let mut tabs: HashMap<String, HashSet<&str>> = HashMap::new();
    let entry = |groups: &mut HashMap<String, McpGroup>, name: &str| {
        groups.entry(name.to_string()).or_insert_with(|| McpGroup {
            name: name.to_string(),
            procs: 0,
            tabs: 0,
            mem_kb: 0,
            orphaned: 0,
            orphaned_mem_kb: 0,
        });
    };
    let mut in_tab: HashSet<i32> = HashSet::new();
    for s in sessions {
        for p in &s.procs {
            if let Some(name) = mcp_name(&p.command) {
                entry(&mut groups, &name);
                let g = groups.get_mut(&name).expect("inserted");
                g.procs += 1;
                g.mem_kb += p.mem_kb;
                tabs.entry(name).or_default().insert(&s.tty);
                in_tab.insert(p.pid);
            }
        }
    }
    for p in all.iter().filter(|p| !in_tab.contains(&p.pid)) {
        let Some(name) = mcp_name(&p.command) else { continue };
        entry(&mut groups, &name);
        let g = groups.get_mut(&name).expect("inserted");
        g.procs += 1;
        g.mem_kb += p.mem_kb;
        if p.ppid == 1 {
            g.orphaned += 1;
            g.orphaned_mem_kb += p.mem_kb;
        }
    }
    let mut out: Vec<McpGroup> = groups
        .into_values()
        .map(|mut g| {
            g.tabs = tabs.get(&g.name).map_or(0, |t| t.len());
            g
        })
        .collect();
    out.sort_by(|a, b| b.mem_kb.cmp(&a.mem_kb).then(a.name.cmp(&b.name)));
    out
}

fn sysctl(name: &str) -> String {
    Command::new("sysctl")
        .args(["-n", name])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn read_system() -> System {
    let (swap_total_bytes, swap_used_bytes) = parse_swapusage(&sysctl("vm.swapusage"));
    let boot = parse_boottime(&sysctl("kern.boottime"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut cmd = Command::new("ioreg");
    cmd.args(["-r", "-c", "IOBlockStorageDevice", "-w0", "-d", "2", "-l"]);
    let ssd_written_bytes = run_with_timeout(cmd, Duration::from_secs(3))
        .and_then(|(out, _)| parse_internal_ssd_writes(&out));
    System {
        swap_used_bytes,
        swap_total_bytes,
        ssd_written_bytes,
        uptime_secs: boot.map_or(0, |b| now.saturating_sub(b)),
    }
}

/// "total = 28672.00M  used = 27691.50M  free = 980.50M  (encrypted)" ->
/// (total, used) bytes. Pure.
pub fn parse_swapusage(raw: &str) -> (u64, u64) {
    let field = |key: &str| -> u64 {
        raw.split(&format!("{key} = "))
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|v| {
                let (num, unit) = v.split_at(v.len().saturating_sub(1));
                let n: f64 = num.parse().ok()?;
                let mult = match unit {
                    "K" => 1u64 << 10,
                    "M" => 1 << 20,
                    "G" => 1 << 30,
                    _ => return None,
                };
                Some((n * mult as f64) as u64)
            })
            .unwrap_or(0)
    };
    (field("total"), field("used"))
}

/// "{ sec = 1790302637, usec = 885563 } Thu Sep 24 ..." -> 1790302637. Pure.
pub fn parse_boottime(raw: &str) -> Option<u64> {
    raw.split("sec = ").nth(1)?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// From `ioreg -r -c IOBlockStorageDevice -d 2 -l`, the "Bytes (Write)"
/// statistic of the driver under the internal NVMe device (skipping SD cards
/// and disk images). Pure.
pub fn parse_internal_ssd_writes(raw: &str) -> Option<u64> {
    let mut internal = false;
    for line in raw.lines() {
        if let Some(i) = line.find("<class ") {
            let class = line[i + 7..].split([',', '>']).next().unwrap_or("");
            if class.ends_with("BlockStorageDevice") || class.ends_with("BlockDevice") {
                internal = class.contains("NVMe") || class.contains("ANS");
            }
            continue;
        }
        if internal {
            if let Some(i) = line.find("\"Bytes (Write)\"=") {
                let digits: String = line[i + 16..].chars().take_while(|c| c.is_ascii_digit()).collect();
                return digits.parse().ok();
            }
        }
    }
    None
}

/// Per-tab scrollback size, reused for this long because reading every tab's
/// history takes ~10s with 100+ tabs.
const SCROLLBACK_TTL: Duration = Duration::from_secs(300);
const SCROLLBACK_TIMEOUT: Duration = Duration::from_secs(90);

#[allow(clippy::type_complexity)]
static SCROLLBACK_CACHE: Mutex<Option<(Instant, HashMap<String, u64>)>> = Mutex::new(None);

/// tty -> characters of scrollback in Terminal.app. Read-only AppleScript.
fn cached_scrollback() -> HashMap<String, u64> {
    let mut cache = match SCROLLBACK_CACHE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some((at, m)) = cache.as_ref() {
        if at.elapsed() < SCROLLBACK_TTL {
            return m.clone();
        }
    }
    let script = [
        "set d to (ASCII character 9)",
        "set out to \"\"",
        "tell application \"Terminal\"",
        "repeat with w in windows",
        "try",
        "repeat with t in tabs of w",
        "set n to 0",
        "try",
        "set h to history of t",
        "set n to length of h",
        "end try",
        "set out to out & (tty of t) & d & n & linefeed",
        "end repeat",
        "end try",
        "end repeat",
        "end tell",
        "return out",
    ];
    let mut cmd = Command::new("osascript");
    for line in script {
        cmd.args(["-e", line]);
    }
    let m: HashMap<String, u64> = run_with_timeout(cmd, SCROLLBACK_TIMEOUT)
        .map(|(out, _)| {
            out.lines()
                .filter_map(|l| {
                    let (tty, n) = l.split_once('\t')?;
                    Some((tty.trim().rsplit('/').next()?.to_string(), n.trim().parse().ok()?))
                })
                .collect()
        })
        .unwrap_or_default();
    *cache = Some((Instant::now(), m.clone()));
    m
}

/// Fill `delta_mem_kb` and `growing` on `current` by comparing each
/// session (and detached subtree, matched by tty / "pid:N" key) against
/// `previous`. Sessions absent from `previous` keep 0/false.
pub fn diff_snapshots(current: &mut Snapshot, previous: &Snapshot) {
    let prev: HashMap<&str, u64> = previous
        .sessions
        .iter()
        .chain(previous.detached.iter())
        .map(|s| (s.tty.as_str(), s.total_mem_kb))
        .collect();
    for s in current
        .sessions
        .iter_mut()
        .chain(current.detached.iter_mut())
    {
        if let Some(&p) = prev.get(s.tty.as_str()) {
            s.delta_mem_kb = s.total_mem_kb as i64 - p as i64;
            s.growing = s.delta_mem_kb >= GROWING_THRESHOLD_KB as i64;
        }
    }
}

/// Parse raw `ps -axo pid=,ppid=,pgid=,tty=,rss=,%cpu=,etime=,command=`
/// output. Pure. Malformed lines are skipped. tty "??" becomes `None`;
/// a "ttys003"-style value is kept as `Some`.
pub fn parse_ps(raw: &str) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let Some((f, command)) = split_fields(line, 7) else {
            continue;
        };
        if command.is_empty() {
            continue;
        }
        let (Ok(pid), Ok(ppid), Ok(pgid)) =
            (f[0].parse::<i32>(), f[1].parse::<i32>(), f[2].parse::<i32>())
        else {
            continue;
        };
        let (Ok(mem_kb), Ok(cpu_pct)) = (f[4].parse::<u64>(), f[5].parse::<f64>()) else {
            continue;
        };
        let tty = if f[3] == "??" {
            None
        } else {
            Some(f[3].to_string())
        };
        out.push(ProcInfo {
            pid,
            ppid,
            pgid,
            tty,
            mem_kb,
            cpu_pct,
            etime: f[6].to_string(),
            command: command.to_string(),
        });
    }
    out
}

/// Group processes into sessions. Pure.
///
/// - Builds the parent->child tree.
/// - Sessions = processes whose tty matches `ttys[0-9]+`; tty-less ("??")
///   processes are attributed by walking ancestry up to a ttys* ancestor.
/// - The session root is usually `login -pfl <user>`; the reported shell
///   is the interactive shell beneath it.
/// - Returns `(sessions, detached, emulators)`:
///   sessions sorted by total_mem_kb desc; detached = launchd-reparented
///   dev-tool subtrees (claude/node/python/docker etc.) over
///   DETACHED_MIN_RSS_KB, as single-pgid pseudo-sessions keyed by pid;
///   emulators = terminal emulator app processes.
/// - `cwd` is left as "?" and `delta_mem_kb`/`growing` as 0/false; the
///   caller fills those from lsof and RSS history.
pub fn group_sessions(procs: Vec<ProcInfo>) -> (Vec<Session>, Vec<Session>, Vec<ProcInfo>) {
    let idx: HashMap<i32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for p in &procs {
        if p.ppid != p.pid && idx.contains_key(&p.ppid) {
            children.entry(p.ppid).or_default().push(p.pid);
        }
    }
    for kids in children.values_mut() {
        kids.sort_unstable();
    }

    // Attribute each process to a ttys* session, walking ancestry for "??".
    let mut sess_of: Vec<Option<String>> = vec![None; procs.len()];
    for i in 0..procs.len() {
        let mut seen = HashSet::new();
        let mut cur = Some(i);
        while let Some(ci) = cur {
            let c = &procs[ci];
            if !seen.insert(c.pid) {
                break;
            }
            if let Some(t) = &c.tty {
                if is_ttys(t) {
                    sess_of[i] = Some(t.clone());
                    break;
                }
            }
            cur = if c.ppid > 1 {
                idx.get(&c.ppid).copied()
            } else {
                None
            };
        }
    }

    // Tty-owned sessions.
    let mut by_tty: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, s) in sess_of.iter().enumerate() {
        if let Some(t) = s {
            by_tty.entry(t.clone()).or_default().push(i);
        }
    }
    let mut sessions: Vec<Session> = by_tty
        .into_iter()
        .map(|(tty, members)| build_session(tty, &members, &procs, &children, &idx))
        .collect();
    sessions.sort_by(|a, b| b.total_mem_kb.cmp(&a.total_mem_kb).then(a.tty.cmp(&b.tty)));

    // Detached: dev-tool subtrees reparented to launchd (pid 1) that no
    // session claims — the ancestry that would have tied them to a tab is gone.
    let mut detached: Vec<Session> = Vec::new();
    for (i, p) in procs.iter().enumerate() {
        if sess_of[i].is_some() || p.ppid != 1 {
            continue;
        }
        if !ORPHAN_HINTS.contains(&cmd_basename(&p.command)) {
            continue;
        }
        // Collect the whole orphaned subtree, skipping anything a session owns.
        let mut members = Vec::new();
        let mut stack = vec![i];
        let mut visited = HashSet::new();
        while let Some(ci) = stack.pop() {
            if !visited.insert(ci) || sess_of[ci].is_some() {
                continue;
            }
            members.push(ci);
            if let Some(kids) = children.get(&procs[ci].pid) {
                stack.extend(kids.iter().filter_map(|k| idx.get(k).copied()));
            }
        }
        let rss: u64 = members.iter().map(|&m| procs[m].mem_kb).sum();
        if rss >= DETACHED_MIN_RSS_KB {
            detached.push(build_session(
                format!("pid:{}", p.pid),
                &members,
                &procs,
                &children,
                &idx,
            ));
        }
    }
    detached.sort_by(|a, b| b.total_mem_kb.cmp(&a.total_mem_kb).then(a.tty.cmp(&b.tty)));

    // Terminal emulator app processes not claimed by any session.
    let emulators: Vec<ProcInfo> = procs
        .iter()
        .enumerate()
        .filter(|(i, p)| {
            sess_of[*i].is_none() && EMULATOR_MARKERS.iter().any(|m| p.command.contains(m))
        })
        .map(|(_, p)| p.clone())
        .collect();

    (sessions, detached, emulators)
}

/// Parse "tty<TAB>title" lines from the emulator AppleScript into
/// (ttys0NN, title) pairs. Pure. Drops rows whose tty is not a ttys* device
/// and rows whose title is the emulator's own placeholder.
pub fn parse_tab_titles(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let Some((tty, title)) = line.split_once('\t') else {
            continue;
        };
        let tty = tty.trim().rsplit('/').next().unwrap_or("").to_string();
        let title = title.trim();
        if !is_ttys(&tty) || title.is_empty() || PLACEHOLDER_TITLES.contains(&title) {
            continue;
        }
        out.push((tty, title.to_string()));
    }
    out
}

/// Query tab titles from whichever emulators are actually running, reusing the
/// previous answer for TAB_TITLE_TTL. Any failure degrades to no titles; a TCC
/// denial (-1743) additionally returns a hint for the UI to show once.
///
/// Shells out (one batched osascript per emulator, 2s budget each).
fn cached_tab_titles(emulators: &[ProcInfo]) -> (HashMap<String, String>, Option<String>) {
    let mut cache = match TAB_TITLE_CACHE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some((at, titles, hint)) = cache.as_ref() {
        if at.elapsed() < TAB_TITLE_TTL {
            return (titles.clone(), hint.clone());
        }
    }

    let mut titles = HashMap::new();
    let mut hint = None;
    for (marker, script) in [
        ("Terminal.app/Contents/MacOS/Terminal", TERMINAL_SCPT),
        ("iTerm.app/Contents/MacOS/iTerm2", ITERM_SCPT),
    ] {
        if !emulators.iter().any(|p| p.command.contains(marker)) {
            continue;
        }
        let mut cmd = Command::new("osascript");
        for line in script {
            cmd.args(["-e", line]);
        }
        let Some((stdout, stderr)) = run_with_timeout(cmd, OSASCRIPT_TIMEOUT) else {
            hint = Some("tab titles unavailable: the emulator did not answer in time".to_string());
            continue;
        };
        if stderr.contains("-1743") {
            hint = Some(
                "tab titles need Automation access: approve this terminal under \
                 System Settings > Privacy & Security > Automation"
                    .to_string(),
            );
            continue;
        }
        titles.extend(parse_tab_titles(&stdout));
    }

    *cache = Some((Instant::now(), titles.clone(), hint.clone()));
    (titles, hint)
}

/// Shown in place of a cwd whose directory was deleted after the shell cd'd into it.
pub const DELETED_CWD: &str = "(folder deleted)";

/// Parse `lsof -a -d cwd -p p1,p2,...` stdout into (pid, cwd) pairs. Pure.
/// MUST be called on whatever stdout was produced even when lsof exited
/// non-zero (some pids may be unreadable while others still resolved).
pub fn parse_lsof_cwd(raw: &str) -> Vec<(i32, String)> {
    let mut out = Vec::new();
    for line in raw.lines() {
        // A shell whose cwd was deleted yields no path, only
        // "zsh 96110 demo cwd  cwd|rtd info error: No such file or directory".
        if line.contains("info error") {
            if let Some((f, _)) = split_fields(line, 4) {
                if let (Ok(pid), "cwd") = (f[1].parse::<i32>(), f[3]) {
                    out.push((pid, DELETED_CWD.to_string()));
                }
            }
            continue;
        }
        // COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
        let Some((f, name)) = split_fields(line, 8) else {
            continue;
        };
        if f[3] != "cwd" || name.is_empty() {
            continue;
        }
        if let Ok(pid) = f[1].parse::<i32>() {
            out.push((pid, name.to_string()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Private helpers (pure unless noted).

/// Physical footprint of `pid` in KB via proc_pid_rusage; `None` when the
/// kernel refuses (another user's process) or the pid is gone.
fn footprint_kb(pid: i32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: rusage_info_v2 is plain data and the kernel fills it for flavor V2.
    let rc = unsafe {
        libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, info.as_mut_ptr() as *mut libc::rusage_info_t)
    };
    (rc == 0).then(|| unsafe { info.assume_init() }.ri_phys_footprint / 1024)
}

/// Split off the first `n` whitespace-separated fields; return them plus the
/// trimmed rest of the line. `None` if fewer than `n` fields exist.
fn split_fields(line: &str, n: usize) -> Option<(Vec<&str>, &str)> {
    let mut fields = Vec::with_capacity(n);
    let mut rest = line;
    for _ in 0..n {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        fields.push(&rest[..end]);
        rest = &rest[end..];
    }
    Some((fields, rest.trim()))
}

/// True for "ttys003"-style controlling terminals (ttys[0-9]+).
fn is_ttys(tty: &str) -> bool {
    tty.strip_prefix("ttys")
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Basename of the command's first token, with the login-shell '-' stripped.
fn cmd_basename(command: &str) -> &str {
    let first = command.split_whitespace().next().unwrap_or("");
    let first = first.strip_prefix('-').unwrap_or(first);
    first.rsplit('/').next().unwrap_or(first)
}

fn is_shell(p: &ProcInfo) -> bool {
    SHELLS.contains(&cmd_basename(&p.command))
}

/// True for the `login -pfl <user>` wrapper (first token ends in "login").
fn is_login_wrapper(p: &ProcInfo) -> bool {
    p.command
        .split_whitespace()
        .next()
        .is_some_and(|t| t.ends_with("login"))
}

/// "'32-03:04:25' / '03:04:25' / '04:25' -> seconds"; 0 on parse failure.
fn etime_secs(s: &str) -> u64 {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().unwrap_or(0), r),
        None => (0, s),
    };
    let parts: Vec<u64> = rest.split(':').map(|p| p.parse().unwrap_or(0)).collect();
    let mut hms = [0u64; 3];
    for (slot, val) in hms
        .iter_mut()
        .rev()
        .zip(parts.iter().rev())
    {
        *slot = *val;
    }
    days * 86400 + hms[0] * 3600 + hms[1] * 60 + hms[2]
}

/// Build one Session from member indices into `procs`. Orders members in
/// tree order (parent before children, roots and siblings by pid) with
/// matching depths, and picks the interactive shell + recent command.
fn build_session(
    tty: String,
    members: &[usize],
    procs: &[ProcInfo],
    children: &HashMap<i32, Vec<i32>>,
    idx: &HashMap<i32, usize>,
) -> Session {
    let member_set: HashSet<i32> = members.iter().map(|&m| procs[m].pid).collect();

    // Tree order: DFS from each root (member whose parent is not a member).
    let mut roots: Vec<usize> = members
        .iter()
        .copied()
        .filter(|&m| !member_set.contains(&procs[m].ppid) || procs[m].ppid == procs[m].pid)
        .collect();
    roots.sort_by_key(|&m| procs[m].pid);
    let mut ordered: Vec<usize> = Vec::with_capacity(members.len());
    let mut depths: Vec<usize> = Vec::with_capacity(members.len());
    let mut placed = HashSet::new();
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&m| (m, 0)).collect();
    while let Some((m, d)) = stack.pop() {
        if !placed.insert(procs[m].pid) {
            continue;
        }
        ordered.push(m);
        depths.push(d);
        if let Some(kids) = children.get(&procs[m].pid) {
            for &k in kids.iter().rev() {
                if member_set.contains(&k) {
                    if let Some(&ki) = idx.get(&k) {
                        stack.push((ki, d + 1));
                    }
                }
            }
        }
    }
    // Any members missed by the DFS (shouldn't happen) go at the end.
    for &m in members {
        if placed.insert(procs[m].pid) {
            ordered.push(m);
            depths.push(0);
        }
    }

    // The interactive shell, not the root `login` wrapper it hangs off:
    // login runs as root, so its cwd is unreadable and its cmdline useless.
    let real_shells: Vec<usize> = ordered
        .iter()
        .copied()
        .filter(|&m| is_shell(&procs[m]) && !is_login_wrapper(&procs[m]))
        .collect();
    let shell_idx = if let Some(&best) = real_shells.iter().max_by_key(|&&m| {
        (etime_secs(&procs[m].etime), std::cmp::Reverse(procs[m].pid))
    }) {
        best
    } else {
        // Fall back to a session root, preferring shells then oldest.
        let mut cands: Vec<usize> = ordered
            .iter()
            .copied()
            .filter(|&m| !member_set.contains(&procs[m].ppid))
            .collect();
        if cands.is_empty() {
            cands = ordered.clone();
        }
        cands.sort_by_key(|&m| (is_shell(&procs[m]) as i32 * -1, std::cmp::Reverse(etime_secs(&procs[m].etime))));
        cands[0]
    };
    let shell_pid = procs[shell_idx].pid;
    let shell_name = cmd_basename(&procs[shell_idx].command).to_string();

    // Youngest non-shell process = what the tab is (or was last) running.
    let recent_command = ordered
        .iter()
        .copied()
        .filter(|&m| !is_shell(&procs[m]) && procs[m].pid != shell_pid)
        .min_by_key(|&m| (etime_secs(&procs[m].etime), procs[m].pid))
        .map(|m| procs[m].command.clone());

    let total_mem_kb: u64 = ordered.iter().map(|&m| procs[m].mem_kb).sum();
    let total_cpu_pct: f64 = ordered.iter().map(|&m| procs[m].cpu_pct).sum();
    let mut pgids: Vec<i32> = ordered
        .iter()
        .map(|&m| procs[m].pgid)
        .filter(|&g| g > 1)
        .collect();
    pgids.sort_unstable();
    pgids.dedup();

    Session {
        tty,
        shell_pid,
        shell_name,
        recent_command,
        cwd: "?".to_string(),
        total_mem_kb,
        delta_mem_kb: 0,
        total_cpu_pct,
        proc_count: ordered.len(),
        pgids,
        procs: ordered.iter().map(|&m| procs[m].clone()).collect(),
        depths,
        growing: false,
        claude: None,
        claude_mem_kb: 0,
        mcp_mem_kb: 0,
        scrollback_chars: None,
    }
}

/// Spawn `cmd`, read its stdout and stderr with a deadline, returning
/// `(stdout, stderr)`. On timeout the spawned child (our own subprocess, never
/// an arbitrary pid) is reaped and `None` is returned; the
/// partial-output-on-nonzero-exit case still yields whatever was produced.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Option<(String, String)> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;
    let (tx, rx) = mpsc::channel();
    let etx = tx.clone();
    // Both pipes must be drained concurrently or a chatty stderr can deadlock
    // a process whose stdout we are still waiting on.
    thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        let _ = etx.send((1, buf));
    });
    thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send((0, buf));
    });
    let deadline = Instant::now() + timeout;
    let mut got = [None, None];
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((which, buf)) => got[which] = Some(buf),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    // lsof exits non-zero when any pid is unreadable; keep stdout anyway.
    let _ = child.wait();
    Some((
        got[0].take().unwrap_or_default(),
        got[1].take().unwrap_or_default(),
    ))
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Canned ps output covering: a launchd root, a Terminal.app emulator,
    /// a normal ttys003 session behind a `login` wrapper with a "??" child
    /// attributed via ancestry, a bare ttys005 session with no login
    /// wrapper, and a detached launchd-reparented node subtree.
    const PS: &str = "\
    1     0     1 ??          10000  0.0 10-00:00:00 /sbin/launchd
  300     1   300 ??          20000  0.3 05:00:00 /System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal
  400   300   400 ttys003      8000  0.0 04:00:00 login -pfl demo /usr/bin/false
  401   400   401 ttys003      9000  0.1 03:59:00 -zsh
  402   401   402 ttys003    120000 12.0 00:05:00 node server.js
  403   402   402 ??          30000  1.0 00:04:30 node worker.js
  500     1   500 ??          80000  2.0 01:00:00 node /Users/demo/.claude/cli.js
  501   500   500 ??          15000  0.5 00:30:00 node child-helper.js
  600     1   600 ttys005      7000  0.0 00:02:00 zsh
garbage line without enough numeric fields
  700     1   700 ??           1000  0.0 00:01 tiny-orphan";

    #[test]
    fn parse_ps_basics() {
        let procs = parse_ps(PS);
        // Malformed line skipped, 10 good rows kept.
        assert_eq!(procs.len(), 10);
        let p401 = procs.iter().find(|p| p.pid == 401).unwrap();
        assert_eq!(p401.ppid, 400);
        assert_eq!(p401.pgid, 401);
        assert_eq!(p401.tty.as_deref(), Some("ttys003"));
        assert_eq!(p401.mem_kb, 9000);
        assert!((p401.cpu_pct - 0.1).abs() < 1e-9);
        assert_eq!(p401.etime, "03:59:00");
        assert_eq!(p401.command, "-zsh");
        // "??" becomes None; command keeps embedded spaces.
        let p403 = procs.iter().find(|p| p.pid == 403).unwrap();
        assert_eq!(p403.tty, None);
        assert_eq!(p403.command, "node worker.js");
        // Multi-day etime survives verbatim.
        assert_eq!(procs.iter().find(|p| p.pid == 1).unwrap().etime, "10-00:00:00");
    }

    #[test]
    fn groups_normal_session_with_tty_ancestry_attribution() {
        let (sessions, _, _) = group_sessions(parse_ps(PS));
        assert_eq!(sessions.len(), 2);
        // Sorted by RSS desc: ttys003 (167000) before ttys005 (7000).
        let s = &sessions[0];
        assert_eq!(s.tty, "ttys003");
        // "??" pid 403 attributed via ancestry 403 -> 402 (ttys003).
        let pids: Vec<i32> = s.procs.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![400, 401, 402, 403]); // tree order
        assert_eq!(s.depths, vec![0, 1, 2, 3]);
        assert_eq!(s.total_mem_kb, 8000 + 9000 + 120000 + 30000);
        assert_eq!(s.proc_count, 4);
        assert_eq!(s.pgids, vec![400, 401, 402]);
        assert_eq!(s.recent_command.as_deref(), Some("node worker.js"));
        assert_eq!(s.cwd, "?");
        assert_eq!(s.delta_mem_kb, 0);
        assert!(!s.growing);
    }

    #[test]
    fn login_wrapper_yields_interactive_shell() {
        let (sessions, _, _) = group_sessions(parse_ps(PS));
        let s = sessions.iter().find(|s| s.tty == "ttys003").unwrap();
        // Not the root `login -pfl demo` (pid 400) but the zsh beneath it.
        assert_eq!(s.shell_pid, 401);
        assert_eq!(s.shell_name, "zsh");
        // Session without a login wrapper: root shell reported directly.
        let s5 = sessions.iter().find(|s| s.tty == "ttys005").unwrap();
        assert_eq!(s5.shell_pid, 600);
        assert_eq!(s5.shell_name, "zsh");
        assert_eq!(s5.recent_command, None);
    }

    #[test]
    fn detached_launchd_subtree_and_emulators() {
        let (_, detached, emulators) = group_sessions(parse_ps(PS));
        // pid 500 subtree (80000 + 15000 KB >= 50 MB) is detached;
        // pid 700 (1000 KB, not a dev tool anyway) is not.
        assert_eq!(detached.len(), 1);
        let d = &detached[0];
        assert_eq!(d.tty, "pid:500");
        assert_eq!(d.shell_pid, 500);
        assert_eq!(d.total_mem_kb, 95000);
        assert_eq!(d.proc_count, 2);
        assert_eq!(d.pgids, vec![500]);
        assert_eq!(d.depths, vec![0, 1]);
        // Terminal.app matched by command marker, not claimed by a session.
        assert_eq!(emulators.len(), 1);
        assert_eq!(emulators[0].pid, 300);
    }

    #[test]
    fn lsof_cwd_parses_partial_output() {
        // Typical table output; login's cwd is unreadable so lsof exits
        // non-zero, but the zsh row still resolves and MUST be kept.
        let raw = "\
COMMAND  PID USER   FD   TYPE DEVICE SIZE/OFF   NODE NAME
zsh      401 demo  cwd    DIR    1,4      640 123456 /Users/demo/dev/my project
zsh      600 demo  cwd    DIR    1,4      320 654321 /Users/demo
zsh      600 demo  txt    REG    1,4     1000 111111 /bin/zsh
zsh      700 demo  cwd                             cwd|rtd info error: No such file or directory";
        let cwds = parse_lsof_cwd(raw);
        assert_eq!(
            cwds,
            vec![
                (401, "/Users/demo/dev/my project".to_string()),
                (600, "/Users/demo".to_string()),
                (700, DELETED_CWD.to_string()),
            ]
        );
    }

    #[test]
    fn diff_snapshots_sets_delta_and_growing() {
        let (sessions, detached, emulators) = group_sessions(parse_ps(PS));
        let mk = |sessions: Vec<Session>, detached: Vec<Session>| Snapshot {
            sessions,
            detached,
            emulators: emulators.clone(),
            total_mem_kb: 0,
            phys_mem_bytes: 0,
            proc_count: 0,
            tab_titles: HashMap::new(),
            tab_title_hint: None,
            taken_at: Instant::now(),
            system: Default::default(),
            mcp: Vec::new(),
        };
        let prev = mk(sessions.clone(), detached.clone());
        let mut cur = mk(sessions, detached);
        cur.sessions[0].total_mem_kb += GROWING_THRESHOLD_KB; // grew 50 MB
        cur.detached[0].total_mem_kb -= 1000;
        diff_snapshots(&mut cur, &prev);
        assert_eq!(cur.sessions[0].delta_mem_kb, GROWING_THRESHOLD_KB as i64);
        assert!(cur.sessions[0].growing);
        assert_eq!(cur.sessions[1].delta_mem_kb, 0);
        assert!(!cur.sessions[1].growing);
        assert_eq!(cur.detached[0].delta_mem_kb, -1000);
        assert!(!cur.detached[0].growing);
    }

    #[test]
    fn tab_titles_parse_and_filter() {
        let raw = "/dev/ttys001\t\u{2733} Build the thing\n\
                   /dev/ttys018\tTerminal\n\
                   /dev/ttys002\t\n\
                   not a tty line\n\
                   /dev/ttyp9\tsomething\n\
                   /dev/ttys034\ttitle with\ttab inside";
        assert_eq!(
            parse_tab_titles(raw),
            vec![
                ("ttys001".to_string(), "\u{2733} Build the thing".to_string()),
                // Untitled ("Terminal"), empty, non-ttys and junk rows dropped;
                // a title containing a tab keeps everything after the first one.
                ("ttys034".to_string(), "title with\ttab inside".to_string()),
            ]
        );
    }

    #[test]
    fn etime_parsing() {
        assert_eq!(etime_secs("04:25"), 4 * 60 + 25);
        assert_eq!(etime_secs("03:04:25"), 3 * 3600 + 4 * 60 + 25);
        assert_eq!(etime_secs("2-03:45:12"), 2 * 86400 + 3 * 3600 + 45 * 60 + 12);
    }

    #[test]
    fn mcp_names() {
        assert_eq!(mcp_name("npm exec @acme/weather-mcp").as_deref(), Some("weather-mcp"));
        assert_eq!(mcp_name("node /x/node_modules/.bin/weather-mcp").as_deref(), Some("weather-mcp"));
        assert_eq!(mcp_name("/v/bin/python3 /u/.local/bin/notes-tool mcp").as_deref(), Some("notes-tool mcp"));
        assert_eq!(mcp_name("/A/Editor.app/x/mcp-server-darwin-arm64 --ws-port 1").as_deref(), Some("mcp-server-darwin-arm64"));
        assert_eq!(mcp_name("uv tool uvx --with mcp[cli]<2.0.0 blender-mcp").as_deref(), Some("blender-mcp"));
        assert_eq!(mcp_name("node server.js --mcp-port 3"), None);
        assert_eq!(mcp_name("zsh"), None);
        assert_eq!(mcp_name("grep -iE mcp|x"), None);
        assert_eq!(mcp_name("osascript -e ^MCP"), None);
        assert_eq!(mcp_name("python3 -c fn mcp_name(command: &str)"), None);
    }

    #[test]
    fn mcp_groups_count_tabs_and_orphans() {
        let p = |pid, ppid, cmd: &str, kb| ProcInfo {
            pid, ppid, pgid: pid, tty: None, mem_kb: kb, cpu_pct: 0.0, etime: "1".into(), command: cmd.into(),
        };
        let seed = vec![p(1, 0, "zsh", 0)];
        let idx = HashMap::from([(1, 0)]);
        let mut a = build_session("ttys001".into(), &[0], &seed, &HashMap::new(), &idx);
        a.procs = vec![p(10, 9, "npm exec @x/foo-mcp", 100), p(11, 10, "node /b/foo-mcp", 200)];
        let mut b = a.clone();
        b.tty = "ttys002".into();
        b.procs = vec![p(20, 19, "node /b/foo-mcp", 300)];
        let all = vec![p(30, 1, "node /b/foo-mcp", 50), p(31, 500, "zsh", 1)];
        let g = mcp_groups(&[a, b], &all);
        assert_eq!(g.len(), 1);
        assert_eq!((g[0].procs, g[0].tabs, g[0].mem_kb, g[0].orphaned, g[0].orphaned_mem_kb), (4, 2, 650, 1, 50));
    }

    #[test]
    fn subtree_mem_sums_descendants_only() {
        let p = |kb| ProcInfo { pid: 0, ppid: 0, pgid: 0, tty: None, mem_kb: kb, cpu_pct: 0.0, etime: String::new(), command: String::new() };
        let procs = vec![p(1), p(10), p(100), p(1000), p(5)];
        let depths = vec![0, 1, 2, 2, 1];
        assert_eq!(subtree_mem(&procs, &depths, 1), 1110);
        assert_eq!(subtree_mem(&procs, &depths, 4), 5);
    }

    #[test]
    fn system_parsers() {
        assert_eq!(
            parse_swapusage("total = 28672.00M  used = 27691.50M  free = 980.50M  (encrypted)"),
            (28672 << 20, (27691.5 * 1048576.0) as u64)
        );
        assert_eq!(parse_boottime("{ sec = 1790302637, usec = 885563 } Thu Sep 24"), Some(1790302637));
        let ioreg = r#"+-o AppleSDXCBlockStorageDevice  <class AppleSDXCBlockStorageDevice, id 1>
  +-o IOBlockStorageDriver  <class IOBlockStorageDriver, id 2>
        "Statistics" = {"Bytes (Write)"=111,"Operations (Write)"=1}
+-o NS_01@1  <class IOEmbeddedNVMeBlockDevice, id 3>
  +-o IOBlockStorageDriver  <class IOBlockStorageDriver, id 4>
        "Statistics" = {"Operations (Write)"=9,"Bytes (Write)"=23802787205120}"#;
        assert_eq!(parse_internal_ssd_writes(ioreg), Some(23802787205120));
        let s = System { ssd_written_bytes: Some(86_400), uptime_secs: 43_200, ..Default::default() };
        assert_eq!(s.ssd_bytes_per_day(), Some(172_800));
    }

}
