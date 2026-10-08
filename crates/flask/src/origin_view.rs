//! The Origin drawer: everything Flask can find out about where a process
//! came from, laid out so something shady stands out.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex};

use sc_core::autoruns;
use sc_core::origin::{self, Origin, Signature};
use sc_core::{actions, procinfo, system};
use sc_ui::{Align, Button, ButtonKind, Canvas, Event, Font, MouseButton, Rect, Win, key, shell};

use crate::app;
use crate::format;
use crate::sampler::Proc;

pub const DRAWER_W: f32 = 560.0;
/// `Event::User` code posted when a background origin lookup finishes.
pub const ORIGIN_READY: usize = 1;
const PAD: f32 = 20.0;
const ROW: f32 = 22.0;
const LABEL_W: f32 = 92.0;
const MAX_CHAIN: usize = 8;
/// Height of the fixed header above the scrolling sections.
const HEADER_H: f32 = 84.0;

/// A process that has exited but may still be somebody's parent.
pub struct Gone {
    pub name: Arc<str>,
    pub create_time: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub name: String,
    pub pid: u32,
    pub exited: bool,
}

/// Parents of `pid` back to the root, root first, ending with the process
/// itself. A parent that has exited is found in `gone`. A claimed parent
/// created after its child is a reused PID and ends the chain.
pub fn launch_chain(pid: u32, procs: &[Proc], gone: &HashMap<u32, Gone>) -> Vec<Link> {
    let live: HashMap<u32, &Proc> = procs.iter().map(|p| (p.row.pid, p)).collect();
    let mut chain = Vec::new();
    let Some(start) = live.get(&pid) else { return chain };
    chain.push(Link { name: start.row.name.to_string(), pid, exited: false });
    let (mut parent, mut child_created) = (start.row.parent_pid, start.row.create_time);
    while chain.len() < 32 {
        let (name, created, next, exited) = match (live.get(&parent), gone.get(&parent)) {
            (Some(p), _) if p.row.create_time <= child_created => {
                (p.row.name.to_string(), p.row.create_time, Some(p.row.parent_pid), false)
            }
            (_, Some(g)) if g.create_time <= child_created => (g.name.to_string(), g.create_time, None, true),
            _ => break,
        };
        if chain.iter().any(|l| l.pid == parent) {
            break;
        }
        chain.push(Link { name, pid: parent, exited });
        match next {
            // PID 0 lists itself as its parent.
            Some(next) if next != parent => (parent, child_created) = (next, created),
            _ => break,
        }
    }
    chain.reverse();
    chain
}

struct Target {
    pid: u32,
    create_time: i64,
    name: String,
    path: String,
    cmdline: String,
    user: String,
    company: String,
    session: u32,
    chain: Vec<Link>,
    exited: bool,
}

/// What a lookup thread hands back: its token, the findings, and the modules.
type Lookup = (u64, Origin, Vec<String>);

pub struct OriginView {
    target: Option<Target>,
    origin: Option<Origin>,
    /// Program and DLLs loaded into the target, the ones from outside the
    /// Windows folder first.
    modules: Vec<String>,
    // Result slot shared with the lookup thread; the token discards results
    // for a process the user has already moved on from.
    slot: Arc<Mutex<Option<Lookup>>>,
    token: u64,
    rect: Rect,
    open_btn: Button,
    end_btn: Button,
    copy_btn: Button,
    lookup_btn: Button,
    close_rect: Rect,
    close_hover: bool,
    scroll: f32,
    max_scroll: f32,
    /// Disable/Enable button of each autostart entry, as last painted.
    toggle_rects: Vec<Rect>,
}

/// What the drawer asks its owner to do.
pub enum Outcome {
    None,
    Redraw,
    /// The drawer handled the event; nothing underneath should see it.
    Consumed,
    /// Ending the target failed; the text is for the status bar.
    Failed(String),
    /// The target was ended.
    Ended,
}

impl OriginView {
    pub fn new() -> Self {
        Self {
            target: None,
            origin: None,
            modules: Vec::new(),
            slot: Arc::default(),
            token: 0,
            rect: Rect::default(),
            open_btn: Button::new("Open path", ButtonKind::Cocoa),
            end_btn: Button::new("End", ButtonKind::Peach),
            copy_btn: Button::new("Copy details", ButtonKind::Cocoa),
            lookup_btn: Button::new("Look up hash", ButtonKind::Cocoa),
            close_rect: Rect::default(),
            close_hover: false,
            scroll: 0.0,
            max_scroll: 0.0,
            toggle_rects: Vec::new(),
        }
    }

