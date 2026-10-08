//! Activity tab: a running log of processes starting and exiting, and of
//! ones that keep the CPU busy for a while.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sc_core::system;
use sc_ui::{
    Align, Button, ButtonKind, Canvas, Color, Column, Event, ListAction, ListModel, ListView, Menu, Rect, TextInput,
    Theme, Win,
};

use crate::app::Goto;
use crate::format;
use crate::sampler::Proc;

const MARGIN: f32 = 12.0;
const TOOLBAR_H: f32 = 34.0;
/// Oldest events are dropped beyond this many.
const LIMIT: usize = 5000;
/// Share of the whole machine's CPU that counts as busy. One process at 25 %
/// is a full core on a four-core PC and four of them on a sixteen-core one.
const BUSY_PERCENT: f32 = 25.0;
/// How long a process has to stay busy before it is worth a line.
const BUSY_HOLD: Duration = Duration::from_secs(30);

const COL_TIME: usize = 0;
const COL_EVENT: usize = 1;
const COL_NAME: usize = 2;
const COL_PID: usize = 3;
const COL_DETAILS: usize = 4;

mod id {
    pub const GO_TO_PROCESS: u32 = 1;
    pub const COPY: u32 = 2;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Started,
    Exited,
    Busy,
    Calmed,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Started => "Started",
            Kind::Exited => "Exited",
            Kind::Busy => "Busy",
            Kind::Calmed => "Calmed down",
        }
    }
}

struct Item {
    /// Counts up forever, so a selection survives old events being dropped.
    seq: u64,
    time: String,
    kind: Kind,
    name: Arc<str>,
    pid: u32,
    details: String,
}

/// One event as the Processes page's Activity panel shows it.
pub struct FeedRow {
    pub time: String,
    pub kind: &'static str,
    /// Worth a second look: drawn in the warning colour.
    pub busy: bool,
    /// Something that is over: drawn dimmed.
    pub quiet: bool,
    pub name: Arc<str>,
    pub note: String,
}

/// A process currently at or above the busy threshold.
struct Hot {
    since: Instant,
    name: Arc<str>,
    logged: bool,
}

/// "42 s", "3 min 10 s", "2 h 5 min".
fn span(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min {} s", secs / 60, secs % 60),
        _ => format!("{} h {} min", secs / 3600, secs % 3600 / 60),
    }
}

struct Model<'a> {
    events: &'a VecDeque<Item>,
    view: &'a [usize],
}

impl ListModel for Model<'_> {
    fn rows(&self) -> usize {
        self.view.len()
    }

    fn cell(&self, row: usize, col: usize, out: &mut String) {
        let e = &self.events[self.view[row]];
        let _ = match col {
            COL_TIME => write!(out, "{}", e.time),
            COL_EVENT => write!(out, "{}", e.kind.label()),
            COL_NAME => write!(out, "{}", e.name),
            COL_PID => write!(out, "{}", e.pid),
            _ => write!(out, "{}", e.details),
        };
    }

    fn bold(&self, _row: usize, col: usize) -> bool {
        col == COL_NAME
    }

    fn text_color(&self, row: usize, col: usize, t: &Theme) -> Option<Color> {
        let kind = self.events[self.view[row]].kind;
        match col {
            COL_EVENT if kind == Kind::Busy => Some(t.butter),
            COL_NAME => (kind == Kind::Exited).then_some(t.text_dim),
            COL_EVENT => Some(t.text),
            _ => Some(t.text_dim),
        }
    }
}

pub struct ActivityPage {
    events: VecDeque<Item>,
    next_seq: u64,
    /// Indices into `events`, newest first.
    view: Vec<usize>,
    search: TextInput,
    spikes_only: Button,
    clear_btn: Button,
    list: ListView,
    /// Processes in the previous frame, by (pid, creation time).
    known: HashMap<(u32, i64), Arc<str>>,
    /// False until the first frame, whose processes did not just start.
    seeded: bool,
    hot: HashMap<(u32, i64), Hot>,
    selected: Option<u64>,
    /// Set when the user asks for something that lives on another tab.
    pub goto: Option<Goto>,
}

impl ActivityPage {
    pub fn new() -> Self {
        let col = |title, width, align| Column { title, width, align, visible: true };
        Self {
            events: VecDeque::new(),
            next_seq: 0,
            view: Vec::new(),
            search: TextInput::new("Search events"),
            spikes_only: Button::new("Only busy", ButtonKind::Cocoa),
            clear_btn: Button::new("Clear", ButtonKind::Cocoa),
            list: ListView::new(vec![
                col("Time", 90.0, Align::Left),
                col("Event", 110.0, Align::Left),
                col("Name", 230.0, Align::Left),
                col("PID", 70.0, Align::Right),
                col("Details", 400.0, Align::Left),
            ]),
            known: HashMap::new(),
            seeded: false,
            hot: HashMap::new(),
            selected: None,
            goto: None,
        }
    }

