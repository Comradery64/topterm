//! Subcommands for scripts and agents: `search`, `show`, `focus` and
//! `hibernate`, plus `--json` for the snapshot. Each mirrors a TUI key.
//! Nothing here acts on a tab unless asked explicitly (`--yes`).

use std::collections::HashMap;
use std::io;

use crate::app::{self, SortCol};
use crate::collector::{Session, Snapshot};

/// `--sort` names, as the TUI headers read.
pub fn parse_sort(name: &str) -> Option<SortCol> {
    Some(match name {
        "tty" => SortCol::Tty,
        "mem" => SortCol::Mem,
        "delta" => SortCol::Delta,
        "cpu" => SortCol::Cpu,
        "procs" => SortCol::Procs,
        "agent" | "idle" => SortCol::Claude,
        "tab" => SortCol::Tab,
        "recent" => SortCol::Recent,
        "cwd" => SortCol::Cwd,
        _ => return None,
    })
}

pub fn sort(snap: &mut Snapshot, col: SortCol) {
    app::sort_sessions(&mut snap.sessions, col, col.default_desc(), &snap.tab_titles, app::now_ms());
}

/// A JSON string literal. Pure.
pub fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn opt(s: Option<&str>) -> String {
    s.map_or("null".into(), js)
}

/// One session as a JSON object. `extra` holds additional `"key":value` pairs.
pub fn session_json(s: &Session, title: Option<&String>, now: u64, extra: &[(&str, String)]) -> String {
    let claude = s.claude.as_ref().map_or("null".into(), |c| {
        let idle = (c.is_idle() && c.status_since_ms > 0).then(|| now.saturating_sub(c.status_since_ms));
        format!(
            "{{\"pid\":{},\"session_id\":{},\"name\":{},\"status\":{},\"idle_ms\":{},\"resume\":{},\"frees_kb\":{}}}",
            c.pid,
            js(&c.session_id),
            opt(c.name.as_deref()),
            js(&c.status),
            idle.map_or("null".into(), |v| v.to_string()),
            js(&format!("{} --resume {}", c.cmd, c.session_id)),
            s.claude_mem_kb
        )
    });
    let agent = app::claude_cell(s, now);
    let mut o = format!(
        "{{\"tty\":{},\"title\":{},\"cwd\":{},\"mem_kb\":{},\"delta_kb\":{},\"cpu_pct\":{:.1},\"procs\":{},\"growing\":{},\"agent\":{},\"recent\":{},\"mcp_kb\":{},\"claude\":{}",
        js(&s.tty),
        opt(title.map(String::as_str)),
        js(&s.cwd),
        s.total_mem_kb,
        s.delta_mem_kb,
        s.total_cpu_pct,
        s.proc_count,
        s.growing,
        opt((!agent.is_empty()).then_some(agent.as_str())),
        opt(s.recent_command.as_deref()),
        s.mcp_mem_kb,
        claude
    );
    for (k, v) in extra {
        o.push_str(&format!(",{}:{}", js(k), v));
    }
    o.push('}');
    o
}