    pub fn close(&mut self) {
        self.target = None;
        self.origin = None;
        self.token += 1;
    }

    /// Opens the drawer for `p` and starts the lookup on a worker thread.
    pub fn open_for(&mut self, p: &Proc, procs: &[Proc], gone: &HashMap<u32, Gone>, win_id: isize) {
        self.token += 1;
        self.origin = None;
        self.modules.clear();
        self.scroll = 0.0;
        self.target = Some(Target {
            pid: p.row.pid,
            create_time: p.row.create_time,
            name: p.row.name.to_string(),
            path: p.info.path.clone(),
            cmdline: p.info.cmdline.clone(),
            user: p.info.user.clone(),
            company: p.info.company.to_string(),
            session: p.row.session,
            chain: launch_chain(p.row.pid, procs, gone),
            exited: false,
        });
        let (slot, token, pid, path) = (self.slot.clone(), self.token, p.row.pid, p.info.path.clone());
        std::thread::spawn(move || {
            let mut modules = procinfo::modules(pid);
            let windows = std::env::var("SystemRoot").unwrap_or_default().to_lowercase();
            // Stable, so each group stays in load order.
            modules.sort_by_key(|m| !windows.is_empty() && m.to_lowercase().starts_with(&windows));
            let result = origin::collect(pid, &path);
            *slot.lock().unwrap() = Some((token, result, modules));
            Win::post(win_id, ORIGIN_READY);
        });
    }

    /// Picks up a finished lookup. Returns true when the drawer changed.
    pub fn poll(&mut self) -> bool {
        match self.slot.lock().unwrap().take() {
            Some((token, origin, modules)) if token == self.token && self.target.is_some() => {
                self.origin = Some(origin);
                self.modules = modules;
                true
            }
            _ => false,
        }
    }

    /// Notes whether the target is still running.
    pub fn sync(&mut self, procs: &[Proc]) {
        if let Some(t) = &mut self.target {
            t.exited = !procs.iter().any(|p| (p.row.pid, p.row.create_time) == (t.pid, t.create_time));
        }
    }

    fn details_text(&self) -> String {
        let Some(t) = &self.target else { return String::new() };
        let mut s = String::new();
        let _ = writeln!(s, "{} (PID {})", t.name, t.pid);
        let _ = writeln!(s, "Path: {}", t.path);
        let _ = writeln!(s, "Command: {}", t.cmdline);
        let _ = writeln!(s, "Started: {}", system::format_filetime(t.create_time));
        let _ = writeln!(s, "User: {}  Session: {}", t.user, t.session);
        let chain: Vec<String> = t.chain.iter().map(|l| format!("{} ({})", l.name, l.pid)).collect();
        let _ = writeln!(s, "Launched by: {}", chain.join(" > "));
        if let Some(o) = &self.origin {
            let _ = writeln!(s, "Signature: {}", signature_text(&o.signature));
            let _ = writeln!(s, "Company (unverified): {}", t.company);
            let _ = writeln!(s, "Integrity: {}", o.integrity.unwrap_or("unknown"));
            if let Some(hash) = &o.sha256 {
                let _ = writeln!(s, "SHA-256: {hash}");
            }
            if let Some(z) = &o.zone {
                let _ = writeln!(s, "Downloaded: {} zone, from {} (referrer {})", z.label(), z.host, z.referrer);
            }
            for name in &o.services {
                let _ = writeln!(s, "Hosts service: {name}");
            }
            for src in &o.start_sources {
                let state = if src.enabled { "" } else { " (disabled)" };
                let _ = writeln!(s, "Starts via {}: {} in {}{state}", src.category.label(), src.name, src.location);
            }
            for w in o.warnings() {
                let _ = writeln!(s, "Check: {w}");
            }
        }
        s
    }

