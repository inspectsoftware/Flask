//! Services tab: every Windows service, with start, stop and start-type control.

use std::cmp::Ordering;
use std::fmt::Write;
use std::sync::{Arc, Mutex};

use sc_core::services::{self, Service, StartType, State};
use sc_ui::{
    Align, Button, ButtonKind, Canvas, Color, Column, Event, ListAction, ListModel, ListView, Menu, Rect, TextInput,
    Theme, Win, key,
};

use crate::app::Goto;
use crate::format;

/// `Event::User` code posted when a fresh service list is ready.
pub const SERVICES_READY: usize = 4;
const MARGIN: f32 = 12.0;
const TOOLBAR_H: f32 = 34.0;
/// Frames between automatic refreshes while the tab is showing.
const REFRESH_EVERY: u32 = 3;

const COL_NAME: usize = 0;
const COL_DISPLAY: usize = 1;
const COL_STATUS: usize = 2;
const COL_START: usize = 3;
const COL_PID: usize = 4;

mod id {
    pub const START_TYPE: u32 = 10; // + index into StartType::CHOICES
    pub const COPY_NAME: u32 = 20;
    pub const GO_TO_PROCESS: u32 = 21;
}

struct Model<'a> {
    services: &'a [Service],
    view: &'a [usize],
}

impl ListModel for Model<'_> {
    fn rows(&self) -> usize {
        self.view.len()
    }

    fn cell(&self, row: usize, col: usize, out: &mut String) {
        let s = &self.services[self.view[row]];
        let _ = match col {
            COL_NAME => write!(out, "{}", s.name),
            COL_DISPLAY => write!(out, "{}", s.display),
            COL_STATUS => write!(out, "{}", s.state.label()),
            COL_START => write!(out, "{}", s.start.label()),
            COL_PID if s.pid != 0 => write!(out, "{}", s.pid),
            _ => Ok(()),
        };
    }

    fn bold(&self, _row: usize, col: usize) -> bool {
        col == COL_NAME
    }

    fn text_color(&self, row: usize, col: usize, t: &Theme) -> Option<Color> {
        let s = &self.services[self.view[row]];
        match col {
            COL_NAME => (s.state == State::Stopped).then_some(t.text_dim),
            COL_STATUS => Some(if s.state == State::Running { t.text } else { t.text_dim }),
            COL_START if s.start == StartType::Disabled => Some(t.butter),
            _ => Some(t.text_dim),
        }
    }
}

fn compare(a: &Service, b: &Service, col: usize) -> Ordering {
    match col {
        COL_DISPLAY => format::cmp_ci(&a.display, &b.display),
        COL_STATUS => a.state.cmp(&b.state),
        COL_START => a.start.cmp(&b.start),
        COL_PID => a.pid.cmp(&b.pid),
        _ => format::cmp_ci(&a.name, &b.name),
    }
}

pub struct ServicesPage {
    services: Vec<Service>,
    view: Vec<usize>,
    sort: (usize, bool),
    search: TextInput,
    start_btn: Button,
    stop_btn: Button,
    restart_btn: Button,
    list: ListView,
    slot: Arc<Mutex<Option<Vec<Service>>>>,
    loading: bool,
    frames: u32,
    /// Service to start again once it has stopped ("Restart").
    restart_pending: Option<String>,
    selected: Option<String>,
    /// Display name of a service to select once the list has it.
    pending: Option<String>,
    /// Scroll the selection into view on the next paint.
    reveal: bool,
    /// Set when the user asks for something that lives on another tab.
    pub goto: Option<Goto>,
    notice: String,
}

impl ServicesPage {
    pub fn new() -> Self {
        let col = |title, width, align| Column { title, width, align, visible: true };
        let mut list = ListView::new(vec![
            col("Name", 220.0, Align::Left),
            col("Description", 320.0, Align::Left),
            col("Status", 100.0, Align::Left),
            col("Starts", 110.0, Align::Left),
            col("PID", 70.0, Align::Right),
        ]);
        list.sort = Some((COL_NAME, false));
        Self {
            services: Vec::new(),
            view: Vec::new(),
            sort: (COL_NAME, false),
            search: TextInput::new("Search services"),
            start_btn: Button::new("Start", ButtonKind::Milk),
            stop_btn: Button::new("Stop", ButtonKind::Peach),
            restart_btn: Button::new("Restart", ButtonKind::Cocoa),
            list,
            slot: Arc::default(),
            loading: false,
            frames: 0,
            restart_pending: None,
            selected: None,
            pending: None,
            reveal: false,
            goto: None,
            notice: String::new(),
        }
    }

    pub fn notice(&self) -> &str {
        &self.notice
    }

    /// Selects the service with this display name, as soon as it is listed.
    pub fn select_display(&mut self, display: String) {
        self.search.text.clear();
        self.pending = Some(display);
        self.rebuild();
    }

