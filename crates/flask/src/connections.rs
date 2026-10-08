//! Network tab: every open TCP and UDP endpoint and the process behind it.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex};

use sc_core::net::{self, Connection};
use sc_ui::{
    Align, Canvas, Color, Column, Event, ListAction, ListModel, ListView, Menu, Rect, TextInput, Theme, Win, key,
};

use crate::app::Goto;
use crate::format;
use crate::sampler::Proc;

/// `Event::User` code posted when a fresh endpoint list is ready.
pub const CONNECTIONS_READY: usize = 6;
const MARGIN: f32 = 12.0;
const TOOLBAR_H: f32 = 34.0;
/// Frames between automatic refreshes while the tab is showing.
const REFRESH_EVERY: u32 = 2;

const COL_PROCESS: usize = 0;
const COL_PID: usize = 1;
const COL_PROTOCOL: usize = 2;
const COL_LOCAL: usize = 3;
const COL_REMOTE: usize = 4;
const COL_STATE: usize = 5;

mod id {
    pub const GO_TO_PROCESS: u32 = 1;
    pub const COPY_REMOTE: u32 = 2;
    pub const COPY_LOCAL: u32 = 3;
}

struct Model<'a> {
    conns: &'a [Connection],
    names: &'a HashMap<u32, Arc<str>>,
    view: &'a [usize],
}

fn name(names: &HashMap<u32, Arc<str>>, pid: u32) -> &str {
    // Windows keeps closing connections around after their owner is gone.
    names.get(&pid).map_or("No longer running", |n| n)
}

impl ListModel for Model<'_> {
    fn rows(&self) -> usize {
        self.view.len()
    }

    fn cell(&self, row: usize, col: usize, out: &mut String) {
        let c = &self.conns[self.view[row]];
        let _ = match col {
            COL_PROCESS => write!(out, "{}", name(self.names, c.pid)),
            COL_PID => write!(out, "{}", c.pid),
            COL_PROTOCOL => write!(out, "{}", c.protocol),
            COL_LOCAL => write!(out, "{}", c.local),
            COL_REMOTE => c.remote.map_or(Ok(()), |r| write!(out, "{r}")),
            _ => write!(out, "{}", c.state),
        };
    }

    fn bold(&self, _row: usize, col: usize) -> bool {
        col == COL_PROCESS
    }

    fn text_color(&self, row: usize, col: usize, t: &Theme) -> Option<Color> {
        let c = &self.conns[self.view[row]];
        match col {
            COL_PROCESS => (!self.names.contains_key(&c.pid)).then_some(t.text_dim),
            COL_REMOTE => Some(t.text),
            _ => Some(t.text_dim),
        }
    }
}

pub struct ConnectionsPage {
    conns: Vec<Connection>,
    names: HashMap<u32, Arc<str>>,
    view: Vec<usize>,
    sort: (usize, bool),
    search: TextInput,
    list: ListView,
    slot: Arc<Mutex<Option<Vec<Connection>>>>,
    loading: bool,
    frames: u32,
    selected: Option<Connection>,
    /// Set when the user asks for something that lives on another tab.
    pub goto: Option<Goto>,
}

impl ConnectionsPage {
    pub fn new() -> Self {
        let col = |title, width, align| Column { title, width, align, visible: true };
        let mut list = ListView::new(vec![
            col("Process", 220.0, Align::Left),
            col("PID", 70.0, Align::Right),
            col("Protocol", 80.0, Align::Left),
            col("Local", 240.0, Align::Left),
            col("Remote", 300.0, Align::Left),
            col("State", 110.0, Align::Left),
        ]);
        list.sort = Some((COL_PROCESS, false));
        Self {
            conns: Vec::new(),
            names: HashMap::new(),
            view: Vec::new(),
            sort: (COL_PROCESS, false),
            search: TextInput::new("Search process, address or port"),
            list,
            slot: Arc::default(),
            loading: false,
            frames: 0,
            selected: None,
            goto: None,
        }
    }

    pub fn refresh(&mut self, win: &Win) {
        if self.loading {
            return;
        }
        self.loading = true;
        let (slot, id) = (self.slot.clone(), win.id());
        std::thread::spawn(move || {
            *slot.lock().unwrap() = Some(net::connections());
            Win::post(id, CONNECTIONS_READY);
        });
    }

