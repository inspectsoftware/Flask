//! Startup tab: every autostart location, Autoruns-style.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::{Arc, Mutex};

use sc_core::autoruns::{self, Category, Entry, Source};
use sc_core::baseline;
use sc_core::origin::{self, Signature};
use sc_core::reg::{Hive, Key, View};
use sc_ui::{
    Align, Button, ButtonKind, Canvas, Color, Column, Event, Font, ListAction, ListModel, ListView, MouseButton, Rect,
    TextInput, Theme, Win, key, shell,
};

use crate::app::Goto;
use crate::format;

/// `Event::User` code posted when the scan thread has something new.
pub const STARTUP_READY: usize = 3;
const MARGIN: f32 = 12.0;
const RAIL_W: f32 = 208.0;
const RAIL_ROW: f32 = 31.0;
const TOOLBAR_H: f32 = 34.0;
const DETAIL_H: f32 = 112.0;

enum Update {
    /// A finished scan and the baseline to compare it with.
    Entries(Vec<Entry>, HashSet<String>),
    Signatures(HashMap<String, Signature>),
}

const COL_ON: usize = 0;
const COL_ENTRY: usize = 1;
const COL_PUBLISHER: usize = 2;
const COL_LOCATION: usize = 3;

struct Model<'a> {
    entries: &'a [Entry],
    view: &'a [usize],
    signatures: &'a HashMap<String, Signature>,
    fresh: &'a [bool],
}

/// Who vouches for the entry's file: the verified signer when the check has
/// run, otherwise whatever company name the file claims.
fn publisher<'a>(e: &'a Entry, signatures: &'a HashMap<String, Signature>) -> (&'a str, bool) {
    match signatures.get(&e.image) {
        Some(Signature::Valid { signer }) if !signer.is_empty() => (signer, false),
        Some(Signature::Unsigned) => ("Not signed", true),
        Some(Signature::Invalid { .. }) => ("Signature does not verify", true),
        _ => (&e.company, false),
    }
}

fn is_microsoft(e: &Entry, signatures: &HashMap<String, Signature>) -> bool {
    match signatures.get(&e.image) {
        Some(Signature::Valid { signer }) => signer.contains("Microsoft"),
        // Unsigned or tampered files do not get to hide behind a claimed name.
        Some(Signature::Unsigned | Signature::Invalid { .. }) => false,
        _ => e.company.contains("Microsoft"),
    }
}

fn is_unverified(e: &Entry, signatures: &HashMap<String, Signature>) -> bool {
    matches!(signatures.get(&e.image), Some(Signature::Unsigned | Signature::Invalid { .. }))
}

impl ListModel for Model<'_> {
    fn rows(&self) -> usize {
        self.view.len()
    }

    fn cell(&self, row: usize, col: usize, out: &mut String) {
        let e = &self.entries[self.view[row]];
        match col {
            COL_ENTRY => out.push_str(&e.name),
            COL_PUBLISHER => out.push_str(publisher(e, self.signatures).0),
            COL_LOCATION => out.push_str(&e.location),
            _ => {}
        }
    }

    fn switch(&self, row: usize, col: usize) -> Option<bool> {
        let e = &self.entries[self.view[row]];
        (col == COL_ON && e.can_toggle()).then_some(e.enabled)
    }

    fn bold(&self, _row: usize, col: usize) -> bool {
        col == COL_ENTRY
    }

    fn secondary(&self, row: usize, col: usize, out: &mut String) {
        if col == COL_ENTRY && self.fresh[self.view[row]] {
            out.push_str("New");
        }
    }

    fn text_color(&self, row: usize, col: usize, t: &Theme) -> Option<Color> {
        let e = &self.entries[self.view[row]];
        match col {
            COL_ENTRY => (!e.enabled).then_some(t.text_dim),
            COL_PUBLISHER => Some(if publisher(e, self.signatures).1 { t.peach } else { t.text_dim }),
            _ => Some(t.text_dim),
        }
    }

    fn icon(&self, row: usize) -> Option<&str> {
        Some(&self.entries[self.view[row]].image)
    }
}

