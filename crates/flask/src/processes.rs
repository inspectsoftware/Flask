use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use sc_core::actions::{self, Priority};
use sc_core::{locks, origin, power, procinfo};
use sc_ui::{
    Align, Anim, Button, ButtonKind, Canvas, Color, Column, Cursor, Event, Font, ListAction, ListModel, ListView, Menu,
    MouseButton, Rect, TextInput, Theme, Win, key, shell,
};

use crate::activity::FeedRow;
use crate::app::{self, Goto};
use crate::format;
use crate::origin_view::{Gone, OriginView, Outcome};
use crate::rules::{self, Rule, Rules};
use crate::sampler::{Frame, Proc, SamplerHandle};
use crate::settings::Settings;

/// `Event::User` code posted when a dump file has been written, or has failed.
pub const DUMP_READY: usize = 5;
/// `Event::User` code posted when a search the system answers has finished.
pub const SPECIAL_READY: usize = 7;
/// `Event::User` code posted when the list of sleep blockers has been read.
pub const AWAKE_READY: usize = 8;
/// Frames between rereads of the sleep blockers while they are showing.
const AWAKE_EVERY: u32 = 5;
/// Height of a panel's header, which is also its handle for moving it.
const PANEL_HEAD: f32 = 44.0;
const GRID_GAP: f32 = 14.0;
/// Smallest a panel may be dragged to.
const PANEL_MIN_W: f32 = 220.0;
const PANEL_MIN_H: f32 = 140.0;
const FEED_ROW: f32 = 30.0;
/// Samples kept for the CPU panel's graph.
const CPU_HISTORY: usize = 60;
/// A meter without its label: gap, track, gap, figure.
const METER_BODY_W: f32 = 8.0 + 72.0 + 4.0 + 38.0;
const METER_GAP: f32 = 20.0;
const COMPACT_ROW: f32 = 30.0;

const TOOLBAR_H: f32 = 54.0;
const MARGIN: f32 = 12.0;
const NAME_MIN_W: f32 = 150.0;
/// How many exited processes are remembered as possible parents.
const GONE_LIMIT: usize = 2048;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Col {
    Name,
    Pid,
    Cpu,
    Gpu,
    Memory,
    Disk,
    GpuMemory,
    Threads,
    Handles,
    User,
    Priority,
    Path,
    CmdLine,
    /// What the process is holding awake: the display, the system, or both.
    Awake,
    /// Empty column that keeps room for the selected row's buttons.
    Actions,
}

struct ColDef {
    col: Col,
    title: &'static str,
    width: f32,
    align: Align,
    visible: bool,
    /// Numeric columns start out sorted largest first.
    numeric: bool,
}

const fn def(col: Col, title: &'static str, width: f32, numeric: bool, visible: bool) -> ColDef {
    ColDef { col, title, width, align: if numeric { Align::Right } else { Align::Left }, visible, numeric }
}

/// List columns, in display order. Indices into this table are the list's
/// column indices. The name column is stretched to fill the leftover width.
const COLS: [ColDef; 15] = [
    def(Col::Name, "Name", NAME_MIN_W, false, true),
    def(Col::Pid, "PID", 64.0, true, true),
    def(Col::Cpu, "CPU", 116.0, true, true),
    def(Col::Gpu, "GPU", 116.0, true, false),
    def(Col::Memory, "Memory", 126.0, true, true),
    def(Col::Disk, "Disk", 84.0, true, false),
    def(Col::GpuMemory, "GPU memory", 96.0, true, false),
    def(Col::Threads, "Threads", 70.0, true, false),
    def(Col::Handles, "Handles", 74.0, true, false),
    def(Col::User, "User", 110.0, false, false),
    def(Col::Priority, "Priority", 100.0, false, false),
    def(Col::Path, "Path", 280.0, false, false),
    def(Col::CmdLine, "Command line", 320.0, false, false),
    def(Col::Awake, "Keeps awake", 150.0, false, false),
    // The selected row's buttons now sit in the panel header.
    def(Col::Actions, "", 0.0, false, false),
];

mod id {
    pub const END: u32 = 1;
    pub const END_TREE: u32 = 2;
    pub const RESTART: u32 = 5;
    pub const END_ALL: u32 = 6;
    pub const SUSPEND: u32 = 3;
    pub const RESUME: u32 = 4;
    pub const PRIORITY: u32 = 10; // + index into Priority::ALL
    pub const REVEAL: u32 = 20;
    pub const PROPERTIES: u32 = 21;
    pub const ORIGIN: u32 = 22;
    pub const DUMP: u32 = 23;
    pub const CONNECTIONS: u32 = 24;
    pub const REMEMBER: u32 = 25;
    pub const COPY_NAME: u32 = 30;
    pub const COPY_PID: u32 = 31;
    pub const COPY_PATH: u32 = 32;
    pub const COPY_CMDLINE: u32 = 33;
    pub const COLUMN: u32 = 100; // + index into COLS
    pub const AFFINITY_ALL: u32 = 199;
    pub const AFFINITY: u32 = 200; // + logical processor number
    pub const AFFINITY_END: u32 = AFFINITY + usize::BITS;
    pub const SERVICE: u32 = 300; // + index into the services the menu listed
}

struct ViewRow {
    idx: u32,
    depth: u16,
    has_children: bool,
    expanded: bool,
    /// For a collapsed parent: what it and everything folded into it use
    /// together, and how many processes are folded away.
    total: Option<(Usage, u32)>,
}

/// The figures a row shows, for one process or summed over a subtree.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Usage {
    cpu: f32,
    gpu: f32,
    memory: u64,
    disk: u64,
    gpu_memory: u64,
    threads: u32,
    handles: u32,
}

impl Usage {
    fn of(p: &Proc) -> Self {
        Self {
            cpu: p.row.cpu,
            gpu: p.gpu,
            memory: p.row.private_working_set,
            disk: p.row.read_rate + p.row.write_rate,
            gpu_memory: p.gpu_memory,
            threads: p.row.threads,
            handles: p.row.handles,
        }
    }

    fn add(&mut self, o: Usage) {
        self.cpu += o.cpu;
        // GPU engines are shared, so a family's use is its busiest member's, as for one process.
        self.gpu = self.gpu.max(o.gpu);
        self.memory += o.memory;
        self.disk += o.disk;
        self.gpu_memory += o.gpu_memory;
        self.threads += o.threads;
        self.handles += o.handles;
    }
}

/// Usage of `root` plus everything below it, and how many are below it.
fn subtree(root: u32, children: &HashMap<u32, Vec<u32>>, procs: &[Proc]) -> (Usage, u32) {
    let mut total = Usage::of(&procs[root as usize]);
    let mut hidden = 0;
    let mut stack: Vec<u32> = children.get(&root).cloned().unwrap_or_default();
    while let Some(i) = stack.pop() {
        total.add(Usage::of(&procs[i as usize]));
        hidden += 1;
        stack.extend(children.get(&i).into_iter().flatten());
    }
    (total, hidden)
}

/// The four panels of the page. Each can be picked up by its header and
/// dropped on another to trade places with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Panel {
    Apps,
    Cpu,
    Background,
    Feed,
}

impl Panel {
    const ALL: [Panel; 4] = [Panel::Apps, Panel::Cpu, Panel::Background, Panel::Feed];

    fn key(self) -> &'static str {
        match self {
            Panel::Apps => "apps",
            Panel::Cpu => "cpu",
            Panel::Background => "background",
            Panel::Feed => "activity",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Panel::Apps => "Apps",
            Panel::Cpu => "CPU",
            Panel::Background => "Background",
            Panel::Feed => "Activity",
        }
    }

    /// Index into `panes` for the two panels that hold a process list.
    fn pane(self) -> Option<usize> {
        match self {
            Panel::Apps => Some(0),
            Panel::Background => Some(1),
            _ => None,
        }
    }
}

fn order_text(order: &[Panel; 4]) -> String {
    order.map(Panel::key).join(",")
}

/// `None` unless the text names each panel exactly once.
fn parse_order(text: &str) -> Option<[Panel; 4]> {
    let named: Vec<Panel> = text.split(',').filter_map(|k| Panel::ALL.into_iter().find(|p| p.key() == k)).collect();
    let order: [Panel; 4] = named.try_into().ok()?;
    Panel::ALL.iter().all(|p| order.contains(p)).then_some(order)
}

fn split_text(split: (f32, f32)) -> String {
    format!("{:.3},{:.3}", split.0, split.1)
}

/// `None` unless the text is two fractions between 0 and 1.
fn parse_split(text: &str) -> Option<(f32, f32)> {
    let (x, y) = text.split_once(',')?;
    let (x, y): (f32, f32) = (x.trim().parse().ok()?, y.trim().parse().ok()?);
    ((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)).then_some((x, y))
}

/// Size of the first of two panels sharing `len`, keeping both at least
/// `min`. When there is no room for that, they share evenly.
fn split_at(frac: f32, len: f32, min: f32) -> f32 {
    if len < 2.0 * min { len / 2.0 } else { (frac * len).clamp(min, len - min) }
}

/// A panel being carried: where the press began and where the pointer is.
#[derive(Clone, Copy)]
struct Carry {
    panel: Panel,
    from: (f32, f32),
    at: (f32, f32),
}

/// A search the system answers, instead of text matched against the list.
#[derive(Clone, PartialEq, Debug)]
enum Query {
    /// Full path of a file: who has it open?
    Locks(String),
    /// Lowercase name or path of a DLL: who loaded it?
    Dll(String),
}

/// Decides whether search text is one of the special questions. `is_file`
/// says whether a path names an existing file.
fn classify(text: &str, is_file: impl Fn(&str) -> bool) -> Option<Query> {
    let text = text.trim().trim_matches('"');
    if text.len() > 4 && text.to_lowercase().ends_with(".dll") {
        return Some(Query::Dll(text.to_lowercase()));
    }
    let looks_like_path = text.get(1..3) == Some(":\\") || text.starts_with("\\\\");
    (looks_like_path && is_file(text)).then(|| Query::Locks(text.to_owned()))
}

/// Where a worker thread leaves its result for the UI thread.
type Slot<T> = Arc<Mutex<Option<T>>>;

