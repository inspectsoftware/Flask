use std::fmt::Write;
use std::sync::{Arc, Mutex};

use sc_core::actions;
use sc_core::specs::{self, Section};
use sc_ui::{
    Align, Anim, Button, ButtonKind, Canvas, Color, Event, Font, Menu, MouseButton, Placement, Rect, Win,
    WindowAction, WindowButtons, anim, key,
};

use crate::activity::ActivityPage;
use crate::connections::{CONNECTIONS_READY, ConnectionsPage};
use crate::origin_view::ORIGIN_READY;
use crate::performance::PerfPage;
use crate::processes::{AWAKE_READY, DUMP_READY, ProcessesPage, SPECIAL_READY};
use crate::rules;
use crate::sampler::SamplerHandle;
use crate::services::{SERVICES_READY, ServicesPage};
use crate::settings::{INTERVALS, Settings};
use crate::startup::{STARTUP_READY, StartupPage};

const TITLE_H: f32 = 46.0;
const STATUS_H: f32 = 32.0;
/// Gap between the window edge, the rail and the content.
const PAD: f32 = 14.0;
const RAIL_W: f32 = 216.0;
const NAV_ROW: f32 = 44.0;
/// Events shown in the Processes page's Activity panel.
const FEED_ROWS: usize = 14;
/// `Event::User` code posted when the hardware inventory has been read.
const SPECS_READY: usize = 2;
/// Smallest the full window may be: two process panels side by side need the width.
pub const MIN_SIZE: (f32, f32) = (1040.0, 620.0);
/// Size of the compact view, and the least it may be dragged to.
// Wide enough for all three meters.
const COMPACT_SIZE: (f32, f32) = (580.0, 300.0);
const COMPACT_MIN: (f32, f32) = (580.0, 150.0);

const TABS: [&str; 7] = ["Processes", "Performance", "Startup", "Services", "Network", "Activity", "Settings"];
const TAB_PROCESSES: usize = 0;
const TAB_PERFORMANCE: usize = 1;
const TAB_STARTUP: usize = 2;
const TAB_SERVICES: usize = 3;
const TAB_NETWORK: usize = 4;
const TAB_ACTIVITY: usize = 5;

/// A page asking for something that lives on another tab.
pub enum Goto {
    Process(u32),
    /// A service, by display name.
    Service(String),
    /// The Origin drawer of whatever is running from `image`; `name` is the
    /// startup entry that asked.
    Origin { image: String, name: String },
    /// The network endpoints of one process.
    Connections(u32),
}

/// Lets the user start a program from the system Run dialog.
pub fn run_task(win: &Win) {
    let note = if actions::is_elevated() {
        "Type a program, folder or document to open. It starts with administrator rights, the same as Flask."
    } else {
        "Type a program, folder or document to open."
    };
    win.run_dialog("Run new task", note);
}

/// The one gate every way of ending a process goes through. Asks before
/// ending anything Windows cannot run without; says yes to the rest.
pub fn confirm_end(win: &Win, targets: &[(u32, Arc<str>)]) -> bool {
    let critical: Vec<String> = targets
        .iter()
        .filter(|(pid, _)| actions::is_critical(*pid))
        .map(|(pid, name)| format!("{name} (PID {pid})"))
        .collect();
    if critical.is_empty() {
        return true;
    }
    let question = format!(
        "Windows cannot keep running without {}.\n\nEnding it stops the PC at once with a bluescreen, and unsaved work in every program is lost.",
        critical.join(", ")
    );
    win.confirm("End a critical process", &question)
}

