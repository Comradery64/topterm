//! App state, input handling, and rendering.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::Frame;

use crate::collector::{ProcInfo, Session, Snapshot, GROWING_THRESHOLD_KB};
use crate::theme::{
    self, bar_cells, braille_grid, braille_line, ease_out_cubic, ramp, shade, Theme, AMBER, BASE,
    BORDER, EMBER, MINT, MUTED, PANEL, SCRIM, SEL_BG, SELECT_MARK, TEXT, ZEBRA_A, ZEBRA_B,
};

/// Samples of RSS history kept per tty for the sparkline.
const HISTORY_LEN: usize = 120;
/// How many recent samples the GROWING flag looks back across.
const GROWING_WINDOW: usize = 15;
/// Height of the detail panel when fully expanded.
const DETAIL_H: f64 = 16.0;
/// Duration of the expand/collapse ease.
const ANIM_MS: f64 = 150.0;
/// Half-period of the GROWING pulse (500ms on, 500ms off == 1Hz).
const PULSE_MS: u128 = 500;
/// Width of the inline RSS bar in the table.
const ROW_BAR_W: usize = 7;

/// TUI application state.
#[derive(Debug)]
pub struct App {
    /// Latest snapshot from the collector thread.
    pub snapshot: Option<Snapshot>,
    /// Selected row index in the session table (sessions then detached).
    pub selected: usize,
    /// Whether the detail view for the selected session is open.
    pub detail_open: bool,
    /// Whether the kill-confirmation overlay is open.
    pub confirm_kill: bool,
    /// Set to true to exit the event loop.
    pub should_quit: bool,
    /// Resolved palette (also carries the `--no-color` decision).
    pub theme: Theme,
    /// One-line status message shown in the footer.
    pub status: Option<String>,
    /// Ring buffer of total RSS (KB) per tty key, newest last.
    history: HashMap<String, VecDeque<u64>>,
    /// Ring buffer of machine-wide summed RSS, for the header sparkline.
    total_history: VecDeque<u64>,
    /// Key (tty) of the selected row, used to keep selection stable across
    /// re-sorted snapshots.
    selected_key: Option<String>,
    table_state: TableState,
    /// Screen y of the first data row of the table (set during draw).
    table_first_data_y: u16,
    /// Number of visible data rows in the table (set during draw).
    table_data_height: u16,
    /// Table area x range for mouse hit-testing (set during draw).
    table_x: (u16, u16),
    /// Table scroll offset at last draw.
    table_offset: usize,
    /// Rendered height of each table row — rows are 1 tall except the first
    /// detached row, which carries the section rule above it.
    row_heights: Vec<u16>,
    /// Monotonic clock the pulse and the ease read from.
    clock: Instant,
    /// In-flight expand/collapse: (started, from_height, to_height).
    anim: Option<(Instant, f64, f64)>,
    /// Current (possibly mid-ease) detail panel height in rows.
    detail_h: f64,
    /// Open hibernate preview, if any.
    pub hibernate: Option<HibUi>,
    /// Open bulk-hibernate overlay, if any.
    pub bulk: Option<BulkUi>,
    /// Bulk hibernate threshold in days (--stale-days, default 7).
    pub stale_days: u64,
    pub sort_col: SortCol,
    pub sort_desc: bool,
    /// Header cell x ranges from the last draw, for click-to-sort.
    header_cells: Vec<(u16, u16)>,
    header_y: u16,
    /// Row filter typed after `/`; empty means no filter.
    pub search: String,
    /// True while the filter is being typed.
    pub search_typing: bool,
    /// Unfiltered rows from the last snapshot; `snapshot` holds the
    /// filtered, sorted view.
    raw: Option<Snapshot>,
    /// Per Claude session id: lowercased recap and last ask, for search.
    search_text: HashMap<String, String>,
    search_job: Option<std::sync::mpsc::Receiver<HashMap<String, String>>>,
    search_loaded: Option<Instant>,
    /// Full-text hits for `ft_query`: session id -> snippet.
    ft_hits: HashMap<String, String>,
    ft_query: String,
    ft_job: Option<std::sync::mpsc::Receiver<(String, HashMap<String, String>)>>,
    /// Bumped per full-text search; a running scan stops when it changes.
    ft_gen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// When to start the next full-text search (debounced typing).
    ft_due: Option<Instant>,
    /// Open search-match preview (`p`), if any.
    pub preview: Option<PreviewUi>,
}

/// The conversation around a search match in one session.
#[derive(Debug)]
pub struct PreviewUi {
    pub tty: String,
    pub title: String,
    /// Chat messages, oldest first: (typed by the user, text).
    msgs: Vec<(bool, String)>,
    /// Indices into `msgs` that match, newest first.
    matches: Vec<usize>,
    /// Current position in `matches`.
    pos: usize,
    words: Vec<String>,
}

/// Table column the rows are sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortCol {
    Tty,
    Mem,
    Delta,
    Cpu,
    Procs,
    Claude,
    Tab,
    Recent,
    Cwd,
}

/// Column order as drawn (the trailing flag column is not sortable).
pub const SORT_COLS: [SortCol; 9] = [
    SortCol::Tty,
    SortCol::Mem,
    SortCol::Delta,
    SortCol::Cpu,
    SortCol::Procs,
    SortCol::Claude,
    SortCol::Tab,
    SortCol::Recent,
    SortCol::Cwd,
];

impl SortCol {
    /// Numbers start biggest-first; text starts A-Z.
    fn default_desc(self) -> bool {
        matches!(self, SortCol::Mem | SortCol::Delta | SortCol::Cpu | SortCol::Procs | SortCol::Claude)
    }
}

/// Order two sessions by `col`, ascending. Rows with nothing to sort on
/// (no Claude, no title) are reported as `None` so callers keep them last
/// in either direction. Pure.
pub fn sort_key_cmp(
    a: &Session,
    b: &Session,
    col: SortCol,
    titles: &HashMap<String, String>,
    now_ms: u64,
) -> Option<std::cmp::Ordering> {
    let label = |s: &Session| s.recent_command.as_deref().map(norm_cmd).unwrap_or_default();
    // Idle sessions by how long they've been idle; busy ones count as 0.
    let claude_age = |s: &Session| {
        s.claude.as_ref().map(|c| if c.is_idle() { now_ms.saturating_sub(c.status_since_ms) } else { 0 })
    };
    Some(match col {
        SortCol::Tty => tty_num(&a.tty).cmp(&tty_num(&b.tty)),
        SortCol::Mem => a.total_mem_kb.cmp(&b.total_mem_kb),
        SortCol::Delta => a.delta_mem_kb.cmp(&b.delta_mem_kb),
        SortCol::Cpu => a.total_cpu_pct.total_cmp(&b.total_cpu_pct),
        SortCol::Procs => a.proc_count.cmp(&b.proc_count),
        SortCol::Claude => claude_age(a)?.cmp(&claude_age(b)?),
        SortCol::Tab => {
            let t = |s: &Session| titles.get(&s.tty).map(|t| t.trim_start_matches(|c: char| !c.is_alphanumeric()).to_lowercase());
            t(a)?.cmp(&t(b)?)
        }
        SortCol::Recent => label(a).cmp(&label(b)),
        SortCol::Cwd => a.cwd.to_lowercase().cmp(&b.cwd.to_lowercase()),
    })
}

/// Split `line` into spans, styling every occurrence of any of `words`
/// (already ASCII-lowercased) with `hi`. Pure.
pub fn highlight(line: &str, words: &[String], base: Style, hi: Style) -> Vec<Span<'static>> {
    let lower = line.to_ascii_lowercase();
    let mut marks = vec![false; line.len()];
    for w in words.iter().filter(|w| !w.is_empty()) {
        for (at, _) in lower.match_indices(w.as_str()) {
            marks[at..at + w.len()].iter_mut().for_each(|m| *m = true);
        }
    }
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..=line.len() {
        if i == line.len() || (line.is_char_boundary(i) && marks[i] != marks[start]) {
            if i == line.len() || line.is_char_boundary(i) {
                out.push(Span::styled(line[start..i].to_string(), if marks[start] { hi } else { base }));
                start = i;
            }
        }
    }
    out
}

/// True if every word of `query` appears (case-insensitive) in the
/// session's tty, folder, tab title, commands, Claude session name, or
/// `extra` (its recap and last ask, already lowercased). Pure.
pub fn session_matches(sess: &Session, title: Option<&String>, extra: Option<&String>, query: &str) -> bool {
    let mut hay = format!("{} {} {}", sess.tty, sess.cwd, title.map_or("", |t| t.as_str()));
    if let Some(c) = &sess.claude {
        hay.push(' ');
        hay.push_str(c.name.as_deref().unwrap_or(""));
    }
    for p in &sess.procs {
        hay.push(' ');
        hay.push_str(&p.command);
    }
    let mut hay = hay.to_lowercase();
    if let Some(e) = extra {
        hay.push(' ');
        hay.push_str(e);
    }
    query.to_lowercase().split_whitespace().all(|w| hay.contains(w))
}

fn tty_num(tty: &str) -> u32 {
    tty.trim_start_matches(|c: char| !c.is_ascii_digit()).parse().unwrap_or(u32::MAX)
}

/// Sort sessions in place by `col`; rows without a value go last, ties by memory.
pub fn sort_sessions(sessions: &mut [Session], col: SortCol, desc: bool, titles: &HashMap<String, String>, now_ms: u64) {
    use std::cmp::Ordering;
    let has = |s: &Session| match col {
        SortCol::Claude => s.claude.is_some(),
        SortCol::Tab => titles.contains_key(&s.tty),
        _ => true,
    };
    sessions.sort_by(|a, b| match (has(a), has(b)) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => b.total_mem_kb.cmp(&a.total_mem_kb),
        (true, true) => {
            let o = sort_key_cmp(a, b, col, titles, now_ms).unwrap_or(Ordering::Equal);
            let o = if desc { o.reverse() } else { o };
            o.then(b.total_mem_kb.cmp(&a.total_mem_kb)).then(a.tty.cmp(&b.tty))
        }
    });
}

/// State of the bulk "hibernate everything idle N+ days" overlay.
#[derive(Debug)]
pub struct BulkUi {
    pub days: u64,
    /// Progress / result lines.
    pub log: Vec<String>,
    job: Option<std::sync::mpsc::Receiver<BulkMsg>>,
    pub done: bool,
}

#[derive(Debug)]
enum BulkMsg {
    Line(String),
    Finished(String),
}

/// State of the hibernate preview overlay.
#[derive(Debug)]
pub struct HibUi {
    pub plan: crate::hibernate::Plan,
    /// Result or progress line shown at the bottom of the overlay.
    pub msg: Option<String>,
    /// Background job (summary refresh or the hibernate itself).
    job: Option<std::sync::mpsc::Receiver<HibJob>>,
}

#[derive(Debug)]
enum HibJob {
    Summary(String),
    Done(Result<String, String>),
}

impl Default for App {
    fn default() -> Self {
        Self {
            snapshot: None,
            selected: 0,
            detail_open: false,
            confirm_kill: false,
            hibernate: None,
            bulk: None,
            stale_days: 7,
            sort_col: SortCol::Mem,
            sort_desc: true,
            header_cells: Vec::new(),
            header_y: 0,
            search: String::new(),
            search_typing: false,
            raw: None,
            search_text: HashMap::new(),
            search_job: None,
            search_loaded: None,
            ft_hits: HashMap::new(),
            ft_query: String::new(),
            ft_job: None,
            ft_gen: Default::default(),
            ft_due: None,
            preview: None,
            should_quit: false,
            theme: Theme::default(),
            status: None,
            history: HashMap::new(),
            total_history: VecDeque::new(),
            selected_key: None,
            table_state: TableState::default(),
            table_first_data_y: 0,
            table_data_height: 0,
            table_x: (0, 0),
            table_offset: 0,
            row_heights: Vec::new(),
            clock: Instant::now(),
            anim: None,
            detail_h: 0.0,
        }
    }
}

impl App {
    pub fn new(no_color: bool) -> Self {
        Self { theme: Theme::detect(no_color), ..Self::default() }
    }

    // ---------------------------------------------------------------- data

    pub fn on_snapshot(&mut self, mut snap: Snapshot) {
        // Update per-tty RSS history and (re)derive delta/growing from it —
        // the collector is stateless per call, so the UI owns the history.
        let mut seen: Vec<String> = Vec::new();
        for sess in snap.sessions.iter_mut().chain(snap.detached.iter_mut()) {
            let hist = self.history.entry(sess.tty.clone()).or_default();
            if let Some(&prev) = hist.back() {
                sess.delta_mem_kb = sess.total_mem_kb as i64 - prev as i64;
            } else {
                sess.delta_mem_kb = 0;
            }
            let lookback = hist.iter().rev().take(GROWING_WINDOW).min().copied();
            if let Some(base) = lookback {
                sess.growing = sess.total_mem_kb >= base + GROWING_THRESHOLD_KB;
            }
            hist.push_back(sess.total_mem_kb);
            while hist.len() > HISTORY_LEN {
                hist.pop_front();
            }
            seen.push(sess.tty.clone());
        }
        self.history.retain(|k, _| seen.iter().any(|s| s == k));
        self.total_history.push_back(snap.total_mem_kb);
        while self.total_history.len() > HISTORY_LEN {
            self.total_history.pop_front();
        }
        self.raw = Some(snap);
        self.apply_sort();
    }

    /// Rebuild the visible rows from the raw snapshot: filter by the
    /// search, sort, and keep the cursor on the same tty.
    fn apply_sort(&mut self) {
        let (col, desc, now) = (self.sort_col, self.sort_desc, now_ms());
        if let Some(raw) = &self.raw {
            let mut snap = raw.clone();
            if !self.search.trim().is_empty() {
                let (q, text) = (&self.search, &self.search_text);
                let ft = if self.ft_query == *q { Some(&self.ft_hits) } else { None };
                let keep = |x: &Session| {
                    let id = x.claude.as_ref().map(|c| &c.session_id);
                    let extra = id.and_then(|i| text.get(i));
                    session_matches(x, raw.tab_titles.get(&x.tty), extra, q)
                        || id.zip(ft).is_some_and(|(i, h)| h.contains_key(i))
                };
                snap.sessions.retain(|x| keep(x));
                snap.detached.retain(|x| keep(x));
            }
            sort_sessions(&mut snap.sessions, col, desc, &snap.tab_titles, now);
            self.snapshot = Some(snap);
        }
        self.resolve_selection();
    }