struct Special {
    query: Query,
    /// `None` while the answer is being worked out.
    pids: Option<HashSet<u32>>,
    /// The answer in words, for the status bar.
    note: String,
}

fn compare(a: &Proc, b: &Proc, col: Col) -> Ordering {
    let (x, y) = (&a.row, &b.row);
    match col {
        Col::Name | Col::Awake | Col::Actions => format::cmp_ci(&x.name, &y.name),
        Col::Pid => x.pid.cmp(&y.pid),
        Col::Cpu => x.cpu.total_cmp(&y.cpu),
        Col::Gpu => a.gpu.total_cmp(&b.gpu),
        Col::Memory => x.private_working_set.cmp(&y.private_working_set),
        Col::Disk => (x.read_rate + x.write_rate).cmp(&(y.read_rate + y.write_rate)),
        Col::GpuMemory => a.gpu_memory.cmp(&b.gpu_memory),
        Col::Threads => x.threads.cmp(&y.threads),
        Col::Handles => x.handles.cmp(&y.handles),
        Col::User => format::cmp_ci(&a.info.user, &b.info.user),
        Col::Priority => x.base_priority.cmp(&y.base_priority),
        Col::Path => format::cmp_ci(&a.info.path, &b.info.path),
        Col::CmdLine => format::cmp_ci(&a.info.cmdline, &b.info.cmdline),
    }
}

/// Bar length for a share in 0..=1. The square root keeps a 1 % user visible
/// next to a 60 % one while staying monotonic.
fn bar_level(share: f32) -> f32 {
    share.clamp(0.0, 1.0).sqrt()
}

struct Model<'a> {
    procs: &'a [Proc],
    view: &'a [ViewRow],
    tree: bool,
    total_memory: u64,
    awake: &'a HashMap<String, String>,
}

impl Model<'_> {
    fn proc(&self, row: usize) -> &Proc {
        &self.procs[self.view[row].idx as usize]
    }

    /// The row's own figures, or its whole family's when it is collapsed.
    fn usage(&self, row: usize) -> Usage {
        self.view[row].total.map_or_else(|| Usage::of(self.proc(row)), |t| t.0)
    }
}

impl ListModel for Model<'_> {
    fn rows(&self) -> usize {
        self.view.len()
    }

    fn cell(&self, row: usize, col: usize, out: &mut String) {
        let p = self.proc(row);
        let r = &p.row;
        let u = self.usage(row);
        let _ = match COLS[col].col {
            Col::Name => write!(out, "{}", r.name),
            Col::Pid => write!(out, "{}", r.pid),
            Col::Cpu => write!(out, "{:.1}%", u.cpu),
            Col::Gpu => write!(out, "{:.1}%", u.gpu),
            Col::Memory => Ok(format::bytes(out, u.memory)),
            Col::Disk => Ok(format::rate(out, u.disk)),
            Col::GpuMemory if u.gpu_memory > 0 => Ok(format::bytes(out, u.gpu_memory)),
            Col::GpuMemory | Col::Actions => Ok(()),
            Col::Threads => write!(out, "{}", u.threads),
            Col::Handles => write!(out, "{}", u.handles),
            Col::Awake => write!(out, "{}", self.awake.get(&r.name.to_lowercase()).map_or("", |a| a)),
            Col::User => write!(out, "{}", p.info.user),
            Col::Priority => Ok(format::priority(out, r.base_priority)),
            Col::Path => write!(out, "{}", p.info.path),
            Col::CmdLine => write!(out, "{}", p.info.cmdline),
        };
    }

    fn text_color(&self, row: usize, col: usize, t: &Theme) -> Option<Color> {
        let p = self.proc(row);
        let u = self.usage(row);
        let quiet = match COLS[col].col {
            Col::Name => false,
            Col::Cpu => u.cpu < 0.05,
            Col::Gpu => u.gpu < 0.05,
            Col::Memory | Col::Awake => false,
            _ => true,
        };
        (p.row.suspended || quiet).then_some(t.text_dim)
    }

    fn bold(&self, _row: usize, col: usize) -> bool {
        COLS[col].col == Col::Name
    }

    fn secondary(&self, row: usize, col: usize, out: &mut String) {
        if COLS[col].col == Col::Name {
            let p = self.proc(row);
            if let Some((_, hidden)) = self.view[row].total {
                let _ = write!(out, "+{hidden}  ");
            }
            out.push_str(if p.row.suspended { "Suspended" } else { &p.info.company });
        }
    }

    fn bar(&self, row: usize, col: usize) -> Option<f32> {
        let p = self.proc(row);
        let u = self.usage(row);
        match COLS[col].col {
            // The idle process's "usage" is the CPU nobody wanted.
            Col::Cpu if p.row.pid != 0 => Some(bar_level(u.cpu / 100.0)),
            Col::Gpu => Some(bar_level(u.gpu / 100.0)),
            Col::Memory if self.total_memory > 0 => Some(bar_level(u.memory as f32 / self.total_memory as f32)),
            _ => None,
        }
    }

    fn icon(&self, row: usize) -> Option<&str> {
        Some(&self.proc(row).info.path)
    }

    fn is_tree(&self) -> bool {
        self.tree
    }

    fn depth(&self, row: usize) -> u32 {
        self.view[row].depth as u32
    }

    fn expander(&self, row: usize) -> Option<bool> {
        let v = &self.view[row];
        v.has_children.then_some(v.expanded)
    }
}

/// One of the two cards: running apps, or everything else.
struct Pane {
    apps: bool,
    card: Rect,
    list: ListView,
    view: Vec<ViewRow>,
    sort: (Col, bool),
}

impl Pane {
    fn new(apps: bool) -> Self {
        let columns = COLS
            .iter()
            .map(|d| Column { title: d.title, width: d.width, align: d.align, visible: d.visible })
            .collect();
        let mut pane = Self {
            apps,
            card: Rect::default(),
            list: ListView::new(columns),
            view: Vec::new(),
            sort: (Col::Cpu, true),
        };
        pane.sync_sort_arrow();
        pane
    }

    fn sync_sort_arrow(&mut self) {
        let col = COLS.iter().position(|d| d.col == self.sort.0);
        self.list.sort = col.map(|c| (c, self.sort.1));
    }

    /// Sort order, then width and visibility of each optional column, as
    /// text for the settings store: `sort:CPU:1;PID:64:1;GPU:116:0`.
    fn layout_text(&self) -> String {
        let sorted_by = COLS.iter().find(|d| d.col == self.sort.0).map_or("", |d| d.title);
        let mut s = format!("sort:{sorted_by}:{}", self.sort.1 as u8);
        for (d, c) in COLS.iter().zip(&self.list.columns) {
            if !matches!(d.col, Col::Name | Col::Actions) {
                let _ = write!(s, ";{}:{:.0}:{}", d.title, c.width, c.visible as u8);
            }
        }
        s
    }

    /// Applies text from `layout_text`. Anything it does not recognize is
    /// skipped, so a layout saved by another version does no harm.
    fn apply_layout(&mut self, text: &str) {
        for part in text.split(';') {
            let mut fields = part.split(':');
            let (Some(a), Some(b), Some(c)) = (fields.next(), fields.next(), fields.next()) else { continue };
            if a == "sort" {
                if let Some(d) = COLS.iter().find(|d| d.title == b && d.col != Col::Actions) {
                    self.sort = (d.col, c == "1");
                }
            } else if let Some(i) = COLS.iter().position(|d| d.title == a && !matches!(d.col, Col::Name | Col::Actions))
                && let Some(width) = b.parse::<f32>().ok().filter(|w| w.is_finite())
            {
                self.list.columns[i].width = width.clamp(40.0, 2000.0);
                self.list.columns[i].visible = c == "1";
            }
        }
        self.sync_sort_arrow();
    }

    /// Recomputes which processes this pane shows and in what order. `only`
    /// limits it to those PIDs, on top of the text filter.
    fn rebuild(&mut self, procs: &[Proc], filter: &str, tree: bool, collapsed: &HashSet<u32>, only: Option<&HashSet<u32>>) {
        let (col, desc) = self.sort;
        let order = |a: &u32, b: &u32| {
            let (pa, pb) = (&procs[*a as usize], &procs[*b as usize]);
            let o = compare(pa, pb, col);
            (if desc { o.reverse() } else { o }).then(pa.row.pid.cmp(&pb.row.pid))
        };
        let mut pid_text = String::new();
        let members: Vec<u32> = (0..procs.len() as u32)
            .filter(|&i| {
                let p = &procs[i as usize];
                // PID 0 is the idle "process": a counter for unused CPU, not something running.
                if p.is_app != self.apps || p.row.pid == 0 || only.is_some_and(|o| !o.contains(&p.row.pid)) {
                    return false;
                }
                pid_text.clear();
                let _ = write!(pid_text, "{}", p.row.pid);
                filter.is_empty()
                    || format::contains_ci(&p.row.name, filter)
                    || pid_text.contains(filter)
                    || format::contains_ci(&p.info.path, filter)
                    || format::contains_ci(&p.info.company, filter)
            })
            .collect();

        self.view.clear();
        if tree {
            // Parents outside this pane (or filtered out) make their children roots here.
            let by_pid: HashMap<u32, u32> = members.iter().map(|&i| (procs[i as usize].row.pid, i)).collect();
            let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
            let mut roots = Vec::new();
            for &i in &members {
                let p = &procs[i as usize].row;
                match by_pid.get(&p.parent_pid) {
                    // A parent created after its "child" is a reused PID, not a parent.
                    Some(&pi) if pi != i && procs[pi as usize].row.create_time <= p.create_time => {
                        children.entry(pi).or_default().push(i)
                    }
                    _ => roots.push(i),
                }
            }
            roots.sort_by(order);
            let mut stack: Vec<(u32, u16)> = roots.into_iter().rev().map(|i| (i, 0)).collect();
            while let Some((i, depth)) = stack.pop() {
                let expanded = !collapsed.contains(&procs[i as usize].row.pid);
                // ponytail: rows are still ordered by their own figures, so a
                // collapsed family can sit lower than its total suggests.
                let total = (!expanded && children.contains_key(&i)).then(|| subtree(i, &children, procs));
                let kids = children.get_mut(&i);
                self.view.push(ViewRow { idx: i, depth, has_children: kids.is_some(), expanded, total });
                if expanded && let Some(kids) = kids {
                    kids.sort_by(order);
                    stack.extend(kids.iter().rev().map(|&k| (k, depth + 1)));
                }
            }
        } else {
            let mut shown = members;
            shown.sort_by(order);
            self.view.extend(shown.into_iter().map(|idx| ViewRow { idx, depth: 0, has_children: false, expanded: false, total: None }));
        }
    }

