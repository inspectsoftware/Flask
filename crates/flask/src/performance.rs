//! Performance tab: live graphs for each device plus the specs sheet.

use std::collections::VecDeque;

use sc_core::perf::PerfSample;
use sc_core::specs::{self, Section};
use sc_ui::{Align, Button, ButtonKind, Canvas, Event, Font, MouseButton, Rect, Win};

use crate::format;

/// Samples kept per graph: one minute at the default refresh rate.
const HISTORY: usize = 60;
const MARGIN: f32 = 12.0;
const RAIL_W: f32 = 232.0;
const GAP: f32 = 12.0;
const SPEC_ROW: f32 = 21.0;
const SPEC_LABEL_W: f32 = 112.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Device {
    Cpu,
    Memory,
    Gpu,
    Disk,
    Network,
}

const DEVICES: [Device; 5] = [Device::Cpu, Device::Memory, Device::Gpu, Device::Disk, Device::Network];

impl Device {
    fn name(self) -> &'static str {
        match self {
            Device::Cpu => "CPU",
            Device::Memory => "Memory",
            Device::Gpu => "GPU",
            Device::Disk => "Disk",
            Device::Network => "Network",
        }
    }

    /// Specs section that goes with this device.
    fn section(self) -> &'static str {
        match self {
            Device::Disk => "Storage",
            other => other.name(),
        }
    }
}

fn rate_text(bytes_per_sec: u64) -> String {
    let mut s = String::new();
    format::bytes(&mut s, bytes_per_sec);
    s.push_str("/s");
    s
}

fn size_text(bytes: u64) -> String {
    let mut s = String::new();
    format::bytes(&mut s, bytes);
    s
}

/// Scales a history of raw rates to 0..=1 against its own peak, with a floor
/// so an idle link does not draw noise at full height.
fn normalize(raw: &VecDeque<f32>, floor: f32) -> Vec<f32> {
    let peak = raw.iter().copied().fold(floor, f32::max);
    raw.iter().map(|v| v / peak).collect()
}

pub struct PerfPage {
    selected: Device,
    // Percent histories for CPU, memory, GPU and disk; raw bytes/s for network.
    history: [VecDeque<f32>; 5],
    latest: PerfSample,
    gpu_percent: f32,
    gpu_memory_used: u64,
    specs: Vec<Section>,
    spec_tab: usize,
    device_rects: [Rect; 5],
    chip_rects: Vec<Rect>,
    hover_device: Option<usize>,
    copy_btn: Button,
    /// Time the graph spans at the current refresh rate.
    span_secs: u32,
}

impl PerfPage {
    pub fn new() -> Self {
        Self {
            selected: Device::Cpu,
            history: Default::default(),
            latest: PerfSample::default(),
            gpu_percent: 0.0,
            gpu_memory_used: 0,
            specs: Vec::new(),
            spec_tab: 0,
            device_rects: [Rect::default(); 5],
            chip_rects: Vec::new(),
            hover_device: None,
            copy_btn: Button::small("Copy all specs", ButtonKind::Cocoa),
            span_secs: HISTORY as u32,
        }
    }

    /// Tells the page how far apart samples now arrive, for the graph's time label.
    pub fn set_interval(&mut self, ms: u32) {
        self.span_secs = HISTORY as u32 * ms / 1000;
    }

    pub fn set_specs(&mut self, specs: Vec<Section>) {
        self.specs = specs;
        self.follow_device();
    }

    fn follow_device(&mut self) {
        if let Some(i) = self.specs.iter().position(|s| s.title == self.selected.section()) {
            self.spec_tab = i;
        }
    }

    pub fn push(&mut self, sample: PerfSample, gpu_percent: f32, gpu_memory_used: u64) {
        let (rx, tx) = sample.net_rates();
        let values =
            [sample.cpu_total, sample.memory.used_percent(), gpu_percent, sample.disk_active(), (rx + tx) as f32];
        for (h, v) in self.history.iter_mut().zip(values) {
            if h.len() == HISTORY {
                h.pop_front();
            }
            h.push_back(v);
        }
        self.latest = sample;
        self.gpu_percent = gpu_percent;
        self.gpu_memory_used = gpu_memory_used;
    }

