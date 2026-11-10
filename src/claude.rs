//! Read-only helpers for Claude Code's on-disk state: which config dirs exist
//! (plain `~/.claude` plus cc-mirror variants), the per-process session file a
//! running Claude keeps at `<config>/sessions/<pid>.json`, and transcripts at
//! `<config>/projects/<dir>/<id>.jsonl`. Nothing here writes or signals.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// A Claude Code config dir and the command that launches it: `~/.claude`
/// is plain `claude`; each cc-mirror variant `~/.cc-mirror/<name>/config` is
/// launched as `<name>`, and keeps its own sessions/ and projects/.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub cmd: String,
    pub dir: PathBuf,
}

/// A running Claude Code process, from its `sessions/<pid>.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub pid: i32,
    pub session_id: String,
    pub name: Option<String>,
    /// "idle" or "busy" as Claude Code reports it.
    pub status: String,
    /// When `status` last changed, Unix epoch milliseconds (0 if unknown).
    pub status_since_ms: u64,
    /// Launch command of the variant that owns it, e.g. "claude" or "work".
    pub cmd: String,
}

impl Live {
    pub fn is_idle(&self) -> bool {
        self.status == "idle"
    }
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

pub fn variants(home: &Path) -> Vec<Variant> {
    let mut out = vec![Variant { cmd: "claude".into(), dir: home.join(".claude") }];
    if let Ok(entries) = fs::read_dir(home.join(".cc-mirror")) {
        let mut extra: Vec<Variant> = entries
            .flatten()
            .filter(|e| e.path().join("config/sessions").is_dir() || e.path().join("config/projects").is_dir())
            .map(|e| Variant { cmd: e.file_name().to_string_lossy().into_owned(), dir: e.path().join("config") })
            .collect();
        extra.sort_by(|a, b| a.cmd.cmp(&b.cmd));
        out.extend(extra);
    }
    out
}

/// The live session for `pid`, if that process is a Claude Code instance.
pub fn live(pid: i32, variants: &[Variant]) -> Option<Live> {
    variants.iter().find_map(|v| {
        let json = fs::read_to_string(v.dir.join(format!("sessions/{pid}.json"))).ok()?;
        parse_live(&json, pid, &v.cmd)
    })
}

/// Parse a `sessions/<pid>.json` body. Pure.
pub fn parse_live(json: &str, pid: i32, cmd: &str) -> Option<Live> {
    let session_id = json_str(json, "sessionId").filter(|s| is_uuid(s))?;
    Some(Live {
        pid,
        session_id,
        name: json_str(json, "name").filter(|n| !n.is_empty()),
        status: json_str(json, "status").unwrap_or_default(),
        status_since_ms: json_num(json, "statusUpdatedAt").unwrap_or(0),
        cmd: cmd.to_string(),
    })
}

/// Map session id -> (launch command, transcript path), searching
/// `<config>/projects/<dir>/<id>.jsonl` in every variant.
pub fn find_transcripts(variants: &[Variant], ids: &[String]) -> HashMap<String, (String, PathBuf)> {
    let mut out = HashMap::new();
    for v in variants {
        let Ok(dirs) = fs::read_dir(v.dir.join("projects")) else { continue };
        for d in dirs.flatten() {
            for id in ids {
                let p = d.path().join(format!("{id}.jsonl"));
                if !out.contains_key(id) && p.is_file() {
                    out.insert(id.clone(), (v.cmd.clone(), p));
                }
            }
        }
    }
    out
}

/// The user's last typed prompt and Claude's last text reply in a transcript.
/// Reads only the file's tail. Pure over the given text.
pub fn last_exchange(raw: &str) -> (Option<String>, Option<String>) {
    let mut user = None;
    let mut reply = None;
    for line in raw.lines().rev() {
        if user.is_some() && reply.is_some() {
            break;
        }
        match chat_text(line) {
            Some((true, t)) if user.is_none() => user = Some(t),
            Some((false, t)) if reply.is_none() => reply = Some(t),
            _ => {}
        }
    }
    (user, reply)
}

/// The readable text of one transcript line: (true, prompt) for something
/// the user typed, (false, reply) for Claude's text. None for tool calls,
/// tool results, and injected messages. Pure.
pub fn chat_text(line: &str) -> Option<(bool, String)> {
    let found = if line.contains("\"type\":\"assistant\"") {
        json_str(line, "text").map(|t| (false, t))
    } else if line.contains("\"type\":\"user\"") && !line.contains("\"tool_use_id\"") {
        // A typed prompt is stored either as a plain string or as a text
        // block; tool results (also "user") are skipped above.
        json_str(line, "content").or_else(|| json_str(line, "text")).filter(|t| !is_injected(t)).map(|t| (true, t))
    } else {
        None
    };
    found.filter(|(_, t)| !t.trim().is_empty())
}

/// Claude Code's own recap: the newest `away_summary` record in a transcript
/// (the "while you were away" text it shows on return).
#[derive(Debug, Clone, PartialEq)]
pub struct Recap {
    pub text: String,
    /// When Claude wrote it, Unix epoch milliseconds (0 if unparseable).
    pub at_ms: u64,
    /// Chat messages added to the transcript after it.
    pub messages_since: usize,
}

/// Find the newest recap in transcript text. Pure.
pub fn latest_recap(raw: &str) -> Option<Recap> {
    let mut found: Option<Recap> = None;
    for line in raw.lines() {
        if line.contains("\"subtype\":\"away_summary\"") {
            if let Some(text) = json_str(line, "content").filter(|t| !t.trim().is_empty()) {
                let at_ms = json_str(line, "timestamp").and_then(|t| parse_iso_ms(&t)).unwrap_or(0);
                found = Some(Recap { text, at_ms, messages_since: 0 });
            }
        } else if let Some(r) = found.as_mut() {
            if is_chat_line(line) {
                r.messages_since += 1;
            }
        }
    }
    found
}

/// A user or assistant message line (tool results included). Pure.
pub fn is_chat_line(line: &str) -> bool {
    line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"")
}