    fn position_of(&self, procs: &[Proc], key: (u32, i64)) -> Option<usize> {
        self.view.iter().position(|v| {
            let r = &procs[v.idx as usize].row;
            (r.pid, r.create_time) == key
        })
    }
}

pub struct ProcessesPage {
    search: TextInput,
    tree_btn: Button,
    run_btn: Button,
    /// Set when the user asks for something that lives on another tab.
    pub goto: Option<Goto>,
    /// Services offered by the menu that is open, by display name.
    menu_services: Vec<String>,
    /// Path of a finished dump, or why it failed.
    dump_slot: Arc<Mutex<Option<Result<String, String>>>>,
    /// Scroll the selection into view on the next paint.
    reveal: bool,
    /// Toggle: show only what is stopping the PC from sleeping.
    awake_btn: Button,
    /// Lowercase process name to what it holds awake.
    awake: HashMap<String, String>,
    /// Drivers and services holding the PC awake, which have no row to show in.
    awake_note: String,
    awake_slot: Slot<Result<Vec<power::Request>, String>>,
    awake_loading: bool,
    frames: u32,
    special: Option<Special>,
    // The token discards answers to a question the user has since changed.
    special_slot: Slot<(u64, Result<HashSet<u32>, String>)>,
    special_token: u64,
    rules: Rules,
    /// Selection and row rectangles of the compact view.
    compact_selected: Option<(u32, i64)>,
    compact_rows: Vec<(Rect, (u32, i64))>,
    open_btn: Button,
    origin_btn: Button,
    end_btn: Button,
    origin: OriginView,
    /// Exited processes, so a launch chain survives a parent that quit.
    gone: HashMap<u32, Gone>,
    gone_order: VecDeque<u32>,
    /// The selected row is on screen, so its buttons are shown.
    actions_visible: bool,
    panes: [Pane; 2],
    procs: Vec<Proc>,
    cpu_total: f32,
    gpu_total: f32,
    memory_percent: f32,
    total_memory: u64,
    /// Which panel sits in each grid slot: top left, top right, bottom left, bottom right.
    order: [Panel; 4],
    slots: [Rect; 4],
    /// The area the slots share.
    grid: Rect,
    /// Share of the width the left column takes, and of the height the top row takes.
    split: (f32, f32),
    /// The gap being dragged: whether it moves the column split, the row split, or both.
    resize: Option<(bool, bool)>,
    /// The panel given the whole page, the others put away.
    focus: Option<Panel>,
    /// One per panel, in `Panel::ALL` order: focuses it, or brings the others back.
    focus_btns: [Button; 4],
    /// Where each panel is drawn (x, y, width, height) on its way to where
    /// it belongs, in `Panel::ALL` order.
    glide: [[Anim; 4]; 4],
    /// Panels are where they belong at once: the page itself is changing
    /// size, or a gap is being dragged.
    snap: bool,
    /// The panel last focused, restored or dropped, drawn over the others
    /// while it travels.
    raised: Option<Panel>,
    /// The toolbar's CPU, GPU and Memory meters.
    meter_anim: [Anim; 3],
    /// A header that has been pressed but not yet dragged far enough to lift.
    press: Option<(Panel, f32, f32)>,
    carry: Option<Carry>,
    /// Total CPU use, 0..=1, oldest first.
    cpu_history: VecDeque<f32>,
    /// Latest events for the Activity panel.
    feed: Vec<FeedRow>,
    collapsed: HashSet<u32>,
    /// (pid, creation time) so the selection follows the process, not the row.
    selected: Option<(u32, i64)>,
    /// Outcome of the last action that failed; shown in the status bar.
    notice: String,
}

impl ProcessesPage {
    pub fn new(rules: Rules) -> Self {
        Self {
            search: TextInput::new("Search, or paste a file or .dll to see what uses it"),
            awake_btn: Button::new("Sleep blockers", ButtonKind::Cocoa),
            awake: HashMap::new(),
            awake_note: String::new(),
            awake_slot: Arc::default(),
            awake_loading: false,
            frames: 0,
            special: None,
            special_slot: Arc::default(),
            special_token: 0,
            rules,
            compact_selected: None,
            compact_rows: Vec::new(),
            tree_btn: Button::new("Tree view", ButtonKind::Cocoa),
            run_btn: Button::new("Run task", ButtonKind::Cocoa),
            goto: None,
            menu_services: Vec::new(),
            dump_slot: Arc::default(),
            reveal: false,
            open_btn: Button::small("Open path", ButtonKind::Cocoa),
            origin_btn: Button::small("Origin", ButtonKind::Milk),
            origin: OriginView::new(),
            gone: HashMap::new(),
            gone_order: VecDeque::new(),
            end_btn: Button::small("End", ButtonKind::Peach),
            actions_visible: false,
            panes: [Pane::new(true), Pane::new(false)],
            procs: Vec::new(),
            cpu_total: 0.0,
            gpu_total: 0.0,
            memory_percent: 0.0,
            total_memory: 0,
            order: Panel::ALL,
            slots: [Rect::default(); 4],
            grid: Rect::default(),
            split: (0.5, 0.5),
            resize: None,
            focus: None,
            focus_btns: Panel::ALL.map(|_| Button::small("Focus", ButtonKind::Cocoa)),
            glide: Default::default(),
            snap: false,
            raised: None,
            meter_anim: Default::default(),
            press: None,
            carry: None,
            cpu_history: VecDeque::new(),
            feed: Vec::new(),
            collapsed: HashSet::new(),
            selected: None,
            notice: String::new(),
        }
    }

    pub fn count(&self) -> usize {
        self.procs.len()
    }

    pub fn restore(&mut self, s: &Settings) {
        self.tree_btn.active = s.tree;
        self.order = parse_order(&s.panels).unwrap_or(Panel::ALL);
        self.split = parse_split(&s.split).unwrap_or((0.5, 0.5));
        for (pane, layout) in self.panes.iter_mut().zip(&s.panes) {
            pane.apply_layout(layout);
        }
    }

    pub fn store(&self, s: &mut Settings) {
        s.tree = self.tree_btn.active;
        s.panels = order_text(&self.order);
        s.split = split_text(self.split);
        s.panes = [self.panes[0].layout_text(), self.panes[1].layout_text()];
    }

    /// Selects the process and brings its row into view, dropping whatever
    /// hides it. False when no such process is running.
    pub fn select_pid(&mut self, pid: u32) -> bool {
        let Some(p) = self.procs.iter().find(|p| p.row.pid == pid) else { return false };
        let key = (pid, p.row.create_time);
        self.search.text.clear();
        self.selected = Some(key);
        self.rebuild();
        if self.selected.is_none() {
            // It sits under a collapsed parent.
            self.collapsed.clear();
            self.selected = Some(key);
            self.rebuild();
        }
        self.notice.clear();
        self.reveal = true;
        true
    }

    /// Opens the Origin drawer for a running process started from `image`.
    /// False when nothing is running from it.
    pub fn show_origin_of(&mut self, image: &str, win: &Win) -> bool {
        let Some(pid) = self.procs.iter().find(|p| p.info.path.eq_ignore_ascii_case(image)).map(|p| p.row.pid) else {
            return false;
        };
        self.select_pid(pid);
        if let Some(i) = self.selected_index() {
            self.origin.open_for(&self.procs[i], &self.procs, &self.gone, win.id());
        }
        true
    }

    /// A dump started from the menu has finished.
    pub fn dump_ready(&mut self) {
        match self.dump_slot.lock().unwrap().take() {
            Some(Ok(path)) => {
                self.notice.clear();
                shell::reveal_in_explorer(&path);
            }
            Some(Err(why)) => self.notice = why,
            None => {}
        }
    }

    fn start_dump(&mut self, pid: u32, name: &str, win: &Win) {
        let path = std::env::temp_dir().join(format!("{}-{pid}.dmp", name.trim_end_matches(".exe")));
        self.notice = format!("Writing a dump of {name} (PID {pid})");
        let (slot, id, name) = (self.dump_slot.clone(), win.id(), name.to_owned());
        std::thread::spawn(move || {
            let result = actions::write_dump(pid, &path)
                .map(|()| path.display().to_string())
                .map_err(|e| format!("Could not dump {name} (PID {pid}): {}", e.message()));
            *slot.lock().unwrap() = Some(result);
            Win::post(id, DUMP_READY);
        });
    }

    /// Outcome of the last action, else the answer to a special search, else
    /// whatever else is keeping the PC awake.
    pub fn notice(&self) -> &str {
        match &self.special {
            _ if !self.notice.is_empty() => &self.notice,
            Some(special) => &special.note,
            None if self.awake_btn.active => &self.awake_note,
            None => "",
        }
    }

    /// Called on every frame, after `set_frame`.
    pub fn tick(&mut self, win: &Win) {
        self.frames += 1;
        if self.awake_btn.active && self.frames.is_multiple_of(AWAKE_EVERY) {
            self.refresh_awake(win);
        }
    }

    fn refresh_awake(&mut self, win: &Win) {
        if std::mem::replace(&mut self.awake_loading, true) {
            return;
        }
        let (slot, id) = (self.awake_slot.clone(), win.id());
        std::thread::spawn(move || {
            *slot.lock().unwrap() = Some(power::requests());
            Win::post(id, AWAKE_READY);
        });
    }