/// Draws the rail icon for section `tab`, centred on (`cx`, `cy`), about 18 px across.
fn nav_icon(c: &Canvas, tab: usize, cx: f32, cy: f32, ink: Color) {
    const W: f32 = 1.9;
    let line = |x0: f32, y0: f32, x1: f32, y1: f32| c.line(cx + x0, cy + y0, cx + x1, cy + y1, W, ink);
    match tab {
        TAB_PROCESSES => {
            line(-8.0, -6.0, 8.0, -6.0);
            line(-8.0, 0.0, 8.0, 0.0);
            line(-8.0, 6.0, 2.0, 6.0);
        }
        TAB_PERFORMANCE => {
            let pulse = [(-9.0, 1.0), (-5.0, 1.0), (-2.0, -7.0), (2.0, 7.0), (5.0, 1.0), (9.0, 1.0)];
            for pair in pulse.windows(2) {
                line(pair[0].0, pair[0].1, pair[1].0, pair[1].1);
            }
        }
        TAB_STARTUP => {
            c.ring(cx, cy + 1.0, 7.0, W, ink);
            line(0.0, -9.0, 0.0, -1.0);
        }
        TAB_SERVICES => {
            for (y, knob) in [(-6.0, -3.0), (0.0, 3.0), (6.0, -4.0)] {
                line(-8.0, y, 8.0, y);
                line(knob, y - 2.5, knob, y + 2.5);
            }
        }
        TAB_NETWORK => {
            c.ring(cx, cy, 8.0, W, ink);
            line(-8.0, 0.0, 8.0, 0.0);
            line(0.0, -8.0, 0.0, 8.0);
        }
        TAB_ACTIVITY => {
            c.ring(cx, cy, 8.0, W, ink);
            line(0.0, -4.5, 0.0, 0.0);
            line(0.0, 0.0, 3.0, 2.0);
        }
        // Settings: a cog.
        _ => {
            c.ring(cx, cy, 4.5, W, ink);
            for (x, y) in [(1.0, 0.0), (0.0, 1.0), (0.7, 0.7), (0.7, -0.7)] {
                line(x * 5.5, y * 5.5, x * 9.0, y * 9.0);
                line(-x * 5.5, -y * 5.5, -x * 9.0, -y * 9.0);
            }
        }
    }
}

pub struct App {
    /// Section on screen: an index into `TABS`.
    active: usize,
    /// Each section's row in the rail, as last painted.
    nav_rects: [Rect; TABS.len()],
    nav_hover: Option<usize>,
    /// Whole-machine figures for the rail's meters, in percent.
    cpu: f32,
    gpu: f32,
    memory: f32,
    window_buttons: WindowButtons,
    /// Shows the refresh rate and opens the menu that changes it.
    speed_btn: Button,
    refresh_ms: u32,
    /// Toggle: keep the window above all others.
    pin_btn: Button,
    /// Toggle: nothing eases or slides.
    still_btn: Button,
    /// Top edge of the rail's highlight, which slides to the section picked.
    nav_y: Anim,
    /// How far a newly shown page has settled into place, 0..=1.
    page_in: Anim,
    /// The rail's CPU, GPU and Memory meters.
    meter_anim: [Anim; 3],
    /// Switches between the full window and the compact view.
    compact_btn: Button,
    /// In the compact view: where the full window was, to go back to.
    compact: Option<Placement>,
    processes: ProcessesPage,
    performance: PerfPage,
    startup: StartupPage,
    services: ServicesPage,
    connections: ConnectionsPage,
    activity: ActivityPage,
    specs_slot: Arc<Mutex<Option<Vec<Section>>>>,
    sampler: SamplerHandle,
    /// Own executable, whose icon is the title bar logo.
    exe: String,
    /// False only in `unelevated` development builds.
    elevated: bool,
    maximized: bool,
    threads: u32,
    handles: u32,
    status: String,
    /// One-off message from the app itself; gone at the next click or key.
    flash: String,
}