pub struct StartupPage {
    entries: Vec<Entry>,
    /// Entries as they were when the user last marked them seen.
    baseline: HashSet<String>,
    /// For each entry, whether it is missing from the baseline.
    fresh: Vec<bool>,
    signatures: HashMap<String, Signature>,
    view: Vec<usize>,
    /// `None` shows every category.
    category: Option<Category>,
    search: TextInput,
    hide_ms: Button,
    only_unsigned: Button,
    only_new: Button,
    seen_btn: Button,
    rescan: Button,
    origin_btn: Button,
    /// Set when the user asks for something that lives on another tab.
    pub goto: Option<Goto>,
    toggle_btn: Button,
    key_btn: Button,
    path_btn: Button,
    delete_btn: Button,
    list: ListView,
    updates: Arc<Mutex<Vec<Update>>>,
    scanning: bool,
    scanned: bool,
    rail_rects: Vec<(Option<Category>, Rect)>,
    hover_rail: Option<usize>,
    /// Identity of the selected entry, so it survives a rescan.
    selected: Option<(Category, String, String)>,
    notice: String,
}

impl StartupPage {
    pub fn new() -> Self {
        let col = |title, width, visible| Column { title, width, align: Align::Left, visible };
        let mut hide_ms = Button::new("Hide Microsoft", ButtonKind::Cocoa);
        hide_ms.active = true;
        Self {
            entries: Vec::new(),
            baseline: HashSet::new(),
            fresh: Vec::new(),
            signatures: HashMap::new(),
            view: Vec::new(),
            category: None,
            search: TextInput::new("Search entries"),
            hide_ms,
            only_unsigned: Button::new("Only unsigned", ButtonKind::Cocoa),
            only_new: Button::new("Only new", ButtonKind::Cocoa),
            seen_btn: Button::new("Mark all seen", ButtonKind::Cocoa),
            rescan: Button::new("Rescan", ButtonKind::Cocoa),
            origin_btn: Button::small("Origin", ButtonKind::Cocoa),
            goto: None,
            toggle_btn: Button::small("Disable", ButtonKind::Milk),
            key_btn: Button::small("Go to key", ButtonKind::Cocoa),
            path_btn: Button::small("Open path", ButtonKind::Cocoa),
            delete_btn: Button::small("Delete", ButtonKind::Peach),
            list: {
                let columns =
                    vec![col("On", 54.0, true), col("Entry", 260.0, true), col("Publisher", 220.0, true), col("Location", 300.0, true)];
                let mut list = ListView::new(columns);
                list.lead_column = Some(COL_ENTRY);
                list
            },
            updates: Arc::default(),
            scanning: false,
            scanned: false,
            rail_rects: Vec::new(),
            hover_rail: None,
            selected: None,
            notice: String::new(),
        }
    }

    pub fn notice(&self) -> &str {
        &self.notice
    }

    pub fn say(&mut self, notice: String) {
        self.notice = notice;
    }

    /// Called when the tab is shown; the first time starts the scan.
    pub fn activate(&mut self, win: &Win) {
        if !self.scanned && !self.scanning {
            self.start_scan(win);
        }
    }

    fn start_scan(&mut self, win: &Win) {
        self.scanning = true;
        let (updates, id) = (self.updates.clone(), win.id());
        std::thread::spawn(move || {
            let entries = autoruns::scan();
            let mut images: Vec<String> = entries.iter().map(|e| e.image.clone()).filter(|i| !i.is_empty()).collect();
            images.sort();
            images.dedup();
            // The first scan ever becomes the baseline, so nothing starts out as new.
            let baseline = baseline::load().unwrap_or_else(|| {
                let _ = baseline::save(&entries);
                baseline::keys(&entries)
            });
            updates.lock().unwrap().push(Update::Entries(entries, baseline));
            Win::post(id, STARTUP_READY);
            // Verifying a few hundred files takes seconds, so it follows in batches.
            for batch in images.chunks(40) {
                let checked = batch.iter().map(|i| (i.clone(), origin::signature(i))).collect();
                updates.lock().unwrap().push(Update::Signatures(checked));
                Win::post(id, STARTUP_READY);
            }
        });
    }