    /// The list of sleep blockers has been read.
    pub fn awake_ready(&mut self) {
        let Some(result) = self.awake_slot.lock().unwrap().take() else { return };
        self.awake_loading = false;
        self.awake.clear();
        self.awake_note.clear();
        match result {
            Ok(requests) => {
                let mut others: Vec<String> = Vec::new();
                for r in &requests {
                    // "DISPLAY" reads better as "Display".
                    let what = r.category[..1].to_owned() + &r.category[1..].to_lowercase();
                    match r.process_name() {
                        // ponytail: matched by file name, so every process of
                        // that name is marked. Map the device path to a drive
                        // path to tell them apart.
                        Some(name) => {
                            let held = self.awake.entry(name.to_lowercase()).or_default();
                            if !held.contains(&what) {
                                if !held.is_empty() {
                                    held.push_str(", ");
                                }
                                held.push_str(&what);
                            }
                        }
                        None => others.push(format!("{} ({})", r.who, r.kind.to_lowercase())),
                    }
                }
                others.dedup();
                if !others.is_empty() {
                    self.awake_note = format!("Also keeping the PC awake: {}", others.join(", "));
                } else if self.awake.is_empty() {
                    self.awake_note.push_str("Nothing is keeping the PC awake");
                }
            }
            Err(why) => {
                self.set_awake(false);
                self.notice = format!("Could not read what keeps the PC awake: {why}");
            }
        }
        self.rebuild();
    }

    fn set_awake(&mut self, on: bool) {
        self.awake_btn.active = on;
        let col = COLS.iter().position(|d| d.col == Col::Awake).unwrap_or(0);
        for pane in &mut self.panes {
            pane.list.columns[col].visible = on;
        }
    }

    /// Starts answering the search box if it holds one of the special
    /// questions. `again` asks afresh even when the question has not changed.
    fn update_special(&mut self, win: &Win, again: bool) {
        let query = classify(&self.search.text, |path| Path::new(path).is_file());
        if !again && query == self.special.as_ref().map(|s| s.query.clone()) {
            return;
        }
        self.special_token += 1;
        self.special = query.clone().map(|query| Special { query, pids: None, note: "Looking".to_owned() });
        let Some(query) = query else { return };
        let (slot, token, id) = (self.special_slot.clone(), self.special_token, win.id());
        let pids: Vec<u32> = self.procs.iter().map(|p| p.row.pid).collect();
        std::thread::spawn(move || {
            let result = match &query {
                Query::Locks(path) => locks::holders(path).map(|v| v.into_iter().collect()).map_err(|e| e.message()),
                Query::Dll(dll) => Ok(pids
                    .into_iter()
                    .filter(|&pid| procinfo::modules(pid).iter().any(|m| format::contains_ci(m, dll)))
                    .collect()),
            };
            *slot.lock().unwrap() = Some((token, result));
            Win::post(id, SPECIAL_READY);
        });
    }

    /// A special search has its answer.
    pub fn special_ready(&mut self) {
        let Some((token, result)) = self.special_slot.lock().unwrap().take() else { return };
        let Some(special) = self.special.as_mut().filter(|_| token == self.special_token) else { return };
        let (what, verb) = match &special.query {
            Query::Locks(path) => (path.as_str(), "open"),
            Query::Dll(dll) => (dll.as_str(), "loaded"),
        };
        special.note = match &result {
            Ok(pids) if pids.is_empty() => format!("No process has {what} {verb}"),
            Ok(pids) if pids.len() == 1 => format!("1 process has {what} {verb}"),
            Ok(pids) => format!("{} processes have {what} {verb}", pids.len()),
            Err(why) => format!("Could not check {what}: {why}"),
        };
        special.pids = Some(result.unwrap_or_default());
        self.rebuild();
    }

    /// A background origin lookup finished. Returns true when it is for the
    /// process the drawer is showing.
    pub fn origin_ready(&mut self) -> bool {
        self.origin.poll()
    }

    /// Index into `procs` of the selected process.
    fn selected_index(&self) -> Option<usize> {
        self.panes.iter().find_map(|pane| Some(pane.view.get(pane.list.selected?)?.idx as usize))
    }

    pub fn set_frame(&mut self, frame: Frame) {
        self.cpu_total = frame.cpu_total;
        if self.cpu_history.len() == CPU_HISTORY {
            self.cpu_history.pop_front();
        }
        self.cpu_history.push_back(frame.cpu_total / 100.0);
        self.gpu_total = frame.gpu_total;
        self.memory_percent = frame.memory.used_percent();
        self.total_memory = frame.memory.total;

        // Remember whoever just exited: they may be a running process's parent.
        let alive: HashSet<(u32, i64)> = frame.procs.iter().map(|p| (p.row.pid, p.row.create_time)).collect();
        for old in self.procs.iter().filter(|p| !alive.contains(&(p.row.pid, p.row.create_time))) {
            let gone = Gone { name: old.row.name.clone(), create_time: old.row.create_time };
            if self.gone.insert(old.row.pid, gone).is_none() {
                self.gone_order.push_back(old.row.pid);
            }
        }
        while self.gone_order.len() > GONE_LIMIT {
            if let Some(pid) = self.gone_order.pop_front() {
                self.gone.remove(&pid);
            }
        }

        self.procs = frame.procs;
        self.origin.sync(&self.procs);
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let mut filter = self.search.text.trim().to_lowercase();
        let only: Option<HashSet<u32>> = match &self.special {
            // The text is the question, not something to match rows against.
            Some(special) => {
                filter.clear();
                Some(special.pids.clone().unwrap_or_default())
            }
            None if self.awake_btn.active => Some(
                self.procs
                    .iter()
                    .filter(|p| self.awake.contains_key(&p.row.name.to_lowercase()))
                    .map(|p| p.row.pid)
                    .collect(),
            ),
            None => None,
        };
        for pane in &mut self.panes {
            pane.rebuild(&self.procs, &filter, self.tree_btn.active, &self.collapsed, only.as_ref());
        }
        self.sync_selection();
    }

    /// Points each list at the selected process, wherever it now sits.
    fn sync_selection(&mut self) {
        let mut found = false;
        for pane in &mut self.panes {
            pane.list.selected = self.selected.and_then(|key| pane.position_of(&self.procs, key));
            found |= pane.list.selected.is_some();
        }
        if !found {
            // The process exited or is filtered out.
            self.selected = None;
        }
    }

    fn selected_proc(&self) -> Option<&Proc> {
        self.panes.iter().find_map(|pane| {
            let row = pane.list.selected?;
            Some(&self.procs[pane.view.get(row)?.idx as usize])
        })
    }

    /// What the Activity panel shows; newest first.
    pub fn set_feed(&mut self, feed: Vec<FeedRow>) {
        self.feed = feed;
    }

    /// Where `panel` is drawn: its slot, or wherever it has been carried to.
    fn panel_rect(&self, panel: Panel) -> Rect {
        if self.focus == Some(panel) {
            return self.grid;
        }
        let slot = self.order.iter().position(|&p| p == panel).unwrap_or(0);
        let r = self.slots[slot];
        match self.carry {
            Some(c) if c.panel == panel => Rect { x: r.x + c.at.0 - c.from.0, y: r.y + c.at.1 - c.from.1, ..r },
            _ => r,
        }
    }

    /// Where the panel is drawn: gliding to where it belongs.
    fn shown_rect(&self, panel: Panel) -> Rect {
        let r = self.panel_rect(panel);
        // A carried panel stays under the pointer.
        let carried = self.carry.is_some_and(|c| c.panel == panel);
        let secs = if self.snap || carried { 0.0 } else { 0.22 };
        let a = &self.glide[panel as usize];
        Rect::new(a[0].get(r.x, secs), a[1].get(r.y, secs), a[2].get(r.w, secs), a[3].get(r.h, secs))
    }

    /// The focused panel has finished growing, so the others are out of sight.
    fn covered(&self, panel: Panel) -> bool {
        self.focus.is_some_and(|focus| focus != panel && self.shown_rect(focus) == self.grid)
    }

    /// The panel a carried one would trade places with if dropped now.
    fn drop_target(&self) -> Option<Panel> {
        let carry = self.carry?;
        let slot = self.slots.iter().position(|r| r.contains(carry.at.0, carry.at.1))?;
        Some(self.order[slot]).filter(|&p| p != carry.panel)
    }

    /// Which gaps between panels the point is in: the one between the
    /// columns, the one between the rows. Both where they cross.
    fn gap_at(&self, x: f32, y: f32) -> Option<(bool, bool)> {
        let first = self.slots[0];
        let gap = |at: f32, edge: f32| at >= edge && at < edge + GRID_GAP;
        let hit = (gap(x, first.right()), gap(y, first.bottom()));
        (self.focus.is_none() && self.grid.contains(x, y) && (hit.0 || hit.1)).then_some(hit)
    }

    fn layout(&mut self, area: Rect) {
        let top = area.y + TOOLBAR_H;
        let x = area.x + MARGIN;
        let w = (area.w - 2.0 * MARGIN).max(0.0);
        let h = (area.bottom() - top - 2.0).max(0.0);
        let grid = Rect::new(x, top, w, h);
        self.snap = self.resize.is_some() || std::mem::replace(&mut self.grid, grid) != grid;
        let (w, h) = ((w - GRID_GAP).max(0.0), (h - GRID_GAP).max(0.0));
        let (cw, ch) = (split_at(self.split.0, w, PANEL_MIN_W), split_at(self.split.1, h, PANEL_MIN_H));
        for (slot, r) in self.slots.iter_mut().enumerate() {
            let (rx, rw) = if slot % 2 == 0 { (x, cw) } else { (x + cw + GRID_GAP, w - cw) };
            let (ry, rh) = if slot / 2 == 0 { (top, ch) } else { (top + ch + GRID_GAP, h - ch) };
            *r = Rect::new(rx, ry, rw, rh);
        }
        for panel in Panel::ALL {
            let Some(i) = panel.pane() else { continue };
            // A list out of sight must not answer the mouse.
            let card = if self.covered(panel) { Rect::default() } else { self.shown_rect(panel) };
            let pane = &mut self.panes[i];
            pane.card = card;
            pane.list.rect = Rect::new(card.x, card.y + PANEL_HEAD, card.w, (card.h - PANEL_HEAD - 8.0).max(0.0));
            // The name column takes whatever the others leave.
            let others: f32 = pane.list.columns.iter().skip(1).filter(|c| c.visible).map(|c| c.width).sum();
            pane.list.columns[0].width = (card.w - 12.0 - others).max(NAME_MIN_W);
        }
    }