impl App {
    pub fn new(win: &Win, settings: Settings) -> Self {
        // The inventory takes a few hundred milliseconds; read it in the background.
        let specs_slot: Arc<Mutex<Option<Vec<Section>>>> = Arc::default();
        let (slot, id) = (specs_slot.clone(), win.id());
        std::thread::spawn(move || {
            *slot.lock().unwrap() = Some(specs::collect());
            Win::post(id, SPECS_READY);
        });
        let rules = rules::load();
        let mut app = Self {
            active: 0,
            nav_rects: [Rect::default(); TABS.len()],
            nav_hover: None,
            cpu: 0.0,
            gpu: 0.0,
            memory: 0.0,
            window_buttons: WindowButtons::default(),
            speed_btn: Button::small("", ButtonKind::Cocoa),
            refresh_ms: 0,
            pin_btn: Button::small("Off", ButtonKind::Cocoa),
            still_btn: Button::small("Off", ButtonKind::Cocoa),
            nav_y: Anim::default(),
            page_in: Anim::default(),
            meter_anim: Default::default(),
            compact_btn: Button::small("Compact", ButtonKind::Cocoa),
            compact: None,
            processes: ProcessesPage::new(rules.clone()),
            performance: PerfPage::new(),
            startup: StartupPage::new(),
            services: ServicesPage::new(),
            connections: ConnectionsPage::new(),
            activity: ActivityPage::new(),
            specs_slot,
            sampler: SamplerHandle::spawn(win.id(), rules),
            exe: std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
            elevated: actions::is_elevated(),
            maximized: win.is_maximized(),
            threads: 0,
            handles: 0,
            status: String::new(),
            flash: String::new(),
        };
        app.processes.restore(&settings);
        app.set_refresh(settings.refresh_ms);
        if settings.topmost {
            app.pin_btn.active = true;
            win.set_topmost(true);
        }
        app.still_btn.active = settings.no_animations;
        anim::set_enabled(!settings.no_animations);
        if settings.tab < TABS.len() {
            app.switch_to(settings.tab, win);
        }
        app
    }

    fn set_refresh(&mut self, ms: u32) {
        self.refresh_ms = ms;
        self.speed_btn.label = INTERVALS.iter().find(|i| i.0 == ms).map_or("", |i| i.1);
        self.sampler.set_interval(ms);
        self.performance.set_interval(ms);
    }

    /// Shows `tab`, starting whatever loading it does when it comes on screen.
    fn switch_to(&mut self, tab: usize, win: &Win) {
        self.active = tab;
        self.page_in.jump(0.0);
        match tab {
            TAB_STARTUP => self.startup.activate(win),
            TAB_SERVICES => self.services.refresh(win),
            TAB_NETWORK => self.connections.refresh(win),
            _ => {}
        }
    }

    fn follow(&mut self, goto: Goto, win: &Win) {
        match goto {
            Goto::Process(pid) => {
                if self.processes.select_pid(pid) {
                    self.switch_to(TAB_PROCESSES, win);
                } else {
                    self.flash = format!("PID {pid} is no longer running");
                }
            }
            Goto::Service(display) => {
                self.services.select_display(display);
                self.switch_to(TAB_SERVICES, win);
            }
            Goto::Origin { image, name } => {
                if self.processes.show_origin_of(&image, win) {
                    self.switch_to(TAB_PROCESSES, win);
                } else {
                    self.startup.say(format!("{name} is not running, so there is no process to inspect"));
                }
            }
            Goto::Connections(pid) => {
                self.connections.show_pid(pid);
                self.switch_to(TAB_NETWORK, win);
            }
        }
    }

    /// Shrinks the window to the compact view, or brings the full one back.
    fn set_compact(&mut self, on: bool, win: &Win) {
        match self.compact.take() {
            None if on => {
                let full = win.placement();
                let [_, top, right, _] = full.rect;
                let s = win.scale();
                // Keeps the top right corner, where the button that undoes this is.
                let rect = [right - (COMPACT_SIZE.0 * s) as i32, top, right, top + (COMPACT_SIZE.1 * s) as i32];
                self.compact = Some(full);
                self.compact_btn.label = "Full";
                win.set_min_size(COMPACT_MIN);
                win.set_placement(Placement { rect, maximized: false });
                // A view this small is for glancing at over other windows.
                win.set_topmost(true);
            }
            Some(full) if !on => {
                self.compact_btn.label = "Compact";
                win.set_min_size(MIN_SIZE);
                win.set_placement(full);
                win.set_topmost(self.pin_btn.active);
            }
            unchanged => self.compact = unchanged,
        }
    }

    fn save_settings(&self, win: &Win) {
        let mut s = Settings {
            // Next time starts in the full window, wherever that last was.
            window: Some(self.compact.unwrap_or_else(|| win.placement())),
            tab: self.active,
            refresh_ms: self.refresh_ms,
            topmost: self.pin_btn.active,
            no_animations: self.still_btn.active,
            ..Default::default()
        };
        self.processes.store(&mut s);
        s.save();
    }

