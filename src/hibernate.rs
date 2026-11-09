//! Hibernate one idle Claude Code session: exit Claude but leave its tab open,
//! with a card printed in the tab saying what it was and how to resume it.
//!
//! Split in two so the TUI can show exactly what will happen first:
//! `plan()` only reads (tab position, on-screen text, last prompt/reply,
//! cached summary, child processes), and `execute()` re-checks the plan
//! against the live system, then asks Claude to exit and prints the card.
//! It never closes a tab, clears scrollback, or acts on a busy session.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::claude::{self, Live};
use crate::collector::{mcp_name, run_with_timeout, ProcInfo, Session};

const OSA_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for Claude to exit after being asked.
const EXIT_WAIT: Duration = Duration::from_secs(15);
/// Lines of the tab's screen shown in the preview.
pub const PREVIEW_LINES: usize = 10;

/// Children of a Claude process that are part of Claude itself and safe to
/// end with it: MCP servers, language servers, and its sleep blocker.
fn is_helper(p: &ProcInfo) -> bool {
    let base = p.command.split_whitespace().next().unwrap_or("").rsplit('/').next().unwrap_or("");
    mcp_name(&p.command).is_some()
        || matches!(base, "caffeinate" | "clangd" | "rust-analyzer" | "tsserver" | "gopls" | "pyright")
        || base.ends_with("-lsp")
        || base.contains("language-server")
        || (matches!(base, "npm" | "node" | "npx" | "uv" | "uvx") && p.command.to_ascii_lowercase().contains("mcp"))
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub text: String,
    /// Age of the cached summary in milliseconds.
    pub age_ms: u64,
    /// Chat messages added to the transcript since the summary was made.
    pub messages_since: usize,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub tty: String,
    pub title: String,
    pub cwd: String,
    pub live: Live,
    pub frees_kb: u64,
    pub screen: Vec<String>,
    pub last_prompt: Option<String>,
    pub last_reply: Option<String>,
    pub transcript: Option<PathBuf>,
    pub summary: Option<Summary>,
    /// Non-helper processes under Claude (a dev server, a build, a
    /// background task). Hibernating would end them, so the plan is blocked.
    pub blockers: Vec<String>,
}

impl Plan {
    pub fn resume_cmd(&self) -> String {
        format!("{} --resume {}", self.live.cmd, self.live.session_id)
    }

    /// Why this plan cannot run, if anything.
    pub fn blocked_reason(&self) -> Option<String> {
        if !self.live.is_idle() {
            return Some("Claude is busy in this tab.".into());
        }
        if !self.blockers.is_empty() {
            return Some(format!("Claude is running other processes: {}", self.blockers.join(", ")));
        }
        None
    }
}

/// Build a hibernate plan for a session. Read-only.
pub fn plan(sess: &Session, title: &str) -> Result<Plan, String> {
    let live = sess.claude.clone().ok_or("No Claude Code session is running in this tab.")?;
    let ci = sess.procs.iter().position(|p| p.pid == live.pid).ok_or("Claude process not found.")?;
    let d = sess.depths[ci];
    let blockers: Vec<String> = sess.procs[ci + 1..]
        .iter()
        .zip(&sess.depths[ci + 1..])
        .take_while(|(_, &dd)| dd > d)
        .filter(|(p, _)| !is_helper(p))
        .map(|(p, _)| format!("{} (pid {})", crate::app::short_cmd(&p.command, 40), p.pid))
        .collect();

    let screen = tab_screen(&sess.tty).unwrap_or_default();

    let home = claude::home();
    let variants = claude::variants(&home);
    let transcript = claude::find_transcripts(&variants, std::slice::from_ref(&live.session_id))
        .remove(&live.session_id)
        .map(|(_, p)| p);
    let (last_prompt, last_reply) = transcript
        .as_deref()
        .and_then(|p| claude::read_tail(p, 512 * 1024))
        .map(|t| claude::last_exchange(&t))
        .unwrap_or((None, None));
    let summary = transcript.as_deref().and_then(|p| cached_summary(p, &live.session_id));

    Ok(Plan {
        tty: sess.tty.clone(),
        title: title.to_string(),
        cwd: sess.cwd.clone(),
        frees_kb: sess.claude_mem_kb,
        live,
        screen,
        last_prompt,
        last_reply,
        transcript,
        summary,
        blockers,
    })
}