    /// History of `device` scaled to 0..=1, oldest first.
    fn series(&self, device: Device) -> Vec<f32> {
        let h = &self.history[device as usize];
        match device {
            Device::Network => normalize(h, 100.0 * 1024.0),
            _ => h.iter().map(|v| v / 100.0).collect(),
        }
    }

    /// Headline value and one-line summary for a device card.
    fn summary(&self, device: Device) -> (String, String) {
        let s = &self.latest;
        match device {
            Device::Cpu => (
                format!("{:.0}%", s.cpu_total),
                format!("{} threads at {:.2} GHz", s.cores.len(), s.mhz as f32 / 1000.0),
            ),
            Device::Memory => (
                format!("{:.0}%", s.memory.used_percent()),
                format!("{} of {} in use", size_text(s.memory.used()), size_text(s.memory.total)),
            ),
            Device::Gpu => (format!("{:.0}%", self.gpu_percent), format!("{} video memory in use", size_text(self.gpu_memory_used))),
            Device::Disk => {
                let (r, w) = s.disk_rates();
                (format!("{:.0}%", s.disk_active()), format!("Read {}, write {}", rate_text(r), rate_text(w)))
            }
            Device::Network => {
                let (rx, tx) = s.net_rates();
                let name = s.adapters.first().map_or("No active adapter", |a| a.name.as_str());
                (rate_text(rx + tx), format!("{name}: {} down, {} up", rate_text(rx), rate_text(tx)))
            }
        }
    }