    /// Message from the page on screen, usually about an action that failed.
    fn notice(&self) -> &str {
        let page = match self.active {
            TAB_PROCESSES => self.processes.notice(),
            TAB_STARTUP => self.startup.notice(),
            TAB_SERVICES => self.services.notice(),
            _ => "",
        };
        if page.is_empty() { &self.flash } else { page }
    }

    fn hints(&self) -> &'static str {
        match self.active {
            TAB_PROCESSES => "Del ends a process      Shift+Del ends its tree      Ctrl+N runs a task      F5 refreshes",
            TAB_PERFORMANCE => "Ctrl+Tab switches tabs",
            TAB_STARTUP => "Switches are reversible      F5 rescans",
            TAB_SERVICES => "Right-click a service to change how it starts      F5 refreshes",
            TAB_NETWORK => "Double-click a row to find its process      F5 refreshes",
            TAB_ACTIVITY => "Double-click an event to find its process",
            _ => "",
        }
    }

    /// The rail down the left: logo, sections, meters, and the refresh rate.
    fn paint_rail(&mut self, c: &mut Canvas, h: f32) {
        let t = c.theme;
        let rail = Rect::new(PAD, PAD, RAIL_W, (h - 2.0 * PAD).max(0.0));
        c.glass(rail, 22.0);
        c.icon(&self.exe, Rect::new(rail.x + 18.0, rail.y + 16.0, 22.0, 22.0));
        c.text_font("Flask", Rect::new(rail.x + 50.0, rail.y + 12.0, 120.0, 30.0), t.text, Align::Left, Font::BRAND);

        let nav_row = |i: usize| Rect::new(rail.x + 10.0, rail.y + 60.0 + i as f32 * (NAV_ROW + 4.0), rail.w - 20.0, NAV_ROW);
        // The highlight slides to the section picked.
        let lit = Rect { y: self.nav_y.get(nav_row(self.active).y, 0.22), ..nav_row(self.active) };
        c.fill_round(lit, 14.0, t.milk);
        c.fill_a(Rect::new(lit.x + 14.0, lit.y + 1.0, lit.w - 28.0, 1.0), t.milk_hi, 1.0);
        for (i, label) in TABS.into_iter().enumerate() {
            let r = nav_row(i);
            self.nav_rects[i] = r;
            let ink = if i == self.active {
                t.ink
            } else {
                if self.nav_hover == Some(i) {
                    c.fill_round_a(r, 14.0, t.milk_hi, 0.07);
                }
                t.text.mix(t.text_dim, 0.45)
            };
            nav_icon(c, i, r.x + 21.0, r.y + r.h / 2.0, ink);
            c.text_font(label, Rect::new(r.x + 44.0, r.y, r.w - 50.0, r.h), ink, Align::Left, Font::BODY_BOLD);
        }

        // Footer: who Flask runs as.
        let foot_y = rail.bottom() - 14.0 - 24.0;
        let mode = if self.elevated { "Admin" } else { "Not elevated" };
        let chip = Rect::new(rail.x + 12.0, foot_y, c.measure_font(mode, Font::LABEL) + 22.0, 24.0);
        c.stroke_round_a(chip, 12.0, t.milk_hi, 0.16);
        c.text_font(mode, chip, t.text_dim, Align::Center, Font::LABEL);

        // Meters, when the rail is tall enough to hold them under the sections.
        let meters = Rect::new(rail.x + 10.0, foot_y - 12.0 - 118.0, rail.w - 20.0, 118.0);
        if meters.y < self.nav_rects[TABS.len() - 1].bottom() + 10.0 {
            return;
        }
        c.well(meters, 16.0);
        let mut value = String::new();
        for (i, (label, percent)) in [("CPU", self.cpu), ("GPU", self.gpu), ("Memory", self.memory)].into_iter().enumerate() {
            let percent = self.meter_anim[i].get(percent, 0.4);
            let row = Rect::new(meters.x + 12.0, meters.y + 10.0 + i as f32 * 34.0, meters.w - 24.0, 18.0);
            c.text(label, row, t.text_dim, Align::Left, false);
            value.clear();
            let _ = write!(value, "{percent:.0}%");
            c.text(&value, row, t.text, Align::Right, true);
            let track = Rect::new(row.x, row.bottom() + 3.0, row.w, 6.0);
            c.fill_round_a(track, 3.0, t.milk_hi, 0.10);
            let level = (percent / 100.0).clamp(0.0, 1.0);
            if level > 0.0 {
                c.pill(Rect { w: (track.w * level).max(6.0), ..track }, t.milk);
            }
        }
    }

    /// The Settings page: one row per setting, its switch on the right.
    fn paint_settings(&mut self, c: &mut Canvas, area: Rect) {
        const ROW: f32 = 64.0;
        let t = c.theme;
        for toggle in [&mut self.pin_btn, &mut self.still_btn] {
            toggle.label = if toggle.active { "On" } else { "Off" };
        }
        let rows = [
            ("Keep on top", "Flask stays above other windows.", &mut self.pin_btn),
            ("Disable animations", "Panels, meters and highlights move at once instead of easing.", &mut self.still_btn),
            ("Refresh rate", "How often Flask reads the system.", &mut self.speed_btn),
        ];
        let card = Rect::new(area.x + 12.0, area.y + 8.0, (area.w - 24.0).clamp(0.0, 680.0), rows.len() as f32 * ROW + 16.0);
        c.glass(card, 22.0);
        for (i, (title, about, button)) in rows.into_iter().enumerate() {
            let row = Rect::new(card.x + 20.0, card.y + 8.0 + i as f32 * ROW, card.w - 40.0, ROW);
            if i > 0 {
                c.fill_a(Rect { h: 1.0, ..row }, t.milk_hi, 0.08);
            }
            let bw = button.width(c).max(64.0);
            button.rect = Rect::new(row.right() - bw, row.y + (ROW - 26.0) / 2.0, bw, 26.0);
            button.paint(c);
            let text_w = (button.rect.x - 16.0 - row.x).max(0.0);
            c.text_font(title, Rect::new(row.x, row.y + 12.0, text_w, 20.0), t.text, Align::Left, Font::BODY_BOLD);
            c.text_font(about, Rect::new(row.x, row.y + 32.0, text_w, 20.0), t.text_dim, Align::Left, Font::SMALL);
        }
    }

    fn paint_compact(&mut self, c: &mut Canvas, w: f32, h: f32) {
        let t = c.theme;
        c.backdrop(w, h);
        c.glass(Rect::new(4.0, 4.0, (w - 8.0).max(0.0), (h - 8.0).max(0.0)), 18.0);
        c.icon(&self.exe, Rect::new(16.0, (TITLE_H - 20.0) / 2.0, 20.0, 20.0));
        c.text_font("Flask", Rect::new(45.0, 0.0, 60.0, TITLE_H), t.text, Align::Left, Font::BRAND);
        self.window_buttons.rect = Rect::new(w - WindowButtons::WIDTH, 0.0, WindowButtons::WIDTH, TITLE_H);
        self.window_buttons.paint(c, self.maximized);
        let bw = self.compact_btn.width(c);
        self.compact_btn.rect = Rect::new(self.window_buttons.rect.x - 10.0 - bw, (TITLE_H - 22.0) / 2.0, bw, 22.0);
        self.compact_btn.paint(c);
        self.processes.paint_compact(c, Rect::new(0.0, TITLE_H, w, (h - TITLE_H).max(0.0)));
    }
}

