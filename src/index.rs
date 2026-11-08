//! `topterm --index`: a read-only map of every Terminal.app tab — title, cwd, and the Claude Code session it holds (live, or
//! the `claude --resume <id>` line printed when it exited) — so tabs can be
//! closed later without losing track of what they were. `--summarize` adds a
//! short recap per session from a one-off Haiku call that reads a copy of the
//! transcript: it never resumes the session and never persists a new one.
//!
//! Nothing here writes to Terminal, signals a process, or touches a session.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::claude::{self, find_transcripts, is_uuid};
use crate::collector::{self, run_with_timeout};

/// How much of each tab's scrollback to read when looking for the exit line.
const TAIL_CHARS: usize = 6000;
/// Reading 100+ tabs' history takes ~11s on a loaded machine.
const HISTORY_TIMEOUT: Duration = Duration::from_secs(90);
/// One Haiku call; generous because the machine may be swapping.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(180);
/// Parallel Haiku calls. Each `claude` process costs a few hundred MB, so keep
/// this low: the tool is most useful exactly when memory is short.
const SUMMARY_WORKERS: usize = 3;
/// Transcript excerpt budget sent to Haiku (characters, newest kept).
const EXCERPT_CHARS: usize = 30_000;
const EXCERPT_LINE_CHARS: usize = 1_500;

const FIELD: char = '\u{1f}';
const RECORD: char = '\u{1e}';

const SUMMARY_PROMPT: &str = "Below is the tail of a Claude Code session transcript (JSON lines). \
In at most two short sentences, say what the user was working on and where it was left off. \
Plain text, no preamble, no markdown. Never include secrets, API keys, passwords, tokens, or \
credentials, even partially.";

/// One Terminal.app tab. There is no reliable on-screen position to report:
/// with macOS native tabs every tab is its own scripting "window", ordered by
/// recent focus, so tabs are identified by tty, title, and folder instead.
#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub tty: String,
    pub title: String,
    pub tail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Claude {
    Running { id: String, name: Option<String> },
    Exited { id: String },
    None,
}

impl Claude {
    fn id(&self) -> Option<&str> {
        match self {
            Claude::Running { id, .. } | Claude::Exited { id } => Some(id),
            Claude::None => None,
        }
    }
}

pub fn run(summarize: bool) -> io::Result<()> {
    let snap = collector::collect()?;
    let tabs = read_tabs().ok_or_else(|| {
        io::Error::other(
            "could not read Terminal.app tabs (is Terminal running, and is Automation \
             access granted under System Settings > Privacy & Security > Automation?)",
        )
    })?;
    let home = claude::home();
    let variants = claude::variants(&home);

    let by_tty: HashMap<&str, &collector::Session> =
        snap.sessions.iter().map(|s| (s.tty.as_str(), s)).collect();

    let rows: Vec<(Tab, String, Claude)> = tabs
        .into_iter()
        .map(|t| {
            let sess = by_tty.get(t.tty.as_str());
            let cwd = sess.map_or("?".to_string(), |s| s.cwd.clone());
            let live = sess
                .and_then(|s| s.claude.clone())
                .map(|l| Claude::Running { id: l.session_id, name: l.name });
            let claude = live.unwrap_or_else(|| match last_resume_id(&t.tail) {
                Some(id) => Claude::Exited { id },
                None => Claude::None,
            });
            (t, cwd, claude)
        })
        .collect();

    let ids: Vec<String> = rows.iter().filter_map(|r| r.2.id().map(str::to_string)).collect();
    // Which variant owns each session decides its resume command.
    let transcripts = find_transcripts(&variants, &ids);
    let cmds: HashMap<String, String> =
        transcripts.iter().map(|(id, (cmd, _))| (id.clone(), cmd.clone())).collect();
    let summaries = if summarize {
        let paths = transcripts.into_iter().map(|(id, (_, p))| (id, p)).collect();
        summarize_all(&ids, paths)
    } else {
        HashMap::new()
    };

    let stamp = Command::new("date")
        .arg("+%Y-%m-%d %H:%M")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let out = render(&rows, &cmds, &summaries, &stamp);
    io::stdout().write_all(out.as_bytes())
}