    /// Called on every process frame while the tab is showing.
    pub fn tick(&mut self, win: &Win, procs: &[Proc]) {
        self.names = procs.iter().map(|p| (p.row.pid, p.row.name.clone())).collect();
        self.frames += 1;
        if self.frames.is_multiple_of(REFRESH_EVERY) || self.conns.is_empty() {
            self.refresh(win);
        }
    }

    /// Takes a finished listing. Returns true on change.
    pub fn poll(&mut self) -> bool {
        let Some(conns) = self.slot.lock().unwrap().take() else { return false };
        self.loading = false;
        self.conns = conns;
        self.rebuild();
        true
    }

    /// Narrows the list to one process.
    pub fn show_pid(&mut self, pid: u32) {
        self.search.text = format!("pid:{pid}");
        self.rebuild();
    }

    fn matches(&self, c: &Connection, filter: &str, scratch: &mut String) -> bool {
        // What "Connections" on a process fills in: that process and nothing else.
        if let Some(pid) = filter.strip_prefix("pid:") {
            return pid.trim().parse() == Ok(c.pid);
        }
        scratch.clear();
        let _ = write!(scratch, "{} {} {} {}", c.pid, c.local, c.protocol, c.state);
        if let Some(remote) = c.remote {
            let _ = write!(scratch, " {remote}");
        }
        format::contains_ci(name(&self.names, c.pid), filter) || format::contains_ci(scratch, filter)
    }

    fn compare(&self, a: &Connection, b: &Connection, col: usize) -> Ordering {
        match col {
            COL_PID => a.pid.cmp(&b.pid),
            COL_PROTOCOL => a.protocol.cmp(b.protocol),
            COL_LOCAL => a.local.cmp(&b.local),
            COL_REMOTE => a.remote.cmp(&b.remote),
            COL_STATE => a.state.cmp(b.state),
            _ => format::cmp_ci(name(&self.names, a.pid), name(&self.names, b.pid)),
        }
    }

    fn rebuild(&mut self) {
        let filter = self.search.text.trim().to_lowercase();
        let (col, desc) = self.sort;
        let mut scratch = String::new();
        let mut view: Vec<usize> =
            (0..self.conns.len()).filter(|&i| filter.is_empty() || self.matches(&self.conns[i], &filter, &mut scratch)).collect();
        view.sort_by(|&a, &b| {
            let (x, y) = (&self.conns[a], &self.conns[b]);
            let o = self.compare(x, y, col);
            (if desc { o.reverse() } else { o }).then_with(|| x.pid.cmp(&y.pid)).then_with(|| x.local.cmp(&y.local))
        });
        self.list.selected = self.selected.as_ref().and_then(|s| view.iter().position(|&i| &self.conns[i] == s));
        if self.list.selected.is_none() {
            self.selected = None;
        }
        self.view = view;
    }

    fn selected_conn(&self) -> Option<&Connection> {
        Some(&self.conns[*self.view.get(self.list.selected?)?])
    }

    pub fn paint(&mut self, c: &mut Canvas, area: Rect) {
        let t = c.theme;
        let inner = Rect::new(area.x + MARGIN, area.y + MARGIN, area.w - 2.0 * MARGIN, (area.h - MARGIN).max(0.0));
        self.search.rect = Rect::new(inner.x + 4.0, inner.y, 340.0_f32.min(inner.w * 0.45), TOOLBAR_H);
        self.search.paint(c);
        let connected = self.conns.iter().filter(|c| c.state == "Connected").count();
        let summary = format!("{} shown of {} endpoints, {connected} connected", self.view.len(), self.conns.len());
        let left = self.search.rect.right() + 12.0;
        c.text(&summary, Rect::new(left, inner.y, (inner.right() - left - 4.0).max(0.0), TOOLBAR_H), t.text_dim, Align::Right, false);

        let card = Rect::new(inner.x, inner.y + TOOLBAR_H + MARGIN, inner.w, (inner.h - TOOLBAR_H - MARGIN).max(0.0));
        c.glass(card, 22.0);
        self.list.rect = Rect::new(card.x, card.y + 6.0, card.w, (card.h - 12.0).max(0.0));
        let fixed: f32 =
            [COL_PROCESS, COL_PID, COL_PROTOCOL, COL_LOCAL, COL_STATE].iter().map(|&i| self.list.columns[i].width).sum();
        self.list.columns[COL_REMOTE].width = (card.w - 12.0 - fixed).max(200.0);
        let model = Model { conns: &self.conns, names: &self.names, view: &self.view };
        self.list.paint(c, &model);
    }