    /// Load each Claude session's recap and last ask in the background so
    /// search can match on what a session was about. Reloads at most every
    /// five minutes.
    fn start_search_load(&mut self) {
        if self.search_job.is_some() || self.search_loaded.is_some_and(|t| t.elapsed() < Duration::from_secs(300)) {
            return;
        }
        let ids: Vec<String> = self
            .raw
            .iter()
            .flat_map(|s| s.sessions.iter())
            .filter_map(|x| x.claude.as_ref().map(|c| c.session_id.clone()))
            .collect();
        let (tx, rx) = std::sync::mpsc::channel();
        self.search_job = Some(rx);
        std::thread::spawn(move || {
            let variants = crate::claude::variants(&crate::claude::home());
            let mut out = HashMap::new();
            for (id, (_, path)) in crate::claude::find_transcripts(&variants, &ids) {
                let mut t = crate::hibernate::best_summary(&path, &id).map(|s| s.text).unwrap_or_default();
                if let Some(ask) = crate::claude::read_tail(&path, 512 * 1024)
                    .and_then(|raw| crate::claude::last_exchange(&raw).0)
                {
                    t.push(' ');
                    t.push_str(&ask);
                }
                out.insert(id, t.to_lowercase());
            }
            let _ = tx.send(out);
        });
    }

    /// The query changed: schedule a full-text search once typing pauses.
    fn search_changed(&mut self) {
        self.ft_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.ft_job = None;
        self.ft_due = (self.search.trim().len() >= 3).then(|| Instant::now() + Duration::from_millis(350));
        self.apply_sort();
        self.jump_to_top();
    }