    pub fn refresh(&mut self, win: &Win) {
        if self.loading {
            return;
        }
        self.loading = true;
        let (slot, id) = (self.slot.clone(), win.id());
        std::thread::spawn(move || {
            *slot.lock().unwrap() = Some(services::list());
            Win::post(id, SERVICES_READY);
        });
    }

    /// Called on every process frame while the tab is showing.
    pub fn tick(&mut self, win: &Win) {
        self.frames += 1;
        if self.frames % REFRESH_EVERY == 0 || self.services.is_empty() {
            self.refresh(win);
        }
    }

    /// Takes a finished listing. Returns true on change.
    pub fn poll(&mut self) -> bool {
        let Some(services) = self.slot.lock().unwrap().take() else { return false };
        self.loading = false;
        self.services = services;
        // Second half of a restart: start once the service has come to rest.
        if let Some(name) = self.restart_pending.clone()
            && let Some(s) = self.services.iter().find(|s| s.name == name)
            && s.state == State::Stopped
        {
            self.restart_pending = None;
            if let Err(e) = services::start(&name) {
                self.notice = format!("Could not start {name} again: {}", e.message());
            }
        }
        self.rebuild();
        true
    }

    fn rebuild(&mut self) {
        if !self.services.is_empty()
            && let Some(display) = self.pending.take()
            && let Some(s) = self.services.iter().find(|s| s.display == display)
        {
            self.selected = Some(s.name.clone());
            self.reveal = true;
        }
        let filter = self.search.text.trim().to_lowercase();
        let (col, desc) = self.sort;
        let mut view: Vec<usize> = (0..self.services.len())
            .filter(|&i| {
                let s = &self.services[i];
                filter.is_empty() || format::contains_ci(&s.name, &filter) || format::contains_ci(&s.display, &filter)
            })
            .collect();
        view.sort_by(|&a, &b| {
            let o = compare(&self.services[a], &self.services[b], col);
            (if desc { o.reverse() } else { o }).then_with(|| format::cmp_ci(&self.services[a].name, &self.services[b].name))
        });
        self.list.selected = self.selected.as_ref().and_then(|name| view.iter().position(|&i| &self.services[i].name == name));
        if self.list.selected.is_none() {
            self.selected = None;
        }
        self.view = view;
    }

    fn selected_service(&self) -> Option<&Service> {
        Some(&self.services[*self.view.get(self.list.selected?)?])
    }

    pub fn paint(&mut self, c: &mut Canvas, area: Rect) {
        let t = c.theme;
        let inner = Rect::new(area.x + MARGIN, area.y + MARGIN, area.w - 2.0 * MARGIN, (area.h - MARGIN).max(0.0));
        self.search.rect = Rect::new(inner.x + 4.0, inner.y, 320.0_f32.min(inner.w * 0.4), TOOLBAR_H);
        self.search.paint(c);

        // Buttons apply to the selected service and only show when they can act.
        let state = self.selected_service().map(|s| (s.state, s.start));
        let mut x = self.search.rect.right() + 10.0;
        let mut place = |c: &mut Canvas, b: &mut Button, show: bool| {
            if show {
                let w = b.width(c);
                b.rect = Rect::new(x, inner.y, w, TOOLBAR_H);
                b.paint(c);
                x += w + 8.0;
            } else {
                b.rect = Rect::default();
            }
        };
        let can_start = state.is_some_and(|(s, start)| s == State::Stopped && start != StartType::Disabled);
        let running = state.is_some_and(|(s, _)| s == State::Running);
        place(c, &mut self.start_btn, can_start);
        place(c, &mut self.stop_btn, running);
        place(c, &mut self.restart_btn, running);

        let running_count = self.services.iter().filter(|s| s.state == State::Running).count();
        let summary = format!("{} services, {running_count} running", self.services.len());
        c.text(&summary, Rect::new(x, inner.y, (inner.right() - x - 4.0).max(0.0), TOOLBAR_H), t.text_dim, Align::Right, false);

        let card = Rect::new(inner.x, inner.y + TOOLBAR_H + MARGIN, inner.w, (inner.h - TOOLBAR_H - MARGIN).max(0.0));
        c.glass(card, 22.0);
        self.list.rect = Rect::new(card.x, card.y + 6.0, card.w, (card.h - 12.0).max(0.0));
        let fixed: f32 = [COL_NAME, COL_STATUS, COL_START, COL_PID].iter().map(|&i| self.list.columns[i].width).sum();
        self.list.columns[COL_DISPLAY].width = (card.w - 12.0 - fixed).max(160.0);
        if std::mem::take(&mut self.reveal)
            && let Some(row) = self.list.selected
        {
            self.list.ensure_visible(row);
        }
        let model = Model { services: &self.services, view: &self.view };
        self.list.paint(c, &model);
    }