/// The newest cached summary for a session, however stale, with its age and
/// how many chat messages arrived after it.
fn cached_summary(transcript: &Path, id: &str) -> Option<Summary> {
    let dir = crate::index::summary_cache_dir();
    let prefix = format!("{id}-");
    let newest = fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())?;
    let name = newest.file_name().to_string_lossy().into_owned();
    let size_then: u64 = name.strip_prefix(&prefix)?.strip_suffix(".txt")?.parse().ok()?;
    let text = fs::read_to_string(newest.path()).ok()?;
    let age_ms = newest
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map_or(0, |d| d.as_millis() as u64);
    let len = fs::metadata(transcript).map(|m| m.len()).unwrap_or(size_then);
    let messages_since = if len > size_then {
        claude::read_tail(transcript, len - size_then)
            .map(|t| {
                t.lines()
                    .filter(|l| l.contains("\"type\":\"user\"") || l.contains("\"type\":\"assistant\""))
                    .count()
            })
            .unwrap_or(0)
    } else {
        0
    };
    Some(Summary { text, age_ms, messages_since })
}

/// The last PREVIEW_LINES non-blank screen lines of the Terminal.app tab on
/// `tty`. Reads `contents`, which is only what is
/// on screen, not the scrollback. Inside AppleScript `contents of x` first
/// dereferences the loop variable, hence `contents of (contents of t)`.
fn tab_screen(tty: &str) -> Option<Vec<String>> {
    let script = format!(
        "tell application \"Terminal\"\n\
         repeat with w in windows\n\
         try\n\
         repeat with t in tabs of w\n\
         if tty of t is \"/dev/{tty}\" then return \"@\" & (contents of (contents of t))\n\
         end repeat\n\
         end try\n\
         end repeat\n\
         end tell\n\
         return \"\""
    );
    let mut cmd = Command::new("osascript");
    cmd.args(["-e", &script]);
    let (out, _) = run_with_timeout(cmd, OSA_TIMEOUT)?;
    let lines: Vec<String> = out.strip_prefix('@')?.lines().map(|l| l.trim_end().to_string()).collect();
    Some(preview_lines(&lines))
}

/// Lines worth previewing from a tab's screen: the conversation, not Claude
/// Code's own chrome at the bottom (input box, status line, mode line). The
/// chrome starts at the rule above the last `❯` prompt; if that can't be
/// found, the last CHROME_FALLBACK lines are dropped. Pure.
pub fn preview_lines(lines: &[String]) -> Vec<String> {
    const CHROME_FALLBACK: usize = 10;
    let is_rule = |l: &str| {
        let t = l.trim_start();
        t.starts_with('\u{2500}') || t.starts_with('\u{256d}') || t.starts_with('\u{250c}')
    };
    let prompt = lines.iter().rposition(|l| l.trim_start().starts_with('\u{276f}'));
    let cut = match prompt {
        Some(p) => lines[..p].iter().rposition(|l| is_rule(l)).unwrap_or(p),
        None => lines.len().saturating_sub(CHROME_FALLBACK),
    };
    let body = &lines[..cut];
    let end = body.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |i| i + 1);
    let start = end.saturating_sub(PREVIEW_LINES);
    body[start..end].to_vec()
}

/// Bring the tab to the front so the user can look at it. Changes focus only.
pub fn reveal(tty: &str) -> bool {
    let script = format!(
        "tell application \"Terminal\"\n\
         repeat with w in windows\n\
         try\n\
         repeat with t in tabs of w\n\
         if tty of t is \"/dev/{tty}\" then\n\
         set selected tab of w to t\n\
         set index of w to 1\n\
         activate\n\
         return \"ok\"\n\
         end if\n\
         end repeat\n\
         end try\n\
         end repeat\n\
         end tell\n\
         return \"\""
    );
    let mut cmd = Command::new("osascript");
    cmd.args(["-e", &script]);
    run_with_timeout(cmd, OSA_TIMEOUT).is_some_and(|(o, _)| o.trim() == "ok")
}