    /// Scan every open Claude session's chat for the query, in the background.
    fn start_full_text(&mut self) {
        self.ft_due = None;
        let query = self.search.clone();
        let ids: Vec<String> = self
            .raw
            .iter()
            .flat_map(|s| s.sessions.iter())
            .filter_map(|x| x.claude.as_ref().map(|c| c.session_id.clone()))
            .collect();
        let gen = self.ft_gen.clone();
        let mine = gen.load(std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.ft_job = Some(rx);
        std::thread::spawn(move || {
            let cancelled = || gen.load(std::sync::atomic::Ordering::Relaxed) != mine;
            let words = crate::search::words(&query);
            let variants = crate::claude::variants(&crate::claude::home());
            let mut hits = HashMap::new();
            for (id, (_, path)) in crate::claude::find_transcripts(&variants, &ids) {
                if cancelled() {
                    return;
                }
                if let Some(snip) = crate::search::search_transcript(&path, &words, &cancelled) {
                    hits.insert(id, snip);
                }
            }
            let _ = tx.send((query, hits));
        });
    }

    fn poll_full_text(&mut self) {
        if self.ft_due.is_some_and(|t| Instant::now() >= t) {
            self.start_full_text();
        }
        let Some(rx) = &self.ft_job else { return };
        match rx.try_recv() {
            Ok((q, hits)) => {
                self.ft_job = None;
                if q == self.search {
                    self.ft_query = q;
                    self.ft_hits = hits;
                    self.apply_sort();
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.ft_job = None,
            Err(_) => {}
        }
    }

    /// Show the conversation around the selected session's search match.
    fn open_preview(&mut self) {
        let words = crate::search::words(&self.search);
        let Some(sess) = self.selected_session() else { return };
        if words.is_empty() {
            self.status = Some("press / and search first; p previews the match".into());
            return;
        }
        let Some(live) = sess.claude.as_ref() else {
            self.status = Some("no Claude conversation in this tab to preview".into());
            return;
        };
        let tty = sess.tty.clone();
        let title = self.snapshot.as_ref().and_then(|s| s.tab_titles.get(&tty)).cloned().unwrap_or_default();
        let variants = crate::claude::variants(&crate::claude::home());
        let path = crate::claude::find_transcripts(&variants, std::slice::from_ref(&live.session_id))
            .remove(&live.session_id)
            .map(|(_, p)| p);
        let msgs = path.map(|p| crate::search::chat_messages(&p)).unwrap_or_default();
        self.show_preview(tty, title, msgs, words);
    }

    fn show_preview(&mut self, tty: String, title: String, msgs: Vec<(bool, String)>, words: Vec<String>) {
        let (matches, pos) = crate::search::match_list(&msgs, &words);
        if matches.is_empty() {
            self.status = Some("this session's chat doesn't mention that".into());
            return;
        }
        self.status = None;
        self.preview = Some(PreviewUi { tty, title, msgs, matches, pos, words });
    }

    /// Search `chats` (tty -> messages) as the full-text scan would and open
    /// the preview on the first match. For `--demo-search` screenshots.
    pub fn demo_search(&mut self, query: &str, chats: Vec<(String, Vec<(bool, String)>)>) {
        let words = crate::search::words(query);
        self.search = query.to_string();
        self.ft_query = query.to_string();
        let ids: HashMap<String, String> = self
            .raw
            .iter()
            .flat_map(|s| s.sessions.iter())
            .filter_map(|x| x.claude.as_ref().map(|c| (x.tty.clone(), c.session_id.clone())))
            .collect();
        for (tty, msgs) in &chats {
            // Forward order: max_by_key keeps the last maximum, the newest.
            let best = msgs
                .iter()
                .filter_map(|(_, t)| crate::search::match_text(t, &words))
                .max_by_key(|(hits, _)| *hits);
            if let (Some(id), Some((_, snip))) = (ids.get(tty), best) {
                self.ft_hits.insert(id.clone(), snip);
            }
        }
        self.apply_sort();
        let Some((tty, msgs)) = chats.into_iter().find(|(t, _)| self.select_tty(t)) else { return };
        let title = self.snapshot.as_ref().and_then(|s| s.tab_titles.get(&tty)).cloned().unwrap_or_default();
        self.show_preview(tty, title, msgs, words);
    }

    fn handle_preview_key(&mut self, code: KeyCode) {
        let Some(p) = self.preview.as_mut() else { return };
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') => self.preview = None,
            KeyCode::Char('n') | KeyCode::Down | KeyCode::Char('j') => {
                p.pos = (p.pos + 1).min(p.matches.len() - 1);
            }
            KeyCode::Char('N') | KeyCode::Up | KeyCode::Char('k') => p.pos = p.pos.saturating_sub(1),
            KeyCode::Char('o') => {
                let tty = p.tty.clone();
                self.preview = None;
                self.status = Some(format!("bringing {tty} to the front\u{2026}"));
                std::thread::spawn(move || crate::hibernate::reveal(&tty));
            }
            _ => {}
        }
    }

    /// Snippet of the selected session's chat that matched the search.
    fn selected_snippet(&self) -> Option<&String> {
        if self.ft_query != self.search {
            return None;
        }
        let id = &self.selected_session()?.claude.as_ref()?.session_id;
        self.ft_hits.get(id)
    }

    fn poll_search(&mut self) {
        self.poll_full_text();
        let Some(rx) = &self.search_job else { return };
        match rx.try_recv() {
            Ok(m) => {
                self.search_text = m;
                self.search_job = None;
                self.search_loaded = Some(Instant::now());
                if !self.search.is_empty() {
                    self.apply_sort();
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.search_job = None,
            Err(_) => {}
        }
    }

    fn handle_search_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.search.clear();
                self.search_typing = false;
                self.search_changed();
            }
            KeyCode::Enter => self.search_typing = false,
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Backspace => {
                self.search.pop();
                self.search_changed();
            }
            KeyCode::Char(c) => {
                self.search.push(c);
                self.search_changed();
            }
            _ => {}
        }
    }

    /// Sort by `col`; choosing the current column again flips the direction.
    pub fn sort_by(&mut self, col: SortCol) {
        if self.sort_col == col {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort_col = col;
            self.sort_desc = col.default_desc();
        }
        self.apply_sort();
        self.jump_to_top();
    }

    /// After the user changes the sort, show the top of the new order.
    fn jump_to_top(&mut self) {
        if self.item_count() > 0 {
            self.set_selected(0);
        }
    }

    /// Selectable rows: sessions first, then detached pseudo-sessions.
    fn item_count(&self) -> usize {
        self.snapshot
            .as_ref()
            .map(|s| s.sessions.len() + s.detached.len())
            .unwrap_or(0)
    }

    fn item(&self, idx: usize) -> Option<&Session> {
        let snap = self.snapshot.as_ref()?;
        if idx < snap.sessions.len() {
            snap.sessions.get(idx)
        } else {
            snap.detached.get(idx - snap.sessions.len())
        }
    }

    fn selected_session(&self) -> Option<&Session> {
        self.item(self.selected)
    }

    /// Re-find the previously selected tty after rows re-sorted; clamp otherwise.
    fn resolve_selection(&mut self) {
        let count = self.item_count();
        if count == 0 {
            self.selected = 0;
            self.set_detail(false);
            self.confirm_kill = false;
            return;
        }
        if let Some(key) = self.selected_key.clone() {
            for i in 0..count {
                if self.item(i).map(|s| s.tty == key).unwrap_or(false) {
                    self.selected = i;
                    return;
                }
            }
            // The session under the cursor vanished; selection is about to
            // move to a DIFFERENT session, so a pending kill confirmation
            // must not carry over to it.
            if self.confirm_kill {
                self.confirm_kill = false;
                self.status = Some(format!("kill cancelled: session {key} is gone"));
            }
        }
        if self.selected >= count {
            self.selected = count - 1;
        }
        self.selected_key = self.item(self.selected).map(|s| s.tty.clone());
    }

    fn set_selected(&mut self, idx: usize) {
        let count = self.item_count();
        if count == 0 {
            return;
        }
        self.selected = idx.min(count - 1);
        self.selected_key = self.item(self.selected).map(|s| s.tty.clone());
    }

    fn move_selection(&mut self, delta: i64) {
        let count = self.item_count();
        if count == 0 {
            return;
        }
        let cur = self.selected as i64;
        let next = (cur + delta).clamp(0, count as i64 - 1) as usize;
        self.set_selected(next);
    }

    // --------------------------------------------------------------- motion

    /// Open/close the detail panel, easing the split rather than snapping it.
    fn set_detail(&mut self, open: bool) {
        if self.detail_open == open {
            return;
        }
        self.detail_open = open;
        let to = if open { DETAIL_H } else { 0.0 };
        self.anim = Some((Instant::now(), self.detail_h, to));
    }

    /// Advance the animation clock. Call once per event-loop iteration.
    pub fn tick(&mut self) {
        self.poll_hibernate();
        self.poll_bulk();
        self.poll_search();
        if let Some((start, from, to)) = self.anim {
            let t = start.elapsed().as_secs_f64() * 1000.0 / ANIM_MS;
            if t >= 1.0 {
                self.detail_h = to;
                self.anim = None;
            } else {
                self.detail_h = from + (to - from) * ease_out_cubic(t);
            }
        }
    }

    /// Jump any in-flight animation to its end state (used by `--demo-frame`).
    pub fn settle(&mut self) {
        self.anim = None;
        self.detail_h = if self.detail_open { DETAIL_H } else { 0.0 };
    }

    /// True while something on screen needs ~30fps: an easing panel, or a
    /// GROWING row whose flag is pulsing.
    pub fn needs_fast_tick(&self) -> bool {
        if self.hibernate.as_ref().is_some_and(|h| h.job.is_some())
            || self.bulk.as_ref().is_some_and(|b| b.job.is_some())
            || self.search_job.is_some()
            || self.ft_job.is_some()
            || self.ft_due.is_some()
        {
            return true;
        }
        if self.anim.is_some() {
            return true;
        }
        self.snapshot
            .as_ref()
            .map(|s| s.sessions.iter().chain(s.detached.iter()).any(|x| x.growing))
            .unwrap_or(false)
    }

    /// Bright half of the 1Hz GROWING pulse.
    fn pulse_bright(&self) -> bool {
        (self.clock.elapsed().as_millis() / PULSE_MS) % 2 == 0
    }

    // --------------------------------------------------------------- input

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        // Ctrl-C always quits.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        if self.hibernate.is_some() {
            self.handle_hibernate_key(key.code);
            return;
        }
        if self.bulk.is_some() {
            self.handle_bulk_key(key.code);
            return;
        }
        if self.confirm_kill {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm_kill = false;
                    self.kill_selected();
                }
                _ => {
                    self.confirm_kill = false;
                    self.status = Some("kill cancelled".to_string());
                }
            }
            return;
        }
        if self.preview.is_some() {
            self.handle_preview_key(key.code);
            return;
        }
        if self.search_typing {
            self.handle_search_key(key.code);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('p') => self.open_preview(),
            KeyCode::Char('/') => {
                self.search_typing = true;
                self.status = None;
                self.start_search_load();
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Enter => {
                if self.selected_session().is_some() {
                    let open = !self.detail_open;
                    self.set_detail(open);
                }
            }
            KeyCode::Esc => {
                if !self.detail_open && !self.search.is_empty() {
                    self.search.clear();
                    self.search_changed();
                } else {
                    self.set_detail(false);
                }
            }
            KeyCode::Char('K') => {
                if self.selected_session().is_some() {
                    self.confirm_kill = true;
                    self.status = None;
                }
            }
            KeyCode::Char('H') => self.open_hibernate(),
            KeyCode::Char('o') => {
                if let Some(tty) = self.selected_session().map(|s| s.tty.clone()) {
                    self.status = Some(format!("bringing {tty} to the front\u{2026}"));
                    // AppleScript can take seconds with 100+ tabs; don't block the UI.
                    std::thread::spawn(move || crate::hibernate::reveal(&tty));
                }
            }
            KeyCode::Char('Z') => {
                self.status = None;
                self.bulk = Some(BulkUi { days: self.stale_days, log: Vec::new(), job: None, done: false });
            }
            KeyCode::Char('s') => {
                let i = SORT_COLS.iter().position(|&c| c == self.sort_col).unwrap_or(0);
                let next = SORT_COLS[(i + 1) % SORT_COLS.len()];
                self.sort_col = next;
                self.sort_desc = next.default_desc();
                self.apply_sort();
                self.jump_to_top();
            }
            KeyCode::Char('r') => {
                self.sort_desc = !self.sort_desc;
                self.apply_sort();
                self.jump_to_top();
            }
            _ => {}
        }
    }

    /// Idle Claude sessions at or past `days`, oldest first.
    pub fn stale_sessions(&self, days: u64) -> Vec<&Session> {
        let now = now_ms();
        let limit = days * 86_400_000;
        let mut v: Vec<&Session> = self
            .raw
            .as_ref()
            .map(|s| s.sessions.iter().collect())
            .unwrap_or_default();
        v.retain(|s| {
            s.claude
                .as_ref()
                .is_some_and(|c| c.is_idle() && c.status_since_ms > 0 && now.saturating_sub(c.status_since_ms) >= limit)
        });
        v.sort_by_key(|s| s.claude.as_ref().map_or(0, |c| c.status_since_ms));
        v
    }

    fn handle_bulk_key(&mut self, code: KeyCode) {
        let Some(b) = self.bulk.as_ref() else { return };
        if b.job.is_some() {
            return;
        }
        let (days, done) = (b.days, b.done);
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.bulk = None,
            _ if done => {}
            KeyCode::Char('+') | KeyCode::Char('=') => {
                if let Some(b) = self.bulk.as_mut() {
                    b.days += 1;
                }
            }
            KeyCode::Char('-') => {
                if let Some(b) = self.bulk.as_mut() {
                    b.days = b.days.saturating_sub(1).max(1);
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let titles = self.snapshot.as_ref().map(|s| s.tab_titles.clone()).unwrap_or_default();
                let targets: Vec<Session> = self.stale_sessions(days).into_iter().cloned().collect();
                if targets.is_empty() {
                    return;
                }
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let stamp = crate::hibernate::stamp();
                    let (mut ok, mut skipped, mut freed) = (0, 0, 0u64);
                    for sess in targets {
                        let title = titles.get(&sess.tty).cloned().unwrap_or_default();
                        let res = crate::hibernate::plan(&sess, &title)
                            .and_then(|plan| crate::hibernate::execute(&plan, &stamp).map(|m| (m, plan.frees_kb)));
                        let line = match res {
                            Ok((_, kb)) => {
                                ok += 1;
                                freed += kb;
                                format!("\u{2713} {} {}", sess.tty, short_text(&title, 50))
                            }
                            Err(e) => {
                                skipped += 1;
                                format!("\u{2717} {} {}", sess.tty, short_text(&e, 70))
                            }
                        };
                        if tx.send(BulkMsg::Line(line)).is_err() {
                            return;
                        }
                    }
                    let _ = tx.send(BulkMsg::Finished(format!(
                        "Hibernated {ok} \u{b7} skipped {skipped} \u{b7} freed ~{}",
                        human_kb(freed)
                    )));
                });
                if let Some(b) = self.bulk.as_mut() {
                    b.job = Some(rx);
                    b.log.push("Working through them one at a time\u{2026}".into());
                }
            }
            _ => {}
        }
    }

    fn poll_bulk(&mut self) {
        let Some(b) = self.bulk.as_mut() else { return };
        let Some(rx) = &b.job else { return };
        loop {
            match rx.try_recv() {
                Ok(BulkMsg::Line(l)) => b.log.push(l),
                Ok(BulkMsg::Finished(m)) => {
                    b.log.push(m.clone());
                    self.status = Some(m);
                    b.job = None;
                    b.done = true;
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(_) => {
                    b.job = None;
                    return;
                }
            }
        }
    }

    /// Select the row for `tty`; false if there is none.
    pub fn select_tty(&mut self, tty: &str) -> bool {
        let idx = self.snapshot.as_ref().and_then(|s| s.sessions.iter().position(|x| x.tty == tty));
        if let Some(i) = idx {
            self.set_selected(i);
        }
        idx.is_some()
    }

    /// Open the preview with a prepared plan (used by --fake-data screenshots).
    pub fn show_hibernate_plan(&mut self, plan: crate::hibernate::Plan) {
        self.hibernate = Some(HibUi { plan, msg: None, job: None });
    }

    /// Open the preview for the selected row, returning the reason if not possible.
    pub fn open_hibernate_preview(&mut self) -> Result<(), String> {
        self.open_hibernate();
        match self.hibernate {
            Some(_) => Ok(()),
            None => Err(self.status.clone().unwrap_or_else(|| "no session selected".into())),
        }
    }

    /// Build a read-only hibernate plan for the selected row and open the preview.
    fn open_hibernate(&mut self) {
        let Some(sess) = self.selected_session().cloned() else { return };
        let title = self
            .snapshot
            .as_ref()
            .and_then(|s| s.tab_titles.get(&sess.tty).cloned())
            .unwrap_or_default();
        match crate::hibernate::plan(&sess, &title) {
            Ok(plan) => {
                self.status = None;
                self.hibernate = Some(HibUi { plan, msg: None, job: None });
            }
            Err(e) => self.status = Some(e),
        }
    }

    fn handle_hibernate_key(&mut self, code: KeyCode) {
        let Some(h) = self.hibernate.as_mut() else { return };
        if h.job.is_some() {
            return; // a job is running; keys wait for it
        }
        let done = h.msg.as_deref().is_some_and(|m| m.starts_with("Hibernated"));
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if let Some(m) = h.msg.take().filter(|_| done) {
                    self.status = Some(m);
                }
                self.hibernate = None;
            }
            _ if done => {}
            KeyCode::Char('R') | KeyCode::Char('r') => {
                h.msg = Some(if crate::hibernate::reveal(&h.plan.tty) {
                    "Tab brought to the front. Come back here to continue.".into()
                } else {
                    "Could not bring the tab to the front.".into()
                });
            }
            KeyCode::Char('S') | KeyCode::Char('s') => {
                let Some(path) = h.plan.transcript.clone() else {
                    h.msg = Some("No transcript found for this session.".into());
                    return;
                };
                let id = h.plan.live.session_id.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let text = crate::index::summarize_one(&path, &id, &crate::index::summary_cache_dir());
                    let _ = tx.send(HibJob::Summary(text));
                });
                h.msg = Some("Asking Haiku for a fresh summary\u{2026}".into());
                h.job = Some(rx);
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(why) = h.plan.blocked_reason() {
                    h.msg = Some(format!("Not hibernated: {why}"));
                    return;
                }
                let plan = h.plan.clone();
                let stamp = crate::hibernate::stamp();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(HibJob::Done(crate::hibernate::execute(&plan, &stamp)));
                });
                h.msg = Some("Checking the session, then asking Claude to exit\u{2026}".into());
                h.job = Some(rx);
            }
            _ => {}
        }
    }

    /// Collect a finished hibernate-overlay job, if any.
    fn poll_hibernate(&mut self) {
        let Some(h) = self.hibernate.as_mut() else { return };
        let Some(rx) = &h.job else { return };
        match rx.try_recv() {
            Ok(HibJob::Summary(text)) => {
                h.plan.summary = Some(crate::hibernate::Summary {
                    source: crate::hibernate::Source::Topterm,
                    text,
                    age_ms: 0,
                    messages_since: 0,
                });
                h.msg = None;
                h.job = None;
            }
            Ok(HibJob::Done(res)) => {
                h.msg = Some(match res {
                    Ok(m) => m,
                    Err(e) => format!("\u{2717} {e}"),
                });
                h.job = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(_) => h.job = None,
        }
    }

    /// Map a screen y to a table row index, honouring variable row heights.
    fn row_at(&self, y: u16) -> Option<usize> {
        let mut top = self.table_first_data_y;
        let end = self.table_first_data_y + self.table_data_height;
        let mut idx = self.table_offset;
        while idx < self.row_heights.len() && top < end {
            let h = self.row_heights[idx].max(1);
            if y >= top && y < top + h {
                return Some(idx);
            }
            top += h;
            idx += 1;
        }
        None
    }

    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        if self.confirm_kill || self.hibernate.is_some() || self.bulk.is_some() {
            return; // overlay is keyboard-only
        }
        match ev.kind {
            MouseEventKind::ScrollUp => self.move_selection(-1),
            MouseEventKind::ScrollDown => self.move_selection(1),
            MouseEventKind::Down(MouseButton::Left) if ev.row == self.header_y => {
                if let Some(i) = self.header_cells.iter().position(|&(a, b)| ev.column >= a && ev.column < b) {
                    if let Some(&col) = SORT_COLS.get(i) {
                        self.sort_by(col);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let (x0, x1) = self.table_x;
                if ev.column < x0 || ev.column >= x1 {
                    return;
                }
                let row = match self.row_at(ev.row) {
                    Some(r) if r < self.item_count() => r,
                    _ => return,
                };
                if row == self.selected && self.detail_open {
                    self.set_detail(false); // click again collapses
                } else {
                    self.set_selected(row);
                    self.set_detail(true);
                }
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- kill

    /// Send SIGTERM to every distinct process group of the selected session.
    fn kill_selected(&mut self) {
        let pgids: Vec<i32> = match self.selected_session() {
            Some(s) => s.pgids.iter().copied().filter(|&g| g > 1).collect(),
            None => return,
        };
        if pgids.is_empty() {
            self.status = Some("no killable process groups".to_string());
            return;
        }
        let mut ok = 0usize;
        let mut errs: Vec<String> = Vec::new();
        for pgid in &pgids {
            // Negative pid == the whole process group.
            let rc = unsafe { libc::kill(-pgid, libc::SIGTERM) };
            if rc == 0 {
                ok += 1;
            } else {
                errs.push(format!("pgid {}: {}", pgid, std::io::Error::last_os_error()));
            }
        }
        self.status = Some(if errs.is_empty() {
            format!("SIGTERM sent to {} process group(s)", ok)
        } else {
            format!("SIGTERM to {} group(s); errors: {}", ok, errs.join("; "))
        });
    }

    // --------------------------------------------------------------- draw

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let th = self.theme;
        if !th.plain() {
            frame
                .buffer_mut()
                .set_style(area, Style::default().bg(th.color(BASE)).fg(th.color(TEXT)));
        }

        let emu_count = self
            .snapshot
            .as_ref()
            .map(|s| s.emulators.len().min(6))
            .unwrap_or(0);
        let emu_h = if emu_count > 0 { emu_count as u16 + 1 } else { 0 };
        let vm_count = self.snapshot.as_ref().map(|s| s.vms.len().min(3)).unwrap_or(0);
        let vm_h = if vm_count > 0 { vm_count as u16 + 1 } else { 0 };
        let detail_h = self.detail_h.round().max(0.0) as u16;

        let chunks = Layout::vertical([
            Constraint::Length(3),        // hero header
            Constraint::Min(5),           // table
            Constraint::Length(detail_h), // detail
            Constraint::Length(emu_h),    // emulators
            Constraint::Length(vm_h),     // virtual machines
            Constraint::Length(1),        // footer
        ])
        .split(area);

        self.draw_header(frame, chunks[0]);
        self.draw_table(frame, chunks[1]);
        if detail_h >= 4 {
            self.draw_detail(frame, chunks[2]);
        }
        if let Some(snap) = &self.snapshot {
            let mut emus: Vec<(String, &ProcInfo)> =
                snap.emulators.iter().map(|p| (short_cmd(&p.command, 20), p)).collect();
            emus.sort_by_key(|e| std::cmp::Reverse(e.1.mem_kb));
            self.draw_proc_rows(frame, chunks[3], "emulators", &emus);
            let vms: Vec<(String, &ProcInfo)> = snap.vms.iter().map(|v| (v.label.clone(), &v.proc)).collect();
            self.draw_proc_rows(frame, chunks[4], "virtual machines", &vms);
        }
        self.draw_footer(frame, chunks[5]);
        if self.confirm_kill {
            self.draw_kill_overlay(frame, area);
        }
        if self.hibernate.is_some() {
            self.draw_hibernate_overlay(frame, area);
        }
        if self.bulk.is_some() {
            self.draw_bulk_overlay(frame, area);
        }
        if self.preview.is_some() {
            self.draw_preview_overlay(frame, area);
        }
    }

    fn draw_preview_overlay(&self, frame: &mut Frame, area: Rect) {
        let Some(p) = &self.preview else { return };
        let th = self.theme;
        self.draw_scrim(frame, area);
        let width = area.width.saturating_sub(4).min(110);
        let height = area.height.saturating_sub(2);
        let inner = width.saturating_sub(4) as usize;
        let at = p.matches[p.pos];
        // Messages around the match; neighbours are capped, the match is not.
        let mut body: Vec<Line> = Vec::new();
        let mut focus = 0;
        for i in at.saturating_sub(3)..(at + 4).min(p.msgs.len()) {
            let (user, text) = &p.msgs[i];
            let here = i == at;
            if here {
                focus = body.len();
            }
            let (who, color) = if *user { ("you", MINT) } else { ("claude", AMBER) };
            body.push(Line::from(Span::styled(
                format!("{who}{}", if here { "  \u{25c2} match" } else { "" }),
                th.modifier(th.fg(color), Modifier::BOLD),
            )));
            let wrapped: Vec<String> =
                text.lines().flat_map(|l| wrap_chars(l, inner.saturating_sub(2))).filter(|l| !l.trim().is_empty()).collect();
            let cap = if here { usize::MAX } else { 4 };
            for l in wrapped.iter().take(cap) {
                let style = if here { th.fg(TEXT) } else { th.dim() };
                let mut spans = vec![Span::raw("  ")];
                spans.extend(highlight(l, &p.words, style, th.modifier(th.fg(AMBER), Modifier::BOLD | Modifier::UNDERLINED)));
                body.push(Line::from(spans));
            }
            if wrapped.len() > cap {
                body.push(Line::from(Span::styled(format!("  \u{2026} {} more lines", wrapped.len() - cap), th.dim())));
            }
            body.push(Line::from(""));
        }
        // Scroll so the first highlighted line of the match sits a third down.
        let body_h = height.saturating_sub(6) as usize;
        let hit = body[focus..]
            .iter()
            .position(|l| l.spans.iter().any(|s| s.style.add_modifier.contains(Modifier::UNDERLINED)))
            .map_or(focus, |k| focus + k);
        let start = hit.saturating_sub(body_h / 3).min(focus).min(body.len().saturating_sub(body_h));
        let mut lines = vec![
            Line::from(vec![
                Span::styled(format!("{} ", p.tty), th.modifier(th.fg(MINT), Modifier::BOLD)),
                Span::styled(truncate_chars(&p.title, inner.saturating_sub(30)), th.fg(TEXT)),
                Span::styled(format!("  match {} of {}", p.pos + 1, p.matches.len()), th.dim()),
            ]),
            Line::from(""),
        ];
        lines.extend(body.into_iter().skip(start).take(body_h));
        let key = |k: &str| Span::styled(format!(" {k} "), th.modifier(th.fg(BASE).bg(th.color(MINT)), Modifier::BOLD));
        lines.push(Line::from(vec![
            key("n"),
            Span::raw(" older   "),
            key("N"),
            Span::raw(" newer   "),
            key("o"),
            Span::raw(" go to tab   "),
            key("Esc"),
            Span::raw(" close"),
        ]));
        let popup = centered_rect(area, width, (lines.len() as u16 + 2).min(height));
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .border_type(if th.plain() { BorderType::Plain } else { BorderType::Rounded })
                    .border_style(th.fg(MINT))
                    .title(inset_title(&format!("\u{201c}{}\u{201d} in this conversation", self.search.trim()), th, MINT))
                    .style(if th.plain() { Style::default() } else { Style::default().bg(th.color(PANEL)) }),
            ),
            popup,
        );
    }

    fn draw_bulk_overlay(&self, frame: &mut Frame, area: Rect) {
        let Some(b) = &self.bulk else { return };
        let th = self.theme;
        self.draw_scrim(frame, area);
        let width = area.width.saturating_sub(4).min(100);
        let inner = width.saturating_sub(4) as usize;
        let titles = self.snapshot.as_ref().map(|s| &s.tab_titles);
        let targets = self.stale_sessions(b.days);
        let now = now_ms();
        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(
                format!("\u{1f4a4} Hibernate every Claude session idle {}+ days", b.days),
                th.modifier(th.fg(MINT), Modifier::BOLD),
            )),
            Line::from(""),
        ];
        let mut total = 0u64;
        let mut eligible = 0usize;
        if b.log.is_empty() {
            if targets.is_empty() {
                lines.push(Line::from(Span::styled("  None. Press - to lower the threshold.", th.dim())));
            }
            let room = (area.height as usize).saturating_sub(12);
            for s in targets.iter().take(room) {
                let c = s.claude.as_ref().expect("filtered");
                let blocked = crate::hibernate::blockers(s).filter(|v| !v.is_empty());
                if blocked.is_none() {
                    total += s.claude_mem_kb;
                    eligible += 1;
                }
                let title = titles.and_then(|t| t.get(&s.tty)).map(String::as_str).unwrap_or("-");
                let head = format!(
                    "  {:<8} idle {:<8} ~{:>8}  ",
                    s.tty,
                    fmt_age(now.saturating_sub(c.status_since_ms)),
                    human_kb(s.claude_mem_kb)
                );
                let room_t = inner.saturating_sub(head.chars().count());
                lines.push(match blocked {
                    None => Line::from(vec![Span::raw(head), Span::raw(truncate_chars(title, room_t))]),
                    Some(v) => Line::from(vec![
                        Span::styled(head, th.dim()),
                        Span::styled(truncate_chars(&format!("skip: running {}", v.join(", ")), room_t), th.fg(EMBER)),
                    ]),
                });
            }
            if targets.len() > room {
                lines.push(Line::from(Span::styled(format!("  \u{2026} and {} more", targets.len() - room), th.dim())));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "{eligible} sessions \u{b7} frees ~{} \u{b7} each gets the same checks and card as H",
                human_kb(total)
            )));
        } else {
            let room = (area.height as usize).saturating_sub(8);
            let skip = b.log.len().saturating_sub(room);
            for l in &b.log[skip..] {
                let style = if l.starts_with('\u{2717}') { th.fg(EMBER) } else { th.fg(TEXT) };
                lines.push(Line::from(Span::styled(format!("  {}", truncate_chars(l, inner.saturating_sub(2))), style)));
            }
        }
        lines.push(Line::from(""));
        let key = |k: &str| {
            Span::styled(
                format!(" {k} "),
                if th.plain() {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().fg(th.color(BASE)).bg(th.color(MINT)).add_modifier(Modifier::BOLD)
                },
            )
        };
        lines.push(if b.done || b.job.is_some() {
            Line::from(vec![key("Esc"), Span::raw(if b.done { " close" } else { " (working\u{2026})" })])
        } else {
            Line::from(vec![
                key("+/-"),
                Span::raw(" days   "),
                key("y"),
                Span::raw(" hibernate all   "),
                key("Esc"),
                Span::raw(" cancel"),
            ])
        });
        let height = (lines.len() as u16 + 2).min(area.height);
        let popup = centered_rect(area, width, height);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .border_type(if th.plain() { BorderType::Plain } else { BorderType::Rounded })
                    .border_style(th.fg(MINT))
                    .title(inset_title("sleep stale sessions", th, MINT))
                    .style(if th.plain() { Style::default() } else { Style::default().bg(th.color(PANEL)) }),
            ),
            popup,
        );
    }

    /// Magnitude of a session's RSS relative to the biggest one on screen.
    fn mem_frac(&self, kb: u64) -> f64 {
        let max = self
            .snapshot
            .as_ref()
            .map(|s| {
                s.sessions
                    .iter()
                    .chain(s.detached.iter())
                    .map(|x| x.total_mem_kb)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        if max == 0 {
            0.0
        } else {
            kb as f64 / max as f64
        }
    }

    /// Gradient-colored inline bar, `width` cells wide.
    fn mini_bar(&self, frac: f64, width: usize) -> Vec<Span<'static>> {
        let th = self.theme;
        bar_cells(frac, width)
            .into_iter()
            .map(|(ch, pos)| Span::styled(ch.to_string(), th.fg(ramp(pos))))
            .collect()
    }

    // ------------------------------------------------------------- header

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        if area.height < 3 || area.width == 0 {
            return;
        }
        let th = self.theme;
        let (title_line, frac, label, hist): (Line, f64, String, Vec<u64>) = match &self.snapshot {
            Some(snap) => {
                let phys_kb = (snap.phys_mem_bytes / 1024).max(1);
                let frac = snap.total_mem_kb as f64 / phys_kb as f64;
                let label = format!(
                    "{} / {} \u{b7} {} sessions \u{b7} {} procs",
                    human_kb(snap.total_mem_kb),
                    human_kb(phys_kb),
                    snap.sessions.len(),
                    snap.proc_count,
                );
                let note = snap.tab_title_hint.clone().or_else(|| system_line(snap)).unwrap_or_else(|| {
                    "memory = physical footprint (incl. compressed + swapped), so the sum can exceed RAM"
                        .to_string()
                });
                let line = Line::from(vec![
                    Span::styled("topterm", th.bold(MINT)),
                    Span::styled("  \u{2502}  ", th.fg(BORDER)),
                    Span::styled(note, th.dim()),
                ]);
                (line, frac, label, self.total_history.iter().copied().collect())
            }
            None => (
                Line::from(vec![
                    Span::styled("topterm", th.bold(MINT)),
                    Span::styled("  \u{2502}  ", th.fg(BORDER)),
                    Span::styled("waiting for first sample\u{2026}", th.dim()),
                ]),
                0.0,
                String::new(),
                Vec::new(),
            ),
        };

        frame.render_widget(
            Paragraph::new(title_line),
            Rect { height: 1, ..area },
        );

        let w = area.width as usize;
        let buf = frame.buffer_mut();

        // Row 2: the pressure gauge. Each cell takes its color from its own
        // position along the bar, so the fill reads as one smooth ramp.
        let cells = bar_cells(frac, w);
        let lchars: Vec<char> = label.chars().collect();
        let lx = w.saturating_sub(lchars.len()) / 2;
        let y = area.y + 1;
        for (i, (ch, pos)) in cells.iter().enumerate() {
            let x = area.x + i as u16;
            let col = ramp(*pos);
            let filled = *ch != ' ';
            match lchars.get(i.wrapping_sub(lx)).filter(|_| i >= lx) {
                Some(&lc) => {
                    let base = if filled { th.on(BASE, col) } else { th.on(TEXT, PANEL) };
                    put(buf, x, y, lc, th.modifier(base, Modifier::BOLD));
                }
                None => put(buf, x, y, *ch, th.on(col, PANEL)),
            }
        }

        // Row 3: machine-wide RSS history as a braille sparkline.
        let y = area.y + 2;
        for (i, (ch, mag)) in braille_line(&hist, w).into_iter().enumerate() {
            put(buf, area.x + i as u16, y, ch, th.fg(shade(ramp(mag), 0.45)));
        }
    }

    // -------------------------------------------------------------- table

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        let th = self.theme;
        let pulse = self.pulse_bright();
        let mut rows: Vec<Row> = Vec::new();
        let mut heights: Vec<u16> = Vec::new();
        let mut boundary: Option<usize> = None;

        let fracs: Vec<f64> = {
            let mut v = Vec::new();
            if let Some(snap) = &self.snapshot {
                for s in snap.sessions.iter().chain(snap.detached.iter()) {
                    v.push(self.mem_frac(s.total_mem_kb));
                }
            }
            v
        };

        if let Some(snap) = &self.snapshot {
            let first_detached = snap.sessions.len();
            for (i, sess) in snap.sessions.iter().chain(snap.detached.iter()).enumerate() {
                let detached = i >= first_detached;
                if detached && boundary.is_none() {
                    boundary = Some(i);
                }
                // Left unclipped: the Fill-weighted columns get the real width
                // and ratatui clips to whatever is actually available.
                let mut label = norm_cmd(&sess.shell_name);
                if let Some(rec) = &sess.recent_command {
                    label.push_str(" / ");
                    label.push_str(&norm_cmd(rec));
                }
                let tab = snap.tab_titles.get(&sess.tty).cloned().unwrap_or_else(|| "-".into());

                let frac = fracs.get(i).copied().unwrap_or(0.0);
                let mut mem_spans = vec![
                    Span::styled(format!("{:>9}", human_kb(sess.total_mem_kb)), th.fg(ramp(frac))),
                    Span::raw(" "),
                ];
                mem_spans.extend(self.mini_bar(frac, ROW_BAR_W));

                let flag = if sess.growing {
                    let c = if pulse { AMBER } else { shade(AMBER, 0.55) };
                    Line::from(Span::styled("GROWING", th.modifier(th.fg(c), Modifier::BOLD)))
                } else if detached {
                    Line::from(Span::styled("DETACHED", th.dim()))
                } else {
                    Line::from("")
                };

                let lines: Vec<Line> = vec![
                    Line::from(Span::styled(sess.tty.clone(), th.fg(TEXT))),
                    Line::from(mem_spans),
                    Line::from(Span::styled(fmt_delta(sess.delta_mem_kb), self.delta_style(sess.delta_mem_kb))),
                    Line::from(format!("{:.1}", sess.total_cpu_pct)),
                    Line::from(Span::styled(format!("{}", sess.proc_count), th.dim())),
                    Line::from(Span::styled(
                        claude_cell(sess, now_ms()),
                        match &sess.claude {
                            Some(c) if c.is_idle() => th.fg(AMBER),
                            Some(_) => th.fg(MINT),
                            None => th.fg(TEXT),
                        },
                    )),
                    Line::from(tab),
                    Line::from(label),
                    Line::from(Span::styled(sess.cwd.clone(), th.dim())),
                    flag,
                ];
                // The first detached row is two rows tall; its blank top line
                // is where the section rule gets painted after the table
                // renders, so the rule scrolls with the row.
                let tall = boundary == Some(i);
                let cells: Vec<Cell> = if tall {
                    lines
                        .into_iter()
                        .map(|l| Cell::from(Text::from(vec![Line::from(""), l])))
                        .collect()
                } else {
                    lines.into_iter().map(Cell::from).collect()
                };
                let h = if tall { 2 } else { 1 };
                heights.push(h);
                let zebra = if i % 2 == 0 { ZEBRA_A } else { ZEBRA_B };
                rows.push(Row::new(cells).height(h).style(if th.plain() {
                    Style::default()
                } else {
                    Style::default().bg(th.color(zebra)).fg(th.color(TEXT))
                }));
            }
        }

        // Numeric columns are fixed; everything left over is split between the
        // three text columns rather than capped at a narrow constant.
        let widths = [
            Constraint::Length(10),
            Constraint::Length(9 + 1 + ROW_BAR_W as u16),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(12),
            Constraint::Fill(3),
            Constraint::Fill(3),
            Constraint::Fill(4),
            Constraint::Length(8),
        ];
        let names = ["TTY", "MEM", "\u{394}MEM", "%CPU", "PROCS", "AGENT", "TAB", "SHELL/RECENT", "CWD"];
        let mut labels: Vec<String> = names
            .iter()
            .zip(SORT_COLS)
            .map(|(n, c)| {
                if c == self.sort_col {
                    format!("{n} {}", if self.sort_desc { "\u{25bc}" } else { "\u{25b2}" })
                } else {
                    n.to_string()
                }
            })
            .collect();
        labels.push(String::new());
        // Same geometry the table uses, so header clicks map to columns:
        // inside the border, after the highlight-symbol gutter, 1-col spacing.
        let gutter = SELECT_MARK.chars().count() as u16;
        let inner = Rect {
            x: area.x + 1 + gutter,
            y: area.y + 1,
            width: area.width.saturating_sub(2 + gutter),
            height: 1,
        };
        self.header_y = inner.y;
        self.header_cells = Layout::horizontal(widths).spacing(1).split(inner).iter().map(|r| (r.x, r.x + r.width)).collect();
        let header = Row::new(labels)
        .style(th.modifier(th.on(MUTED, PANEL), Modifier::BOLD));

        let block = Block::bordered()
            .border_type(if th.plain() { BorderType::Plain } else { BorderType::Rounded })
            .border_style(th.fg(BORDER))
            .title(inset_title("sessions", th, MINT));

        let sel_style = if th.plain() {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().bg(th.color(SEL_BG)).add_modifier(Modifier::BOLD)
        };
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .row_highlight_style(sel_style)
            .highlight_symbol(Span::styled(SELECT_MARK, th.fg(MINT)))
            .highlight_spacing(HighlightSpacing::Always);

        if self.item_count() > 0 {
            self.table_state.select(Some(self.selected));
        } else {
            self.table_state.select(None);
        }
        frame.render_stateful_widget(table, area, &mut self.table_state);

        // Remember geometry for mouse hit-testing: border(1) + header(1).
        self.table_first_data_y = area.y + 2;
        self.table_data_height = area.height.saturating_sub(3);
        self.table_x = (area.x, area.x + area.width);
        self.table_offset = self.table_state.offset();
        self.row_heights = heights;

        if let Some(b) = boundary {
            if let Some(y) = self.row_y(b) {
                let x = area.x + 1;
                let w = area.width.saturating_sub(2);
                draw_rule(frame.buffer_mut(), x, y, w, "detached", th);
            }
        }
    }

    /// Screen y of the top line of table row `idx`, if it is on screen.
    fn row_y(&self, idx: usize) -> Option<u16> {
        if idx < self.table_offset {
            return None;
        }
        let end = self.table_first_data_y + self.table_data_height;
        let mut y = self.table_first_data_y;
        for i in self.table_offset..idx {
            y += self.row_heights.get(i).copied().unwrap_or(1);
        }
        if y < end {
            Some(y)
        } else {
            None
        }
    }

    fn delta_style(&self, kb: i64) -> Style {
        let th = self.theme;
        if kb < 0 {
            th.fg(MINT)
        } else if kb >= GROWING_THRESHOLD_KB as i64 {
            th.modifier(th.fg(EMBER), Modifier::BOLD)
        } else if kb > 0 {
            th.fg(AMBER)
        } else {
            th.dim()
        }
    }

    // ------------------------------------------------------------- detail

    fn draw_detail(&self, frame: &mut Frame, area: Rect) {
        let sess = match self.selected_session() {
            Some(s) => s.clone(),
            None => return,
        };
        let th = self.theme;
        let tab = self
            .snapshot
            .as_ref()
            .and_then(|s| s.tab_titles.get(&sess.tty))
            .cloned()
            .map(|t| truncate_chars(&t, 40))
            .unwrap_or_else(|| sess.tty.clone());
        let block = Block::bordered()
            .border_type(if th.plain() { BorderType::Plain } else { BorderType::Rounded })
            .border_style(th.fg(BORDER))
            .title(inset_title(&tab, th, MINT))
            .title(Line::from(vec![
                Span::styled(
                    format!(
                        "\u{2574}{} \u{b7} {} \u{b7} {} procs \u{b7} {}\u{2576}",
                        sess.tty,
                        human_kb(sess.total_mem_kb),
                        sess.proc_count,
                        sess.cwd
                    ),
                    th.dim(),
                ),
            ])
            .right_aligned());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 2 || inner.width == 0 {
            return;
        }

        let chart_h = 6u16.min(inner.height.saturating_sub(2));
        let parts =
            Layout::vertical([Constraint::Min(1), Constraint::Length(chart_h)]).split(inner);

        // --- process tree
        let max_proc_mem = sess.procs.iter().map(|p| p.mem_kb).max().unwrap_or(0);
        let prefixes = tree_prefixes(&sess.depths);
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            format!("{:>7} {:>7} {:>10} {:>ROW_BAR_W$} {:>6}  COMMAND", "PID", "PGID", "MEM", "", "%CPU"),
            th.modifier(th.dim(), Modifier::BOLD),
        ))];
        for (i, p) in sess.procs.iter().enumerate() {
            let frac = if max_proc_mem == 0 { 0.0 } else { p.mem_kb as f64 / max_proc_mem as f64 };
            let mut spans = vec![
                Span::styled(format!("{:>7}", p.pid), th.dim()),
                Span::styled(format!(" {:>7}", p.pgid), th.dim()),
                Span::styled(format!(" {:>10} ", human_kb(p.mem_kb)), th.fg(ramp(frac))),
            ];
            spans.extend(self.mini_bar(frac, ROW_BAR_W));
            spans.push(Span::styled(format!(" {:>6.1}  ", p.cpu_pct), th.fg(TEXT)));
            spans.push(Span::styled(
                prefixes.get(i).cloned().unwrap_or_default(),
                th.fg(BORDER),
            ));
            spans.push(Span::styled(p.command.clone(), th.fg(TEXT)));
            lines.push(Line::from(spans));
        }
        // Wrapped, not truncated: the full command line is the point of this view.
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), parts[0]);

        // --- braille RSS history chart
        let data: Vec<u64> = self
            .history
            .get(&sess.tty)
            .map(|h| h.iter().copied().collect())
            .unwrap_or_default();
        let carea = parts[1];
        if carea.height == 0 || carea.width == 0 {
            return;
        }
        let rows = carea.height.saturating_sub(1) as usize;
        let grid = braille_grid(&data, carea.width as usize, rows);
        let buf = frame.buffer_mut();
        for (r, row) in grid.iter().enumerate() {
            for (c, (ch, mag)) in row.iter().enumerate() {
                put(
                    buf,
                    carea.x + c as u16,
                    carea.y + r as u16,
                    *ch,
                    th.fg(ramp(*mag)),
                );
            }
        }
        let lo = data.iter().copied().min().unwrap_or(0);
        let hi = data.iter().copied().max().unwrap_or(0);
        let now = data.last().copied().unwrap_or(0);
        let annot = Line::from(vec![
            Span::styled(format!("{} samples", data.len()), th.dim()),
            Span::styled("   min ", th.dim()),
            Span::styled(human_kb(lo), th.fg(MINT)),
            Span::styled("   max ", th.dim()),
            Span::styled(human_kb(hi), th.fg(EMBER)),
            Span::styled("   now ", th.dim()),
            Span::styled(human_kb(now), th.fg(TEXT)),
        ]);
        frame.render_widget(
            Paragraph::new(annot),
            Rect { y: carea.y + rows as u16, height: 1, ..carea },
        );
    }

    // ---------------------------------------------------------- emulators

    /// A ruled section of named processes (emulators, VMs), biggest bar first.
    fn draw_proc_rows(&self, frame: &mut Frame, area: Rect, title: &str, rows: &[(String, &ProcInfo)]) {
        if area.height == 0 || rows.is_empty() {
            return;
        }
        let th = self.theme;
        let max = rows.iter().map(|(_, p)| p.mem_kb).max().unwrap_or(0);
        let mut lines: Vec<Line> = Vec::new();
        for (label, p) in rows.iter().take(area.height as usize - 1) {
            let frac = if max == 0 { 0.0 } else { p.mem_kb as f64 / max as f64 };
            let mut spans = vec![
                Span::styled(format!("  {:<20}", truncate_chars(label, 20)), th.fg(TEXT)),
                Span::styled(format!("{:>10} ", human_kb(p.mem_kb)), th.fg(ramp(frac))),
            ];
            spans.extend(self.mini_bar(frac, ROW_BAR_W));
            spans.push(Span::styled(format!(" {:>6.1}", p.cpu_pct), th.fg(TEXT)));
            spans.push(Span::styled(format!("   pid {}", p.pid), th.dim()));
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(lines), Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area });
        draw_rule(frame.buffer_mut(), area.x, area.y, area.width, title, th);
    }

    // ------------------------------------------------------------- footer

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let th = self.theme;
        let chip = |key: &str, label: &str| -> Vec<Span<'static>> {
            vec![
                Span::styled(
                    format!(" {key} {label} "),
                    if th.plain() {
                        Style::default()
                    } else {
                        Style::default().fg(th.color(BASE)).bg(th.color(MUTED))
                    },
                ),
                Span::styled(" ", th.dim()),
            ]
        };
        let mut spans = vec![Span::raw(" ")];
        if self.search_typing || !self.search.is_empty() {
            let cursor = if self.search_typing { "\u{258f}" } else { "" };
            spans.push(Span::styled(format!("/{}{cursor} ", self.search), th.modifier(th.fg(MINT), Modifier::BOLD)));
            let loading = if self.ft_job.is_some() || self.ft_due.is_some() || self.search_job.is_some() {
                " \u{b7} searching chats\u{2026}"
            } else {
                ""
            };
            spans.push(Span::styled(format!("{} match{}{loading} ", self.item_count(), if self.item_count() == 1 { "" } else { "es" }), th.dim()));
            if let Some(snip) = self.selected_snippet() {
                spans.push(Span::styled(format!("\u{201c}{snip}\u{201d} "), th.fg(AMBER)));
            }
            if self.search_typing {
                spans.extend(chip("\u{21b5}", "done"));
                spans.extend(chip("esc", "clear"));
                frame.render_widget(Paragraph::new(Line::from(spans)), area);
                return;
            }
        }
        spans.extend(chip("\u{21b5}", "expand"));
        spans.extend(chip("esc", "collapse"));
        spans.extend(chip("j/k", "move"));
        spans.extend(chip("click", "select"));
        if !self.search.is_empty() {
            spans.extend(chip("p", "preview match"));
        }
        spans.extend(chip("/", "search"));
        spans.extend(chip("s/r", "sort"));
        spans.extend(chip("o", "go to tab"));
        spans.extend(chip("H", "hibernate"));
        spans.extend(chip("Z", "sleep stale"));
        spans.extend(chip("K", "kill"));
        spans.extend(chip("q", "quit"));
        // One stat() per frame; cheap enough to stay live.
        let idx = match crate::index::auto_age() {
            Some(a) => format!("index {}", fmt_age(a.as_millis() as u64)),
            None => "index building\u{2026}".to_string(),
        };
        spans.push(Span::styled(format!(" {idx}"), th.dim()));
        if let Some(msg) = &self.status {
            spans.push(Span::styled(format!(" {msg}"), th.fg(AMBER)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    // --------------------------------------------------- hibernate overlay

    fn draw_hibernate_overlay(&self, frame: &mut Frame, area: Rect) {
        let Some(h) = &self.hibernate else { return };
        let th = self.theme;
        let p = &h.plan;
        self.draw_scrim(frame, area);
        let width = area.width.saturating_sub(4).min(96);
        let inner = width.saturating_sub(4) as usize;
        let label = |k: &str| Span::styled(format!("{k:<9}"), th.dim());
        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled("\u{1f4a4} Hibernate this session?", th.modifier(th.fg(MINT), Modifier::BOLD))),
            Line::from(""),
            Line::from(Span::styled(
                format!(
                    "{}{}",
                    p.tty,
                    if p.title.is_empty() { String::new() } else { format!(" \u{b7} {}", truncate_chars(&p.title, 50)) }
                ),
                th.fg(TEXT),
            )),
            Line::from(vec![label("Folder"), Span::raw(truncate_chars(&p.cwd, inner.saturating_sub(9)))]),
            Line::from(vec![
                label("Session"),
                Span::raw(format!(
                    "{}{} \u{b7} frees ~{}",
                    p.live.name.clone().map(|n| format!("{n} \u{b7} ")).unwrap_or_default(),
                    match (p.live.is_idle(), p.live.status_since_ms) {
                        (true, 0) => "idle".to_string(),
                        (true, t) => format!("idle {}", fmt_age(now_ms().saturating_sub(t))),
                        (false, _) => "busy".to_string(),
                    },
                    human_kb(p.frees_kb)
                )),
            ]),
            Line::from(""),
        ];
        let quoted = |s: &Option<String>| match s {
            Some(t) => format!("\"{}\"", short_text(t, inner.saturating_sub(17))),
            None => "(not found)".to_string(),
        };
        lines.push(Line::from(vec![Span::styled("You last asked   ", th.dim()), Span::raw(quoted(&p.last_prompt))]));
        lines.push(Line::from(vec![Span::styled("Claude replied   ", th.dim()), Span::raw(quoted(&p.last_reply))]));
        match &p.summary {
            Some(s) => {
                let when = if s.age_ms == 0 {
                    "just now".to_string()
                } else if s.messages_since > 0 {
                    format!("as of {} ago, {} messages since", fmt_age(s.age_ms), s.messages_since)
                } else {
                    format!("as of {} ago, up to date", fmt_age(s.age_ms))
                };
                lines.push(Line::from(vec![
                    Span::styled("Summary          ", th.dim()),
                    Span::styled(format!("({}, {when}) ", s.source.label()), th.dim()),
                ]));
                for l in wrap_chars(&short_text(&s.text, 500), inner.saturating_sub(17)).into_iter().take(5) {
                    lines.push(Line::from(format!("                 {l}")));
                }
            }
            None => lines.push(Line::from(vec![
                Span::styled("Summary          ", th.dim()),
                Span::styled("none yet", th.dim()),
            ])),
        }
        lines.push(Line::from(Span::styled(
            "                 [S] new topterm summary (one Haiku call, ~26k tokens of plan usage)",
            th.dim(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("\u{2500} last lines on this tab's screen {}", "\u{2500}".repeat(inner.saturating_sub(33))),
            th.fg(BORDER),
        )));
        if p.screen.is_empty() {
            lines.push(Line::from(Span::styled("  (could not read the tab's screen)", th.dim())));
        }
        for l in &p.screen {
            lines.push(Line::from(Span::styled(format!("  {}", truncate_chars(l, inner.saturating_sub(2))), th.fg(MUTED))));
        }
        lines.push(Line::from(Span::styled("\u{2500}".repeat(inner), th.fg(BORDER))));
        lines.push(Line::from(""));
        match p.blocked_reason() {
            Some(why) => lines.push(Line::from(Span::styled(format!("Can't hibernate: {why}"), th.fg(EMBER)))),
            None => {
                lines.push(Line::from("The tab stays open with its history. Claude exits; resume anytime with:"));
                lines.push(Line::from(Span::styled(format!("  {}", p.resume_cmd()), th.modifier(th.fg(TEXT), Modifier::BOLD))));
            }
        }
        lines.push(Line::from(""));
        let key = |k: &str| {
            Span::styled(
                format!(" {k} "),
                if th.plain() {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().fg(th.color(BASE)).bg(th.color(MINT)).add_modifier(Modifier::BOLD)
                },
            )
        };
        let done = h.msg.as_deref().is_some_and(|m| m.starts_with("Hibernated"));
        if done {
            lines.push(Line::from(vec![key("Esc"), Span::raw(" close")]));
        } else {
            lines.push(Line::from(vec![
                key("R"),
                Span::raw(" reveal tab   "),
                key("y"),
                Span::raw(" hibernate   "),
                key("Esc"),
                Span::raw(" cancel"),
            ]));
        }
        if let Some(m) = &h.msg {
            lines.push(Line::from(""));
            let style = if m.starts_with('\u{2717}') || m.starts_with("Not") { th.fg(EMBER) } else { th.fg(AMBER) };
            lines.push(Line::from(Span::styled(truncate_chars(m, inner), style)));
        }
        let height = (lines.len() as u16 + 2).min(area.height);
        let popup = centered_rect(area, width, height);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .border_type(if th.plain() { BorderType::Plain } else { BorderType::Rounded })
                    .border_style(th.fg(MINT))
                    .title(inset_title("hibernate", th, MINT))
                    .style(if th.plain() { Style::default() } else { Style::default().bg(th.color(PANEL)) }),
            ),
            popup,
        );
    }

    // -------------------------------------------------------- kill overlay

    fn draw_kill_overlay(&self, frame: &mut Frame, area: Rect) {
        let sess = match self.selected_session() {
            Some(s) => s,
            None => return,
        };
        let pgids: Vec<i32> = sess.pgids.iter().copied().filter(|&g| g > 1).collect();
        self.draw_scrim(frame, area);

        let h = (pgids.len() as u16 + 7).min(area.height);
        let popup = centered_rect(area, 60, h);
        self.draw_kill_body(frame, popup, sess, &pgids);
    }

    /// Darken everything behind a modal.
    fn draw_scrim(&self, frame: &mut Frame, area: Rect) {
        let th = self.theme;
        if !th.plain() {
            let scrim = th.color(SCRIM);
            let ghost = th.color(shade(MUTED, 0.6));
            let buf = frame.buffer_mut();
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    if let Some(c) = buf.cell_mut((x, y)) {
                        c.set_bg(scrim);
                        c.set_fg(ghost);
                        c.modifier = Modifier::empty();
                    }
                }
            }
        }

    }

    fn draw_kill_body(&self, frame: &mut Frame, popup: Rect, sess: &Session, pgids: &[i32]) {
        let th = self.theme;
        frame.render_widget(Clear, popup);

        let mut lines = vec![
            Line::from(vec![
                Span::styled("SIGTERM ", th.modifier(th.fg(EMBER), Modifier::BOLD)),
                Span::styled(sess.tty.clone(), th.modifier(th.fg(TEXT), Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled(short_cmd(&sess.shell_name, 20), th.fg(TEXT)),
                Span::styled(
                    format!("  \u{b7} {} procs \u{b7} {}", sess.proc_count, human_kb(sess.total_mem_kb)),
                    th.dim(),
                ),
            ]),
            Line::from(""),
        ];
        for g in pgids {
            lines.push(Line::from(vec![
                Span::styled("  pgid ", th.dim()),
                Span::styled(format!("{:<8}", g), th.fg(AMBER)),
                Span::styled("whole group signalled", th.dim()),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                " y ",
                if th.plain() {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().fg(th.color(BASE)).bg(th.color(EMBER)).add_modifier(Modifier::BOLD)
                },
            ),
            Span::styled(" confirm", th.fg(TEXT)),
            Span::styled("   any other key cancels", th.dim()),
        ]));

        let para = Paragraph::new(lines).block(
            Block::bordered()
                .border_type(if th.plain() { BorderType::Plain } else { BorderType::Double })
                .border_style(th.fg(EMBER))
                .title(inset_title("confirm kill", th, EMBER))
                .style(if th.plain() { Style::default() } else { Style::default().bg(th.color(PANEL)) }),
        );
        frame.render_widget(para, popup);
    }
}

// ------------------------------------------------------------ draw helpers

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_style(style);
    }
}

