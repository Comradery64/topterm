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
    pub window: usize,
    pub tab: usize,
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

    let (window, tab, screen) = tab_screen(&sess.tty).unwrap_or((0, 0, Vec::new()));

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
        window,
        tab,
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

/// (window index, tab index, last PREVIEW_LINES non-blank screen lines) for
/// the Terminal.app tab on `tty`. Reads `contents`, which is only what is
/// on screen, not the scrollback. Inside AppleScript `contents of x` first
/// dereferences the loop variable, hence `contents of (contents of t)`.
fn tab_screen(tty: &str) -> Option<(usize, usize, Vec<String>)> {
    let script = format!(
        "set f to (ASCII character 31)\n\
         tell application \"Terminal\"\n\
         set wi to 0\n\
         repeat with w in windows\n\
         set wi to wi + 1\n\
         try\n\
         set ti to 0\n\
         repeat with t in tabs of w\n\
         set ti to ti + 1\n\
         if tty of t is \"/dev/{tty}\" then return (wi as text) & f & (ti as text) & f & (contents of (contents of t))\n\
         end repeat\n\
         end try\n\
         end repeat\n\
         end tell\n\
         return \"\""
    );
    let mut cmd = Command::new("osascript");
    cmd.args(["-e", &script]);
    let (out, _) = run_with_timeout(cmd, OSA_TIMEOUT)?;
    let mut it = out.splitn(3, '\u{1f}');
    let w = it.next()?.trim().parse().ok()?;
    let t = it.next()?.trim().parse().ok()?;
    let lines: Vec<String> = it.next().unwrap_or("").lines().map(|l| l.trim_end().to_string()).collect();
    let end = lines.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |i| i + 1);
    let start = end.saturating_sub(PREVIEW_LINES);
    Some((w, t, lines[start..end].to_vec()))
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

/// The card written into the tab after Claude exits.
pub fn card(plan: &Plan, stamp: &str) -> String {
    let mut s = format!(
        "\r\n\x1b[1m💤 HIBERNATED\x1b[0m  {stamp}  ·  freed ~{}\r\n",
        crate::app::human_kb(plan.frees_kb)
    );
    s.push_str(&format!("Folder:  {}\r\n", plan.cwd));
    if let Some(p) = &plan.last_prompt {
        s.push_str(&format!("Last ask: {}\r\n", crate::app::short_text(p, 100)));
    }
    if let Some(sum) = &plan.summary {
        s.push_str(&format!("Summary: {}\r\n", crate::app::short_text(&sum.text, 200)));
    }
    s.push_str(&format!("Wake:    \x1b[1m{}\x1b[0m\r\n\r\n", plan.resume_cmd()));
    s
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
    if !write_to_tty(&plan.tty, &card(plan, stamp)) {
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
            tty: "ttys001".into(), window: 1, tab: 2, title: "T".into(), cwd: "/a".into(),
            live: Live { pid: 1, session_id: "11111111-2222-4333-8444-555555555555".into(), name: None,
                         status: "idle".into(), status_since_ms: 0, cmd: "work".into() },
            frees_kb: 700 * 1024, screen: vec![], last_prompt: Some("ok merge it".into()), last_reply: None,
            transcript: None, summary: None, blockers: vec![],
        };
        let c = card(&plan, "Nov 7 15:02");
        assert!(c.contains("HIBERNATED") && c.contains("freed ~700 MB") && c.contains("Last ask: ok merge it"));
        assert!(c.contains("work --resume 11111111-2222-4333-8444-555555555555"));
        assert_eq!(plan.blocked_reason(), None);
        let busy = Plan { live: Live { status: "busy".into(), ..plan.live.clone() }, ..plan.clone() };
        assert!(busy.blocked_reason().is_some());
        let blocked = Plan { blockers: vec!["npm run dev (pid 9)".into()], ..plan };
        assert!(blocked.blocked_reason().unwrap().contains("npm run dev"));
    }
}
