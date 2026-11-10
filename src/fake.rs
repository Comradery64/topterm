//! Hard-coded synthetic snapshot behind the hidden `--fake-data` flag.
//!
//! It exists so the README sample block and the TUI screenshot in `assets/`
//! can be regenerated from real `topterm` output without publishing the
//! author's actual tab titles, project names and paths. Nothing here shells
//! out; `snapshot(step)` is a pure function of `step`.

use std::collections::HashMap;

use crate::collector::{ProcInfo, Session, Snapshot};

/// Number of samples the fake timeline is made of — enough to fill the UI's
/// history ring, so the header sparkline and the detail chart draw a full
/// curve instead of a few dots at the right edge. Feeding all of them to
/// `App::on_snapshot` in order replays the timeline.
pub const STEPS: usize = 120;

/// Shape of one session's memory over the fake timeline.
#[derive(Clone, Copy)]
enum Curve {
    /// A leak: a long climb, then a steep allocation burst at the end.
    Leak,
    /// Steady state: a slow wobble small enough that even the largest of
    /// these sessions never trips the GROWING threshold.
    Flat(f64),
}

impl Curve {
    fn at(self, step: usize) -> f64 {
        let t = step.min(STEPS - 1) as f64 / (STEPS - 1) as f64;
        match self {
            Curve::Leak => {
                let tail = if t > 0.92 { 0.15 * ((t - 0.92) / 0.08).powi(2) } else { 0.0 };
                0.30 + 0.55 * t.powf(1.25) + tail
            }
            Curve::Flat(phase) => {
                1.0 + 0.006 * (t * std::f64::consts::TAU * 6.5 + phase).sin()
            }
        }
    }
}

fn at(base: u64, step: usize, curve: Curve) -> u64 {
    (base as f64 * curve.at(step)) as u64
}

/// Value at `step` and the change since the previous step.
fn ramp(base: u64, step: usize, curve: Curve) -> (u64, i64) {
    let cur = at(base, step, curve);
    let prev = if step == 0 { cur } else { at(base, step - 1, curve) };
    (cur, cur as i64 - prev as i64)
}

const MB: u64 = 1024;
const GB: u64 = 1024 * 1024;

struct Proc<'a> {
    pid: i32,
    ppid: i32,
    depth: usize,
    mem_kb: u64,
    cpu_pct: f64,
    etime: &'a str,
    command: &'a str,
}