/// `──╴title╶────…` written straight into the buffer.
fn draw_rule(buf: &mut Buffer, x: u16, y: u16, width: u16, title: &str, th: Theme) {
    if width == 0 {
        return;
    }
    let mut text = String::from("\u{2500}\u{2500}\u{2574}");
    text.push_str(title);
    text.push('\u{2576}');
    let head = text.chars().count();
    text.extend(std::iter::repeat('\u{2500}').take((width as usize).saturating_sub(head)));
    for (i, ch) in text.chars().take(width as usize).enumerate() {
        let style = if i >= 3 && i < 3 + title.chars().count() {
            th.dim()
        } else {
            th.fg(BORDER)
        };
        put(buf, x + i as u16, y, ch, style);
    }
}

/// Block title inset into the top border: `╴title╶`.
fn inset_title(title: &str, th: Theme, color: theme::Rgb) -> Line<'static> {
    Line::from(vec![
        Span::styled("\u{2574}", th.fg(BORDER)),
        Span::styled(title.to_string(), th.modifier(th.fg(color), Modifier::BOLD)),
        Span::styled("\u{2576}", th.fg(BORDER)),
    ])
}

/// Box-drawing guides for a depth-annotated, parent-before-children list.
fn tree_prefixes(depths: &[usize]) -> Vec<String> {
    let n = depths.len();
    let mut last = vec![true; n];
    for i in 0..n {
        for j in i + 1..n {
            if depths[j] < depths[i] {
                break;
            }
            if depths[j] == depths[i] {
                last[i] = false;
                break;
            }
        }
    }
    let mut out = Vec::with_capacity(n);
    // stack[k] == "the ancestor at depth k still has siblings below".
    let mut stack: Vec<bool> = Vec::new();
    for i in 0..n {
        let d = depths[i];
        stack.truncate(d);
        let mut p = String::new();
        // Depth 0 has no connector column, so trunks start at depth 1.
        for k in 1..d {
            p.push_str(if stack.get(k) == Some(&true) { "\u{2502}  " } else { "   " });
        }
        if d > 0 {
            p.push_str(if last[i] { "\u{2514}\u{2500} " } else { "\u{251c}\u{2500} " });
        }
        stack.push(!last[i]);
        out.push(p);
    }
    out
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

// -------------------------------------------------------- buffer -> ANSI

fn sgr_color(c: Color, fg: bool) -> String {
    let base = if fg { 3 } else { 4 };
    match c {
        Color::Reset => format!("\x1b[{base}9m"),
        Color::Rgb(r, g, b) => format!("\x1b[{base}8;2;{r};{g};{b}m"),
        Color::Indexed(i) => format!("\x1b[{base}8;5;{i}m"),
        Color::Black => format!("\x1b[{base}0m"),
        Color::Red => format!("\x1b[{base}1m"),
        Color::Green => format!("\x1b[{base}2m"),
        Color::Yellow => format!("\x1b[{base}3m"),
        Color::Blue => format!("\x1b[{base}4m"),
        Color::Magenta => format!("\x1b[{base}5m"),
        Color::Cyan => format!("\x1b[{base}6m"),
        Color::Gray => format!("\x1b[{base}7m"),
        _ => format!("\x1b[{base}9m"),
    }
}

/// Serialize a rendered buffer to ANSI text — the `--demo-frame` path, and the
/// only way the styled TUI can be asserted on outside a real terminal.
pub fn buffer_to_ansi(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in buf.area.top()..buf.area.bottom() {
        let mut cur: Option<(Color, Color, Modifier)> = None;
        for x in buf.area.left()..buf.area.right() {
            let cell = match buf.cell((x, y)) {
                Some(c) => c,
                None => continue,
            };
            let sym = cell.symbol();
            if sym.is_empty() {
                continue; // continuation cell of a wide glyph
            }
            let key = (cell.fg, cell.bg, cell.modifier);
            if cur != Some(key) {
                out.push_str("\x1b[0m");
                out.push_str(&sgr_color(cell.fg, true));
                out.push_str(&sgr_color(cell.bg, false));
                if cell.modifier.contains(Modifier::BOLD) {
                    out.push_str("\x1b[1m");
                }
                if cell.modifier.contains(Modifier::DIM) {
                    out.push_str("\x1b[2m");
                }
                if cell.modifier.contains(Modifier::REVERSED) {
                    out.push_str("\x1b[7m");
                }
                cur = Some(key);
            }
            out.push_str(sym);
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

// ------------------------------------------------------------------ helpers

/// "512 MB" / "2.31 GB" from KB.
pub fn human_kb(kb: u64) -> String {
    let mb = kb as f64 / 1024.0;
    if mb >= 1024.0 * 1024.0 {
        format!("{:.1} TB", mb / 1024.0 / 1024.0)
    } else if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else {
        format!("{:.0} MB", mb)
    }
}

fn fmt_delta(kb: i64) -> String {
    if kb == 0 {
        "0 MB".to_string()
    } else {
        format!("{:+.0} MB", kb as f64 / 1024.0)
    }
}

/// Basename of the executable + args, with nothing truncated.
pub fn norm_cmd(cmd: &str) -> String {
    short_cmd(cmd, usize::MAX)
}

/// Basename of the executable + args, truncated to `width` chars.
pub fn short_cmd(cmd: &str, width: usize) -> String {
    let mut parts = cmd.split_whitespace();
    let exe = match parts.next() {
        Some(p) => p.trim_start_matches('-').rsplit('/').next().unwrap_or(p),
        None => return String::new(),
    };
    let rest: Vec<&str> = parts.collect();
    let s = if rest.is_empty() {
        exe.to_string()
    } else {
        format!("{} {}", exe, rest.join(" "))
    };
    truncate_chars(&s, width)
}

/// Break `s` into chunks of at most `width` chars, preferring a space boundary
/// but hard-splitting tokens longer than the line. Always returns >= 1 chunk.
pub fn wrap_chars(s: &str, width: usize) -> Vec<String> {
    if width == usize::MAX || s.chars().count() <= width {
        return vec![s.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in s.split_whitespace() {
        let wl = word.chars().count();
        if cur_len > 0 && cur_len + 1 + wl > width {
            out.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        if wl > width {
            // Token longer than a whole line: hard-split it.
            for ch in word.chars() {
                if cur_len == width {
                    out.push(std::mem::take(&mut cur));
                    cur_len = 0;
                }
                cur.push(ch);
                cur_len += 1;
            }
            continue;
        }
        if cur_len > 0 {
            cur.push(' ');
            cur_len += 1;
        }
        cur.push_str(word);
        cur_len += wl;
    }
    out.push(cur);
    out
}

/// Collapse whitespace and truncate to `width` characters with an ellipsis.
pub fn short_text(s: &str, width: usize) -> String {
    truncate_chars(&s.split_whitespace().collect::<Vec<_>>().join(" "), width)
}

fn truncate_chars(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let cut: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{}\u{2026}", cut)
}

// -------------------------------------------------------------- once output

const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// Usable output width: $COLUMNS (so piped output honours the caller), then the
/// real terminal size, then a sane default.
pub fn output_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&w| w >= 40)
        .or_else(|| crossterm::terminal::size().ok().map(|(w, _)| w as usize))
        .filter(|&w| w >= 40)
        .unwrap_or(120)
}

/// Truncate to `width` unless `wide`, in which case long lines just overflow.
fn clip(s: &str, width: usize, wide: bool) -> String {
    if wide {
        s.to_string()
    } else {
        truncate_chars(s, width)
    }
}

/// Plain-text one-shot report for `--once` (no TUI). `wide` disables all
/// truncation; otherwise every column past the fixed numeric ones is sized
/// from the real terminal width.
pub fn plain_report(snap: &Snapshot, color: bool, wide: bool) -> String {
    let (b, r, d) = if color { (BOLD, RESET, DIM) } else { ("", "", "") };
    let width = output_width();
    // 10 tty + 10 mem + 9 delta + 6 cpu + 5 procs + 12 claude, one space between each.
    let fixed = 10 + 10 + 9 + 6 + 5 + 12 + 6;
    let rest = (width.saturating_sub(fixed + 2)).max(48);
    let tab_w = (rest * 34 / 100).clamp(14, 48);
    let lbl_w = (rest * 30 / 100).clamp(18, 44);
    let cwd_w = rest.saturating_sub(tab_w + lbl_w).max(12);
    let color_for = |kb: u64| -> &'static str {
        if !color {
            ""
        } else if kb > 2 * 1024 * 1024 {
            RED
        } else if kb > 1024 * 1024 {
            YELLOW
        } else {
            ""
        }
    };

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "{}topterm{}  {} terminal sessions, {} processes  |  memory {} of {} physical",
        b,
        r,
        snap.sessions.len(),
        snap.proc_count,
        human_kb(snap.total_mem_kb),
        human_kb(snap.phys_mem_bytes / 1024),
    ));
    lines.push(format!(
        "{}memory = physical footprint (incl. compressed + swapped), so the sum can exceed RAM{}",
        d, r
    ));
    if let Some(sys) = system_line(snap) {
        lines.push(sys);
    }
    if let Some(hint) = &snap.tab_title_hint {
        lines.push(format!("{}{}{}", d, hint, r));
    }
    lines.push(String::new());
    lines.push(format!(
        "{}{:<10} {:>10} {:>9} {:>6} {:>5} {:<12} {:<tw$} {:<lw$} {}{}",
        b,
        "TTY",
        "MEM",
        "\u{394}MEM",
        "%CPU",
        "PROCS",
        "AGENT",
        "TAB",
        "SHELL/RECENT",
        "CWD",
        r,
        tw = tab_w,
        lw = lbl_w
    ));
    for sess in &snap.sessions {
        let mut label = norm_cmd(&sess.shell_name);
        if let Some(rec) = &sess.recent_command {
            label.push_str(" / ");
            label.push_str(&norm_cmd(rec));
        }
        let tab = snap.tab_titles.get(&sess.tty).map(String::as_str).unwrap_or("-");
        let c = color_for(sess.total_mem_kb);
        let flag = if sess.growing { "   \u{2190} GROWING" } else { "" };
        lines.push(format!(
            "{}{:<10} {:>10} {:>9} {:>6.1} {:>5} {:<12} {:<tw$} {:<lw$} {}{}{}",
            c,
            sess.tty,
            human_kb(sess.total_mem_kb),
            fmt_delta(sess.delta_mem_kb),
            sess.total_cpu_pct,
            sess.proc_count,
            claude_cell(sess, now_ms()),
            clip(tab, tab_w, wide),
            clip(&label, lbl_w, wide),
            clip(&sess.cwd, cwd_w, wide),
            flag,
            if c.is_empty() { "" } else { r },
            tw = tab_w,
            lw = lbl_w,
        ));
    }

    if !snap.emulators.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "{}terminal emulator apps (scrollback buffers){}",
            b, r
        ));
        let mut emus: Vec<&ProcInfo> = snap.emulators.iter().collect();
        emus.sort_by(|a, bb| bb.mem_kb.cmp(&a.mem_kb));
        for p in emus {
            let c = color_for(p.mem_kb);
            lines.push(format!(
                "{}{:<20} {:>10} {:>6.1}  pid {}  (app itself, not a tab){}",
                c,
                short_cmd(&p.command, 20),
                human_kb(p.mem_kb),
                p.cpu_pct,
                p.pid,
                if c.is_empty() { "" } else { r },
            ));
        }
    }

    if !snap.vms.is_empty() {
        lines.push(String::new());
        lines.push(format!("{}virtual machines (guest memory, not owned by any tab){}", b, r));
        for v in &snap.vms {
            let c = color_for(v.proc.mem_kb);
            lines.push(format!(
                "{}{:<28} {:>10} {:>6.1}  pid {}{}",
                c,
                clip(&v.label, 28, false),
                human_kb(v.proc.mem_kb),
                v.proc.cpu_pct,
                v.proc.pid,
                if c.is_empty() { "" } else { r },
            ));
        }
    }

    if !snap.mcp.is_empty() {
        lines.push(String::new());
        lines.push(format!("{}MCP servers (every agent session starts its own copy){}", b, r));
        lines.push(format!("{}{:<28} {:>5} {:>6} {:>10}  ORPHANED{}", d, "SERVER", "TABS", "PROCS", "MEM", r));
        for g in &snap.mcp {
            let orphan = if g.orphaned > 0 {
                format!("{} procs, {} (session gone)", g.orphaned, human_kb(g.orphaned_mem_kb))
            } else {
                "-".to_string()
            };
            lines.push(format!(
                "{:<28} {:>5} {:>6} {:>10}  {}",
                clip(&g.name, 28, false),
                g.tabs,
                g.procs,
                human_kb(g.mem_kb),
                orphan
            ));
        }
    }

    let scroll: Vec<&Session> = {
        let mut v: Vec<&Session> = snap.sessions.iter().filter(|s| s.scrollback_chars.unwrap_or(0) > 0).collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.scrollback_chars));
        v
    };
    if !scroll.is_empty() {
        let total: u64 = scroll.iter().filter_map(|s| s.scrollback_chars).sum();
        let term_kb = snap
            .emulators
            .iter()
            .filter(|p| p.command.contains("Terminal.app"))
            .map(|p| p.mem_kb)
            .sum::<u64>();
        lines.push(String::new());
        lines.push(format!(
            "{}Terminal.app scrollback by tab{}  {}{} chars total; share of Terminal's {} is estimated from each tab's share of characters{}",
            b, r, d, human_count(total), human_kb(term_kb), r
        ));
        for s in scroll.iter().take(if wide { usize::MAX } else { 15 }) {
            let n = s.scrollback_chars.unwrap_or(0);
            let share = if total == 0 { 0 } else { (term_kb as u128 * n as u128 / total as u128) as u64 };
            let tab = snap.tab_titles.get(&s.tty).map(String::as_str).unwrap_or("-");
            lines.push(format!(
                "{:<10} {:>8} chars  \u{2248}{:>9}  {}",
                s.tty,
                human_count(n),
                human_kb(share),
                clip(tab, 48, wide)
            ));
        }
    }

    if !snap.detached.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "{}detached (reparented to launchd - no tab to blame, kill by pid){}",
            b, r
        ));
        // pid + full command line is the only identity these rows have, so the
        // command is never clipped — it wraps underneath itself instead.
        // Matches the head format below: 4 columns, 3 single spaces, 2 trailing.
        let indent = 10 + 10 + 6 + 5 + 3 + 2;
        for sess in &snap.detached {
            let head = format!(
                "{:<10} {:>10} {:>6.1} {:>5}  ",
                sess.tty,
                human_kb(sess.total_mem_kb),
                sess.total_cpu_pct,
                sess.proc_count,
            );
            let cmd = sess.procs.first().map(|p| p.command.as_str()).unwrap_or("");
            let avail = width.saturating_sub(indent).max(30);
            let mut chunks = wrap_chars(cmd, if wide { usize::MAX } else { avail });
            lines.push(format!("{}{}", head, chunks.remove(0)));
            for cont in chunks {
                lines.push(format!("{}{}{}{}", d, " ".repeat(indent), cont, r));
            }
        }
    }

    lines.join("\n")
}