impl sc_ui::App for App {
    fn paint(&mut self, c: &mut Canvas, w: f32, h: f32) {
        if self.compact.is_some() {
            return self.paint_compact(c, w, h);
        }
        let t = c.theme;
        c.backdrop(w, h);
        self.paint_rail(c, h);

        // Strip above the page: the section's name, window controls on the right.
        let x0 = PAD + RAIL_W + PAD;
        c.text_font(TABS[self.active], Rect::new(x0 + 4.0, 4.0, 300.0, TITLE_H), t.text, Align::Left, Font::DISPLAY);
        self.window_buttons.rect = Rect::new(w - WindowButtons::WIDTH - 4.0, 0.0, WindowButtons::WIDTH, TITLE_H);
        self.window_buttons.paint(c, self.maximized);
        let bw = self.compact_btn.width(c);
        self.compact_btn.rect = Rect::new(self.window_buttons.rect.x - 10.0 - bw, (TITLE_H - 24.0) / 2.0, bw, 24.0);
        self.compact_btn.paint(c);
        // These live on the Settings page, and are out of reach elsewhere.
        for button in [&mut self.pin_btn, &mut self.still_btn, &mut self.speed_btn] {
            button.rect = Rect::default();
        }

        // Pages keep their own 12 px margin, so this lines their cards up
        // with the rail's gap on the left and the window's on the right.
        let page = Rect::new(x0 - 12.0, TITLE_H, (w - x0 - PAD + 24.0).max(0.0), (h - TITLE_H - STATUS_H).max(0.0));
        // A page just switched to rises into place.
        let drop = (1.0 - self.page_in.get(1.0, 0.22)) * 14.0;
        let page = Rect { y: page.y + drop, h: (page.h - drop).max(0.0), ..page };
        match self.active {
            TAB_PROCESSES => self.processes.paint(c, page),
            TAB_PERFORMANCE => self.performance.paint(c, page),
            TAB_STARTUP => self.startup.paint(c, page),
            TAB_SERVICES => self.services.paint(c, page),
            TAB_NETWORK => self.connections.paint(c, page),
            TAB_ACTIVITY => self.activity.paint(c, page),
            _ => self.paint_settings(c, page),
        }

        let bar = Rect::new(x0 + 6.0, h - STATUS_H - 2.0, (w - x0 - PAD - 12.0).max(0.0), STATUS_H);
        self.status.clear();
        let _ = write!(
            self.status,
            "{} processes      {} threads      {} handles",
            self.processes.count(),
            self.threads,
            self.handles
        );
        c.text_font(&self.status, bar, t.text_dim, Align::Left, Font::SMALL);
        let left_w = c.measure_font(&self.status, Font::SMALL) + 24.0;
        let rest = Rect { x: bar.x + left_w, w: (bar.w - left_w).max(0.0), ..bar };
        match self.notice() {
            "" => c.text_font(self.hints(), rest, t.text_dim, Align::Right, Font::SMALL),
            notice => c.text_font(notice, rest, t.peach, Align::Right, Font::LABEL),
        }
    }