    fn tiles(&self, device: Device) -> Vec<(&'static str, String)> {
        let s = &self.latest;
        match device {
            Device::Cpu => vec![
                ("Processes", s.processes.to_string()),
                ("Threads", s.threads.to_string()),
                ("Handles", s.handles.to_string()),
                ("Speed", format!("{:.2} GHz", s.mhz as f32 / 1000.0)),
                ("Logical cores", s.cores.len().to_string()),
                ("Temperature", "Needs a driver".into()),
            ],
            Device::Memory => vec![
                ("In use", size_text(s.memory.used())),
                ("Available", size_text(s.memory.available)),
                ("Committed", format!("{} of {}", size_text(s.memory.committed), size_text(s.memory.commit_limit))),
                ("Cached", size_text(s.memory.cached)),
                ("Paged pool", size_text(s.memory.paged_pool)),
                ("Non-paged pool", size_text(s.memory.nonpaged_pool)),
            ],
            Device::Gpu => vec![
                ("Busiest engine", format!("{:.0}%", self.gpu_percent)),
                ("Video memory in use", size_text(self.gpu_memory_used)),
            ],
            Device::Disk => {
                let mut tiles: Vec<(&'static str, String)> = Vec::new();
                let (r, w) = s.disk_rates();
                tiles.push(("Read", rate_text(r)));
                tiles.push(("Write", rate_text(w)));
                for d in s.disks.iter().take(4) {
                    tiles.push(("Disk", format!("{}  {:.0}%", d.name, d.active)));
                }
                tiles
            }
            Device::Network => {
                let (rx, tx) = s.net_rates();
                let mut tiles = vec![("Download", rate_text(rx)), ("Upload", rate_text(tx))];
                for a in s.adapters.iter().take(2) {
                    let mbps = a.link_speed as f64 / 1e6;
                    let link = if mbps >= 1000.0 { format!("{:.1} Gbps", mbps / 1000.0) } else { format!("{mbps:.0} Mbps") };
                    tiles.push((if a.wifi { "Wi-Fi link" } else { "Ethernet link" }, link));
                }
                tiles
            }
        }
    }

    fn paint_rail(&mut self, c: &mut Canvas, area: Rect) {
        let t = c.theme;
        let card_h = ((area.h - 4.0 * 8.0) / 5.0).clamp(64.0, 116.0);
        for (i, device) in DEVICES.into_iter().enumerate() {
            let r = Rect::new(area.x, area.y + i as f32 * (card_h + 8.0), area.w, card_h);
            self.device_rects[i] = r;
            let selected = device == self.selected;
            // The selected device is the same glass, lit more brightly.
            c.glass(r, 18.0);
            if selected {
                c.fill_round_a(r, 18.0, t.milk_hi, 0.08);
                c.stroke_round_a(r, 18.0, t.milk_hi, 0.30);
            } else if self.hover_device == Some(i) {
                c.fill_round_a(r, 18.0, t.milk_hi, 0.04);
            }
            let (ink, dim, line) = (t.text, t.text_dim, t.milk);
            let (value, sub) = self.summary(device);
            let inner = r.pad_x(13.0);
            c.text_font(device.name(), Rect { y: r.y + 8.0, h: 22.0, ..inner }, ink, Align::Left, Font::TITLE);
            c.text_font(&value, Rect { y: r.y + 7.0, h: 24.0, ..inner }, ink, Align::Right, Font::VALUE);
            c.text_font(&sub, Rect { y: r.y + 30.0, h: 18.0, ..inner }, dim, Align::Left, Font::SMALL);
            let spark = Rect::new(inner.x, r.y + 52.0, inner.w, (r.h - 64.0).max(0.0));
            if spark.h >= 10.0 {
                c.graph(spark, &self.series(device), HISTORY, line, 2.0, false);
            }
        }
    }

    fn paint_main(&mut self, c: &mut Canvas, area: Rect) {
        let t = c.theme;
        let device = self.selected;
        let is_cpu = device == Device::Cpu;
        let cores = &self.latest.cores;

        // Core glasses: up to 16 per row.
        let per_row = cores.len().clamp(1, 16);
        let core_rows = if is_cpu { cores.len().div_ceil(per_row) } else { 0 };
        let cores_h = core_rows as f32 * 64.0;
        let tiles_h = 54.0;
        let specs_h = (area.h * 0.34).clamp(150.0, 230.0);
        let head_h = (area.h - tiles_h - specs_h - 2.0 * GAP).max(160.0);

        // Header card with the big graph.
        let head = Rect::new(area.x, area.y, area.w, head_h);
        c.glass(head, 22.0);
        let inner = head.pad_x(16.0);
        let (value, sub) = self.summary(device);
        c.text_font(device.name(), Rect { y: head.y + 12.0, h: 30.0, ..inner }, t.text, Align::Left, Font::DISPLAY);
        let name_w = c.measure_font(device.name(), Font::DISPLAY) + 12.0;
        let value_w = c.measure_font(&value, Font::BIG) + 4.0;
        let model = self.specs.iter().find(|s| s.title == device.section()).and_then(|s| s.rows.first()).map(|r| r.1.as_str());
        let caption = model.unwrap_or(&sub);
        c.text(caption, Rect::new(inner.x + name_w, head.y + 14.0, (inner.w - name_w - value_w - 12.0).max(0.0), 28.0), t.text_dim, Align::Left, false);
        c.text_font(&value, Rect { y: head.y + 8.0, h: 36.0, ..inner }, t.text, Align::Right, Font::BIG);

        let graph_y = head.y + 52.0;
        let graph_h = (head.h - 52.0 - 14.0 - if is_cpu { cores_h + 10.0 } else { 0.0 }).max(40.0);
        let well = Rect::new(inner.x, graph_y, inner.w, graph_h);
        c.well(well, 14.0);
        for i in 1..4 {
            c.fill_a(Rect::new(well.x + 8.0, well.y + well.h * i as f32 / 4.0, well.w - 16.0, 1.0), t.milk_hi, 0.05);
        }
        c.clip(well);
        c.graph(Rect::new(well.x, well.y + 6.0, well.w, well.h - 12.0), &self.series(device), HISTORY, t.milk, 2.2, true);
        c.unclip();
        let scale_label = match device {
            Device::Network => {
                let peak = self.history[Device::Network as usize].iter().copied().fold(100.0 * 1024.0, f32::max);
                rate_text(peak as u64)
            }
            _ => "100%".to_owned(),
        };
        c.text_font(&scale_label, Rect::new(well.x + 10.0, well.y + 4.0, 160.0, 18.0), t.text_dim, Align::Left, Font::SMALL);
        c.text_font(&format!("{} seconds ago", self.span_secs), Rect::new(well.x + 10.0, well.bottom() - 22.0, 160.0, 18.0), t.text_dim, Align::Left, Font::SMALL);
        c.text_font("Now", Rect::new(well.right() - 60.0, well.bottom() - 22.0, 50.0, 18.0), t.text_dim, Align::Right, Font::SMALL);

        if is_cpu {
            let top = well.bottom() + 10.0;
            let gap = 6.0;
            let cw = (inner.w - gap * (per_row - 1) as f32) / per_row as f32;
            let mut label = String::new();
            for (i, usage) in cores.iter().enumerate() {
                let (col, row) = (i % per_row, i / per_row);
                let glass = Rect::new(inner.x + col as f32 * (cw + gap), top + row as f32 * 64.0, cw, 44.0);
                c.fill_round_a(glass, 9.0, t.well, 0.55);
                let level = (usage / 100.0).clamp(0.0, 1.0);
                if level > 0.0 {
                    let h = (glass.h * level).max(4.0);
                    c.clip(Rect::new(glass.x, glass.bottom() - h, glass.w, h));
                    c.fill_round(glass, 9.0, t.milk);
                    c.unclip();
                }
                label.clear();
                use std::fmt::Write;
                let _ = write!(label, "{usage:.0}%");
                c.text_font(&label, Rect::new(glass.x, glass.bottom() + 2.0, glass.w, 16.0), t.text_dim, Align::Center, Font::TINY);
            }
        }

        // Tiles.
        let tiles = self.tiles(device);
        let ty = head.bottom() + GAP;
        let n = tiles.len().max(1);
        let tw = (area.w - 8.0 * (n - 1) as f32) / n as f32;
        for (i, (label, value)) in tiles.iter().enumerate() {
            let r = Rect::new(area.x + i as f32 * (tw + 8.0), ty, tw, tiles_h);
            c.glass(r, 16.0);
            c.text_font(label, Rect::new(r.x + 12.0, r.y + 7.0, r.w - 24.0, 16.0), t.text_dim, Align::Left, Font::SMALL);
            let needs_driver = value == "Needs a driver";
            let (font, color) = if needs_driver { (Font::BODY_BOLD, t.text_dim) } else { (Font::TILE, t.text) };
            c.text_font(value, Rect::new(r.x + 12.0, r.y + 24.0, r.w - 24.0, 24.0), color, Align::Left, font);
        }

        // Specs.
        let sr = Rect::new(area.x, ty + tiles_h + GAP, area.w, (area.bottom() - ty - tiles_h - GAP).max(0.0));
        c.glass(sr, 22.0);
        c.clip(sr);
        c.text_font("Specs", Rect::new(sr.x + 16.0, sr.y + 10.0, 60.0, 24.0), t.text, Align::Left, Font::TITLE);
        let bw = self.copy_btn.width(c);
        self.copy_btn.rect = Rect::new(sr.right() - 14.0 - bw, sr.y + 10.0, bw, 24.0);
        self.copy_btn.paint(c);
        let mut x = sr.x + 70.0;
        self.chip_rects.clear();
        for (i, section) in self.specs.iter().enumerate() {
            let w = c.measure_font(section.title, Font::LABEL) + 24.0;
            let chip = Rect::new(x, sr.y + 11.0, w, 22.0);
            self.chip_rects.push(chip);
            if i == self.spec_tab {
                c.button_body(chip, t.milk_hi, t.milk);
                c.text_font(section.title, Rect { h: chip.h - 2.0, ..chip }, t.ink, Align::Center, Font::LABEL);
            } else {
                c.fill_round_a(chip, chip.h / 2.0, t.milk_hi, 0.09);
                c.text_font(section.title, chip, t.text, Align::Center, Font::LABEL);
            }
            x += w + 6.0;
        }
        match self.specs.get(self.spec_tab) {
            None => c.text("Reading hardware details", Rect::new(sr.x + 16.0, sr.y + 44.0, sr.w - 32.0, SPEC_ROW), t.text_dim, Align::Left, false),
            Some(section) => {
                // Two columns, filled top to bottom.
                let top = sr.y + 44.0;
                let per_col = section.rows.len().div_ceil(2).max(((sr.bottom() - top - 8.0) / SPEC_ROW) as usize).max(1);
                let col_w = (sr.w - 32.0 - 16.0) / 2.0;
                for (i, (label, value)) in section.rows.iter().enumerate() {
                    let (col, row) = (i / per_col, i % per_col);
                    let rx = sr.x + 16.0 + col as f32 * (col_w + 16.0);
                    let ry = top + row as f32 * SPEC_ROW;
                    c.text(label, Rect::new(rx, ry, SPEC_LABEL_W, SPEC_ROW), t.text_dim, Align::Left, false);
                    c.text(value, Rect::new(rx + SPEC_LABEL_W, ry, col_w - SPEC_LABEL_W, SPEC_ROW), t.text, Align::Left, false);
                }
            }
        }
        c.unclip();
    }

    pub fn paint(&mut self, c: &mut Canvas, area: Rect) {
        let inner = Rect::new(area.x + MARGIN, area.y + MARGIN, area.w - 2.0 * MARGIN, (area.h - MARGIN).max(0.0));
        self.paint_rail(c, Rect { w: RAIL_W, ..inner });
        let main_x = inner.x + RAIL_W + GAP;
        self.paint_main(c, Rect::new(main_x, inner.y, (inner.right() - main_x).max(0.0), inner.h));
    }

    /// Returns true when the page needs repainting.
    pub fn event(&mut self, ev: &Event, win: &Win) -> bool {
        let (mut redraw, copy) = self.copy_btn.event(ev);
        if copy {
            win.copy_text(&specs::to_text(&self.specs));
            return true;
        }
        match *ev {
            Event::MouseMove { x, y } => {
                let hover = self.device_rects.iter().position(|r| r.contains(x, y));
                redraw |= std::mem::replace(&mut self.hover_device, hover) != hover;
            }
            Event::MouseLeave => redraw |= self.hover_device.take().is_some(),
            Event::MouseDown { x, y, button: MouseButton::Left, .. } => {
                if let Some(i) = self.device_rects.iter().position(|r| r.contains(x, y)) {
                    self.selected = DEVICES[i];
                    self.follow_device();
                    redraw = true;
                } else if let Some(i) = self.chip_rects.iter().position(|r| r.contains(x, y)) {
                    self.spec_tab = i;
                    redraw = true;
                }
            }
            _ => {}
        }
        redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_bounded_and_ordered() {
        let mut page = PerfPage::new();
        for i in 0..(HISTORY + 5) {
            let sample = PerfSample { cpu_total: i as f32, ..Default::default() };
            page.push(sample, 50.0, 0);
        }
        let cpu = &page.history[Device::Cpu as usize];
        assert_eq!(cpu.len(), HISTORY);
        assert_eq!((cpu[0], cpu[HISTORY - 1]), (5.0, (HISTORY + 4) as f32));
        assert!(page.series(Device::Gpu).iter().all(|&v| v == 0.5));
    }

    #[test]
    fn network_scales_to_its_peak_with_a_floor() {
        let quiet: VecDeque<f32> = [0.0, 1024.0].into();
        // Below the floor nothing reaches full height.
        assert!(normalize(&quiet, 102_400.0).iter().all(|&v| v <= 0.01));
        let busy: VecDeque<f32> = [1_000_000.0, 4_000_000.0, 2_000_000.0].into();
        assert_eq!(normalize(&busy, 102_400.0), [0.25, 1.0, 0.5]);
    }

    #[test]
    fn selecting_a_device_follows_to_its_specs_section() {
        let mut page = PerfPage::new();
        page.set_specs(vec![
            Section { title: "CPU", rows: vec![] },
            Section { title: "Storage", rows: vec![] },
            Section { title: "Network", rows: vec![] },
        ]);
        page.selected = Device::Disk;
        page.follow_device();
        assert_eq!(page.spec_tab, 1);
        // A device without a matching section leaves the tab where it was.
        page.selected = Device::Gpu;
        page.follow_device();
        assert_eq!(page.spec_tab, 1);
    }
}