/// "swap 27.0 GB of 28.0 GB · SSD 23.8 TB written in 44d (~540 GB/day)".
pub fn system_line(snap: &Snapshot) -> Option<String> {
    let sys = &snap.system;
    let mut parts = Vec::new();
    if sys.swap_total_bytes > 0 {
        parts.push(format!(
            "swap {} of {}",
            human_kb(sys.swap_used_bytes / 1024),
            human_kb(sys.swap_total_bytes / 1024)
        ));
    }
    if let (Some(w), Some(per_day)) = (sys.ssd_written_bytes, sys.ssd_bytes_per_day()) {
        parts.push(format!(
            "internal SSD {} written in {} (~{}/day)",
            human_kb(w / 1024),
            fmt_age(sys.uptime_secs * 1000),
            human_kb(per_day / 1024)
        ));
    }
    (!parts.is_empty()).then(|| parts.join("  \u{b7}  "))
}

/// "idle 6d 2h" / "busy" for a Claude session, another agent's name
/// ("codex"), or "".
pub fn claude_cell(sess: &Session, now_ms: u64) -> String {
    match &sess.claude {
        Some(c) if c.is_idle() && c.status_since_ms > 0 => {
            format!("idle {}", fmt_age(now_ms.saturating_sub(c.status_since_ms)))
        }
        Some(c) if c.is_idle() => "idle".to_string(),
        Some(_) => "busy".to_string(),
        None => other_agent(sess).unwrap_or_default().to_string(),
    }
}