/// Read every tab's position, tty, title and scrollback tail in one
/// AppleScript call. Read-only: `history` and `custom title` are getters.
fn read_tabs() -> Option<Vec<Tab>> {
    let script = [
        "set f to (ASCII character 31)",
        "set r to (ASCII character 30)",
        "set out to \"\"",
        "tell application \"Terminal\"",
        "repeat with w in windows",
        "try",
        "repeat with t in tabs of w",
        "set ct to \"\"",
        "try",
        "set ct to custom title of t",
        "end try",
        "set h to \"\"",
        "try",
        "set h to history of t",
        "set n to length of h",
        &format!("if n > {TAIL_CHARS} then set h to text (n - {}) thru n of h", TAIL_CHARS - 1),
        "end try",
        "set out to out & (tty of t) & f & ct & f & h & r",
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
    let (stdout, _) = run_with_timeout(cmd, HISTORY_TIMEOUT)?;
    let tabs = parse_tabs(&stdout);
    (!tabs.is_empty()).then_some(tabs)
}

/// Parse the RECORD/FIELD-separated AppleScript output. Pure.
pub fn parse_tabs(raw: &str) -> Vec<Tab> {
    raw.split(RECORD)
        .filter_map(|rec| {
            let mut it = rec.trim_start_matches('\n').splitn(3, FIELD);
            let tty = it.next()?.trim().rsplit('/').next()?.to_string();
            if !tty.starts_with("ttys") {
                return None;
            }
            let title = clean_title(it.next()?);
            let tail = it.next().unwrap_or("").to_string();
            Some(Tab { tty, title, tail })
        })
        .collect()
}

/// Drop the spinner/status glyph Claude Code puts in front of tab titles.
fn clean_title(t: &str) -> String {
    t.trim()
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '(' && c != '[')
        .trim()
        .to_string()
}

/// The last `claude --resume <uuid>` (or `-r <uuid>`) in a scrollback tail —
/// what Claude Code prints on exit, or what the user typed last. Pure.
pub fn last_resume_id(tail: &str) -> Option<String> {
    let mut found = None;
    for pat in ["claude --resume ", "claude -r "] {
        for (i, _) in tail.match_indices(pat) {
            let cand: String = tail[i + pat.len()..].chars().take(36).collect();
            if is_uuid(&cand) && found.as_ref().is_none_or(|(at, _)| i > *at) {
                found = Some((i, cand));
            }
        }
    }
    found.map(|(_, id)| id)
}

// ---------------------------------------------------------------------------
// Summaries