    /// Draws CPU, GPU and Memory meters ending at `right`. When they do not
    /// all fit to the right of `limit`, the later ones are left out: CPU is
    /// the one people look for first.
    fn meters(&self, c: &mut Canvas, right: f32, mid_y: f32, limit: f32) {
        let all = [("CPU", self.cpu_total), ("GPU", self.gpu_total), ("Memory", self.memory_percent)];
        let widths = all.map(|(label, _)| METER_BODY_W + c.measure_font(label, Font::BODY_BOLD));
        let mut shown = all.len();
        while shown > 0 && widths[..shown].iter().sum::<f32>() + METER_GAP * (shown - 1) as f32 > right - limit {
            shown -= 1;
        }
        let mut right = right;
        for (i, &(label, percent)) in all[..shown].iter().enumerate().rev() {
            right = Self::meter(c, right, mid_y, label, self.meter_anim[i].get(percent, 0.4));
        }
    }

    /// Draws a meter ending at `right` and returns where the next one ends.
    fn meter(c: &mut Canvas, right: f32, mid_y: f32, label: &str, percent: f32) -> f32 {
        let t = c.theme;
        let value = format!("{percent:.0}%");
        let value_r = Rect::new(right - 38.0, mid_y - 12.0, 38.0, 24.0);
        c.text_font(&value, value_r, t.text, Align::Right, Font::BODY_BOLD);
        let track = Rect::new(value_r.x - 4.0 - 72.0, mid_y - 4.0, 72.0, 8.0);
        c.pill(track, t.raised);
        let level = (percent / 100.0).clamp(0.0, 1.0);
        if level > 0.0 {
            c.pill(Rect { w: (track.w * level).max(8.0), ..track }, t.milk);
        }
        let lw = c.measure_font(label, Font::BODY_BOLD);
        let label_r = Rect::new(track.x - 8.0 - lw, mid_y - 12.0, lw + 1.0, 24.0);
        c.text_font(label, label_r, t.text_dim, Align::Left, Font::BODY_BOLD);
        label_r.x - METER_GAP
    }

    pub fn paint(&mut self, c: &mut Canvas, area: Rect) {
        self.layout(area);
        if std::mem::take(&mut self.reveal) {
            for pane in &mut self.panes {
                if let Some(row) = pane.list.selected {
                    pane.list.ensure_visible(row);
                }
            }
        }

        let mid = area.y + TOOLBAR_H / 2.0;
        let (bw, rw, aw) = (self.tree_btn.width(c), self.run_btn.width(c), self.awake_btn.width(c));
        let search_w = (area.w - 2.0 * MARGIN - bw - rw - aw - 46.0).clamp(140.0, 420.0);
        self.search.rect = Rect::new(area.x + MARGIN + 4.0, mid - 17.0, search_w, 34.0);
        self.search.paint(c);
        self.tree_btn.rect = Rect::new(self.search.rect.right() + 10.0, mid - 17.0, bw, 34.0);
        self.tree_btn.paint(c);
        self.run_btn.rect = Rect::new(self.tree_btn.rect.right() + 8.0, mid - 17.0, rw, 34.0);
        self.run_btn.paint(c);
        self.awake_btn.rect = Rect::new(self.run_btn.rect.right() + 8.0, mid - 17.0, aw, 34.0);
        self.awake_btn.paint(c);

        self.actions_visible = false;
        for b in [&mut self.open_btn, &mut self.origin_btn, &mut self.end_btn].into_iter().chain(&mut self.focus_btns) {
            b.rect = Rect::default();
        }
        // Panels at rest first; the one being carried last, so it is on top.
        let carried = self.carry.map(|c| c.panel);
        let top = carried.or(self.raised);
        let target = self.drop_target();
        for panel in self.order {
            if Some(panel) != top && !self.covered(panel) {
                self.paint_panel(c, panel, false, Some(panel) == target);
            }
        }
        if let Some(panel) = top {
            self.paint_panel(c, panel, carried.is_some(), false);
        }
        self.origin.paint(c, area);
    }

    fn paint_panel(&mut self, c: &mut Canvas, panel: Panel, lifted: bool, target: bool) {
        let t = c.theme;
        let r = self.shown_rect(panel);
        if r.w < 1.0 || r.h < PANEL_HEAD {
            return;
        }
        if lifted {
            c.glass_lifted(r, 22.0);
        } else {
            c.glass(r, 22.0);
        }
        if target {
            // Where the carried panel will land.
            c.stroke_round_a(r, 22.0, t.milk_hi, 0.60);
            c.stroke_round_a(r.inset(1.0), 21.0, t.milk_hi, 0.60);
        }

        // Header: a grip, the title, then whatever the panel adds.
        let head = Rect { h: PANEL_HEAD, ..r };
        for i in 0..6 {
            c.dot(head.x + 17.0 + (i % 2) as f32 * 6.0, head.y + 16.0 + (i / 2) as f32 * 6.0, 1.3, t.text_dim);
        }
        let title_x = head.x + 34.0;
        c.text_font(panel.title(), Rect::new(title_x, head.y, 160.0, head.h), t.text, Align::Left, Font::TITLE);
        let after_title = title_x + c.measure_font(panel.title(), Font::TITLE) + 10.0;
        let body = Rect::new(r.x, r.y + PANEL_HEAD, r.w, (r.h - PANEL_HEAD).max(0.0));

        let focus_btn = &mut self.focus_btns[panel as usize];
        focus_btn.label = if self.focus == Some(panel) { "Restore" } else { "Focus" };
        let fw = focus_btn.width(c);
        focus_btn.rect = Rect::new(head.right() - 12.0 - fw, head.y + 10.0, fw, 24.0);
        focus_btn.paint(c);
        // Where the rest of the header's right side ends.
        let head_right = focus_btn.rect.x - 6.0;

        match panel {
            Panel::Apps | Panel::Background => {
                let i = panel.pane().unwrap_or(0);
                let count = self.panes[i].view.len().to_string();
                let cw = c.measure_font(&count, Font::LABEL) + 18.0;
                let chip = Rect::new(after_title, head.y + 12.0, cw, 20.0);
                c.fill_round_a(chip, 10.0, t.milk_hi, 0.09);
                c.text_font(&count, chip, t.text_dim, Align::Center, Font::LABEL);

                let pane = &mut self.panes[i];
                let model = Model {
                    procs: &self.procs,
                    view: &pane.view,
                    tree: self.tree_btn.active,
                    total_memory: self.total_memory,
                    awake: &self.awake,
                };
                pane.list.paint(c, &model);

                // Buttons for the selected process sit in its panel's header.
                if let Some(row) = pane.list.selected.filter(|&row| row < pane.view.len()) {
                    let has_path = !model.proc(row).info.path.is_empty();
                    let (ew, gw, ow) = (self.end_btn.width(c), self.origin_btn.width(c), self.open_btn.width(c));
                    let y = head.y + 10.0;
                    self.end_btn.rect = Rect::new(head_right - ew, y, ew, 24.0);
                    self.origin_btn.rect = Rect::new(self.end_btn.rect.x - 6.0 - gw, y, gw, 24.0);
                    if has_path && self.origin_btn.rect.x - 6.0 - ow > chip.right() + 8.0 {
                        self.open_btn.rect = Rect::new(self.origin_btn.rect.x - 6.0 - ow, y, ow, 24.0);
                        self.open_btn.paint(c);
                    }
                    self.origin_btn.paint(c);
                    self.end_btn.paint(c);
                    self.actions_visible = true;
                }
            }
            Panel::Cpu => {
                c.text("last minute", Rect::new(after_title, head.y, 120.0, head.h), t.text_dim, Align::Left, false);
                let value = format!("{:.0}%", self.cpu_total);
                c.text_font(&value, Rect::new(head.x, head.y + 2.0, head_right - 4.0 - head.x, head.h), t.text, Align::Right, Font::DISPLAY);

                let tiles_h = 52.0;
                let well = Rect::new(body.x + 14.0, body.y, (body.w - 28.0).max(0.0), (body.h - tiles_h - 14.0).max(0.0));
                if well.h >= 24.0 {
                    c.well(well, 14.0);
                    let history: Vec<f32> = self.cpu_history.iter().copied().collect();
                    c.clip(well);
                    c.graph(Rect::new(well.x, well.y + 6.0, well.w, well.h - 12.0), &history, CPU_HISTORY, t.milk, 2.2, true);
                    c.unclip();
                }
                let busiest = self.procs.iter().filter(|p| p.row.pid != 0).max_by(|a, b| a.row.cpu.total_cmp(&b.row.cpu));
                let tiles = [
                    ("Processes", self.procs.len().to_string()),
                    ("Busiest", busiest.map_or(String::new(), |p| format!("{}  {:.1}%", p.row.name, p.row.cpu))),
                    ("Memory in use", format!("{:.0}%", self.memory_percent)),
                ];
                let tw = (body.w - 32.0) / tiles.len() as f32;
                let ty = body.bottom() - tiles_h - 6.0;
                for (i, (label, value)) in tiles.iter().enumerate() {
                    let tx = body.x + 16.0 + i as f32 * tw;
                    c.text_font(label, Rect::new(tx, ty, tw - 8.0, 18.0), t.text_dim, Align::Left, Font::SMALL);
                    c.text_font(value, Rect::new(tx, ty + 18.0, tw - 8.0, 24.0), t.text, Align::Left, Font::BODY_BOLD);
                }
            }
            Panel::Feed => {
                c.text("newest first", Rect::new(after_title, head.y, 120.0, head.h), t.text_dim, Align::Left, false);
                if self.feed.is_empty() {
                    let quiet = "Nothing has started or exited since Flask opened";
                    c.text(quiet, Rect::new(body.x + 16.0, body.y, body.w - 32.0, FEED_ROW), t.text_dim, Align::Left, false);
                }
                c.clip(Rect { h: (body.h - 8.0).max(0.0), ..body });
                for (i, e) in self.feed.iter().enumerate() {
                    let y = body.y + i as f32 * FEED_ROW;
                    if y > body.bottom() {
                        break;
                    }
                    let kind_color = if e.busy { t.butter } else if e.quiet { t.text_dim } else { t.text };
                    c.text(&e.time, Rect::new(body.x + 16.0, y, 66.0, FEED_ROW), t.text_dim, Align::Left, false);
                    c.text(e.kind, Rect::new(body.x + 86.0, y, 96.0, FEED_ROW), kind_color, Align::Left, true);
                    let name_r = Rect::new(body.x + 186.0, y, (body.w - 202.0).max(0.0), FEED_ROW);
                    c.text(&e.name, name_r, if e.quiet { t.text_dim } else { t.text }, Align::Left, true);
                    let used = c.measure(&e.name, true) + 10.0;
                    if used < name_r.w {
                        c.text(&e.note, Rect { x: name_r.x + used, w: name_r.w - used, ..name_r }, t.text_dim, Align::Left, false);
                    }
                }
                c.unclip();
            }
        }
    }