    pub fn paint(&mut self, c: &mut Canvas, area: Rect) {
        let Some(t) = &self.target else { return };
        let th = c.theme;
        let w = DRAWER_W.min(area.w);
        let r = Rect::new(area.right() - w, area.y, w, area.h);
        self.rect = r;
        c.fill(r, th.card);
        c.fill(Rect::new(r.x, r.y, 1.0, r.h), th.line);
        c.clip(r);

        // Header.
        let icon = Rect::new(r.x + PAD, r.y + 14.0, 34.0, 34.0);
        if !c.icon(&t.path, icon) {
            c.fill_round(icon, 10.0, th.cocoa);
        }
        let ew = self.end_btn.width(c);
        let ow = self.open_btn.width(c);
        self.close_rect = Rect::new(r.right() - PAD - 22.0, r.y + 20.0, 26.0, 26.0);
        self.end_btn.rect = Rect::new(self.close_rect.x - 10.0 - ew, r.y + 18.0, ew, 30.0);
        self.open_btn.rect =
            if t.path.is_empty() { Rect::default() } else { Rect::new(self.end_btn.rect.x - 8.0 - ow, r.y + 18.0, ow, 30.0) };
        let text_right = if t.path.is_empty() { self.end_btn.rect.x } else { self.open_btn.rect.x } - 12.0;
        let head = Rect::new(icon.right() + 12.0, r.y + 21.0, (text_right - icon.right() - 12.0).max(0.0), 24.0);
        c.text_font(&t.name, head, th.text, Align::Left, Font::HEADING);
        let name_w = c.measure_font(&t.name, Font::HEADING) + 10.0;
        let pid_text = if t.exited { format!("PID {}, exited", t.pid) } else { format!("PID {}", t.pid) };
        if name_w < head.w {
            c.text(&pid_text, Rect { x: head.x + name_w, w: head.w - name_w, ..head }, th.text_dim, Align::Left, false);
        }
        let verdict = Rect::new(r.x + PAD, r.y + 54.0, r.w - 2.0 * PAD, 20.0);
        match &self.origin {
            None => c.text("Checking signature, download source and autostart entries", verdict, th.text_dim, Align::Left, false),
            Some(o) => match o.warnings().as_slice() {
                [] => c.text("Nothing unusual found", verdict, th.text_dim, Align::Left, false),
                ws => c.text(&format!("To check: {}", ws.join(", ")), verdict, th.butter, Align::Left, false),
            },
        }
        if !t.path.is_empty() {
            self.open_btn.paint(c);
        }
        if !t.exited {
            self.end_btn.paint(c);
        }
        let (cx, cy) = (self.close_rect.x + 13.0, self.close_rect.y + 13.0);
        if self.close_hover {
            c.dot(cx, cy, 13.0, th.raised);
        }
        c.line(cx - 4.5, cy - 4.5, cx + 4.5, cy + 4.5, 1.4, th.text_dim);
        c.line(cx + 4.5, cy - 4.5, cx - 4.5, cy + 4.5, 1.4, th.text_dim);

        c.unclip();

        // Sections scroll under the header when the window is short.
        let body = Rect::new(r.x, r.y + HEADER_H, r.w, (r.h - HEADER_H).max(0.0));
        c.clip(body);
        let x = r.x + PAD;
        let sw = r.w - 2.0 * PAD;
        let mut y = body.y - self.scroll;
        let section = |c: &mut Canvas, x: f32, y: f32, w: f32, title: &str, rows: usize| -> f32 {
            let h = 12.0 + 22.0 + rows as f32 * ROW + 10.0;
            c.fill_round(Rect::new(x, y, w, h), 14.0, th.ground);
            c.text_font(title, Rect::new(x + 14.0, y + 10.0, w - 28.0, 22.0), th.text, Align::Left, Font::TITLE);
            y + 12.0 + 24.0
        };
        let kv = |c: &mut Canvas, x: f32, y: f32, w: f32, label: &str, value: &str, color| {
            c.text(label, Rect::new(x + 14.0, y, LABEL_W, ROW), th.text_dim, Align::Left, false);
            let value = if value.is_empty() { "Unknown" } else { value };
            c.text(value, Rect::new(x + 14.0 + LABEL_W, y, w - 28.0 - LABEL_W, ROW), color, Align::Left, false);
        };

        // Who launched it.
        let shown = &t.chain[t.chain.len().saturating_sub(MAX_CHAIN)..];
        let mut ry = section(c, x, y, sw, "Who launched it", shown.len().max(1));
        if shown.is_empty() {
            c.text("The process exited before its parents could be read", Rect::new(x + 14.0, ry, sw - 28.0, ROW), th.text_dim, Align::Left, false);
        }
        for (i, link) in shown.iter().enumerate() {
            let indent = x + 14.0 + i as f32 * 16.0;
            let me = i + 1 == shown.len();
            c.dot(indent + 4.0, ry + ROW / 2.0, 4.0, if me { th.milk } else { th.cocoa });
            let name_r = Rect::new(indent + 16.0, ry, (x + sw - 14.0 - indent - 16.0).max(0.0), ROW);
            c.text(&link.name, name_r, th.text, Align::Left, true);
            let nw = c.measure(&link.name, true) + 8.0;
            let note = if link.exited { format!("PID {}, exited", link.pid) } else { format!("PID {}", link.pid) };
            if nw < name_r.w {
                c.text(&note, Rect { x: name_r.x + nw, w: name_r.w - nw, ..name_r }, th.text_dim, Align::Left, false);
            }
            ry += ROW;
        }
        y = ry + 10.0 + 12.0;

        // What makes it start.
        let (services, sources) = match &self.origin {
            Some(o) => (o.services.as_slice(), o.start_sources.as_slice()),
            None => (&[][..], &[][..]),
        };
        let rows = (services.len() + sources.len()).max(1);
        let mut ry = section(c, x, y, sw, "What makes it start", rows);
        // `reserve` keeps room on the right for a button.
        let entry = |c: &mut Canvas, ry: f32, kind: &str, text: &str, reserve: f32| {
            let cw = c.measure_font(kind, Font::LABEL) + 18.0;
            let chip = Rect::new(x + 14.0, ry + 1.0, cw, ROW - 2.0);
            c.pill(chip, th.raised);
            c.text_font(kind, chip, th.butter, Align::Center, Font::LABEL);
            let text_r = Rect::new(chip.right() + 10.0, ry, (sw - 38.0 - cw - reserve).max(0.0), ROW);
            c.text(text, text_r, th.text, Align::Left, false);
        };
        if self.origin.is_none() {
            c.text("Looking", Rect::new(x + 14.0, ry, sw - 28.0, ROW), th.text_dim, Align::Left, false);
        } else if services.is_empty() && sources.is_empty() {
            let none = "No service, Run key, Startup folder item or scheduled task launches this file";
            c.text(none, Rect::new(x + 14.0, ry, sw - 28.0, ROW), th.text_dim, Align::Left, false);
        }
        for name in services {
            entry(c, ry, "Service", name, 0.0);
            ry += ROW;
        }
        self.toggle_rects.clear();
        for s in sources {
            let label = if s.enabled { "Disable" } else { "Enable" };
            let bw = c.measure_font(label, Font::LABEL) + 22.0;
            let btn = Rect::new(x + sw - 14.0 - bw, ry + 1.0, bw, ROW - 2.0);
            let state = if s.enabled { "" } else { "  (disabled)" };
            entry(c, ry, s.category.label(), &format!("{}  in  {}{state}", s.name, s.location), bw + 10.0);
            let (top, bottom, ink) = if s.enabled { (th.milk_hi, th.milk, th.ink) } else { (th.cocoa, th.cocoa, th.text) };
            c.button_body(btn, top, bottom);
            c.text_font(label, Rect { h: btn.h - 2.0, ..btn }, ink, Align::Center, Font::LABEL);
            self.toggle_rects.push(btn);
            ry += ROW;
        }
        y = y + 12.0 + 24.0 + rows as f32 * ROW + 10.0 + 12.0;

        // Who made it / How it runs, side by side.
        let half = (sw - 12.0) / 2.0;
        let ry = section(c, x, y, half, "Who made it", 2);
        let (sig, sig_color) = match &self.origin {
            None => ("Checking".to_owned(), th.text_dim),
            Some(o) => {
                let bad = matches!(o.signature, Signature::Unsigned | Signature::Invalid { .. });
                (signature_text(&o.signature), if bad { th.peach } else { th.text })
            }
        };
        kv(c, x, ry, half, "Signature", &sig, sig_color);
        kv(c, x, ry + ROW, half, "File says", if t.company.is_empty() { "No company name" } else { &t.company }, th.text_dim);

        let x2 = x + half + 12.0;
        let ry2 = section(c, x2, y, half, "How it runs", 2);
        let integrity = self.origin.as_ref().and_then(|o| o.integrity).unwrap_or("");
        kv(c, x2, ry2, half, "User", &format!("{} (session {})", if t.user.is_empty() { "Unknown" } else { &t.user }, t.session), th.text);
        kv(c, x2, ry2 + ROW, half, "Integrity", integrity, th.text);
        y = y + 12.0 + 24.0 + 2.0 * ROW + 10.0 + 12.0;

        // Where the file came from.
        let ry = section(c, x, y, sw, "Where the file came from", 5);
        let scratch = self.origin.as_ref().is_some_and(|o| o.scratch_location);
        kv(c, x, ry, sw, "Path", &t.path, if scratch { th.butter } else { th.text });
        kv(c, x, ry + ROW, sw, "Command", &t.cmdline, th.text);
        kv(c, x, ry + 2.0 * ROW, sw, "Started", &system::format_filetime(t.create_time), th.text);
        let file = self.origin.as_ref().and_then(|o| o.file.as_ref());
        let created = file.map_or(String::new(), |f| {
            let mut s = f.created.map(|u| system::format_filetime(system::unix_to_filetime(u))).unwrap_or_default();
            s.push_str("   ");
            format::bytes(&mut s, f.size);
            s
        });
        kv(c, x, ry + 3.0 * ROW, sw, "File created", &created, th.text);
        match self.origin.as_ref().map(|o| &o.zone) {
            Some(Some(z)) => {
                let from = if z.host.is_empty() { &z.referrer } else { &z.host };
                let text = if from.is_empty() { format!("{} zone", z.label()) } else { format!("{} zone, {from}", z.label()) };
                kv(c, x, ry + 4.0 * ROW, sw, "Downloaded", &text, if z.id >= 3 { th.butter } else { th.text });
            }
            Some(None) => kv(c, x, ry + 4.0 * ROW, sw, "Downloaded", "No download mark on the file", th.text_dim),
            None => kv(c, x, ry + 4.0 * ROW, sw, "Downloaded", "Checking", th.text_dim),
        }
        y = y + 12.0 + 24.0 + 5.0 * ROW + 10.0 + 12.0;

        // Loaded modules. Hundreds of rows, so only the ones in view are drawn.
        let title = if self.origin.is_some() { format!("Loaded modules ({})", self.modules.len()) } else { "Loaded modules".to_owned() };
        let ry = section(c, x, y, sw, &title, self.modules.len().max(1));
        if self.modules.is_empty() {
            let why = if self.origin.is_some() { "Flask is not allowed to read this process" } else { "Looking" };
            c.text(why, Rect::new(x + 14.0, ry, sw - 28.0, ROW), th.text_dim, Align::Left, false);
        }
        let windows = std::env::var("SystemRoot").unwrap_or_default().to_lowercase();
        for (i, module) in self.modules.iter().enumerate() {
            let my = ry + i as f32 * ROW;
            if my + ROW >= body.y && my <= body.bottom() {
                let system = !windows.is_empty() && module.to_lowercase().starts_with(&windows);
                let color = if system { th.text_dim } else { th.text };
                c.text(module, Rect::new(x + 14.0, my, sw - 28.0, ROW), color, Align::Left, false);
            }
        }
        y = y + 12.0 + 24.0 + self.modules.len().max(1) as f32 * ROW + 10.0 + 12.0;

        let cw = self.copy_btn.width(c);
        self.copy_btn.rect = Rect::new(x, y, cw, 30.0);
        self.copy_btn.paint(c);
        // Only once there is a hash to look up.
        self.lookup_btn.rect = if self.origin.as_ref().is_some_and(|o| o.sha256.is_some()) {
            Rect::new(x + cw + 8.0, y, self.lookup_btn.width(c), 30.0)
        } else {
            Rect::default()
        };
        if self.lookup_btn.rect.w > 0.0 {
            self.lookup_btn.paint(c);
        }
        c.unclip();
        let content_h = y + 30.0 + 16.0 + self.scroll - body.y;
        self.max_scroll = (content_h - body.h).max(0.0);
        self.scroll = self.scroll.min(self.max_scroll);
    }