/// Coding agents besides Claude Code, by executable name. topterm can't
/// read their state, so the AGENT column names them without idle time.
const AGENTS: &[&str] =
    &["codex", "gemini", "aider", "opencode", "goose", "amp", "cursor-agent", "crush", "qwen", "copilot", "droid", "kiro-cli"];

/// The first non-Claude coding agent running in the tab, matched on the
/// executable or the script a node/python launcher runs. Pure.
pub fn other_agent(sess: &Session) -> Option<&'static str> {
    sess.procs.iter().find_map(|p| {
        let mut args = p.command.split_whitespace();
        let first = args.next()?.rsplit('/').next()?;
        let name = if matches!(first, "node" | "bun" | "python" | "python3") {
            args.next()?.rsplit('/').next()?
        } else {
            first
        };
        AGENTS.iter().copied().find(|a| *a == name)
    })
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Milliseconds -> "6d 2h" / "3h 10m" / "4m" / "now".
pub fn fmt_age(ms: u64) -> String {
    let m = ms / 60_000;
    let (d, h, mm) = (m / 1440, (m / 60) % 24, m % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {mm}m")
    } else if mm > 0 {
        format!("{mm}m")
    } else {
        "now".to_string()
    }
}

/// 1_234_567 -> "1.2M".
pub fn human_count(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.0}K", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// Render exactly one styled TUI frame off-screen. Used by `--demo-frame` and
/// by the render tests, so the visuals stay assertable without a real tty.
pub fn render_frame(app: &mut App, width: u16, height: u16) -> Buffer {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut term = Terminal::new(TestBackend::new(width, height))
        .expect("TestBackend terminal cannot fail");
    app.settle();
    // The buffer handed back by `draw` is the one that was just painted;
    // the terminal's "current" buffer has already been swapped by then.
    term.draw(|f| app.draw(f)).expect("TestBackend draw cannot fail").buffer.clone()
}