    /// Returns true when the page needs repainting.
    pub fn event(&mut self, ev: &Event, win: &Win, sampler: &SamplerHandle) -> bool {
        let mut redraw = false;

        // The drawer sits on top of everything else on the page.
        match self.origin.event(ev, win) {
            Outcome::Consumed => return true,
            Outcome::Ended => {
                self.notice.clear();
                sampler.refresh_now();
                return true;
            }
            Outcome::Failed(why) => {
                self.notice = why;
                return true;
            }
            Outcome::Redraw => redraw = true,
            Outcome::None => {}
        }

        if self.search.event(ev) {
            self.update_special(win, false);
            self.rebuild();
            redraw = true;
        }
        let (r, clicked) = self.awake_btn.event(ev);
        redraw |= r;
        if clicked {
            self.notice.clear();
            self.set_awake(!self.awake_btn.active);
            self.awake.clear();
            self.awake_note.clear();
            if self.awake_btn.active {
                self.awake_note.push_str("Looking");
                self.refresh_awake(win);
            }
            self.rebuild();
            return true;
        }
        let (r, clicked) = self.tree_btn.event(ev);
        redraw |= r;
        if clicked {
            self.tree_btn.active = !self.tree_btn.active;
            self.rebuild();
            return true;
        }
        let (r, clicked) = self.run_btn.event(ev);
        redraw |= r;
        if clicked {
            app::run_task(win);
            return true;
        }

        if self.actions_visible {
            let (r1, open) = self.open_btn.event(ev);
            let (r2, origin) = self.origin_btn.event(ev);
            let (r3, end) = self.end_btn.event(ev);
            redraw |= r1 | r2 | r3;
            if open || origin || end {
                let cmd = if open {
                    id::REVEAL
                } else if origin {
                    id::ORIGIN
                } else {
                    id::END
                };
                self.run(cmd, win, sampler);
                return true;
            }
        }

        // Moving panels: press a header, drag, and drop on another panel to
        // trade places with it.
        for panel in Panel::ALL {
            let (r, clicked) = self.focus_btns[panel as usize].event(ev);
            redraw |= r;
            if clicked {
                self.focus = (self.focus != Some(panel)).then_some(panel);
                self.raised = Some(panel);
                return true;
            }
        }

        // Resizing panels: drag the gap between them; a double click evens
        // them out again.
        match *ev {
            Event::MouseMove { x, y } => {
                if let Some((cols, rows)) = self.resize {
                    let g = self.grid;
                    let (w, h) = (g.w - GRID_GAP, g.h - GRID_GAP);
                    if cols && w > 0.0 {
                        self.split.0 = split_at((x - g.x - GRID_GAP / 2.0) / w, w, PANEL_MIN_W) / w;
                    }
                    if rows && h > 0.0 {
                        self.split.1 = split_at((y - g.y - GRID_GAP / 2.0) / h, h, PANEL_MIN_H) / h;
                    }
                    return true;
                }
            }
            Event::MouseDown { x, y, button: MouseButton::Left, clicks } => {
                if let Some((cols, rows)) = self.gap_at(x, y) {
                    if clicks == 2 {
                        if cols {
                            self.split.0 = 0.5;
                        }
                        if rows {
                            self.split.1 = 0.5;
                        }
                    } else {
                        self.resize = Some((cols, rows));
                    }
                    return true;
                }
            }
            Event::MouseUp { button: MouseButton::Left, .. } if self.resize.take().is_some() => return true,
            _ => {}
        }

        match *ev {
            Event::MouseMove { x, y } => {
                if let Some(carry) = &mut self.carry {
                    carry.at = (x, y);
                    return true;
                }
                // A press only becomes a carry once it has clearly moved, so
                // a plain click on a header does nothing.
                if let Some((panel, px, py)) = self.press
                    && (x - px).hypot(y - py) > 5.0
                {
                    self.carry = Some(Carry { panel, from: (px, py), at: (x, y) });
                    return true;
                }
                // Lists only ever ask for the column-resize cursor, so reset first.
                win.set_cursor(match self.gap_at(x, y) {
                    Some((true, _)) => Cursor::ResizeH,
                    Some(_) => Cursor::ResizeV,
                    None => Cursor::Arrow,
                });
            }
            Event::MouseDown { x, y, button: MouseButton::Left, .. } => {
                let header = self.slots.iter().position(|r| Rect { h: PANEL_HEAD, ..*r }.contains(x, y));
                // A focused panel has nowhere to be moved to.
                self.press = header.filter(|_| self.focus.is_none()).map(|slot| (self.order[slot], x, y));
            }
            Event::MouseUp { x, y, button: MouseButton::Left } => {
                self.press = None;
                if let Some(carry) = self.carry.take() {
                    self.raised = Some(carry.panel);
                    let from = self.order.iter().position(|&p| p == carry.panel);
                    let to = self.slots.iter().position(|r| r.contains(x, y));
                    if let (Some(from), Some(to)) = (from, to) {
                        self.order.swap(from, to);
                    }
                    return true;
                }
            }
            _ => {}
        }
        // Nothing underneath reacts while a panel is in the air.
        if self.carry.is_some() {
            return redraw;
        }

        let is_key = matches!(ev, Event::Key { .. });
        if let Event::Key { vk, shift, .. } = *ev {
            match vk {
                key::DELETE if shift => self.run(id::END_TREE, win, sampler),
                key::DELETE => self.run(id::END, win, sampler),
                key::F5 => {
                    sampler.refresh_now();
                    self.update_special(win, true);
                }
                _ => {}
            }
        }

        // Keys go to the pane holding the selection; the mouse to whichever it is over.
        // With nothing selected, arrow keys start in the first pane that has rows.
        let key_pane = self
            .panes
            .iter()
            .position(|p| p.list.selected.is_some())
            .or_else(|| self.panes.iter().position(|p| !p.view.is_empty()))
            .unwrap_or(0);
        for i in 0..self.panes.len() {
            if is_key && i != key_pane {
                continue;
            }
            let resp = {
                let pane = &mut self.panes[i];
                let model = Model {
                    procs: &self.procs,
                    view: &pane.view,
                    tree: self.tree_btn.active,
                    total_memory: self.total_memory,
                    awake: &self.awake,
                };
                pane.list.event(ev, &model, win)
            };
            redraw |= resp.redraw;
            match resp.action {
                Some(ListAction::Select(row)) => {
                    self.selected = row.map(|r| {
                        let p = &self.procs[self.panes[i].view[r].idx as usize].row;
                        (p.pid, p.create_time)
                    });
                    self.notice.clear();
                    self.sync_selection();
                }
                Some(ListAction::Sort(ci)) if COLS[ci].col != Col::Actions => {
                    let d = &COLS[ci];
                    let pane = &mut self.panes[i];
                    pane.sort = if pane.sort.0 == d.col { (d.col, !pane.sort.1) } else { (d.col, d.numeric) };
                    pane.sync_sort_arrow();
                    self.rebuild();
                    redraw = true;
                }
                Some(ListAction::Toggle(row)) => {
                    let pid = self.procs[self.panes[i].view[row].idx as usize].row.pid;
                    if !self.collapsed.remove(&pid) {
                        self.collapsed.insert(pid);
                    }
                    self.rebuild();
                    redraw = true;
                }
                Some(ListAction::Activate(_)) => self.run(id::PROPERTIES, win, sampler),
                Some(ListAction::Context { row: Some(_) }) => {
                    // The right-button press already selected the row; show it before the menu blocks.
                    self.sync_selection();
                    win.invalidate();
                    if let Some(cmd) = self.process_menu(win) {
                        self.run(cmd, win, sampler);
                    }
                    redraw = true;
                }
                Some(ListAction::HeaderContext) => {
                    let mut menu = Menu::new();
                    for (ci, d) in COLS.iter().enumerate() {
                        // The name anchors the row, the last column holds its
                        // buttons, and the awake column follows its toggle.
                        if !matches!(d.col, Col::Name | Col::Actions | Col::Awake) {
                            menu.item_with(id::COLUMN + ci as u32, d.title, self.panes[i].list.columns[ci].visible, true);
                        }
                    }
                    if let Some(cmd) = win.popup(&menu)
                        && let Some(col) = self.panes[i].list.columns.get_mut((cmd - id::COLUMN) as usize)
                    {
                        col.visible = !col.visible;
                    }
                    redraw = true;
                }
                _ => {}
            }
        }
        redraw
    }