    pub fn event(&mut self, ev: &Event, win: &Win) -> Outcome {
        let Some(t) = &self.target else { return Outcome::None };
        if let Event::Key { vk: key::ESCAPE, .. } = *ev {
            self.close();
            return Outcome::Consumed;
        }
        let pos = match *ev {
            Event::MouseMove { x, y }
            | Event::MouseDown { x, y, .. }
            | Event::MouseUp { x, y, .. }
            | Event::Wheel { x, y, .. } => Some((x, y)),
            _ => None,
        };
        let inside = pos.is_some_and(|(x, y)| self.rect.contains(x, y));

        let mut redraw = false;
        let (r1, open) = self.open_btn.event(ev);
        let (r2, end) = self.end_btn.event(ev);
        let (r3, copy) = self.copy_btn.event(ev);
        let (r4, lookup) = self.lookup_btn.event(ev);
        redraw |= r1 | r2 | r3 | r4;
        if let Some((x, y)) = pos {
            let hover = self.close_rect.contains(x, y);
            redraw |= std::mem::replace(&mut self.close_hover, hover) != hover;
        }
        if let Event::MouseDown { x, y, button: MouseButton::Left, .. } = *ev
            && self.close_rect.contains(x, y)
        {
            self.close();
            return Outcome::Consumed;
        }
        if let Event::MouseDown { x, y, button: MouseButton::Left, .. } = *ev
            && let Some(i) = self.toggle_rects.iter().position(|r| r.contains(x, y))
            && let Some(src) = self.origin.as_mut().and_then(|o| o.start_sources.get_mut(i))
        {
            // Same reversible switch the Startup tab uses.
            return match autoruns::set_enabled(src, !src.enabled) {
                Ok(()) => {
                    src.enabled = !src.enabled;
                    Outcome::Consumed
                }
                Err(why) => Outcome::Failed(format!("Could not change {}: {why}", src.name)),
            };
        }
        if open {
            shell::reveal_in_explorer(&t.path);
            return Outcome::Consumed;
        }
        if copy {
            win.copy_text(&self.details_text());
            return Outcome::Consumed;
        }
        if lookup {
            // Only the fingerprint leaves the machine, never the file.
            if let Some(hash) = self.origin.as_ref().and_then(|o| o.sha256.as_ref()) {
                shell::open_url(&format!("https://www.virustotal.com/gui/file/{hash}"));
            }
            return Outcome::Consumed;
        }
        if end && !t.exited {
            if !app::confirm_end(win, &[(t.pid, t.name.as_str().into())]) {
                return Outcome::Consumed;
            }
            return match actions::terminate(t.pid) {
                Ok(()) => Outcome::Ended,
                Err(e) => Outcome::Failed(format!("Could not end {} (PID {}): {}", t.name, t.pid, e.message())),
            };
        }
        if inside {
            if let Event::Wheel { delta, .. } = *ev {
                self.scroll = (self.scroll - delta * 48.0).clamp(0.0, self.max_scroll);
            }
            Outcome::Consumed
        } else if redraw {
            Outcome::Redraw
        } else {
            Outcome::None
        }
    }
}