/// Build one session from a flat parent-before-child listing.
fn session(
    tty: &str,
    shell_name: &str,
    recent: Option<&str>,
    cwd: &str,
    base_mem_kb: u64,
    step: usize,
    curve: Curve,
    cpu: f64,
    procs: &[Proc<'_>],
) -> Session {
    let (total_mem_kb, delta_mem_kb) = ramp(base_mem_kb, step, curve);
    // Scale the per-process RSS by the same factor so the tree adds up to the
    // session total the header row shows.
    let scale = total_mem_kb as f64 / base_mem_kb.max(1) as f64;
    let tty_field = if tty.starts_with("pid:") { None } else { Some(tty.to_string()) };
    let mut pgids: Vec<i32> = Vec::new();
    let mut out = Vec::with_capacity(procs.len());
    let mut depths = Vec::with_capacity(procs.len());
    for p in procs {
        if !pgids.contains(&p.pid) && p.depth < 2 {
            pgids.push(p.pid);
        }
        out.push(ProcInfo {
            pid: p.pid,
            ppid: p.ppid,
            pgid: if p.depth == 0 { p.pid } else { procs[0].pid },
            tty: tty_field.clone(),
            mem_kb: (p.mem_kb as f64 * scale) as u64,
            cpu_pct: p.cpu_pct,
            etime: p.etime.to_string(),
            command: p.command.to_string(),
        });
        depths.push(p.depth);
    }
    Session {
        tty: tty.to_string(),
        shell_pid: procs[0].pid,
        shell_name: shell_name.to_string(),
        recent_command: recent.map(str::to_string),
        cwd: cwd.to_string(),
        total_mem_kb,
        delta_mem_kb,
        total_cpu_pct: cpu,
        proc_count: procs.len(),
        pgids,
        procs: out,
        depths,
        growing: false,
        claude: None,
        claude_mem_kb: 0,
        mcp_mem_kb: 0,
        scrollback_chars: None,
    }
}

/// One sample of the fictional machine. `step` walks the timeline from 0 to
/// `STEPS - 1`; the last step is what a single `--fake-data` run reports.
pub fn snapshot(step: usize) -> Snapshot {
    let leak = {
        let mut s = session(
            "ttys004",
            "zsh",
            Some("python train.py --epochs 50"),
            "/Users/demo/dev/vision-lab",
            6360 * MB,
            step,
            Curve::Leak,
            188.4,
            &[
                Proc { pid: 24118, ppid: 24110, depth: 0, mem_kb: 44 * MB, cpu_pct: 0.0, etime: "04:12:07", command: "-zsh" },
                Proc { pid: 24390, ppid: 24118, depth: 1, mem_kb: 4820 * MB, cpu_pct: 121.6, etime: "02:47:31", command: "python train.py --epochs 50 --batch-size 64 --checkpoint runs/042" },
                Proc { pid: 24404, ppid: 24390, depth: 2, mem_kb: 748 * MB, cpu_pct: 33.9, etime: "02:47:29", command: "python -c from multiprocessing.spawn import spawn_main; spawn_main(tracker_fd=7)" },
                Proc { pid: 24405, ppid: 24390, depth: 2, mem_kb: 748 * MB, cpu_pct: 32.9, etime: "02:47:29", command: "python -c from multiprocessing.spawn import spawn_main; spawn_main(tracker_fd=9)" },
            ],
        );
        s.growing = true;
        s
    };

    let api = session(
        "ttys001",
        "zsh",
        Some("node server.js --watch"),
        "/Users/demo/dev/api-server",
        1416 * MB,
        step,
        Curve::Flat(0.0),
        24.7,
        &[
            Proc { pid: 10432, ppid: 10428, depth: 0, mem_kb: 41 * MB, cpu_pct: 0.0, etime: "1-06:18:44", command: "-zsh" },
            Proc { pid: 11907, ppid: 10432, depth: 1, mem_kb: 963 * MB, cpu_pct: 18.2, etime: "05:31:12", command: "node server.js --watch --port 8080" },
            Proc { pid: 11930, ppid: 11907, depth: 2, mem_kb: 268 * MB, cpu_pct: 4.1, etime: "05:31:10", command: "node /Users/demo/dev/api-server/node_modules/.bin/tsc --watch --preserveWatchOutput" },
            Proc { pid: 11944, ppid: 11907, depth: 2, mem_kb: 144 * MB, cpu_pct: 2.4, etime: "05:31:10", command: "esbuild --service=0.21.5 --ping" },
        ],
    );

    let docker = session(
        "ttys007",
        "zsh",
        Some("docker compose up"),
        "/Users/demo/dev/edgecast",
        822 * MB,
        step,
        Curve::Flat(1.7),
        9.3,
        &[
            Proc { pid: 30211, ppid: 30205, depth: 0, mem_kb: 39 * MB, cpu_pct: 0.0, etime: "22:40:02", command: "-zsh" },
            Proc { pid: 31877, ppid: 30211, depth: 1, mem_kb: 512 * MB, cpu_pct: 6.8, etime: "03:02:55", command: "docker compose up" },
            Proc { pid: 31902, ppid: 31877, depth: 2, mem_kb: 271 * MB, cpu_pct: 2.5, etime: "03:02:54", command: "com.docker.backend --watchdog" },
        ],
    );

    let vite = session(
        "ttys012",
        "zsh",
        Some("vite dev server"),
        "/Users/demo/dev/storefront",
        514 * MB,
        step,
        Curve::Flat(3.1),
        5.1,
        &[
            Proc { pid: 40190, ppid: 40186, depth: 0, mem_kb: 38 * MB, cpu_pct: 0.0, etime: "08:09:33", command: "-zsh" },
            Proc { pid: 41562, ppid: 40190, depth: 1, mem_kb: 476 * MB, cpu_pct: 5.1, etime: "01:14:06", command: "node /Users/demo/dev/storefront/node_modules/.bin/vite --host" },
        ],
    );

    // No tab title for this one on purpose: the TAB column renders "-".
    let idle = session(
        "ttys019",
        "zsh",
        None,
        "/Users/demo",
        92 * MB,
        step,
        Curve::Flat(4.4),
        0.2,
        &[
            Proc { pid: 52007, ppid: 52001, depth: 0, mem_kb: 40 * MB, cpu_pct: 0.0, etime: "00:41:18", command: "-zsh" },
            Proc { pid: 52344, ppid: 52007, depth: 1, mem_kb: 52 * MB, cpu_pct: 0.2, etime: "00:12:47", command: "ssh deploy@staging.example.internal" },
        ],
    );

    let orphan = session(
        "pid:6188",
        "node",
        None,
        "/Users/demo/dev/storefront",
        648 * MB,
        step,
        Curve::Flat(5.6),
        3.6,
        &[Proc {
            pid: 6188,
            ppid: 1,
            depth: 0,
            mem_kb: 648 * MB,
            cpu_pct: 3.6,
            etime: "2-11:26:40",
            command: "node /Users/demo/dev/storefront/node_modules/next/dist/server/next-server.js --port 3000",
        }],
    );

    // Claude Code sessions: one busy, one recently idle, two long idle
    // (what `Z` would hibernate). Each runs its own MCP servers.
    let claude_tab = |tty: &str, cwd: &str, base: u64, pid: i32, cpu: f64, status: &str, idle_ms: u64, name: &str| {
        let mut s = session(
            tty,
            "zsh",
            Some("claude"),
            cwd,
            base,
            step,
            Curve::Flat(0.6),
            cpu,
            &[
                Proc { pid: pid - 6, ppid: pid - 10, depth: 0, mem_kb: 38 * MB, cpu_pct: 0.0, etime: "2-01:10:00", command: "-zsh" },
                Proc { pid, ppid: pid - 6, depth: 1, mem_kb: base - 260 * MB, cpu_pct: cpu, etime: "2-01:09:41", command: "claude" },
                Proc { pid: pid + 1, ppid: pid, depth: 2, mem_kb: 96 * MB, cpu_pct: 0.0, etime: "2-01:09:39", command: "npm exec @acme/weather-mcp" },
                Proc { pid: pid + 2, ppid: pid + 1, depth: 3, mem_kb: 74 * MB, cpu_pct: 0.0, etime: "2-01:09:38", command: "node /Users/demo/.npm/_npx/1a2b/node_modules/.bin/weather-mcp" },
                Proc { pid: pid + 3, ppid: pid, depth: 2, mem_kb: 52 * MB, cpu_pct: 0.0, etime: "2-01:09:39", command: "python3 /Users/demo/.local/bin/notes-tool mcp" },
            ],
        );
        s.claude_mem_kb = s.total_mem_kb.saturating_sub(38 * MB);
        s.mcp_mem_kb = 222 * MB;
        s.claude = Some(crate::claude::Live {
            pid,
            session_id: format!("{pid:08}-0000-4000-8000-000000000000"),
            name: Some(name.to_string()),
            status: status.to_string(),
            status_since_ms: crate::app::now_ms().saturating_sub(idle_ms),
            cmd: "claude".to_string(),
        });
        s
    };
    const H: u64 = 3_600_000;
    let busy = claude_tab("ttys021", "/Users/demo/dev/api-server", 1180 * MB, 61020, 42.3, "busy", 0, "auth-refactor");
    let recent = claude_tab("ttys023", "/Users/demo/dev/storefront", 780 * MB, 62030, 0.4, "idle", 2 * H + 14 * 60_000, "checkout-copy");
    let stale = claude_tab("ttys026", "/Users/demo/dev/vision-lab", 690 * MB, 63040, 0.1, "idle", 9 * 24 * H + 5 * H, "dataset-cleanup");
    let staler = claude_tab("ttys031", "/Users/demo/dev/edgecast", 640 * MB, 64050, 0.1, "idle", 16 * 24 * H + 2 * H, "cdn-headers");

    let emulator = ProcInfo {
        pid: 812,
        ppid: 1,
        pgid: 812,
        tty: None,
        mem_kb: at(1180 * MB, step, Curve::Flat(2.2)),
        cpu_pct: 1.9,
        etime: "3-02:55:19".to_string(),
        command: "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal".to_string(),
    };

    let sessions = vec![leak, api, busy, docker, recent, stale, staler, vite, idle];
    let detached = vec![orphan];
    let all: Vec<ProcInfo> = sessions.iter().chain(&detached).flat_map(|s| s.procs.clone()).collect();
    let mcp = crate::collector::mcp_groups(&sessions, &all);

    Snapshot {
        total_mem_kb: at(12_680 * MB, step, Curve::Flat(0.8))
            + sessions.first().map(|s| s.total_mem_kb).unwrap_or(0),
        sessions,
        detached,
        emulators: vec![emulator],
        phys_mem_bytes: 16 * GB * 1024,
        proc_count: 702,
        tab_titles: HashMap::from([
            ("ttys004".to_string(), "ml-training run 42".to_string()),
            ("ttys001".to_string(), "api-server (dev)".to_string()),
            ("ttys007".to_string(), "docker compose up".to_string()),
            ("ttys012".to_string(), "vite dev server".to_string()),
            ("ttys021".to_string(), "\u{2733} Refactor auth middleware".to_string()),
            ("ttys023".to_string(), "\u{2733} Checkout copy tweaks".to_string()),
            ("ttys026".to_string(), "\u{2733} Dataset dedupe script".to_string()),
            ("ttys031".to_string(), "\u{2733} CDN cache headers".to_string()),
        ]),
        tab_title_hint: None,
        system: crate::collector::System {
            swap_used_bytes: 9_400 * MB * 1024,
            swap_total_bytes: 10_240 * MB * 1024,
            ssd_written_bytes: Some(9_100 * GB * 1024),
            uptime_secs: 21 * 86_400 + 4 * 3_600,
        },
        mcp,
    }
}

/// A hibernate plan for one of the fake Claude tabs, for screenshots.
/// Synthetic conversations by tty, for the search screenshot.
pub fn chats() -> Vec<(String, Vec<(bool, String)>)> {
    let m = |u: bool, t: &str| (u, t.to_string());
    vec![
        (
            "ttys026".into(),
            vec![
                m(true, "the training set has a lot of repeated images, can we clean it up?"),
                m(false, "Yes. There are two kinds: exact copies (same bytes) and near-duplicates (resized or re-encoded). Exact copies are easy with sha256. For near-duplicates I'd use a perceptual hash (phash) and treat a Hamming distance of 6 or less as the same image."),
                m(true, "do both, but put the phash matches in a review list instead of deleting them"),
                m(false, "Done. dedupe.py now drops exact repeats and writes phash near-matches to review/near_dupes.csv with both file paths and the distance.\nNothing on the near-match list is deleted; you approve those by hand."),
                m(true, "ok run it on the sample shard first and show me the counts"),
                m(false, "On the sample shard it removed 18,402 of 240,113 rows as exact repeats and flagged 3,117 phash pairs for review."),
            ],
        ),
        (
            "ttys031".into(),
            vec![
                m(true, "why are the product images re-downloading every visit?"),
                m(false, "The CDN sends Cache-Control: no-store on /img. I changed it to max-age=31536000, immutable; filenames already carry a content hash (phash-style suffix), so stale images aren't a risk."),
            ],
        ),
    ]
}

pub fn plan(tty: &str) -> Option<crate::hibernate::Plan> {
    let snap = snapshot(STEPS - 1);
    let sess = snap.sessions.iter().find(|s| s.tty == tty)?;
    let live = sess.claude.clone()?;
    Some(crate::hibernate::Plan {
        tty: sess.tty.clone(),
        title: snap.tab_titles.get(tty).cloned().unwrap_or_default(),
        cwd: sess.cwd.clone(),
        frees_kb: sess.claude_mem_kb,
        live,
        screen: [
            "\u{23fa} The dedupe script now hashes rows on (source_url, sha256) and drops",
            "  exact repeats. On the sample shard it removed 18,402 of 240,113 rows.",
            "",
            "  Before I run it on the full dataset (~41 GB), confirm two things:",
            "  1. Keep the earliest copy of each duplicate, or the newest?",
            "  2. Write to a new folder (data/clean-v2) or overwrite in place?",
        ]
        .iter()
        .map(|l| l.to_string())
        .collect(),
        last_prompt: Some("ok run it on the sample shard first and show me the counts".into()),
        last_reply: Some("The dedupe script now hashes rows on (source_url, sha256) and drops exact repeats. On the sample shard it removed 18,402 of 240,113 rows.".into()),
        transcript: None,
        summary: Some(crate::hibernate::Summary {
            source: crate::hibernate::Source::Claude,
            text: "Deduplicating the vision-lab dataset: the script works on a sample shard (18,402 duplicates removed). Waiting on you to choose which copy to keep and where to write the full run.".into(),
            age_ms: 9 * 86_400_000 + 5 * 3_600_000,
            messages_since: 0,
        }),
        blockers: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the fake path: nothing from a real machine can reach
    /// the README or the screenshot through it. The invariant is positive — the
    /// only home directory the fake data may mention is `/Users/demo`, and no
    /// path may point at a mounted volume — so it stays meaningful without
    /// naming anything private.
    #[test]
    fn fake_snapshot_only_references_the_demo_home() {
        const DEMO_HOME: &str = "/Users/demo";

        let mut text = String::new();
        for step in 0..STEPS {
            let snap = snapshot(step);
            for s in snap.sessions.iter().chain(snap.detached.iter()) {
                assert!(
                    s.cwd == "?" || s.cwd.starts_with(DEMO_HOME),
                    "fake session cwd outside the demo home: {:?}",
                    s.cwd
                );
                text.push_str(&format!("{s:?}"));
            }
            for p in &snap.emulators {
                text.push_str(&format!("{p:?}"));
            }
            for (k, v) in &snap.tab_titles {
                text.push_str(k);
                text.push_str(v);
            }
        }

        // Every absolute path the fake data mentions must sit under the demo
        // home or a stock system prefix. Anything else — another user's home, a
        // mounted volume, an external disk — fails, without this test having to
        // name the thing it is guarding against.
        const ALLOWED_ROOTS: &[&str] = &[
            DEMO_HOME,
            "/System/",
            "/Applications/",
            "/Library/",
            "/usr/",
            "/bin/",
            "/sbin/",
            "/opt/",
            "/private/",
            "/tmp/",
            "/var/",
            "/dev/",
        ];

        for token in text.split_whitespace() {
            let path = token.trim_matches(|c: char| !c.is_ascii_graphic() || "\"',:{}()[]".contains(c));
            if !path.starts_with('/') {
                continue;
            }
            assert!(
                ALLOWED_ROOTS.iter().any(|root| path.starts_with(root)),
                "fake snapshot references a path outside {DEMO_HOME} and the system prefixes: {path:?}"
            );
        }
    }

    #[test]
    fn leaking_session_climbs_across_the_timeline() {
        let series: Vec<u64> = (0..STEPS).map(|s| snapshot(s).sessions[0].total_mem_kb).collect();
        assert!(series.windows(2).all(|w| w[1] > w[0]), "{series:?}");
        // Sessions must stay sorted by RSS descending, like the collector emits.
        let last = snapshot(STEPS - 1);
        assert!(last.sessions.windows(2).all(|w| w[0].total_mem_kb >= w[1].total_mem_kb));
    }
}