    fn process_menu(&mut self, win: &Win) -> Option<u32> {
        let pid = self.selected_proc()?.row.pid;
        // The idle process and the kernel host no services.
        self.menu_services = if pid > 4 { origin::services_in(pid) } else { Vec::new() };
        let p = self.selected_proc()?;
        let has_path = !p.info.path.is_empty();
        let current = actions::priority(pid);
        let named = self.procs.iter().filter(|q| q.row.name == p.row.name).count();
        // Only what runs on the user's own desktop: services are restarted
        // from the Services tab, by the service manager.
        let own_session = self.procs.iter().find(|q| q.row.pid == std::process::id()).map(|q| q.row.session);
        let can_restart = has_path && pid > 4 && Some(p.row.session) == own_session;
        let remembered = has_path && self.rules.lock().unwrap().contains_key(&rules::key(&p.info.path));

        let mut priority = Menu::new();
        for (i, pr) in Priority::ALL.into_iter().enumerate() {
            priority.item_with(id::PRIORITY + i as u32, pr.label(), current == Some(pr), true);
        }
        let mut copy = Menu::new();
        copy.item(id::COPY_NAME, "Name")
            .item(id::COPY_PID, "PID")
            .item_with(id::COPY_PATH, "Path", false, has_path)
            .item_with(id::COPY_CMDLINE, "Command line", false, !p.info.cmdline.is_empty());

        let mut menu = Menu::new();
        menu.item(id::END, "End process\tDel").item(id::END_TREE, "End process tree\tShift+Del");
        if named > 1 {
            menu.item(id::END_ALL, &format!("End all {named} named {}", p.row.name));
        }
        menu.item_with(id::RESTART, "Restart", false, can_restart).separator();
        if p.row.suspended {
            menu.item(id::RESUME, "Resume");
        } else {
            menu.item(id::SUSPEND, "Suspend");
        }
        menu.submenu("Priority", priority);
        if let Some((mask, system)) = actions::affinity(pid).filter(|a| a.1 != 0) {
            let mut cores = Menu::new();
            cores.item_with(id::AFFINITY_ALL, "All processors", mask == system, true).separator();
            for n in (0..usize::BITS).filter(|n| system >> n & 1 == 1) {
                cores.item_with(id::AFFINITY + n, &format!("CPU {n}"), mask >> n & 1 == 1, true);
            }
            menu.submenu("Affinity", cores);
        }
        menu.item_with(id::REMEMBER, &format!("Always start {} this way", p.row.name), remembered, has_path)
            .item(id::DUMP, "Create dump file")
            .separator()
            .item(id::ORIGIN, "Origin")
            .item(id::CONNECTIONS, "Connections");
        if !self.menu_services.is_empty() {
            let mut services = Menu::new();
            for (i, name) in self.menu_services.iter().enumerate() {
                services.item(id::SERVICE + i as u32, name);
            }
            menu.submenu("Go to service", services);
        }
        menu.item_with(id::REVEAL, "Open path", false, has_path)
            .item_with(id::PROPERTIES, "Properties", false, has_path)
            .submenu("Copy", copy);
        win.popup(&menu)
    }