fn signature_text(s: &Signature) -> String {
    match s {
        Signature::Valid { signer } if signer.is_empty() => "Verified".into(),
        Signature::Valid { signer } => format!("Verified: {signer}"),
        Signature::Unsigned => "Not signed".into(),
        Signature::Invalid { code } => format!("Does not verify (0x{code:08X})"),
        Signature::Unknown => "Could not read the file".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_core::process::ProcessRow;

    fn proc(pid: u32, parent: u32, created: i64, name: &str) -> Proc {
        Proc {
            row: ProcessRow {
                pid,
                parent_pid: parent,
                create_time: created,
                name: name.into(),
                cpu: 0.0,
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

    fn names(chain: &[Link]) -> Vec<(&str, bool)> {
        chain.iter().map(|l| (l.name.as_str(), l.exited)).collect()
    }

    #[test]
    fn chain_runs_root_first_through_live_parents() {
        let procs = [proc(4, 0, 1, "System"), proc(100, 4, 10, "services.exe"), proc(200, 100, 20, "svchost.exe")];
        let chain = launch_chain(200, &procs, &HashMap::new());
        assert_eq!(names(&chain), [("System", false), ("services.exe", false), ("svchost.exe", false)]);
    }

    #[test]
    fn chain_survives_an_exited_parent_and_stops_there() {
        let procs = [proc(100, 4, 10, "explorer.exe"), proc(300, 250, 40, "updater.exe")];
        let gone = HashMap::from([(250, Gone { name: "cmd.exe".into(), create_time: 30 })]);
        let chain = launch_chain(300, &procs, &gone);
        assert_eq!(names(&chain), [("cmd.exe", true), ("updater.exe", false)]);
    }

    #[test]
    fn reused_parent_pid_ends_the_chain() {
        // PID 500 was reused by a process younger than its supposed child.
        let procs = [proc(500, 4, 90, "newcomer.exe"), proc(600, 500, 50, "orphan.exe")];
        assert_eq!(names(&launch_chain(600, &procs, &HashMap::new())), [("orphan.exe", false)]);
        assert!(launch_chain(999, &procs, &HashMap::new()).is_empty());
    }

    #[test]
    fn self_parented_root_does_not_loop() {
        let procs = [proc(0, 0, 0, "Idle"), proc(4, 0, 1, "System")];
        assert_eq!(names(&launch_chain(4, &procs, &HashMap::new())), [("Idle", false), ("System", false)]);
    }
}