/// The whole snapshot as one JSON object.
pub fn snapshot_json(snap: &Snapshot) -> String {
    let now = app::now_ms();
    let list = |v: &[Session]| v.iter().map(|s| session_json(s, snap.tab_titles.get(&s.tty), now, &[])).collect::<Vec<_>>().join(",");
    let vms = snap
        .vms
        .iter()
        .map(|v| format!("{{\"label\":{},\"pid\":{},\"mem_kb\":{}}}", js(&v.label), v.proc.pid, v.proc.mem_kb))
        .collect::<Vec<_>>()
        .join(",");
    let mcp = snap
        .mcp
        .iter()
        .map(|g| {
            format!(
                "{{\"name\":{},\"tabs\":{},\"procs\":{},\"mem_kb\":{},\"orphaned\":{},\"orphaned_kb\":{}}}",
                js(&g.name), g.tabs, g.procs, g.mem_kb, g.orphaned, g.orphaned_mem_kb
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let sys = &snap.system;
    format!(
        "{{\"phys_mem_bytes\":{},\"total_mem_kb\":{},\"swap_used_bytes\":{},\"ssd_bytes_per_day\":{},\"sessions\":[{}],\"detached\":[{}],\"vms\":[{}],\"mcp\":[{}]}}",
        snap.phys_mem_bytes,
        snap.total_mem_kb,
        sys.swap_used_bytes,
        sys.ssd_bytes_per_day().map_or("null".into(), |v| v.to_string()),
        list(&snap.sessions),
        list(&snap.detached),
        vms,
        mcp
    )
}

fn find<'a>(snap: &'a Snapshot, tty: &str) -> io::Result<&'a Session> {
    let tty = tty.trim_start_matches("/dev/");
    snap.sessions
        .iter()
        .find(|s| s.tty == tty)
        .ok_or_else(|| io::Error::other(format!("no terminal session on {tty}")))
}

fn claude_ids(snap: &Snapshot) -> Vec<String> {
    snap.sessions.iter().filter_map(|s| s.claude.as_ref().map(|c| c.session_id.clone())).collect()
}

/// Chat messages of the Claude session in `sess` (fake chats with --fake-data).
fn messages(sess: &Session, fake: bool) -> Vec<(bool, String)> {
    if fake {
        return crate::fake::chats().into_iter().find(|(t, _)| *t == sess.tty).map(|(_, m)| m).unwrap_or_default();
    }
    let Some(live) = &sess.claude else { return Vec::new() };
    let variants = crate::claude::variants(&crate::claude::home());
    crate::claude::find_transcripts(&variants, std::slice::from_ref(&live.session_id))
        .remove(&live.session_id)
        .map(|(_, p)| crate::search::chat_messages(&p))
        .unwrap_or_default()
}

/// `topterm search QUERY`: the rows `/` would show, with a chat snippet when
/// the match is inside a conversation. Returns false when nothing matched.
pub fn search(snap: &Snapshot, query: &str, json: bool, fake: bool) -> bool {
    let words = crate::search::words(query);
    let (extras, hits): (HashMap<String, String>, HashMap<String, String>) = if fake {
        let mut hits = HashMap::new();
        for s in &snap.sessions {
            let msgs = messages(s, true);
            let all = msgs.iter().map(|(_, t)| t.to_ascii_lowercase()).collect::<Vec<_>>().join(" ");
            if let (Some(c), true) = (&s.claude, !words.is_empty() && words.iter().all(|w| all.contains(w.as_str()))) {
                let (list, best) = crate::search::match_list(&msgs, &words);
                if let Some(snip) = list.get(best).and_then(|&i| crate::search::match_text(&msgs[i].1, &words)) {
                    hits.insert(c.session_id.clone(), snip.1);
                }
            }
        }
        (HashMap::new(), hits)
    } else {
        let ids = claude_ids(snap);
        (app::search_extras(&ids), app::full_text_hits(&ids, query, &|| false))
    };
    let now = app::now_ms();
    let mut rows = Vec::new();
    for s in snap.sessions.iter().chain(&snap.detached) {
        let id = s.claude.as_ref().map(|c| &c.session_id);
        let snippet = id.and_then(|i| hits.get(i));
        if app::session_matches(s, snap.tab_titles.get(&s.tty), id.and_then(|i| extras.get(i)), query) || snippet.is_some() {
            rows.push((s, snippet));
        }
    }
    if json {
        let items: Vec<String> = rows
            .iter()
            .map(|(s, snip)| session_json(s, snap.tab_titles.get(&s.tty), now, &[("snippet", opt(snip.map(String::as_str)))]))
            .collect();
        println!("[{}]", items.join(","));
    } else {
        for (s, snip) in &rows {
            let title = snap.tab_titles.get(&s.tty).map_or("-", String::as_str);
            println!("{:<9} {:>9}  {:<12} {}  {}", s.tty, app::human_kb(s.total_mem_kb), app::claude_cell(s, now), title, s.cwd);
            if let Some(t) = snip {
                println!("          \u{201c}{t}\u{201d}");
            }
        }
        if rows.is_empty() {
            eprintln!("no tab matches {query:?}");
        }
    }
    !rows.is_empty()
}

const SHOW_MATCHES: usize = 3;
const SHOW_MSG_CHARS: usize = 600;

/// `topterm show TTY [--match QUERY]`: one tab in detail, like the TUI's
/// detail panel, hibernate preview and `p` preview.
pub fn show(snap: &Snapshot, tty: &str, query: Option<&str>, json: bool, fake: bool) -> io::Result<()> {
    let s = find(snap, tty)?;
    let title = snap.tab_titles.get(&s.tty);
    let now = app::now_ms();
    let plan = if fake { crate::fake::plan(&s.tty) } else { crate::hibernate::plan(s, title.map_or("", String::as_str)).ok() };
    let (ask, reply, recap) = plan.as_ref().map_or((None, None, None), |p| {
        (p.last_prompt.clone(), p.last_reply.clone(), p.summary.as_ref().map(|x| x.text.clone()))
    });
    let words = query.map(crate::search::words).unwrap_or_default();
    let msgs = if words.is_empty() { Vec::new() } else { messages(s, fake) };
    let (list, _) = crate::search::match_list(&msgs, &words);
    let picked: Vec<usize> = list.into_iter().take(SHOW_MATCHES).collect();
    let clip = |t: &str| app::short_text(t, SHOW_MSG_CHARS);
    let who = |user: bool| if user { "you" } else { "agent" };
    if json {
        let procs = s
            .procs
            .iter()
            .map(|p| format!("{{\"pid\":{},\"mem_kb\":{},\"cpu_pct\":{:.1},\"command\":{}}}", p.pid, p.mem_kb, p.cpu_pct, js(&p.command)))
            .collect::<Vec<_>>()
            .join(",");
        let matches = picked
            .iter()
            .map(|&i| {
                let ctx = (i.saturating_sub(1)..(i + 2).min(msgs.len()))
                    .map(|k| format!("{{\"from\":{},\"text\":{},\"match\":{}}}", js(who(msgs[k].0)), js(&clip(&msgs[k].1)), k == i))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("[{ctx}]")
            })
            .collect::<Vec<_>>()
            .join(",");
        let extra = [
            ("processes", format!("[{procs}]")),
            ("last_ask", opt(ask.as_deref())),
            ("last_reply", opt(reply.as_deref())),
            ("recap", opt(recap.as_deref())),
            ("matches", format!("[{matches}]")),
        ];
        println!("{}", session_json(s, title, now, &extra));
        return Ok(());
    }
    println!("{}  {}", s.tty, title.map_or("-", String::as_str));
    println!("folder   {}", s.cwd);
    println!("memory   {}  ({} processes, {:.1}% cpu)", app::human_kb(s.total_mem_kb), s.proc_count, s.total_cpu_pct);
    if let Some(c) = &s.claude {
        println!("agent    {}  {}", app::claude_cell(s, now), c.name.as_deref().unwrap_or(""));
        println!("resume   {} --resume {}", c.cmd, c.session_id);
    } else if let Some(a) = Some(app::claude_cell(s, now)).filter(|a| !a.is_empty()) {
        println!("agent    {a}");
    }
    for (label, v) in [("recap", &recap), ("you asked", &ask), ("reply", &reply)] {
        if let Some(t) = v {
            println!("{label:<10}{}", clip(t));
        }
    }
    println!("processes");
    for (p, d) in s.procs.iter().zip(&s.depths) {
        println!("  {:>7} {:>9}  {}{}", p.pid, app::human_kb(p.mem_kb), "  ".repeat(*d), p.command);
    }
    if !words.is_empty() {
        println!();
        if picked.is_empty() {
            println!("no message in this chat matches {:?}", query.unwrap_or(""));
        }
        for &i in &picked {
            println!("\u{2500}\u{2500} match");
            for k in i.saturating_sub(1)..(i + 2).min(msgs.len()) {
                println!("{}{}: {}", if k == i { "\u{25b8} " } else { "  " }, who(msgs[k].0), clip(&msgs[k].1));
            }
        }
    }
    Ok(())
}

/// `topterm focus TTY`: bring the tab to the front (Terminal.app).
pub fn focus(snap: &Snapshot, tty: &str) -> io::Result<()> {
    let s = find(snap, tty)?;
    if crate::hibernate::reveal(&s.tty) {
        Ok(())
    } else {
        Err(io::Error::other(format!("could not bring {} to the front (Terminal.app only)", s.tty)))
    }
}

/// `topterm hibernate TTY` / `topterm hibernate --stale [DAYS]`. Prints the
/// plan; acts only with `yes`. Returns false if any session was skipped.
pub fn hibernate(snap: &Snapshot, tty: Option<&str>, stale_days: Option<u64>, yes: bool, json: bool, fake: bool) -> io::Result<bool> {
    if fake && yes {
        return Err(io::Error::other("--fake-data never hibernates; drop --yes"));
    }
    let targets: Vec<&Session> = match (tty, stale_days) {
        (Some(t), _) => vec![find(snap, t)?],
        (None, Some(days)) => {
            let (now, limit) = (app::now_ms(), days * 86_400_000);
            let mut v: Vec<&Session> = snap
                .sessions
                .iter()
                .filter(|s| s.claude.as_ref().is_some_and(|c| c.is_idle() && c.status_since_ms > 0 && now.saturating_sub(c.status_since_ms) >= limit))
                .collect();
            v.sort_by_key(|s| s.claude.as_ref().map_or(0, |c| c.status_since_ms));
            v
        }
        (None, None) => return Err(io::Error::other("hibernate needs a TTY or --stale [DAYS]")),
    };
    let stamp = crate::hibernate::stamp();
    let (mut all_ok, mut freed, mut out) = (true, 0u64, Vec::new());
    for s in targets {
        let title = snap.tab_titles.get(&s.tty).cloned().unwrap_or_default();
        let plan = if fake {
            crate::fake::plan(&s.tty).ok_or_else(|| format!("No Claude session in {}.", s.tty))
        } else {
            crate::hibernate::plan(s, &title)
        };
        let result: Result<String, String> = match &plan {
            Err(e) => Err(e.clone()),
            Ok(p) => match p.blocked_reason() {
                Some(why) => Err(format!("Not hibernated: {why}")),
                None if !yes => Ok("would hibernate (dry run)".into()),
                None => crate::hibernate::execute(p, &stamp),
            },
        };
        if result.is_ok() {
            freed += plan.as_ref().map_or(0, |p| p.frees_kb);
        } else {
            all_ok = false;
        }
        out.push((s, title, plan.ok(), result));
    }
    if json {
        let items: Vec<String> = out
            .iter()
            .map(|(s, title, plan, r)| {
                format!(
                    "{{\"tty\":{},\"title\":{},\"ok\":{},\"message\":{},\"frees_kb\":{},\"resume\":{}}}",
                    js(&s.tty),
                    js(title),
                    r.is_ok(),
                    js(r.as_ref().unwrap_or_else(|e| e)),
                    plan.as_ref().map_or(0, |p| p.frees_kb),
                    plan.as_ref().map_or("null".into(), |p| js(&p.resume_cmd()))
                )
            })
            .collect();
        println!("{{\"dry_run\":{},\"freed_kb\":{},\"results\":[{}]}}", !yes, freed, items.join(","));
    } else {
        if out.is_empty() {
            println!("No Claude session has been idle {}+ days.", stale_days.unwrap_or(0));
            return Ok(true);
        }
        for (s, title, plan, r) in &out {
            let mark = if r.is_ok() { "\u{2713}" } else { "\u{2717}" };
            println!("{mark} {:<9} {}  {}", s.tty, app::short_text(title, 40), r.as_ref().unwrap_or_else(|e| e));
            if let Some(p) = plan {
                println!("    {}  frees ~{}  resume: {}", p.cwd, app::human_kb(p.frees_kb), p.resume_cmd());
            }
        }
        let verb = if yes { "freed" } else { "would free" };
        println!("{verb} ~{}{}", app::human_kb(freed), if yes { "" } else { "  (dry run: add --yes to hibernate)" });
    }
    Ok(all_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_strings_escape() {
        assert_eq!(js("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
    }

    #[test]
    fn snapshot_json_is_balanced_and_lists_every_session() {
        let snap = crate::fake::snapshot(crate::fake::STEPS - 1);
        let j = snapshot_json(&snap);
        assert_eq!(j.matches('{').count(), j.matches('}').count());
        assert_eq!(j.matches("\"tty\":").count(), snap.sessions.len() + snap.detached.len());
        assert!(j.contains("\"label\":\"Docker Desktop VM\""));
    }

    #[test]
    fn sort_names_cover_every_column() {
        for n in ["tty", "mem", "delta", "cpu", "procs", "agent", "idle", "tab", "recent", "cwd"] {
            assert!(parse_sort(n).is_some(), "{n}");
        }
        assert!(parse_sort("bogus").is_none());
    }
}