    fn act(&mut self, win: &Win, what: &str, result: sc_core::Result<()>, name: &str) {
        self.notice.clear();
        if let Err(e) = result {
            let _ = write!(self.notice, "Could not {what} {name}: {}", e.message());
        }
        self.refresh(win);
    }

    /// Returns true when the page needs repainting.
    pub fn event(&mut self, ev: &Event, win: &Win) -> bool {
        let mut redraw = false;
        if self.search.event(ev) {
            self.rebuild();
            redraw = true;
        }
        let (r1, start) = self.start_btn.event(ev);
        let (r2, stop) = self.stop_btn.event(ev);
        let (r3, restart) = self.restart_btn.event(ev);
        redraw |= r1 | r2 | r3;
        if (start || stop || restart)
            && let Some(name) = self.selected_service().map(|s| s.name.clone())
        {
            if start {
                self.act(win, "start", services::start(&name), &name);
            } else {
                let result = services::stop(&name);
                if restart && result.is_ok() {
                    self.restart_pending = Some(name.clone());
                }
                self.act(win, "stop", result, &name);
            }
            return true;
        }
        if let Event::Key { vk: key::F5, .. } = *ev {
            self.refresh(win);
        }
        if let Event::MouseMove { .. } = *ev {
            win.set_cursor(sc_ui::Cursor::Arrow);
        }

        let model = Model { services: &self.services, view: &self.view };
        let resp = self.list.event(ev, &model, win);
        redraw |= resp.redraw;
        match resp.action {
            Some(ListAction::Select(row)) => {
                self.selected = row.and_then(|r| self.view.get(r)).map(|&i| self.services[i].name.clone());
                self.notice.clear();
            }
            Some(ListAction::Sort(col)) => {
                // Text columns start A to Z; a second click reverses.
                self.sort = if self.sort.0 == col { (col, !self.sort.1) } else { (col, false) };
                self.list.sort = Some(self.sort);
                self.rebuild();
                redraw = true;
            }
            Some(ListAction::Context { row: Some(_) }) => {
                win.invalidate();
                if let Some(s) = self.selected_service().cloned() {
                    let mut starts = Menu::new();
                    for (i, choice) in StartType::CHOICES.into_iter().enumerate() {
                        starts.item_with(id::START_TYPE + i as u32, choice.label(), s.start == choice, true);
                    }
                    let mut menu = Menu::new();
                    menu.submenu("Starts", starts)
                        .separator()
                        .item_with(id::GO_TO_PROCESS, "Go to process", false, s.pid != 0)
                        .item(id::COPY_NAME, "Copy name");
                    match win.popup(&menu) {
                        Some(id::COPY_NAME) => win.copy_text(&s.name),
                        Some(id::GO_TO_PROCESS) => self.goto = Some(Goto::Process(s.pid)),
                        Some(cmd) => {
                            if let Some(&choice) = StartType::CHOICES.get((cmd - id::START_TYPE) as usize) {
                                self.act(win, "change how it starts for", services::set_start(&s.name, choice), &s.name);
                            }
                        }
                        None => {}
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

    fn svc(name: &str, display: &str, state: State, start: StartType, pid: u32) -> Service {
        Service { name: name.into(), display: display.into(), state, pid, start }
    }

    #[test]
    fn filter_sort_and_selection_follow_the_service() {
        let mut page = ServicesPage::new();
        page.services = vec![
            svc("Spooler", "Print Spooler", State::Running, StartType::Automatic, 900),
            svc("BITS", "Background Intelligent Transfer", State::Stopped, StartType::Manual, 0),
            svc("wuauserv", "Windows Update", State::Running, StartType::Manual, 400),
        ];
        page.selected = Some("Spooler".into());
        page.rebuild();
        let names = |p: &ServicesPage| p.view.iter().map(|&i| p.services[i].name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&page), ["BITS", "Spooler", "wuauserv"]);
        assert_eq!(page.list.selected, Some(1));

        page.sort = (COL_PID, true);
        page.rebuild();
        assert_eq!(names(&page), ["Spooler", "wuauserv", "BITS"]);
        assert_eq!(page.list.selected, Some(0));

        page.search.text = "update".into();
        page.rebuild();
        assert_eq!(names(&page), ["wuauserv"]);
        // The selected service is filtered out, so the selection is dropped.
        assert_eq!((page.list.selected, page.selected.clone()), (None, None));

        // Arriving from a process clears the filter and finds the service by display name.
        page.select_display("Print Spooler".into());
        assert_eq!((page.selected.as_deref(), page.search.text.as_str()), (Some("Spooler"), ""));
        page.select_display("No such service".into());
        assert_eq!((page.selected.as_deref(), page.pending.clone()), (Some("Spooler"), None));
    }
}