fn summarize_all(ids: &[String], transcripts: HashMap<String, PathBuf>) -> HashMap<String, String> {
    let mut uniq: Vec<String> = ids.to_vec();
    uniq.sort();
    uniq.dedup();
    let cache = summary_cache_dir();
    let _ = fs::create_dir_all(&cache);

    let queue = Arc::new(Mutex::new(uniq));
    let results = Arc::new(Mutex::new(HashMap::new()));
    let total = queue.lock().map(|q| q.len()).unwrap_or(0);
    let done = Arc::new(Mutex::new(0usize));
    let transcripts = Arc::new(transcripts);
    let handles: Vec<_> = (0..SUMMARY_WORKERS)
        .map(|_| {
            let (queue, results, done, transcripts, cache) =
                (queue.clone(), results.clone(), done.clone(), transcripts.clone(), cache.clone());
            thread::spawn(move || loop {
                let Some(id) = queue.lock().ok().and_then(|mut q| q.pop()) else { return };
                let text = match transcripts.get(&id) {
                    Some(path) => summarize_one(path, &id, &cache),
                    None => "(transcript not found)".to_string(),
                };
                if let Ok(mut r) = results.lock() {
                    r.insert(id, text);
                }
                if let Ok(mut d) = done.lock() {
                    *d += 1;
                    eprint!("\rsummarizing {}/{total}", *d);
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
    if total > 0 {
        eprintln!();
    }
    Arc::try_unwrap(results).ok().and_then(|m| m.into_inner().ok()).unwrap_or_default()
}

/// Summary for one transcript, cached by (id, transcript size) so re-runs
/// only pay for sessions that changed.
pub fn summary_cache_dir() -> PathBuf {
    claude::home().join("Library/Caches/topterm/summaries")
}

pub fn summarize_one(transcript: &Path, id: &str, cache: &Path) -> String {
    let size = fs::metadata(transcript).map(|m| m.len()).unwrap_or(0);
    let cached = cache.join(format!("{id}-{size}.txt"));
    if let Ok(s) = fs::read_to_string(&cached) {
        return s;
    }
    let Ok(raw) = fs::read_to_string(transcript) else {
        return "(transcript unreadable)".to_string();
    };
    let excerpt = excerpt(&raw);
    let mut cmd = Command::new("claude");
    cmd.args([
        "-p",
        "--model", "haiku",
        "--effort", "low",
        "--no-session-persistence",
        "--tools", "",
        "--strict-mcp-config",
        SUMMARY_PROMPT,
    ]);
    // Run from a neutral directory so no project CLAUDE.md is loaded.
    cmd.current_dir(std::env::temp_dir());
    let Some(text) = run_with_stdin(cmd, &excerpt, SUMMARY_TIMEOUT) else {
        return "(summary failed)".to_string();
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return "(summary failed)".to_string();
    }
    let _ = fs::write(&cached, &text);
    text
}

/// Newest user/assistant transcript lines, each capped, within EXCERPT_CHARS. Pure.
pub fn excerpt(raw: &str) -> String {
    let mut picked: Vec<&str> = Vec::new();
    let mut used = 0;
    for line in raw.lines().rev() {
        if !(line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"")) {
            continue;
        }
        let cut = line.char_indices().nth(EXCERPT_LINE_CHARS).map_or(line, |(i, _)| &line[..i]);
        if used + cut.len() > EXCERPT_CHARS {
            break;
        }
        used += cut.len() + 1;
        picked.push(cut);
    }
    picked.reverse();
    picked.join("\n")
}

fn run_with_stdin(mut cmd: Command, input: &str, timeout: Duration) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let input = input.to_string();
    thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    match rx.recv_timeout(timeout) {
        Ok(buf) => {
            let ok = child.wait().map(|s| s.success()).unwrap_or(false);
            ok.then_some(buf.trim().to_string())
        }
        Err(_) => {
            // Our own child only.
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Output

pub fn render(
    rows: &[(Tab, String, Claude)],
    cmds: &HashMap<String, String>,
    summaries: &HashMap<String, String>,
    stamp: &str,
) -> String {
    let cmd = |id: &str| cmds.get(id).map_or("claude", String::as_str).to_string();
    let mut sorted: Vec<&(Tab, String, Claude)> = rows.iter().collect();
    sorted.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.title.cmp(&b.0.title)));
    let mut s = format!(
        "# Terminal tab index ({stamp})\n\n{} tabs, grouped by folder. To find a tab, match its title; \
         `topterm -t` then `H`, `R` brings it to the front. To bring one back after closing: open a tab, \
         `cd` to its folder, run its resume command.\n",
        rows.len()
    );
    let mut folder: Option<&str> = None;
    for (t, cwd, claude) in sorted {
        if folder != Some(cwd.as_str()) {
            folder = Some(cwd.as_str());
            s.push_str(&format!("\n## `{cwd}`\n"));
        }
        let title = if t.title.is_empty() { "(untitled)" } else { &t.title };
        s.push_str(&format!("\n### {title} · {}\n", t.tty));
        match claude {
            Claude::Running { id, name } => {
                let name = name.as_deref().map(|n| format!(" ({n})")).unwrap_or_default();
                s.push_str(&format!("- Claude: running{name} · `{} --resume {id}`\n", cmd(id)));
            }
            Claude::Exited { id } => s.push_str(&format!("- Claude: exited · `{} --resume {id}`\n", cmd(id))),
            Claude::None => s.push_str("- Claude: none found in this tab\n"),
        }
        if let Some(sum) = claude.id().and_then(|id| summaries.get(id)) {
            s.push_str(&format!("- Summary: {sum}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "11111111-2222-4333-8444-555555555555";
    const ID2: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    #[test]
    fn resume_id_takes_last_and_ignores_non_uuids() {
        let tail = format!("claude --resume {ID}\nfoo\nclaude --resume not-a-uuid\nhost % claude -r {ID2}\n$ ");
        assert_eq!(last_resume_id(&tail), Some(ID2.to_string()));
        assert_eq!(last_resume_id("claude --resume"), None);
    }

    #[test]
    fn parse_tabs_reads_records() {
        let raw = "/dev/ttys004\u{1f}✳ Fix login\u{1f}hist\nline\u{1e}\n/dev/ttys009\u{1f}\u{1f}\u{1e}";
        let tabs = parse_tabs(raw);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].tty, "ttys004");
        assert_eq!(tabs[0].title, "Fix login");
        assert_eq!(tabs[0].tail, "hist\nline");
        assert_eq!(tabs[1].title, "");
    }

    #[test]
    fn excerpt_keeps_newest_chat_lines_only() {
        let raw = "{\"type\":\"user\",\"a\":1}\n{\"type\":\"summary\"}\n{\"type\":\"assistant\",\"b\":2}";
        assert_eq!(excerpt(raw), "{\"type\":\"user\",\"a\":1}\n{\"type\":\"assistant\",\"b\":2}");
    }

    #[test]
    fn render_groups_by_folder() {
        let tab = |t: u8, title: &str| Tab { tty: format!("ttys00{t}"), title: title.into(), tail: String::new() };
        let rows = vec![
            (tab(3, "T"), "/c".into(), Claude::Running { id: ID2.into(), name: Some("n".into()) }),
            (tab(1, "T"), "/a".into(), Claude::Exited { id: ID.into() }),
            (tab(2, "T"), "/b".into(), Claude::None),
        ];
        let sums = HashMap::from([(ID.to_string(), "Did X.".to_string())]);
        let cmds = HashMap::from([(ID2.to_string(), "work".to_string())]);
        let out = render(&rows, &cmds, &sums, "now");
        assert!(out.contains("## `/a`\n\n### T · ttys001\n- Claude: exited · `claude --resume 0abc"));
        assert!(out.find("`/a`").unwrap() < out.find("`/b`").unwrap());
        assert!(out.contains("- Summary: Did X.\n"));
        assert!(!out.contains("Window"));
        assert!(out.contains(&format!("- Claude: running (n) · `work --resume {ID2}`")));
    }
}