    /// The newest `n` events, newest first.
    pub fn recent(&self, n: usize) -> Vec<FeedRow> {
        self.events
            .iter()
            .rev()
            .take(n)
            .map(|e| FeedRow {
                time: e.time.clone(),
                kind: e.kind.label(),
                busy: e.kind == Kind::Busy,
                quiet: e.kind == Kind::Exited,
                name: e.name.clone(),
                note: e.details.clone(),
            })
            .collect()
    }

    fn push(&mut self, time: &str, kind: Kind, name: Arc<str>, pid: u32, details: String) {
        if self.events.len() == LIMIT {
            self.events.pop_front();
        }
        self.events.push_back(Item { seq: self.next_seq, time: time.to_owned(), kind, name, pid, details });
        self.next_seq += 1;
    }

    /// Compares a new frame with the last one and logs what changed.
    pub fn observe(&mut self, procs: &[Proc]) {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let ticks = system::unix_to_filetime(secs);
        let stamp = system::format_filetime(ticks);
        self.observe_at(procs, Instant::now(), stamp.get(11..).unwrap_or_default(), ticks);
    }

    // ponytail: built on the regular snapshots, so a process that starts and
    // exits between two of them is never seen. Subscribing to the kernel's
    // process events (ETW) would catch those.
    fn observe_at(&mut self, procs: &[Proc], now: Instant, time: &str, now_ticks: i64) {
        let before = self.next_seq;
        let current: HashMap<(u32, i64), &Proc> = procs.iter().map(|p| ((p.row.pid, p.row.create_time), p)).collect();

        if self.seeded {
            let started: Vec<&Proc> =
                current.iter().filter(|(key, _)| !self.known.contains_key(key)).map(|(_, p)| *p).collect();
            for p in started {
                let parent = procs
                    .iter()
                    .find(|q| q.row.pid == p.row.parent_pid && q.row.create_time <= p.row.create_time)
                    .map_or("a process that has exited", |q| &q.row.name);
                let details = format!("by {parent}   {}", p.info.cmdline);
                self.push(time, Kind::Started, p.row.name.clone(), p.row.pid, details);
            }
            let exited: Vec<_> =
                self.known.iter().filter(|(key, _)| !current.contains_key(key)).map(|(k, n)| (*k, n.clone())).collect();
            for ((pid, created), name) in exited {
                let ran = (now_ticks - created).max(0) as u64 / 10_000_000;
                self.push(time, Kind::Exited, name, pid, format!("after running for {}", span(ran)));
            }
        }
        self.known = current.iter().map(|(key, p)| (*key, p.row.name.clone())).collect();
        self.seeded = true;

        // The idle process's "usage" is the CPU nobody wanted.
        let mut held: Vec<(u32, Arc<str>)> = Vec::new();
        for (key, p) in current.iter().filter(|(key, p)| key.0 != 0 && p.row.cpu >= BUSY_PERCENT) {
            let hot = self.hot.entry(*key).or_insert_with(|| Hot { since: now, name: p.row.name.clone(), logged: false });
            if !hot.logged && now.duration_since(hot.since) >= BUSY_HOLD {
                hot.logged = true;
                held.push((key.0, p.row.name.clone()));
            }
        }
        for (pid, name) in held {
            let details = format!("above {BUSY_PERCENT:.0}% CPU for {} and counting", span(BUSY_HOLD.as_secs()));
            self.push(time, Kind::Busy, name, pid, details);
        }
        let cooled: Vec<_> = self
            .hot
            .iter()
            .filter(|(key, _)| !current.get(key).is_some_and(|p| p.row.cpu >= BUSY_PERCENT))
            .map(|(key, hot)| (*key, hot.name.clone(), hot.since, hot.logged))
            .collect();
        for (key, name, since, logged) in cooled {
            self.hot.remove(&key);
            if logged {
                let held = span(now.duration_since(since).as_secs());
                self.push(time, Kind::Calmed, name, key.0, format!("was above {BUSY_PERCENT:.0}% CPU for {held}"));
            }
        }

        if self.next_seq != before {
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        let filter = self.search.text.trim().to_lowercase();
        let mut pid_text = String::new();
        self.view = (0..self.events.len())
            .rev()
            .filter(|&i| {
                let e = &self.events[i];
                if self.spikes_only.active && !matches!(e.kind, Kind::Busy | Kind::Calmed) {
                    return false;
                }
                pid_text.clear();
                let _ = write!(pid_text, "{}", e.pid);
                filter.is_empty()
                    || format::contains_ci(&e.name, &filter)
                    || format::contains_ci(&e.details, &filter)
                    || format::contains_ci(e.kind.label(), &filter)
                    || pid_text.contains(&filter)
            })
            .collect();
        self.list.selected = self.selected.and_then(|seq| self.view.iter().position(|&i| self.events[i].seq == seq));
        if self.list.selected.is_none() {
            self.selected = None;
        }
    }

    fn selected_item(&self) -> Option<&Item> {
        Some(&self.events[*self.view.get(self.list.selected?)?])
    }

    pub fn paint(&mut self, c: &mut Canvas, area: Rect) {
        let t = c.theme;
        let inner = Rect::new(area.x + MARGIN, area.y + MARGIN, area.w - 2.0 * MARGIN, (area.h - MARGIN).max(0.0));
        self.search.rect = Rect::new(inner.x + 4.0, inner.y, 320.0_f32.min(inner.w * 0.4), TOOLBAR_H);
        self.search.paint(c);
        let sw = self.spikes_only.width(c);
        self.spikes_only.rect = Rect::new(self.search.rect.right() + 10.0, inner.y, sw, TOOLBAR_H);
        self.spikes_only.paint(c);
        let cw = self.clear_btn.width(c);
        self.clear_btn.rect = Rect::new(inner.right() - cw, inner.y, cw, TOOLBAR_H);
        self.clear_btn.paint(c);
        let summary = if self.events.is_empty() {
            "Nothing has started or exited since Flask opened".to_owned()
        } else {
            format!("{} shown of {} events", self.view.len(), self.events.len())
        };
        let left = self.spikes_only.rect.right() + 12.0;
        let summary_r = Rect::new(left, inner.y, (self.clear_btn.rect.x - left - 12.0).max(0.0), TOOLBAR_H);
        c.text(&summary, summary_r, t.text_dim, Align::Right, false);

        let card = Rect::new(inner.x, inner.y + TOOLBAR_H + MARGIN, inner.w, (inner.h - TOOLBAR_H - MARGIN).max(0.0));
        c.glass(card, 22.0);
        self.list.rect = Rect::new(card.x, card.y + 6.0, card.w, (card.h - 12.0).max(0.0));
        let fixed: f32 = [COL_TIME, COL_EVENT, COL_NAME, COL_PID].iter().map(|&i| self.list.columns[i].width).sum();
        self.list.columns[COL_DETAILS].width = (card.w - 12.0 - fixed).max(200.0);
        let model = Model { events: &self.events, view: &self.view };
        self.list.paint(c, &model);
    }

    /// Returns true when the page needs repainting.
    pub fn event(&mut self, ev: &Event, win: &Win) -> bool {
        let mut redraw = false;
        if self.search.event(ev) {
            self.rebuild();
            redraw = true;
        }
        let (r, clicked) = self.spikes_only.event(ev);
        redraw |= r;
        if clicked {
            self.spikes_only.active = !self.spikes_only.active;
            self.rebuild();
            return true;
        }
        let (r, clicked) = self.clear_btn.event(ev);
        redraw |= r;
        if clicked {
            self.events.clear();
            self.rebuild();
            return true;
        }
        if let Event::MouseMove { .. } = *ev {
            win.set_cursor(sc_ui::Cursor::Arrow);
        }

        let model = Model { events: &self.events, view: &self.view };
        let resp = self.list.event(ev, &model, win);
        redraw |= resp.redraw;
        match resp.action {
            Some(ListAction::Select(row)) => {
                self.selected = row.and_then(|r| self.view.get(r)).map(|&i| self.events[i].seq);
            }
            Some(ListAction::Activate(_)) => self.goto = self.selected_item().map(|e| Goto::Process(e.pid)),
            Some(ListAction::Context { row: Some(_) }) => {
                win.invalidate();
                if let Some(e) = self.selected_item() {
                    let mut menu = Menu::new();
                    menu.item(id::GO_TO_PROCESS, "Go to process").item(id::COPY, "Copy");
                    match win.popup(&menu) {
                        Some(id::GO_TO_PROCESS) => self.goto = Some(Goto::Process(e.pid)),
                        Some(id::COPY) => {
                            win.copy_text(&format!("{}  {}  {} (PID {})  {}", e.time, e.kind.label(), e.name, e.pid, e.details))
                        }
                        _ => {}
                    }
                }
                redraw = true;
            }
            _ => {}
        }
        redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_core::process::ProcessRow;

    fn proc(pid: u32, parent: u32, created: i64, name: &str, cpu: f32) -> Proc {
        Proc {
            row: ProcessRow {
                pid,
                parent_pid: parent,
                create_time: created,
                name: name.into(),
                cpu,
                cpu_time: 0,
                working_set: 0,
                private_working_set: 0,
                private_bytes: 0,
                read_rate: 0,
                write_rate: 0,
                threads: 1,
                handles: 1,
                session: 1,
                base_priority: 8,
                suspended: false,
            },
            info: Arc::default(),
            is_app: false,
            gpu: 0.0,
            gpu_memory: 0,
        }
    }

    fn log(page: &ActivityPage) -> Vec<(Kind, &str, u32)> {
        page.events.iter().map(|e| (e.kind, &*e.name, e.pid)).collect()
    }

    #[test]
    fn starts_and_exits_are_logged_after_the_first_frame() {
        const SEC: i64 = 10_000_000;
        let mut page = ActivityPage::new();
        let t0 = Instant::now();
        page.observe_at(&[proc(4, 0, 1, "System", 0.0), proc(100, 4, 5, "shell.exe", 0.0)], t0, "10:00:00", 100 * SEC);
        // Whatever was already running did not just start.
        assert!(page.events.is_empty());

        let frame = [proc(4, 0, 1, "System", 0.0), proc(100, 4, 5, "shell.exe", 0.0), proc(200, 100, 100 * SEC, "tool.exe", 0.0)];
        page.observe_at(&frame, t0, "10:00:01", 101 * SEC);
        assert_eq!(log(&page), [(Kind::Started, "tool.exe", 200)]);
        assert!(page.events[0].details.starts_with("by shell.exe"));

        // The same PID with another creation time is a different process.
        let frame = [proc(4, 0, 1, "System", 0.0), proc(100, 4, 5, "shell.exe", 0.0), proc(200, 100, 190 * SEC, "other.exe", 0.0)];
        page.observe_at(&frame, t0, "10:01:30", 190 * SEC);
        let mut got = log(&page)[1..].to_vec();
        got.sort_by_key(|e| e.0 as u8);
        assert_eq!(got, [(Kind::Started, "other.exe", 200), (Kind::Exited, "tool.exe", 200)]);
        let exit = page.events.iter().find(|e| e.kind == Kind::Exited).unwrap();
        assert_eq!(exit.details, "after running for 1 min 30 s");
        // Newest first on screen.
        assert_eq!(page.view.len(), 3);
        assert_eq!(page.view[2], 0);
    }

    #[test]
    fn busy_is_logged_once_after_the_hold_and_calm_when_it_ends() {
        let mut page = ActivityPage::new();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let frame = |cpu| [proc(300, 4, 7, "render.exe", cpu), proc(0, 0, 0, "Idle", 90.0)];
        page.observe_at(&frame(60.0), at(0), "", 0);
        page.observe_at(&frame(60.0), at(29), "", 0);
        assert!(page.events.is_empty());
        page.observe_at(&frame(40.0), at(30), "", 0);
        page.observe_at(&frame(40.0), at(90), "", 0);
        assert_eq!(log(&page), [(Kind::Busy, "render.exe", 300)]);
        page.observe_at(&frame(3.0), at(125), "", 0);
        assert_eq!(log(&page)[1], (Kind::Calmed, "render.exe", 300));
        assert_eq!(page.events[1].details, "was above 25% CPU for 2 min 5 s");

        // A short burst leaves no trace.
        page.observe_at(&frame(99.0), at(130), "", 0);
        page.observe_at(&frame(1.0), at(140), "", 0);
        assert_eq!(page.events.len(), 2);

        page.spikes_only.active = true;
        page.rebuild();
        assert_eq!(page.view.len(), 2);
    }

    #[test]
    fn spans_read_naturally_and_the_log_is_bounded() {
        assert_eq!((span(0), span(59), span(61), span(3725)), ("0 s".into(), "59 s".into(), "1 min 1 s".into(), "1 h 2 min".into()));
        let mut page = ActivityPage::new();
        for i in 0..(LIMIT as u32 + 10) {
            page.push("", Kind::Started, "x".into(), i, String::new());
        }
        assert_eq!((page.events.len(), page.events[0].pid), (LIMIT, 10));
    }
}