    /// Takes whatever the scan thread has produced. Returns true on change.
    pub fn poll(&mut self) -> bool {
        let updates: Vec<Update> = std::mem::take(&mut *self.updates.lock().unwrap());
        if updates.is_empty() {
            return false;
        }
        for update in updates {
            match update {
                Update::Entries(entries, baseline) => {
                    self.entries = entries;
                    self.baseline = baseline;
                    self.scanning = false;
                    self.scanned = true;
                }
                Update::Signatures(map) => self.signatures.extend(map),
            }
        }
        self.rebuild();
        true
    }

    fn passes_filters(&self, index: usize, filter: &str) -> bool {
        let e = &self.entries[index];
        if self.only_new.active && !self.fresh[index] {
            return false;
        }
        if self.hide_ms.active && is_microsoft(e, &self.signatures) {
            return false;
        }
        if self.only_unsigned.active && !is_unverified(e, &self.signatures) {
            return false;
        }
        filter.is_empty()
            || format::contains_ci(&e.name, filter)
            || format::contains_ci(&e.command, filter)
            || format::contains_ci(&e.location, filter)
            || format::contains_ci(publisher(e, &self.signatures).0, filter)
    }

    fn rebuild(&mut self) {
        self.fresh = self.entries.iter().map(|e| !self.baseline.contains(&baseline::key(e))).collect();
        let filter = self.search.text.trim().to_lowercase();
        let mut view: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.category.is_none_or(|c| c == self.entries[i].category) && self.passes_filters(i, &filter))
            .collect();
        view.sort_by(|&a, &b| {
            let (x, y) = (&self.entries[a], &self.entries[b]);
            x.category.cmp(&y.category).then_with(|| format::cmp_ci(&x.name, &y.name))
        });
        self.list.selected = self.selected.as_ref().and_then(|key| {
            view.iter().position(|&i| {
                let e = &self.entries[i];
                (e.category, &e.name, &e.location) == (key.0, &key.1, &key.2)
            })
        });
        if self.list.selected.is_none() {
            self.selected = None;
        }
        self.view = view;
    }

    fn selected_entry(&self) -> Option<&Entry> {
        Some(&self.entries[*self.view.get(self.list.selected?)?])
    }

    /// (shown with current filters, has an enabled unverified entry) for a category.
    fn rail_stats(&self, category: Option<Category>, filter: &str) -> (usize, bool) {
        let mut count = 0;
        let mut flagged = false;
        for (i, e) in self.entries.iter().enumerate().filter(|(_, e)| category.is_none_or(|c| c == e.category)) {
            if self.passes_filters(i, filter) {
                count += 1;
                flagged |= e.enabled && is_unverified(e, &self.signatures);
            }
        }
        (count, flagged)
    }

    pub fn paint(&mut self, c: &mut Canvas, area: Rect) {
        let t = c.theme;
        let inner = Rect::new(area.x + MARGIN, area.y + MARGIN, area.w - 2.0 * MARGIN, (area.h - MARGIN).max(0.0));
        let filter = self.search.text.trim().to_lowercase();

        // Category rail.
        let rail = Rect { w: RAIL_W, ..inner };
        c.glass(rail, 22.0);
        self.rail_rects.clear();
        let cats = std::iter::once(None).chain(Category::ALL.into_iter().map(Some));
        let mut count_text = String::new();
        for (i, cat) in cats.enumerate() {
            let r = Rect::new(rail.x + 8.0, rail.y + 8.0 + i as f32 * (RAIL_ROW + 2.0), rail.w - 16.0, RAIL_ROW);
            if r.bottom() > rail.bottom() - 4.0 {
                break;
            }
            self.rail_rects.push((cat, r));
            let selected = self.category == cat;
            if selected {
                c.fill_round_a(r, 12.0, t.milk_hi, 0.11);
            } else if self.hover_rail == Some(i) {
                c.fill_round_a(r, 12.0, t.milk_hi, 0.05);
            }
            let (count, flagged) = self.rail_stats(cat, &filter);
            let label = cat.map_or("Everything", Category::label);
            let font = if selected { Font::BODY_BOLD } else { Font::BODY };
            c.text_font(label, Rect::new(r.x + 10.0, r.y, r.w - 64.0, r.h), t.text, Align::Left, font);
            count_text.clear();
            let _ = write!(count_text, "{count}");
            c.text(&count_text, Rect::new(r.right() - 50.0, r.y, 40.0, r.h), t.text_dim, Align::Right, false);
            if flagged {
                c.dot(r.right() - 58.0, r.y + r.h / 2.0, 4.0, t.butter);
            }
        }

        // Toolbar.
        let x0 = rail.right() + MARGIN;
        let main_w = (inner.right() - x0).max(0.0);
        let rw = self.rescan.width(c);
        self.rescan.rect = Rect::new(inner.right() - rw, inner.y, rw, TOOLBAR_H);
        self.rescan.paint(c);
        let new_count = self.fresh.iter().filter(|&&f| f).count();
        // Offered only while there is something new to accept.
        if new_count > 0 {
            let sw = self.seen_btn.width(c);
            self.seen_btn.rect = Rect::new(self.rescan.rect.x - 8.0 - sw, inner.y, sw, TOOLBAR_H);
            self.seen_btn.paint(c);
        } else {
            self.seen_btn.rect = Rect { w: 0.0, ..self.rescan.rect };
        }
        let (hw, uw, nw) = (self.hide_ms.width(c), self.only_unsigned.width(c), self.only_new.width(c));
        let search_w = (main_w - hw - uw - nw - rw - self.seen_btn.rect.w - 206.0).clamp(140.0, 280.0);
        self.search.rect = Rect::new(x0, inner.y, search_w, TOOLBAR_H);
        self.search.paint(c);
        self.hide_ms.rect = Rect::new(self.search.rect.right() + 10.0, inner.y, hw, TOOLBAR_H);
        self.hide_ms.paint(c);
        self.only_unsigned.rect = Rect::new(self.hide_ms.rect.right() + 8.0, inner.y, uw, TOOLBAR_H);
        self.only_unsigned.paint(c);
        self.only_new.rect = Rect::new(self.only_unsigned.rect.right() + 8.0, inner.y, nw, TOOLBAR_H);
        self.only_new.paint(c);
        let status = if self.scanning {
            "Scanning".to_owned()
        } else {
            let checked = self.signatures.len();
            let total = { let mut v: Vec<&str> = self.entries.iter().map(|e| e.image.as_str()).filter(|i| !i.is_empty()).collect(); v.sort(); v.dedup(); v.len() };
            let news = if new_count > 0 { format!("{new_count} new, ") } else { String::new() };
            if checked < total {
                format!("{news}{} shown of {}, verifying signatures {checked}/{total}", self.view.len(), self.entries.len())
            } else {
                format!("{news}{} shown of {}", self.view.len(), self.entries.len())
            }
        };
        let status_r = Rect::new(self.only_new.rect.right() + 12.0, inner.y, (self.seen_btn.rect.x - self.only_new.rect.right() - 24.0).max(0.0), TOOLBAR_H);
        c.text(&status, status_r, t.text_dim, Align::Right, false);

        // List card.
        let has_detail = self.selected_entry().is_some();
        let detail_h = if has_detail { DETAIL_H + MARGIN } else { 0.0 };
        let card = Rect::new(x0, inner.y + TOOLBAR_H + MARGIN, main_w, (inner.h - TOOLBAR_H - MARGIN - detail_h).max(0.0));
        c.glass(card, 22.0);
        self.list.rect = Rect::new(card.x, card.y + 6.0, card.w, (card.h - 12.0).max(0.0));
        let fixed: f32 = [COL_ON, COL_PUBLISHER, COL_LOCATION].iter().map(|&i| self.list.columns[i].width).sum();
        self.list.columns[COL_ENTRY].width = (card.w - 12.0 - fixed).max(180.0);
        let model = Model { entries: &self.entries, view: &self.view, signatures: &self.signatures, fresh: &self.fresh };
        self.list.paint(c, &model);
        if self.view.is_empty() && !self.scanning && self.scanned {
            c.text("Nothing matches the current filters", card, t.text_dim, Align::Center, false);
        }

        // Detail strip for the selected entry.
        let Some(index) = self.list.selected.and_then(|row| self.view.get(row).copied()) else { return };
        let e = &self.entries[index];
        let d = Rect::new(x0, card.bottom() + MARGIN, main_w, DETAIL_H);
        c.glass(d, 22.0);
        c.clip(d);
        let mut bx = d.right() - 14.0;
        let mut place = |c: &mut Canvas, b: &mut Button, show: bool| {
            if show {
                let w = b.width(c);
                bx -= w;
                b.rect = Rect::new(bx, d.y + 12.0, w, 24.0);
                b.paint(c);
                bx -= 6.0;
            } else {
                b.rect = Rect::default();
            }
        };
        let image_exists = !e.image.is_empty() && std::path::Path::new(&e.image).exists();
        self.toggle_btn.label = if e.enabled { "Disable" } else { "Enable" };
        let (can_toggle, can_delete, has_key) = (e.can_toggle(), e.can_delete(), e.registry_path().is_some());
        let external = matches!(e.source, Source::Task { .. } | Source::Service { .. });
        self.key_btn.label = match e.source {
            Source::Task { .. } => "Open Task Scheduler",
            _ => "Go to key",
        };
        place(c, &mut self.delete_btn, can_delete);
        place(c, &mut self.origin_btn, image_exists);
        place(c, &mut self.path_btn, image_exists);
        place(c, &mut self.key_btn, has_key || external);
        place(c, &mut self.toggle_btn, can_toggle);

        let title_r = Rect::new(d.x + 14.0, d.y + 10.0, (bx - d.x - 24.0).max(0.0), 26.0);
        c.text_font(&e.name, title_r, t.text, Align::Left, Font::TITLE);
        let mut chip_x = title_r.x + c.measure_font(&e.name, Font::TITLE) + 10.0;
        let mut chip = |c: &mut Canvas, text: &str, color: Color| {
            let w = c.measure_font(text, Font::LABEL) + 18.0;
            if chip_x + w <= title_r.right() {
                let r = Rect::new(chip_x, d.y + 13.0, w, 20.0);
                c.fill_round_a(r, r.h / 2.0, t.milk_hi, 0.09);
                c.text_font(text, r, color, Align::Center, Font::LABEL);
                chip_x += w + 6.0;
            }
        };
        if self.fresh[index] {
            chip(c, "New since last marked seen", t.butter);
        }
        if is_unverified(e, &self.signatures) {
            chip(c, "Not signed", t.peach);
        }
        if origin::in_scratch_location(&e.image) {
            chip(c, "Runs from Temp or Downloads", t.butter);
        }
        if !e.enabled {
            chip(c, "Disabled", t.text_dim);
        }
        if !e.can_toggle() {
            chip(c, "View only", t.text_dim);
        }
        let rows = [("Launches", e.command.as_str()), ("Found in", e.location.as_str()), ("File", e.image.as_str())];
        for (i, (label, value)) in rows.iter().enumerate() {
            let y = d.y + 42.0 + i as f32 * 21.0;
            c.text(label, Rect::new(d.x + 14.0, y, 76.0, 21.0), t.text_dim, Align::Left, false);
            let value = if value.is_empty() { "Not a file on disk" } else { value };
            c.text(value, Rect::new(d.x + 90.0, y, d.w - 104.0, 21.0), t.text, Align::Left, false);
        }
        c.unclip();
    }

    /// Flips the entry at `index` and refreshes the list.
    fn toggle(&mut self, index: usize) {
        let entry = self.entries[index].clone();
        self.notice.clear();
        match autoruns::set_enabled(&entry, !entry.enabled) {
            Ok(()) => self.entries[index].enabled = !entry.enabled,
            Err(why) => {
                let verb = if entry.enabled { "disable" } else { "enable" };
                let _ = write!(self.notice, "Could not {verb} {}: {why}", entry.name);
            }
        }
        self.rebuild();
    }

    fn go_to_source(e: &Entry) {
        match &e.source {
            Source::Task { .. } => shell::run("taskschd.msc", ""),
            _ => {
                // Regedit opens at the key named in its own LastKey setting.
                if let Some(path) = e.registry_path()
                    && let Some(k) = Key::create(Hive::Hkcu, r"Software\Microsoft\Windows\CurrentVersion\Applets\Regedit", View::Native)
                {
                    k.set_string("LastKey", &path);
                    shell::run("regedit.exe", "/m");
                }
            }
        }
    }

    /// Returns true when the page needs repainting.
    pub fn event(&mut self, ev: &Event, win: &Win) -> bool {
        let mut redraw = false;
        if self.search.event(ev) {
            self.rebuild();
            redraw = true;
        }
        for which in 0..3 {
            let b = match which {
                0 => &mut self.hide_ms,
                1 => &mut self.only_unsigned,
                _ => &mut self.only_new,
            };
            let (r, clicked) = b.event(ev);
            redraw |= r;
            if clicked {
                b.active = !b.active;
                self.rebuild();
                return true;
            }
        }
        let (r, clicked) = self.rescan.event(ev);
        redraw |= r;
        if clicked && !self.scanning {
            self.signatures.clear();
            self.start_scan(win);
            return true;
        }
        let (r, clicked) = self.seen_btn.event(ev);
        redraw |= r;
        if clicked {
            self.notice.clear();
            match baseline::save(&self.entries) {
                Ok(()) => self.baseline = baseline::keys(&self.entries),
                Err(why) => {
                    let _ = write!(self.notice, "Could not save the list of seen entries: {why}");
                }
            }
            self.rebuild();
            return true;
        }

        if let Some(index) = self.list.selected.and_then(|row| self.view.get(row).copied()) {
            let (r1, toggle) = self.toggle_btn.event(ev);
            let (r2, go) = self.key_btn.event(ev);
            let (r3, path) = self.path_btn.event(ev);
            let (r4, delete) = self.delete_btn.event(ev);
            let (r5, origin) = self.origin_btn.event(ev);
            redraw |= r1 | r2 | r3 | r4 | r5;
            if origin {
                let e = &self.entries[index];
                self.goto = Some(Goto::Origin { image: e.image.clone(), name: e.name.clone() });
                return true;
            }
            if toggle {
                self.toggle(index);
                return true;
            }
            if go {
                Self::go_to_source(&self.entries[index]);
                return true;
            }
            if path {
                shell::reveal_in_explorer(&self.entries[index].image);
                return true;
            }
            if delete {
                let entry = self.entries[index].clone();
                let question = format!(
                    "Delete \"{}\" from {}?\n\nThis cannot be undone. Disabling it instead keeps a way back.",
                    entry.name, entry.location
                );
                if win.confirm("Delete startup entry", &question) {
                    self.notice.clear();
                    match autoruns::delete(&entry) {
                        Ok(()) => {
                            self.entries.remove(index);
                        }
                        Err(why) => {
                            let _ = write!(self.notice, "Could not delete {}: {why}", entry.name);
                        }
                    }
                    self.rebuild();
                }
                return true;
            }
        }

        match *ev {
            Event::MouseMove { x, y } => {
                let hover = self.rail_rects.iter().position(|(_, r)| r.contains(x, y));
                redraw |= std::mem::replace(&mut self.hover_rail, hover) != hover;
                win.set_cursor(sc_ui::Cursor::Arrow);
            }
            Event::MouseLeave => redraw |= self.hover_rail.take().is_some(),
            Event::MouseDown { x, y, button: MouseButton::Left, .. } => {
                if let Some(&(cat, _)) = self.rail_rects.iter().find(|(_, r)| r.contains(x, y)) {
                    self.category = cat;
                    self.rebuild();
                    return true;
                }
            }
            Event::Key { vk: key::F5, .. } if !self.scanning => {
                self.signatures.clear();
                self.start_scan(win);
                return true;
            }
            _ => {}
        }

        let model = Model { entries: &self.entries, view: &self.view, signatures: &self.signatures, fresh: &self.fresh };
        let resp = self.list.event(ev, &model, win);
        redraw |= resp.redraw;
        match resp.action {
            Some(ListAction::Select(row)) => {
                self.selected = row.and_then(|r| self.view.get(r)).map(|&i| {
                    let e = &self.entries[i];
                    (e.category, e.name.clone(), e.location.clone())
                });
                self.notice.clear();
            }
            Some(ListAction::Switch(row)) => {
                if let Some(&index) = self.view.get(row) {
                    let e = &self.entries[index];
                    self.selected = Some((e.category, e.name.clone(), e.location.clone()));
                    self.toggle(index);
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

    fn entry(category: Category, name: &str, image: &str, company: &str, enabled: bool) -> Entry {
        Entry {
            category,
            name: name.into(),
            command: image.into(),
            image: image.into(),
            location: "test".into(),
            enabled,
            source: Source::Locked { hive: Hive::Hklm, key: "x".into() },
            company: company.into(),
        }
    }

    fn names(page: &StartupPage) -> Vec<&str> {
        page.view.iter().map(|&i| page.entries[i].name.as_str()).collect()
    }

    #[test]
    fn filters_and_microsoft_hiding_follow_verified_signatures() {
        let mut page = StartupPage::new();
        page.entries = vec![
            entry(Category::Services, "WinSvc", r"C:\w\a.exe", "Microsoft Corporation", true),
            entry(Category::Logon, "Fake", r"C:\t\fake.exe", "Microsoft Corporation", true),
            entry(Category::Logon, "Tool", r"C:\p\tool.exe", "Tool Co", true),
            entry(Category::Tasks, "Old", r"C:\p\old.exe", "", false),
        ];
        page.rebuild();
        // Before verification the claimed company is all there is to go on.
        assert_eq!(names(&page), ["Tool", "Old"]);

        page.signatures.insert(r"C:\w\a.exe".into(), Signature::Valid { signer: "Microsoft Windows".into() });
        page.signatures.insert(r"C:\t\fake.exe".into(), Signature::Unsigned);
        page.signatures.insert(r"C:\p\tool.exe".into(), Signature::Valid { signer: "Tool Co Ltd".into() });
        page.rebuild();
        // An unsigned file claiming to be Microsoft's is no longer hidden.
        assert_eq!(names(&page), ["Fake", "Tool", "Old"]);
        assert_eq!(page.rail_stats(Some(Category::Logon), ""), (2, true));
        assert_eq!(page.rail_stats(Some(Category::Tasks), ""), (1, false));

        page.only_unsigned.active = true;
        page.rebuild();
        assert_eq!(names(&page), ["Fake"]);

        page.only_unsigned.active = false;
        page.hide_ms.active = false;
        page.category = Some(Category::Services);
        page.rebuild();
        assert_eq!(names(&page), ["WinSvc"]);

        page.category = None;
        page.search.text = "tool co".into();
        page.rebuild();
        assert_eq!(names(&page), ["Tool"]);

        // Against a baseline that knows everything but Old, only Old is new.
        page.search.text.clear();
        page.baseline = baseline::keys(&page.entries[..3]);
        page.only_new.active = true;
        page.rebuild();
        assert_eq!(names(&page), ["Old"]);
        assert_eq!(page.rail_stats(None, ""), (1, false));
    }

    #[test]
    fn publisher_prefers_the_verified_signer() {
        let e = entry(Category::Logon, "X", r"C:\x.exe", "Claimed Inc", true);
        let mut sigs = HashMap::new();
        assert_eq!(publisher(&e, &sigs), ("Claimed Inc", false));
        sigs.insert(e.image.clone(), Signature::Valid { signer: "Real Signer LLC".into() });
        assert_eq!(publisher(&e, &sigs), ("Real Signer LLC", false));
        sigs.insert(e.image.clone(), Signature::Unsigned);
        assert_eq!(publisher(&e, &sigs), ("Not signed", true));
    }
}