/// Re-check the plan against the live system right before acting: same
/// process, still on the same tab, same conversation, still idle.
pub fn verify(plan: &Plan) -> Result<(), String> {
    let tty = Command::new("ps")
        .args(["-o", "tty=", "-p", &plan.live.pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if tty != plan.tty {
        return Err("Not hibernated: that Claude process is no longer running in this tab.".into());
    }
    let variants = claude::variants(&claude::home());
    let now = claude::live(plan.live.pid, &variants)
        .ok_or("Not hibernated: that Claude session has ended.")?;
    if now.session_id != plan.live.session_id {
        return Err("Not hibernated: this tab switched to a different conversation.".into());
    }
    if !now.is_idle() {
        return Err("Not hibernated: this session became busy since you opened the preview.".into());
    }
    Ok(())
}

/// Wait for `pid` to exit; true if it did within `limit`.
fn wait_exit(pid: i32, limit: Duration) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        // SAFETY: signal 0 only checks that the pid exists; nothing is sent.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// The card written into the tab after Claude exits: a full-width block
/// with a coloured left bar (no right border, so wide characters can't
/// misalign it). `width` is the tab's column count.
pub fn card(plan: &Plan, stamp: &str, width: usize) -> String {
    use crate::app::{fmt_age, human_kb, now_ms, short_text, wrap_chars};
    const BAR: &str = "\x1b[38;5;79m\u{2503}\x1b[0m";
    const DIM: &str = "\x1b[2m";
    const BOLD: &str = "\x1b[1m";
    const RESET: &str = "\x1b[0m";
    let width = width.clamp(60, 120);
    let text_w = width - 6;
    let mut out: Vec<String> = Vec::new();
    let rule = |label: &str| {
        let fill = width.saturating_sub(label.chars().count() + 5);
        format!("\x1b[38;5;79m\u{250f}\u{2501} {BOLD}{label}{RESET}\x1b[38;5;79m {}{RESET}", "\u{2501}".repeat(fill))
    };
    let line = |s: &str| format!("{BAR}  {s}");
    let blank = || BAR.to_string();
    let block = |out: &mut Vec<String>, heading: &str, body: &str, max_lines: usize| {
        out.push(line(&format!("{DIM}{heading}{RESET}")));
        // Strip markdown markers, then cut so the wrapped text ends in "…"
        // inside the last line instead of being dropped silently.
        let plain = body.replace("**", "").replace("## ", "").replace("# ", "").replace('`', "");
        let budget = (text_w - 2) * max_lines - max_lines * 8;
        let wrapped = wrap_chars(&short_text(&plain, budget), text_w - 2);
        for l in wrapped.into_iter().take(max_lines) {
            out.push(line(&format!("  {l}")));
        }
    };

    out.push(String::new());
    out.push(rule(&format!("\u{1f4a4} HIBERNATED \u{b7} {stamp} \u{b7} freed ~{}", human_kb(plan.frees_kb))));
    out.push(blank());
    let idle = match plan.live.status_since_ms {
        0 => String::new(),
        t => format!("was idle {}", fmt_age(now_ms().saturating_sub(t))),
    };
    match (&plan.live.name, idle.is_empty()) {
        (Some(n), false) => out.push(line(&format!("{DIM}Session{RESET}  {n} \u{b7} {idle}"))),
        (Some(n), true) => out.push(line(&format!("{DIM}Session{RESET}  {n}"))),
        (None, false) => out.push(line(&format!("{DIM}Session{RESET}  {idle}"))),
        (None, true) => {}
    }
    if !plan.title.is_empty() {
        out.push(line(&format!("{DIM}Tab{RESET}      {}", short_text(&plan.title, text_w - 9))));
    }
    out.push(line(&format!("{DIM}Folder{RESET}   {}", plan.cwd)));
    out.push(blank());
    if let Some(p) = &plan.last_prompt {
        block(&mut out, "You last asked", p, 3);
        out.push(blank());
    }
    if let Some(r) = &plan.last_reply {
        block(&mut out, "Claude last replied", r, 3);
        out.push(blank());
    }
    if let Some(sum) = &plan.summary {
        let when = if sum.age_ms == 0 { "just now".to_string() } else { format!("as of {} ago", fmt_age(sum.age_ms)) };
        block(&mut out, &format!("Summary ({when})"), &sum.text, 6);
        out.push(blank());
    }
    out.push(line(&format!("{DIM}Wake{RESET}     \x1b[1;38;5;79m{}{RESET}", plan.resume_cmd())));
    out.push(format!("\x1b[38;5;79m\u{2517}{}{RESET}", "\u{2501}".repeat(width - 1)));
    out.push(String::new());
    out.join("\r\n") + "\r\n"
}

/// Column count of a tty, or 100 if it can't be read.
fn tty_width(tty: &str) -> usize {
    use std::os::fd::AsRawFd;
    let Ok(f) = fs::OpenOptions::new().read(true).open(format!("/dev/{tty}")) else { return 100 };
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ only reads the terminal's size into `ws`.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 { ws.ws_col as usize } else { 100 }
}

/// Write `text` to the tab's terminal as output (appears on screen; nothing
/// is typed into the shell).
fn write_to_tty(tty: &str, text: &str) -> bool {
    fs::OpenOptions::new()
        .write(true)
        .open(format!("/dev/{tty}"))
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .is_ok()
}

/// Verify, ask Claude to exit, wait for it, print the card. Returns a
/// one-line result for the UI.
pub fn execute(plan: &Plan, stamp: &str) -> Result<String, String> {
    if let Some(why) = plan.blocked_reason() {
        return Err(format!("Not hibernated: {why}"));
    }
    verify(plan)?;
    request_exit(plan)?;
    if !wait_exit(plan.live.pid, EXIT_WAIT) {
        return Err(
            "Claude did not exit within 15s. Nothing else was done; check the tab.".into(),
        );
    }
    if !write_to_tty(&plan.tty, &card(plan, stamp, tty_width(&plan.tty))) {
        return Ok(format!(
            "Hibernated, but the card could not be printed. Wake: {}",
            plan.resume_cmd()
        ));
    }
    Ok(format!("Hibernated {} · freed ~{}", plan.tty, crate::app::human_kb(plan.frees_kb)))
}

/// Type `/exit` + Enter into the tab. A pty probe showed Claude Code treats
/// LF as "insert newline" and only CR as submit, so the command carries an
/// explicit `return` (CR); whatever newline Terminal appends afterwards
/// lands on the shell prompt once Claude has exited and is harmless.
fn request_exit(plan: &Plan) -> Result<(), String> {
    let script = format!(
        "tell application \"Terminal\"\n\
         repeat with w in windows\n\
         try\n\
         repeat with t in tabs of w\n\
         if tty of t is \"/dev/{}\" then\n\
         do script (\"/exit\" & return) in t\n\
         return \"ok\"\n\
         end if\n\
         end repeat\n\
         end try\n\
         end repeat\n\
         end tell\n\
         return \"\"",
        plan.tty
    );
    let mut cmd = Command::new("osascript");
    cmd.args(["-e", &script]);
    match run_with_timeout(cmd, OSA_TIMEOUT) {
        Some((o, _)) if o.trim() == "ok" => Ok(()),
        _ => Err("Not hibernated: could not reach the tab in Terminal.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_(pid: i32, cmd: &str) -> ProcInfo {
        ProcInfo { pid, ppid: 0, pgid: 0, tty: None, mem_kb: 0, cpu_pct: 0.0, etime: String::new(), command: cmd.into() }
    }

    #[test]
    fn preview_skips_claude_chrome() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let screen = l(&["older", "\u{23fa} Merged #214.", "", "  Next: confirm the scope.", "",
            "\u{2500}\u{2500}\u{2500}\u{2500}", "\u{276f} ", "\u{2500}\u{2500}\u{2500}\u{2500}",
            "  plans \u{b7} main \u{b7} 27%", "  \u{23f5}\u{23f5} bypass permissions on"]);
        assert_eq!(preview_lines(&screen), l(&["older", "\u{23fa} Merged #214.", "", "  Next: confirm the scope."]));
        // No prompt found: drop the last 10 lines.
        let plain: Vec<String> = (0..15).map(|i| format!("line {i}")).collect();
        assert_eq!(preview_lines(&plain), (0..5).map(|i| format!("line {i}")).collect::<Vec<_>>());
    }

    #[test]
    fn helpers_vs_blockers() {
        for c in ["npm exec @acme/weather-mcp", "node /x/.bin/acme-mcp-server", "caffeinate -i -t 300",
                  "/opt/clangd --background-index", "/x/sourcekit-lsp", "/A/Editor.app/x/mcp-server-darwin-arm64"] {
            assert!(is_helper(&proc_(1, c)), "{c}");
        }
        for c in ["npm run dev", "node server.js", "/bin/zsh -c make test", "cargo build"] {
            assert!(!is_helper(&proc_(1, c)), "{c}");
        }
    }

    #[test]
    fn card_has_wake_command() {
        let plan = Plan {
            tty: "ttys001".into(), title: "T".into(), cwd: "/a".into(),
            live: Live { pid: 1, session_id: "11111111-2222-4333-8444-555555555555".into(), name: None,
                         status: "idle".into(), status_since_ms: 0, cmd: "work".into() },
            frees_kb: 700 * 1024, screen: vec![], last_prompt: Some("ok merge it".into()), last_reply: None,
            transcript: None, summary: None, blockers: vec![],
        };
        let c = card(&plan, "Nov 7 15:02", 80);
        assert!(c.contains("HIBERNATED") && c.contains("freed ~700 MB") && c.contains("You last asked"));
        assert!(c.contains("ok merge it"));
        assert!(c.contains("work --resume 11111111-2222-4333-8444-555555555555"));
        assert_eq!(plan.blocked_reason(), None);
        let busy = Plan { live: Live { status: "busy".into(), ..plan.live.clone() }, ..plan.clone() };
        assert!(busy.blocked_reason().is_some());
        let blocked = Plan { blockers: vec!["npm run dev (pid 9)".into()], ..plan };
        assert!(blocked.blocked_reason().unwrap().contains("npm run dev"));
    }
}