    fn is_caption(&self, x: f32, y: f32) -> bool {
        let on_button = self.compact_btn.rect.contains(x, y);
        // The strip above the page, and in the full window the rail's logo too.
        let strip = y < TITLE_H && (self.compact.is_some() || x > PAD + RAIL_W);
        let logo = self.compact.is_none() && x < PAD + RAIL_W && y < PAD + 52.0;
        (strip || logo) && !on_button && !self.window_buttons.rect.contains(x, y)
    }

    fn event(&mut self, ev: &Event, win: &Win) {
        match *ev {
            Event::User(ORIGIN_READY) => {
                if self.processes.origin_ready() {
                    win.invalidate();
                }
                return;
            }
            Event::User(SPECS_READY) => {
                if let Some(specs) = self.specs_slot.lock().unwrap().take() {
                    self.performance.set_specs(specs);
                    win.invalidate();
                }
                return;
            }
            Event::User(STARTUP_READY) => {
                if self.startup.poll() && self.active == TAB_STARTUP {
                    win.invalidate();
                }
                return;
            }
            Event::User(DUMP_READY) => {
                self.processes.dump_ready();
                win.invalidate();
                return;
            }
            Event::User(SPECIAL_READY) => {
                self.processes.special_ready();
                win.invalidate();
                return;
            }
            Event::User(AWAKE_READY) => {
                self.processes.awake_ready();
                win.invalidate();
                return;
            }
            Event::User(SERVICES_READY) => {
                if self.services.poll() && self.active == TAB_SERVICES {
                    win.invalidate();
                }
                return;
            }
            Event::User(CONNECTIONS_READY) => {
                if self.connections.poll() && self.active == TAB_NETWORK {
                    win.invalidate();
                }
                return;
            }
            Event::User(_) => {
                if let Some(mut frame) = self.sampler.take() {
                    self.threads = frame.threads;
                    self.handles = frame.handles;
                    self.cpu = frame.cpu_total;
                    self.gpu = frame.gpu_total;
                    self.memory = frame.memory.used_percent();
                    self.performance.push(std::mem::take(&mut frame.perf), frame.gpu_total, frame.gpu_memory_used);
                    self.activity.observe(&frame.procs);
                    self.processes.set_feed(self.activity.recent(FEED_ROWS));
                    if self.active == TAB_NETWORK {
                        self.connections.tick(win, &frame.procs);
                    }
                    self.processes.set_frame(frame);
                    self.processes.tick(win);
                    if self.active == TAB_SERVICES {
                        self.services.tick(win);
                    }
                    win.invalidate();
                }
                return;
            }
            Event::Resized => {
                self.maximized = win.is_maximized();
                return;
            }
            Event::Closing => return self.save_settings(win),
            Event::Key { vk: key::TAB, ctrl: true, shift } if self.compact.is_none() => {
                // Shift goes backwards.
                let step = if shift { TABS.len() - 1 } else { 1 };
                self.switch_to((self.active + step) % TABS.len(), win);
                win.invalidate();
                return;
            }
            Event::Key { vk: key::N, ctrl: true, .. } => return run_task(win),
            Event::MouseDown { .. } | Event::Key { .. } => self.flash.clear(),
            _ => {}
        }

        let (mut redraw, action) = self.window_buttons.event(ev);
        match action {
            Some(WindowAction::Minimize) => win.minimize(),
            Some(WindowAction::ToggleMaximize) => win.toggle_maximize(),
            Some(WindowAction::Close) => win.close(),
            None => {}
        }
        let (r, clicked) = self.compact_btn.event(ev);
        redraw |= r;
        if clicked {
            self.set_compact(self.compact.is_none(), win);
            win.invalidate();
            return;
        }
        if self.compact.is_some() {
            if redraw | self.processes.event_compact(ev, win, &self.sampler) {
                win.invalidate();
            }
            return;
        }

        // The rail's sections.
        let mut switched = None;
        match *ev {
            Event::MouseMove { x, y } => {
                let hover = self.nav_rects.iter().position(|r| r.contains(x, y));
                redraw |= std::mem::replace(&mut self.nav_hover, hover) != hover;
            }
            Event::MouseLeave => redraw |= self.nav_hover.take().is_some(),
            Event::MouseDown { x, y, button: MouseButton::Left, .. } => {
                switched = self.nav_rects.iter().position(|r| r.contains(x, y)).filter(|&tab| tab != self.active);
            }
            _ => {}
        }
        if let Some(tab) = switched {
            self.switch_to(tab, win);
            redraw = true;
        }
        let (r, clicked) = self.pin_btn.event(ev);
        redraw |= r;
        if clicked {
            self.pin_btn.active = !self.pin_btn.active;
            win.set_topmost(self.pin_btn.active);
            redraw = true;
        }
        let (r, clicked) = self.still_btn.event(ev);
        redraw |= r;
        if clicked {
            self.still_btn.active = !self.still_btn.active;
            anim::set_enabled(!self.still_btn.active);
            redraw = true;
        }
        let (r, clicked) = self.speed_btn.event(ev);
        redraw |= r;
        if clicked {
            let mut menu = Menu::new();
            for (i, &(ms, label)) in INTERVALS.iter().enumerate() {
                menu.item_with(i as u32 + 1, label, ms == self.refresh_ms, true);
            }
            if let Some(&(ms, _)) = win.popup(&menu).and_then(|cmd| INTERVALS.get(cmd as usize - 1)) {
                self.set_refresh(ms);
            }
            redraw = true;
        }
        // The click that switches tabs is not also a click on the new page.
        if switched.is_none() {
            redraw |= match self.active {
                TAB_PROCESSES => self.processes.event(ev, win, &self.sampler),
                TAB_PERFORMANCE => self.performance.event(ev, win),
                TAB_STARTUP => self.startup.event(ev, win),
                TAB_SERVICES => self.services.event(ev, win),
                TAB_NETWORK => self.connections.event(ev, win),
                TAB_ACTIVITY => self.activity.event(ev, win),
                _ => false,
            };
        }
        let goto = (self.processes.goto.take())
            .or_else(|| self.services.goto.take())
            .or_else(|| self.startup.goto.take())
            .or_else(|| self.connections.goto.take())
            .or_else(|| self.activity.goto.take());
        if let Some(goto) = goto {
            self.follow(goto, win);
            redraw = true;
        }
        if redraw {
            win.invalidate();
        }
    }
}