    /// Returns true when the page needs repainting.
    pub fn event(&mut self, ev: &Event, win: &Win) -> bool {
        let mut redraw = false;
        if self.search.event(ev) {
            self.rebuild();
            redraw = true;
        }
        if let Event::Key { vk: key::F5, .. } = *ev {
            self.refresh(win);
        }
        if let Event::MouseMove { .. } = *ev {
            win.set_cursor(sc_ui::Cursor::Arrow);
        }

        let model = Model { conns: &self.conns, names: &self.names, view: &self.view };
        let resp = self.list.event(ev, &model, win);
        redraw |= resp.redraw;
        match resp.action {
            Some(ListAction::Select(row)) => {
                self.selected = row.and_then(|r| self.view.get(r)).map(|&i| self.conns[i].clone());
            }
            Some(ListAction::Sort(col)) => {
                self.sort = if self.sort.0 == col { (col, !self.sort.1) } else { (col, false) };
                self.list.sort = Some(self.sort);
                self.rebuild();
                redraw = true;
            }
            Some(ListAction::Activate(_)) => self.goto = self.selected_conn().map(|c| Goto::Process(c.pid)),
            Some(ListAction::Context { row: Some(_) }) => {
                win.invalidate();
                if let Some(conn) = self.selected_conn().cloned() {
                    let mut menu = Menu::new();
                    menu.item(id::GO_TO_PROCESS, "Go to process")
                        .separator()
                        .item_with(id::COPY_REMOTE, "Copy remote address", false, conn.remote.is_some())
                        .item(id::COPY_LOCAL, "Copy local address");
                    match win.popup(&menu) {
                        Some(id::GO_TO_PROCESS) => self.goto = Some(Goto::Process(conn.pid)),
                        Some(id::COPY_REMOTE) => win.copy_text(&conn.remote.map(|r| r.to_string()).unwrap_or_default()),
                        Some(id::COPY_LOCAL) => win.copy_text(&conn.local.to_string()),
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

    fn conn(pid: u32, local: &str, remote: Option<&str>, state: &'static str) -> Connection {
        Connection { pid, protocol: "TCP", local: local.parse().unwrap(), remote: remote.map(|r| r.parse().unwrap()), state }
    }

    #[test]
    fn search_matches_names_addresses_and_exact_pids() {
        let mut page = ConnectionsPage::new();
        page.names = HashMap::from([(10, "browser.exe".into()), (100, "sync.exe".into())]);
        page.conns = vec![
            conn(100, "10.0.0.5:50001", Some("93.184.216.34:443"), "Connected"),
            conn(10, "10.0.0.5:50002", Some("151.101.1.69:443"), "Connected"),
            conn(10, "0.0.0.0:8080", None, "Listening"),
            conn(999, "10.0.0.5:50003", Some("1.1.1.1:53"), "Closing"),
        ];
        let pids = |p: &ConnectionsPage| p.view.iter().map(|&i| p.conns[i].pid).collect::<Vec<_>>();
        page.rebuild();
        // By process name; the one whose owner is gone sorts by its stand-in name.
        assert_eq!(pids(&page), [10, 10, 999, 100]);

        page.search.text = "BROWSER".into();
        page.rebuild();
        assert_eq!(pids(&page), [10, 10]);
        page.search.text = ":443".into();
        page.rebuild();
        assert_eq!(pids(&page), [10, 100]);
        page.search.text = "listening".into();
        page.rebuild();
        assert_eq!(pids(&page), [10]);

        // "pid:10" must not also catch PID 100.
        page.show_pid(10);
        assert_eq!(pids(&page), [10, 10]);
        page.search.text = "pid:x".into();
        page.rebuild();
        assert!(page.view.is_empty());
    }
}