/// The newest recap in a transcript file. Recaps are frequent, so the last
/// 2 MB usually has one; only then is the whole file read.
pub fn latest_recap_in(path: &Path) -> Option<Recap> {
    if let Some(r) = read_tail(path, 2 * 1024 * 1024).and_then(|t| latest_recap(&t)) {
        return Some(r);
    }
    latest_recap(&fs::read_to_string(path).ok()?)
}

/// "2026-11-06T22:02:26.931Z" -> epoch milliseconds (UTC). Pure.
pub fn parse_iso_ms(s: &str) -> Option<u64> {
    let (date, time) = s.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':');
    let (hh, mm): (i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let secs: f64 = t.next()?.parse().ok()?;
    // Days from civil (Howard Hinnant's algorithm).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let ms = (days * 86_400 + hh * 3600 + mm * 60) * 1000 + (secs * 1000.0) as i64;
    u64::try_from(ms).ok()
}

/// Text that lands in the transcript as a "user" message without the user
/// typing it: messages from other sessions, slash-command echoes, hooks.
fn is_injected(t: &str) -> bool {
    t.starts_with('<')
        || ["<teammate-message", "<command-name>", "<local-command", "<system-reminder>", "<task-notification>"]
            .iter()
            .any(|m| t.contains(m))
}

/// Read the last `bytes` of a file as text (lossy). Used for transcripts.
pub fn read_tail(path: &Path, bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(bytes))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

pub fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Minimal `"key":"value"` extractor for the flat JSON Claude Code writes.
/// Avoids a JSON dependency for a handful of string fields. Pure.
pub fn json_str(json: &str, key: &str) -> Option<String> {
    let start = json.find(&format!("\"{key}\":\""))? + key.len() + 4;
    let mut out = String::new();
    let mut esc = false;
    for c in json[start..].chars() {
        match (esc, c) {
            (true, 'n') => {
                out.push(' ');
                esc = false;
            }
            (true, c) => {
                out.push(c);
                esc = false;
            }
            (false, '\\') => esc = true,
            (false, '"') => return Some(out),
            (false, c) => out.push(c),
        }
    }
    None
}

/// `"key":123` extractor. Pure.
pub fn json_num(json: &str, key: &str) -> Option<u64> {
    let start = json.find(&format!("\"{key}\":"))? + key.len() + 3;
    let digits: String = json[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    #[test]
    fn json_helpers() {
        let j = format!(r#"{{"pid":1,"sessionId":"{ID}","name":"say \"hi\"","statusUpdatedAt":1793940587862}}"#);
        assert_eq!(json_str(&j, "sessionId").as_deref(), Some(ID));
        assert_eq!(json_str(&j, "name").as_deref(), Some("say \"hi\""));
        assert_eq!(json_str(&j, "missing"), None);
        assert_eq!(json_num(&j, "statusUpdatedAt"), Some(1793940587862));
    }

    #[test]
    fn parse_live_reads_status() {
        let j = format!(r#"{{"sessionId":"{ID}","status":"idle","statusUpdatedAt":5,"name":""}}"#);
        let l = parse_live(&j, 42, "work").unwrap();
        assert!(l.is_idle());
        assert_eq!((l.pid, l.status_since_ms, l.cmd.as_str(), l.name), (42, 5, "work", None));
        assert_eq!(parse_live(r#"{"sessionId":"nope"}"#, 1, "claude"), None);
    }

    #[test]
    fn recap_newest_wins_and_counts_messages_after() {
        let raw = r#"{"type":"system","subtype":"away_summary","content":"old","timestamp":"2026-11-06T22:02:26.931Z"}
{"type":"user","message":{"content":"a"}}
{"type":"system","subtype":"away_summary","content":"You're fixing tabs.","timestamp":"1970-01-01T00:00:01.500Z"}
{"type":"user","message":{"content":"b"}}
{"type":"assistant","message":{"content":[]}}
{"type":"system","subtype":"turn_duration"}"#;
        let r = latest_recap(raw).unwrap();
        assert_eq!((r.text.as_str(), r.at_ms, r.messages_since), ("You're fixing tabs.", 1500, 2));
        assert_eq!(latest_recap(r#"{"type":"user"}"#), None);
        assert_eq!(parse_iso_ms("2026-11-06T22:02:26.931Z"), Some(1_794_002_546_931));
    }

    #[test]
    fn last_exchange_skips_tool_results() {
        let raw = r#"{"type":"user","message":{"role":"user","content":"ok merge it"}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Merged #214."}]}}
{"type":"user","message":{"content":[{"tool_use_id":"t1","type":"tool_result","content":"x"}]}}
{"type":"user","message":{"role":"user","content":"Another session sent: <teammate-message id=1>hi</teammate-message>"}}"#;
        let (u, r) = last_exchange(raw);
        assert_eq!(u.as_deref(), Some("ok merge it"));
        assert_eq!(r.as_deref(), Some("Merged #214."));
    }
}
