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
        if reply.is_none() && line.contains("\"type\":\"assistant\"") {
            reply = json_str(line, "text").filter(|t| !t.trim().is_empty());
        } else if user.is_none() && line.contains("\"type\":\"user\"") && !line.contains("\"tool_use_id\"") {
            // A typed prompt is stored either as a plain string or as a
            // text block; tool results (also "user") are skipped above.
            user = json_str(line, "content")
                .or_else(|| json_str(line, "text"))
                .filter(|t| !t.trim().is_empty() && !t.starts_with('<'));
        }
    }
    (user, reply)
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
    fn last_exchange_skips_tool_results() {
        let raw = r#"{"type":"user","message":{"role":"user","content":"ok merge it"}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Merged #214."}]}}
{"type":"user","message":{"content":[{"tool_use_id":"t1","type":"tool_result","content":"x"}]}}"#;
        let (u, r) = last_exchange(raw);
        assert_eq!(u.as_deref(), Some("ok merge it"));
        assert_eq!(r.as_deref(), Some("Merged #214."));
    }
}