    /// Carries out a command on the selected process. Nothing here asks for
    /// confirmation; failures are reported in the status bar.
    fn run(&mut self, cmd: u32, win: &Win, sampler: &SamplerHandle) {
        if cmd == id::ORIGIN {
            if let Some(i) = self.selected_index() {
                self.origin.open_for(&self.procs[i], &self.procs, &self.gone, win.id());
            }
            return;
        }
        if let Some(service) = cmd.checked_sub(id::SERVICE).and_then(|i| self.menu_services.get(i as usize)) {
            self.goto = Some(Goto::Service(service.clone()));
            return;
        }
        let Some(p) = self.selected_proc() else { return };
        let (pid, name, path) = (p.row.pid, p.row.name.clone(), p.info.path.clone());
        let named = |pids: &[u32]| -> Vec<(u32, Arc<str>)> {
            self.procs.iter().filter(|q| pids.contains(&q.row.pid)).map(|q| (q.row.pid, q.row.name.clone())).collect()
        };

        let result = match cmd {
            id::END if !app::confirm_end(win, &[(pid, name.clone())]) => return,
            id::END => actions::terminate(pid),
            id::END_TREE => {
                let nodes: Vec<_> =
                    self.procs.iter().map(|p| (p.row.pid, p.row.parent_pid, p.row.create_time)).collect();
                let mut doomed = actions::descendants(pid, &nodes);
                doomed.push(pid);
                if !app::confirm_end(win, &named(&doomed)) {
                    return;
                }
                match actions::terminate_tree(pid, &nodes).1 {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
            id::END_ALL => {
                let doomed: Vec<_> =
                    self.procs.iter().filter(|q| q.row.name == name).map(|q| (q.row.pid, q.row.name.clone())).collect();
                let question = format!("End all {} processes named {name}?\n\nUnsaved work in them is lost.", doomed.len());
                if !win.confirm("End all", &question) || !app::confirm_end(win, &doomed) {
                    return;
                }
                let mut result = Ok(());
                for (pid, _) in &doomed {
                    if let Err(e) = actions::terminate(*pid) {
                        result = Err(e);
                    }
                }
                result
            }
            id::RESTART if !app::confirm_end(win, &[(pid, name.clone())]) => return,
            id::RESTART => actions::restart(pid, &path, &p.info.cmdline).map(|_| ()),
            id::CONNECTIONS => {
                self.goto = Some(Goto::Connections(pid));
                return;
            }
            id::REMEMBER => {
                let wanted = match self.rules.lock().unwrap().contains_key(&rules::key(&path)) {
                    true => None,
                    false => Some(Rule { priority: actions::priority(pid), affinity: actions::affinity(pid).map(|a| a.0) }),
                };
                self.notice.clear();
                if !rules::set(&self.rules, &path, wanted) {
                    let _ = write!(self.notice, "Could not save that for {name}: it needs administrator rights");
                }
                return;
            }
            id::SUSPEND => actions::suspend(pid),
            id::RESUME => actions::resume(pid),
            id::REVEAL => return shell::reveal_in_explorer(&p.info.path),
            id::PROPERTIES => {
                if !p.info.path.is_empty() {
                    shell::show_properties(&p.info.path);
                }
                return;
            }
            id::COPY_NAME => return win.copy_text(&name),
            id::COPY_PID => return win.copy_text(&pid.to_string()),
            id::COPY_PATH => return win.copy_text(&p.info.path),
            id::COPY_CMDLINE => return win.copy_text(&p.info.cmdline),
            id::DUMP => return self.start_dump(pid, &name, win),
            id::AFFINITY_ALL..id::AFFINITY_END => {
                let Some((mask, system)) = actions::affinity(pid) else { return };
                let wanted = if cmd == id::AFFINITY_ALL { system } else { mask ^ (1 << (cmd - id::AFFINITY)) } & system;
                // A process has to be left at least one processor.
                if wanted == 0 {
                    return;
                }
                actions::set_affinity(pid, wanted)
            }
            _ => match Priority::ALL.get(cmd.wrapping_sub(id::PRIORITY) as usize) {
                Some(&pr) => actions::set_priority(pid, pr),
                None => return,
            },
        };

        self.notice.clear();
        match result {
            Ok(()) => {
                sampler.refresh_now();
                // A remembered program keeps whatever it was last given.
                let changed = (id::PRIORITY..id::PRIORITY + 6).contains(&cmd) || (id::AFFINITY_ALL..id::AFFINITY_END).contains(&cmd);
                if changed && self.rules.lock().unwrap().contains_key(&rules::key(&path)) {
                    let rule = Rule { priority: actions::priority(pid), affinity: actions::affinity(pid).map(|a| a.0) };
                    rules::set(&self.rules, &path, Some(rule));
                }
            }
            Err(e) => {
                let what = match cmd {
                    id::END | id::END_TREE => "Could not end",
                    id::END_ALL => "Could not end every",
                    id::RESTART => "Could not restart",
                    id::SUSPEND => "Could not suspend",
                    id::RESUME => "Could not resume",
                    id::AFFINITY_ALL..id::AFFINITY_END => "Could not change the processors of",
                    _ => "Could not change the priority of",
                };
                let _ = write!(self.notice, "{what} {name} (PID {pid}): {}", e.message());
            }
        }
    }
}

impl ProcessesPage {
    /// The compact view: the three meters and the busiest processes, with a
    /// way to end one. Independent of the search box and panes.
    pub fn paint_compact(&mut self, c: &mut Canvas, area: Rect) {
        let t = c.theme;
        self.meters(c, area.right() - MARGIN, area.y + 22.0, area.x + MARGIN);

        let top = area.y + 44.0;
        let mut busiest: Vec<&Proc> = self.procs.iter().filter(|p| p.row.pid != 0).collect();
        busiest.sort_by(|a, b| b.row.cpu.total_cmp(&a.row.cpu).then(a.row.pid.cmp(&b.row.pid)));
        busiest.truncate(((area.bottom() - top - 6.0) / COMPACT_ROW).max(0.0) as usize);

        self.compact_rows.clear();
        self.end_btn.rect = Rect::default();
        let mut text = String::new();
        for (i, p) in busiest.into_iter().enumerate() {
            let key = (p.row.pid, p.row.create_time);
            let r = Rect::new(area.x + 6.0, top + i as f32 * COMPACT_ROW, area.w - 12.0, COMPACT_ROW);
            self.compact_rows.push((r, key));
            let selected = self.compact_selected == Some(key);
            if selected {
                c.fill_round_a(Rect { y: r.y + 1.0, h: r.h - 2.0, ..r }, 12.0, t.milk_hi, 0.11);
            }
            let icon = Rect::new(r.x + 8.0, r.y + 6.0, 18.0, 18.0);
            if !c.icon(&p.info.path, icon) {
                c.fill_round(icon, 6.0, t.cocoa);
            }
            // The End button takes the memory figure's place on the selected row.
            let figures = if selected {
                let ew = self.end_btn.width(c);
                self.end_btn.rect = Rect::new(r.right() - 8.0 - ew, r.y + 3.0, ew, 24.0);
                self.end_btn.paint(c);
                self.end_btn.rect.x - 8.0
            } else {
                text.clear();
                format::bytes(&mut text, p.row.private_working_set);
                c.text(&text, Rect::new(r.right() - 98.0, r.y, 90.0, r.h), t.text_dim, Align::Right, false);
                r.right() - 98.0
            };
            text.clear();
            let _ = write!(text, "{:.1}%", p.row.cpu);
            c.text(&text, Rect::new(figures - 64.0, r.y, 60.0, r.h), t.text, Align::Right, false);
            let name_r = Rect::new(icon.right() + 10.0, r.y, (figures - 64.0 - icon.right() - 16.0).max(0.0), r.h);
            c.text(&p.row.name, name_r, t.text, Align::Left, true);
        }
    }

    /// Returns true when the compact view needs repainting.
    pub fn event_compact(&mut self, ev: &Event, win: &Win, sampler: &SamplerHandle) -> bool {
        let (redraw, clicked) = self.end_btn.event(ev);
        let delete = matches!(*ev, Event::Key { vk: key::DELETE, .. });
        if (clicked || delete)
            && let Some(p) = self.procs.iter().find(|p| Some((p.row.pid, p.row.create_time)) == self.compact_selected)
        {
            let (pid, name) = (p.row.pid, p.row.name.clone());
            if app::confirm_end(win, &[(pid, name)]) && actions::terminate(pid).is_ok() {
                sampler.refresh_now();
            }
            return true;
        }
        if let Event::MouseDown { x, y, button: MouseButton::Left, .. } = *ev {
            self.compact_selected = self.compact_rows.iter().find(|(r, _)| r.contains(x, y)).map(|&(_, key)| key);
            return true;
        }
        redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_core::process::ProcessRow;

    fn proc(pid: u32, parent: u32, created: i64, name: &str, is_app: bool, cpu: f32) -> Proc {
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
            is_app,
            gpu: 0.0,
            gpu_memory: 0,
        }
    }

    fn names(pane: &Pane, procs: &[Proc]) -> Vec<(String, u16)> {
        pane.view.iter().map(|v| (procs[v.idx as usize].row.name.to_string(), v.depth)).collect()
    }

    #[test]
    fn panes_split_apps_from_background_and_sort() {
        let procs = vec![
            proc(10, 1, 100, "browser.exe", true, 5.0),
            proc(20, 10, 110, "renderer.exe", false, 9.0),
            proc(30, 1, 120, "editor.exe", true, 7.0),
            proc(40, 1, 90, "service.exe", false, 1.0),
        ];
        let (mut apps, mut bg) = (Pane::new(true), Pane::new(false));
        apps.rebuild(&procs, "", false, &HashSet::new(), None);
        bg.rebuild(&procs, "", false, &HashSet::new(), None);
        // Default sort is CPU, highest first.
        assert_eq!(names(&apps, &procs), [("editor.exe".into(), 0), ("browser.exe".into(), 0)]);
        assert_eq!(names(&bg, &procs), [("renderer.exe".into(), 0), ("service.exe".into(), 0)]);

        apps.rebuild(&procs, "brow", false, &HashSet::new(), None);
        assert_eq!(names(&apps, &procs), [("browser.exe".into(), 0)]);
        bg.rebuild(&procs, "40", false, &HashSet::new(), None);
        assert_eq!(names(&bg, &procs), [("service.exe".into(), 0)]);
    }

    #[test]
    fn tree_nests_within_a_pane_and_roots_orphans() {
        let procs = vec![
            proc(10, 1, 100, "app.exe", true, 0.0),
            // Parent is an app, so in the background pane this is a root.
            proc(20, 10, 110, "helper.exe", false, 2.0),
            proc(30, 20, 120, "worker.exe", false, 1.0),
            // Claims parent 20 but predates it: a reused PID, not a child.
            proc(40, 20, 50, "old.exe", false, 3.0),
        ];
        let mut bg = Pane::new(false);
        bg.rebuild(&procs, "", true, &HashSet::new(), None);
        assert_eq!(names(&bg, &procs), [("old.exe".into(), 0), ("helper.exe".into(), 0), ("worker.exe".into(), 1)]);

        bg.rebuild(&procs, "", true, &HashSet::from([20]), None);
        assert_eq!(names(&bg, &procs), [("old.exe".into(), 0), ("helper.exe".into(), 0)]);
        assert!(bg.view[1].has_children && !bg.view[1].expanded);
    }

    #[test]
    fn collapsed_parent_shows_its_whole_family() {
        let mut procs = vec![
            proc(10, 1, 100, "browser.exe", false, 2.0),
            proc(20, 10, 110, "tab.exe", false, 5.0),
            proc(30, 20, 120, "gpu.exe", false, 1.0),
            proc(40, 1, 90, "other.exe", false, 0.5),
        ];
        for (p, mb) in procs.iter_mut().zip([100u64, 300, 50, 7]) {
            p.row.private_working_set = mb << 20;
        }
        let mut bg = Pane::new(false);
        bg.rebuild(&procs, "", true, &HashSet::from([10]), None);
        // Children are folded away and counted into the parent.
        assert_eq!(names(&bg, &procs), [("browser.exe".into(), 0), ("other.exe".into(), 0)]);
        let (total, hidden) = bg.view[0].total.unwrap();
        assert_eq!((total.cpu, total.memory, total.threads, hidden), (8.0, 450 << 20, 3, 2));
        assert_eq!(bg.view[1].total, None);

        let awake = HashMap::new();
        let model = Model { procs: &procs, view: &bg.view, tree: true, total_memory: 1 << 30, awake: &awake };
        let cell = |row, col: Col| {
            let mut s = String::new();
            model.cell(row, COLS.iter().position(|d| d.col == col).unwrap(), &mut s);
            s
        };
        assert_eq!((cell(0, Col::Cpu), cell(0, Col::Memory), cell(1, Col::Cpu)), ("8.0%".into(), "450.0 MB".into(), "0.5%".into()));
        let mut after_name = String::new();
        model.secondary(0, 0, &mut after_name);
        assert_eq!(after_name, "+2  ");

        // Expanded again, every row speaks for itself.
        bg.rebuild(&procs, "", true, &HashSet::new(), None);
        assert!(bg.view.iter().all(|v| v.total.is_none()));
    }

    #[test]
    fn panel_order_round_trips_and_rejects_anything_but_a_full_set() {
        let order = [Panel::Feed, Panel::Apps, Panel::Cpu, Panel::Background];
        assert_eq!(parse_order(&order_text(&order)), Some(order));
        for bad in ["", "apps,cpu,background", "apps,apps,cpu,background", "apps,cpu,background,activity,cpu", "a,b,c,d"] {
            assert_eq!(parse_order(bad), None, "{bad}");
        }
    }

    #[test]
    fn panel_split_round_trips_rejects_nonsense_and_keeps_both_panels_usable() {
        assert_eq!(parse_split(&split_text((0.625, 0.25))), Some((0.625, 0.25)));
        for bad in ["", "0.5", "0.5,x", "1.5,0.5", "-0.1,0.5", "NaN,0.5", "0.5,0.5,0.5"] {
            assert_eq!(parse_split(bad), None, "{bad}");
        }
        assert_eq!(split_at(0.5, 1000.0, 220.0), 500.0);
        assert_eq!(split_at(0.0, 1000.0, 220.0), 220.0);
        assert_eq!(split_at(1.0, 1000.0, 220.0), 780.0);
        // Too narrow for two minimums: share evenly.
        assert_eq!(split_at(0.9, 300.0, 220.0), 150.0);
    }

    #[test]
    fn only_limits_a_pane_to_the_given_pids() {
        let procs = vec![proc(10, 1, 100, "a.exe", false, 1.0), proc(20, 1, 110, "b.exe", false, 2.0)];
        let mut bg = Pane::new(false);
        bg.rebuild(&procs, "", false, &HashSet::new(), Some(&HashSet::from([10])));
        assert_eq!(names(&bg, &procs), [("a.exe".into(), 0)]);
        bg.rebuild(&procs, "", false, &HashSet::new(), Some(&HashSet::new()));
        assert!(bg.view.is_empty());
    }

    #[test]
    fn search_text_is_recognized_as_a_file_or_dll_question() {
        let exists = |p: &str| p == r"C:\data\report.xlsx";
        assert_eq!(classify(r#" "C:\data\report.xlsx" "#, exists), Some(Query::Locks(r"C:\data\report.xlsx".into())));
        assert_eq!(classify(r"C:\data\missing.txt", exists), None);
        assert_eq!(classify("report.xlsx", exists), None);
        assert_eq!(classify("chrome", exists), None);
        assert_eq!(classify("D3D11.DLL", exists), Some(Query::Dll("d3d11.dll".into())));
        assert_eq!(classify(r"C:\Windows\System32\ntdll.dll", exists), Some(Query::Dll(r"c:\windows\system32\ntdll.dll".into())));
        assert_eq!(classify(".dll", exists), None);
        assert_eq!(classify("Ã©", exists), None);
    }

    #[test]
    fn column_layout_round_trips_and_ignores_junk() {
        let mut pane = Pane::new(true);
        let gpu = COLS.iter().position(|d| d.col == Col::Gpu).unwrap();
        let path = COLS.iter().position(|d| d.col == Col::Path).unwrap();
        pane.list.columns[gpu].visible = false;
        pane.list.columns[path].visible = true;
        pane.list.columns[path].width = 444.0;
        pane.sort = (Col::Memory, false);

        let mut restored = Pane::new(true);
        restored.apply_layout(&pane.layout_text());
        assert_eq!(restored.layout_text(), pane.layout_text());
        assert_eq!((restored.sort, restored.list.sort), ((Col::Memory, false), Some((4, false))));
        assert!(!restored.list.columns[gpu].visible && restored.list.columns[path].visible);

        // Unknown columns, bad numbers and attempts on the fixed columns change nothing.
        let before = restored.layout_text();
        restored.apply_layout("sort::1;sort:Nope:1;Nope:50:1;PID:NaN:0;PID:x:0;Name:10:0;:10:0;;PID");
        assert_eq!(restored.layout_text(), before);
        assert!(restored.list.columns[0].visible);
        restored.apply_layout("PID:999999:1");
        assert_eq!(restored.list.columns[1].width, 2000.0);
    }

    #[test]
    fn bar_level_is_monotonic_and_bounded() {
        assert_eq!(bar_level(0.0), 0.0);
        assert_eq!(bar_level(1.0), 1.0);
        assert_eq!(bar_level(7.0), 1.0);
        assert!(bar_level(0.01) > 0.05 && bar_level(0.01) < bar_level(0.25));
    }
}