/// Poll timeout for the event loop: ~30fps while anything is moving,
/// otherwise the (capped) sampling interval.
pub fn poll_timeout(app: &App, interval: Duration) -> Duration {
    if app.needs_fast_tick() {
        Duration::from_millis(33)
    } else {
        interval.min(Duration::from_millis(100))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_kb_units() {
        assert_eq!(human_kb(512 * 1024), "512 MB");
        assert_eq!(human_kb(2 * 1024 * 1024 + 317 * 1024), "2.31 GB");
        assert_eq!(human_kb(0), "0 MB");
        assert_eq!(human_kb(23 * 1024 * 1024 * 1024 + 800 * 1024 * 1024), "23.8 TB");
    }

    #[test]
    fn short_cmd_basename_and_truncate() {
        assert_eq!(short_cmd("/bin/zsh", 20), "zsh");
        assert_eq!(short_cmd("-zsh", 20), "zsh");
        assert_eq!(
            short_cmd("/usr/local/bin/node server.js --port 3000", 15),
            "node server.js\u{2026}"
        );
    }

    fn sess(tty: &str) -> Session {
        Session {
            tty: tty.to_string(),
            shell_pid: 1000,
            shell_name: "zsh".to_string(),
            recent_command: None,
            cwd: "?".to_string(),
            total_mem_kb: 1024,
            delta_mem_kb: 0,
            total_cpu_pct: 0.0,
            proc_count: 1,
            pgids: vec![1000],
            procs: vec![],
            depths: vec![],
            growing: false,
            claude: None,
            claude_mem_kb: 0,
            mcp_mem_kb: 0,
            scrollback_chars: None,
        }
    }

    fn snap(ttys: &[&str]) -> Snapshot {
        Snapshot {
            sessions: ttys.iter().map(|t| sess(t)).collect(),
            detached: vec![],
            emulators: vec![],
            vms: vec![],
            total_mem_kb: 0,
            phys_mem_bytes: 0,
            proc_count: 0,
            tab_titles: HashMap::new(),
            tab_title_hint: None,
            system: Default::default(),
            mcp: Vec::new(),
        }
    }

    #[test]
    fn kill_confirm_cancelled_when_selected_session_vanishes() {
        let mut app = App::new(true);
        app.on_snapshot(snap(&["ttys001", "ttys002"]));
        app.set_selected(1); // ttys002
        app.confirm_kill = true;
        // ttys002 dies; selection must not silently move the pending kill
        // onto a different session.
        app.on_snapshot(snap(&["ttys001", "ttys003"]));
        assert!(!app.confirm_kill);
        assert!(app.status.as_deref().unwrap_or("").contains("ttys002"));
        // But if the selected session survives a re-sort, confirm persists.
        app.set_selected(0); // ttys001
        app.confirm_kill = true;
        app.on_snapshot(snap(&["ttys003", "ttys001"]));
        assert!(app.confirm_kill);
        assert_eq!(app.selected_session().unwrap().tty, "ttys001");
    }

    #[test]
    fn fmt_delta_signs() {
        assert_eq!(fmt_delta(0), "0 MB");
        assert_eq!(fmt_delta(2048), "+2 MB");
        assert_eq!(fmt_delta(-3072), "-3 MB");
    }

    #[test]
    fn wrap_keeps_every_character() {
        let cmd = "/usr/bin/node --inspect server.js --flag averyveryverylongtokenthatcannotfitonaline";
        let chunks = wrap_chars(cmd, 20);
        assert!(chunks.iter().all(|c| c.chars().count() <= 20), "{chunks:?}");
        // Nothing is dropped: rejoining on whitespace round-trips the tokens.
        let rejoined: String = chunks.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(rejoined.replace(' ', ""), cmd.replace(' ', ""));
        // Short input and --wide (usize::MAX) both stay on one line.
        assert_eq!(wrap_chars("short", 20), vec!["short"]);
        assert_eq!(wrap_chars(cmd, usize::MAX), vec![cmd]);
    }

    #[test]
    fn norm_cmd_does_not_truncate() {
        let long = "/usr/local/bin/node server.js --port 3000 --very --long --argument --list";
        assert_eq!(
            norm_cmd(long),
            "node server.js --port 3000 --very --long --argument --list"
        );
    }

    #[test]
    fn tree_prefixes_draw_box_guides() {
        // root, two children, one grandchild under the first child.
        let p = tree_prefixes(&[0, 1, 2, 1]);
        assert_eq!(p[0], "");
        assert_eq!(p[1], "\u{251c}\u{2500} "); // has a sibling below
        assert_eq!(p[2], "\u{2502}  \u{2514}\u{2500} "); // continues the parent's trunk
        assert_eq!(p[3], "\u{2514}\u{2500} "); // last child
        assert!(tree_prefixes(&[]).is_empty());
    }

    // ------------------------------------------------------------ rendering

    fn rich_snapshot() -> Snapshot {
        let proc = |pid: i32, rss: u64, cmd: &str| ProcInfo {
            pid,
            ppid: 1,
            pgid: pid,
            tty: Some("ttys001".into()),
            mem_kb: rss,
            cpu_pct: 1.5,
            etime: "01:00".into(),
            command: cmd.to_string(),
        };
        let mut a = sess("ttys001");
        a.total_mem_kb = 3 * 1024 * 1024;
        a.delta_mem_kb = 120 * 1024;
        a.growing = true;
        a.proc_count = 3;
        a.procs = vec![
            proc(1000, 40 * 1024, "-zsh"),
            proc(1001, 900 * 1024, "node server.js"),
            proc(1002, 200 * 1024, "esbuild"),
        ];
        a.depths = vec![0, 1, 2];
        let mut b = sess("ttys002");
        b.total_mem_kb = 400 * 1024;
        b.delta_mem_kb = -8 * 1024;
        let mut d = sess("pid:4242");
        d.total_mem_kb = 700 * 1024;
        d.procs = vec![proc(4242, 700 * 1024, "python -m http.server")];
        d.depths = vec![0];
        Snapshot {
            sessions: vec![a, b],
            detached: vec![d],
            emulators: vec![proc(300, 1500 * 1024, "/Applications/iTerm.app/.../iTerm2")],
            vms: vec![],
            total_mem_kb: 12 * 1024 * 1024,
            phys_mem_bytes: 18 * 1024 * 1024 * 1024,
            proc_count: 812,
            tab_titles: HashMap::from([("ttys001".to_string(), "claude".to_string())]),
            tab_title_hint: None,
            system: Default::default(),
            mcp: Vec::new(),
        }
    }

    /// Every glyph on screen, as one string.
    fn screen(buf: &Buffer) -> String {
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn styled_frame_draws_the_full_obsidian_ember_language() {
        std::env::set_var("COLORTERM", "truecolor");
        let mut app = App::new(false);
        app.theme = Theme::detect(false);
        // Two samples so the history sparklines have something to draw.
        app.on_snapshot(rich_snapshot());
        app.on_snapshot(rich_snapshot());
        app.detail_open = true;
        let buf = render_frame(&mut app, 140, 40);
        let s = screen(&buf);

        // Gauge + inline bars: eighth-block glyphs.
        assert!(s.contains('\u{2588}'), "no full blocks in frame");
        // Braille history (header sparkline and detail chart).
        assert!(s.chars().any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)), "no braille dots");
        // Rounded panel corners and the inset title brackets.
        assert!(s.contains('\u{256d}') && s.contains('\u{2570}'), "borders are not rounded");
        assert!(s.contains('\u{2574}') && s.contains('\u{2576}'), "no inset title brackets");
        // Section rule for detached rows and the tree guides in the detail pane.
        assert!(s.contains("detached"), "detached rule missing");
        assert!(s.contains('\u{251c}') || s.contains('\u{2514}'), "no tree guides");
        // Selection marker and footer chips.
        assert!(s.contains(SELECT_MARK), "no selected-row marker");
        assert!(s.contains("expand") && s.contains("quit"), "footer chips missing");
        // No emojis anywhere.
        assert!(
            !s.chars().any(|c| c as u32 >= 0x1f300),
            "frame contains an emoji"
        );

        // Truecolor made it into the ANSI serialization, including the ramp.
        let ansi = buffer_to_ansi(&buf);
        assert!(ansi.contains("\x1b[38;2;"), "no truecolor fg sequences");
        assert!(ansi.contains("\x1b[48;2;"), "no truecolor bg sequences");
        assert!(ansi.contains("38;2;45;212;167"), "mint gradient stop missing");
        assert!(ansi.lines().count() >= 40);
    }

    #[test]
    fn no_color_frame_is_unstyled() {
        let mut app = App::new(true);
        app.on_snapshot(rich_snapshot());
        app.detail_open = true;
        let buf = render_frame(&mut app, 120, 30);
        // Nothing carries a color; the selected row falls back to reverse video.
        assert!(
            buf.content().iter().all(|c| c.fg == Color::Reset && c.bg == Color::Reset),
            "--no-color frame has colored cells"
        );
        let ansi = buffer_to_ansi(&buf);
        assert!(!ansi.contains("38;2;"), "--no-color frame emitted truecolor");
    }

    #[test]
    fn expansion_eases_instead_of_snapping() {
        let mut app = App::new(true);
        app.on_snapshot(snap(&["ttys001"]));
        assert_eq!(app.detail_h, 0.0);
        assert!(!app.needs_fast_tick());

        app.set_detail(true);
        assert!(app.needs_fast_tick(), "animation must request the fast tick");
        app.tick();
        // Mid-flight: partially open, not yet at the target.
        assert!(app.detail_h > 0.0 && app.detail_h < DETAIL_H, "{}", app.detail_h);
        assert_eq!(poll_timeout(&app, Duration::from_secs(2)), Duration::from_millis(33));

        app.settle();
        assert_eq!(app.detail_h, DETAIL_H);
        assert!(!app.needs_fast_tick());
        assert_eq!(poll_timeout(&app, Duration::from_secs(2)), Duration::from_millis(100));

        // A visible GROWING row keeps the pulse clock running.
        app.on_snapshot(rich_snapshot());
        app.on_snapshot(rich_snapshot());
        assert!(app.needs_fast_tick(), "GROWING pulse must request the fast tick");
    }

    #[test]
    fn kill_overlay_is_a_scrimmed_ember_modal() {
        std::env::set_var("COLORTERM", "truecolor");
        let mut app = App::new(false);
        app.on_snapshot(rich_snapshot());
        app.confirm_kill = true; // rendering only — no signal is sent
        let buf = render_frame(&mut app, 120, 30);
        let s = screen(&buf);
        assert!(s.contains("SIGTERM") && s.contains("confirm kill"));
        assert!(s.contains("pgid"), "pgid list missing");
        // Double border in ember.
        let ember = Color::Rgb(EMBER.0, EMBER.1, EMBER.2);
        assert!(s.contains('\u{2554}') && s.contains('\u{255d}'), "modal is not double-bordered");
        assert!(buf.content().iter().any(|c| c.fg == ember), "modal is not ember");
        // The backdrop was repainted with the scrim, so the top-left corner of
        // the screen is no longer the base background.
        let scrim = Color::Rgb(SCRIM.0, SCRIM.1, SCRIM.2);
        assert_eq!(buf.cell((0, 0)).unwrap().bg, scrim, "backdrop was not darkened");
        // The confirm key is knocked out of an ember chip.
        assert!(
            buf.content().iter().any(|c| c.bg == ember),
            "no inverse-ember confirm chip"
        );
    }

    #[test]
    fn mouse_click_maps_through_variable_row_heights() {
        let mut app = App::new(true);
        app.on_snapshot(rich_snapshot());
        let _ = render_frame(&mut app, 140, 30);
        // Row 2 is the first detached row and is 2 lines tall (it carries the
        // section rule), so row 1 must still be reachable at its own line.
        let click = |app: &mut App, y: u16| {
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: app.table_x.0 + 2,
                row: y,
                modifiers: KeyModifiers::NONE,
            });
        };
        let y0 = app.table_first_data_y;
        click(&mut app, y0);
        assert_eq!(app.selected, 0);
        click(&mut app, y0 + 1);
        assert_eq!(app.selected, 1);
        // The 2-tall detached row occupies y0+2 and y0+3; both select it.
        click(&mut app, y0 + 3);
        assert_eq!(app.selected, 2);
        assert_eq!(app.selected_session().unwrap().tty, "pid:4242");
        // Clicking the open row again collapses.
        assert!(app.detail_open);
        click(&mut app, y0 + 2);
        assert!(!app.detail_open);
    }

    #[test]
    fn sort_by_claude_idle_keeps_non_claude_last() {
        use crate::claude::Live;
        let live = |since, status: &str| Some(Live {
            pid: 1, session_id: String::new(), name: None, status: status.into(), status_since_ms: since, cmd: "claude".into(),
        });
        let mut a = sess("ttys001");
        a.claude = live(900, "idle"); // idle 100
        let mut b = sess("ttys002");
        b.claude = live(100, "idle"); // idle 900
        let mut c = sess("ttys003");
        c.claude = live(0, "busy");
        let d = sess("ttys004"); // no Claude
        let mut v = vec![d.clone(), a.clone(), c.clone(), b.clone()];
        let titles = HashMap::new();
        sort_sessions(&mut v, SortCol::Claude, true, &titles, 1000);
        assert_eq!(v.iter().map(|s| s.tty.as_str()).collect::<Vec<_>>(), ["ttys002", "ttys001", "ttys003", "ttys004"]);
        sort_sessions(&mut v, SortCol::Claude, false, &titles, 1000);
        assert_eq!(v.iter().map(|s| s.tty.as_str()).collect::<Vec<_>>(), ["ttys003", "ttys001", "ttys002", "ttys004"]);
    }

    #[test]
    fn sort_by_tab_title_and_tty_number() {
        let mut v = vec![sess("ttys010"), sess("ttys002"), sess("ttys100")];
        let titles = HashMap::from([
            ("ttys010".to_string(), "\u{2733} beta".to_string()),
            ("ttys100".to_string(), "Alpha".to_string()),
        ]);
        sort_sessions(&mut v, SortCol::Tab, false, &titles, 0);
        assert_eq!(v.iter().map(|s| s.tty.as_str()).collect::<Vec<_>>(), ["ttys100", "ttys010", "ttys002"]);
        sort_sessions(&mut v, SortCol::Tty, false, &titles, 0);
        assert_eq!(v.iter().map(|s| s.tty.as_str()).collect::<Vec<_>>(), ["ttys002", "ttys010", "ttys100"]);
    }


    #[test]
    fn agent_column_names_other_agents() {
        let snap = crate::fake::snapshot(crate::fake::STEPS - 1);
        let cell = |t: &str| claude_cell(snap.sessions.iter().find(|s| s.tty == t).unwrap(), now_ms());
        assert_eq!(cell("ttys014"), "codex");
        assert_eq!(cell("ttys016"), "gemini");
        assert_eq!(cell("ttys021"), "busy");
        assert_eq!(cell("ttys012"), "");
    }

    #[test]
    fn highlight_marks_every_occurrence() {
        let (b, h) = (Style::default(), Style::default().add_modifier(Modifier::BOLD));
        let spans = highlight("Walk the walk, café", &["walk".into()], b, h);
        let text: Vec<(&str, bool)> = spans.iter().map(|s| (s.content.as_ref(), s.style == h)).collect();
        assert_eq!(text, vec![("Walk", true), (" the ", false), ("walk", true), (", café", false)]);
    }

    #[test]
    fn preview_overlay_shows_the_match_and_steps() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new(true);
        app.on_snapshot(crate::fake::snapshot(crate::fake::STEPS - 1));
        app.search = "shadow".into();
        let m = |u: bool, t: &str| (u, t.to_string());
        app.preview = Some(PreviewUi {
            tty: "ttys021".into(),
            title: "demo".into(),
            msgs: vec![m(true, "add a shadow under the player"), m(false, "Done, the shadow is drawn first."), m(true, "commit it")],
            matches: vec![1, 0],
            pos: 0,
            words: vec!["shadow".into()],
        });
        let shown = screen(&render_frame(&mut app, 120, 30));
        assert!(shown.contains("match 1 of 2") && shown.contains("the shadow is drawn first"));
        assert!(shown.contains("commit it") && shown.contains("older"));
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(screen(&render_frame(&mut app, 120, 30)).contains("match 2 of 2"));
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.preview.is_none());
    }

    #[test]
    fn search_matches_every_word_anywhere() {
        let mut s = sess("ttys004");
        s.cwd = "/Users/demo/dev/Storefront".into();
        let title = "vite dev".to_string();
        let extra = "fixing the checkout flow".to_string();
        assert!(session_matches(&s, Some(&title), None, "store VITE"));
        assert!(session_matches(&s, None, Some(&extra), "checkout ttys004"));
        assert!(!session_matches(&s, None, None, "store checkout"));
        assert!(session_matches(&s, None, None, "  "));
    }

    #[test]
    fn search_filters_rows_and_esc_restores_them() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new(true);
        app.on_snapshot(crate::fake::snapshot(crate::fake::STEPS - 1));
        let all = app.item_count();
        app.handle_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "api-server".chars() {
            app.handle_key(KeyEvent::from(KeyCode::Char(c)));
        }
        let n = app.item_count();
        assert!(n >= 1 && n < all);
        assert!(app.selected_session().unwrap().cwd.contains("api-server"));
        // Enter keeps the filter; keys act on rows again ('j' doesn't type).
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.search, "api-server");
        // A new snapshot stays filtered.
        app.on_snapshot(crate::fake::snapshot(crate::fake::STEPS - 1));
        assert_eq!(app.item_count(), n);
        let shown = screen(&render_frame(&mut app, 160, 30));
        assert!(shown.contains("/api-server") && shown.contains("match"));
        assert!(!shown.contains("storefront"));
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.item_count(), all);
    }

    #[test]
    fn bulk_threshold_keys_step_one_day() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new(true);
        app.on_snapshot(crate::fake::snapshot(crate::fake::STEPS - 1));
        let press = |app: &mut App, c: char| app.handle_key(KeyEvent::from(KeyCode::Char(c)));
        press(&mut app, 'Z');
        let days = |app: &App| app.bulk.as_ref().unwrap().days;
        assert_eq!(days(&app), 7);
        press(&mut app, '+');
        assert_eq!(days(&app), 8);
        press(&mut app, '-');
        press(&mut app, '-');
        assert_eq!(days(&app), 6);
    }

}
