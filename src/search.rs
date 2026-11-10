//! Full-text search over what was said in a Claude Code session: the
//! user's prompts and Claude's text replies, not tool calls or tool output
//! (which would match every file Claude ever read). Streams the transcript
//! line by line, so memory stays flat however big it is.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Characters of context shown around a match.
const SNIPPET_W: usize = 80;

/// Lowercased words of a query. ASCII lowercasing keeps byte offsets
/// stable, so a match position in lowercased text is valid in the original.
pub fn words(query: &str) -> Vec<String> {
    query.split_whitespace().map(|w| w.to_ascii_lowercase()).collect()
}

/// If every word appears somewhere in the session's chat, a snippet from
/// the newest message containing the most of them. `cancelled` is polled
/// so a newer query can stop this one.
pub fn search_transcript(path: &Path, words: &[String], cancelled: &dyn Fn() -> bool) -> Option<String> {
    if words.is_empty() {
        return None;
    }
    let mut r = BufReader::new(File::open(path).ok()?);
    let mut buf = Vec::new();
    let mut seen = vec![false; words.len()];
    let mut best: Option<(usize, String)> = None;
    let mut n = 0u32;
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf).ok()? == 0 {
            break;
        }
        n += 1;
        if n % 512 == 0 && cancelled() {
            return None;
        }
        let line = String::from_utf8_lossy(&buf);
        // Cheap filters before any JSON parsing.
        if !crate::claude::is_chat_line(&line) {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if !words.iter().any(|w| lower.contains(w.as_str())) {
            continue;
        }
        let Some((_, text)) = crate::claude::chat_text(&line) else { continue };
        if let Some((hits, snip)) = match_text(&text, words) {
            for (i, w) in words.iter().enumerate() {
                if text.to_ascii_lowercase().contains(w.as_str()) {
                    seen[i] = true;
                }
            }
            if best.as_ref().is_none_or(|(b, _)| hits >= *b) {
                best = Some((hits, snip));
            }
        }
    }
    if seen.iter().all(|&s| s) { best.map(|(_, s)| s) } else { None }
}

/// How many of `words` are in `text`, and a one-line snippet around the
/// first word found. None if none are. Pure.
pub fn match_text(text: &str, words: &[String]) -> Option<(usize, String)> {
    let lower = text.to_ascii_lowercase();
    let hits = words.iter().filter(|w| lower.contains(w.as_str())).count();
    let pos = words.iter().find_map(|w| lower.find(w.as_str()))?;
    Some((hits, snippet(text, pos)))
}

/// About SNIPPET_W characters of `text` around byte `pos`, on one line. Pure.
fn snippet(text: &str, pos: usize) -> String {
    let before = text[..pos].chars().count();
    let start = before.saturating_sub(SNIPPET_W / 3);
    let flat: String = text.chars().map(|c| if c.is_whitespace() { ' ' } else { c }).collect();
    let total = flat.chars().count();
    let body: String = flat.chars().skip(start).take(SNIPPET_W).collect();
    let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{}{body}{}",
        if start > 0 { "\u{2026}" } else { "" },
        if start + SNIPPET_W < total { "\u{2026}" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(q: &str) -> Vec<String> {
        words(q)
    }

    #[test]
    fn snippet_centres_on_the_match() {
        let text = format!("{} the Occlusion fix for Ada\nis done {}", "x ".repeat(60), "y ".repeat(60));
        let (hits, s) = match_text(&text, &w("occlusion ada")).unwrap();
        assert_eq!(hits, 2);
        assert!(s.starts_with('\u{2026}') && s.ends_with('\u{2026}'));
        assert!(s.contains("Occlusion fix for Ada is done"));
        assert_eq!(match_text("nothing here", &w("ada")), None);
    }

    #[test]
    fn searches_chat_but_not_tool_output() {
        let dir = std::env::temp_dir().join(format!("topterm-search-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        std::fs::write(
            &p,
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"please fix the walk cycle"}}"#, "\n",
                r#"{"type":"user","message":{"content":[{"tool_use_id":"x","type":"tool_result","content":"secret banana in a file"}]}}"#, "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Fixed the Walk timing."}]}}"#, "\n",
            ),
        )
        .unwrap();
        let never = || false;
        assert_eq!(search_transcript(&p, &w("walk timing"), &never).as_deref(), Some("Fixed the Walk timing."));
        assert_eq!(search_transcript(&p, &w("banana"), &never), None);
        assert_eq!(search_transcript(&p, &w("walk banana"), &never), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
