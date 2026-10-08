//! Virtualized list / tree with a sortable, resizable header. Only the rows
//! that are on screen are asked for and drawn. The list paints no background
//! of its own; it is meant to sit on a card.

use crate::{Align, Canvas, Color, Cursor, Event, Font, MouseButton, Rect, Theme, Win, key};

pub const HEADER_H: f32 = 26.0;
pub const ROW_H: f32 = 32.0;
const CELL_PAD: f32 = 8.0;
/// Gap between the list edge and the rounded row highlight.
const ROW_INSET: f32 = 6.0;
const INDENT: f32 = 16.0;
const ICON: f32 = 18.0;
const BAR_W: f32 = 30.0;
/// Width reserved for the number next to a usage bar.
const BAR_TEXT_W: f32 = 60.0;
const SCROLLBAR_W: f32 = 10.0;
const RESIZE_GRIP: f32 = 4.0;
const MIN_COL_W: f32 = 40.0;

pub struct Column {
    pub title: &'static str,
    pub width: f32,
    pub align: Align,
    pub visible: bool,
}

/// Supplies the list's content. Column indices refer to `ListView::columns`,
/// hidden ones included.
pub trait ListModel {
    fn rows(&self) -> usize;
    /// Appends the cell text to `out` (already cleared).
    fn cell(&self, row: usize, col: usize, out: &mut String);
    fn text_color(&self, _row: usize, _col: usize, _theme: &Theme) -> Option<Color> {
        None
    }
    fn bold(&self, _row: usize, _col: usize) -> bool {
        false
    }
    /// Dimmed text that follows the cell's main text, e.g. a publisher after a name.
    fn secondary(&self, _row: usize, _col: usize, _out: &mut String) {}
    /// Fill level 0..=1 of a small usage bar drawn beside the cell's number.
    fn bar(&self, _row: usize, _col: usize) -> Option<f32> {
        None
    }
    /// `Some(on)` draws an on/off switch in the cell instead of text;
    /// clicking it reports [`ListAction::Switch`].
    fn switch(&self, _row: usize, _col: usize) -> Option<bool> {
        None
    }
    /// File whose shell icon leads the first visible column. `Some("")` asks
    /// for the stand-in tile.
    fn icon(&self, _row: usize) -> Option<&str> {
        None
    }
    /// When true the first visible column gets an indent and expander gutter.
    fn is_tree(&self) -> bool {
        false
    }
    fn depth(&self, _row: usize) -> u32 {
        0
    }
    /// `Some(expanded)` when the row has children.
    fn expander(&self, _row: usize) -> Option<bool> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ListAction {
    Sort(usize),
    Select(Option<usize>),
    Activate(usize),
    Toggle(usize),
    /// The switch in this row was clicked.
    Switch(usize),
    Context { row: Option<usize> },
    HeaderContext,
}

#[derive(Default)]
pub struct Response {
    pub redraw: bool,
    pub action: Option<ListAction>,
}

enum Drag {
    Column { col: usize, start_x: f32, start_w: f32 },
    Thumb { grab_dy: f32 },
}

pub struct ListView {
    pub rect: Rect,
    pub columns: Vec<Column>,
    pub selected: Option<usize>,
    /// (column, descending) shown as an arrow in the header.
    pub sort: Option<(usize, bool)>,
    /// Column that carries the icon and tree indent; the first visible one
    /// unless set.
    pub lead_column: Option<usize>,
    scroll_x: f32,
    scroll_y: f32,
    hover: Option<usize>,
    drag: Option<Drag>,
    buf: String,
}

impl ListView {
    pub fn new(columns: Vec<Column>) -> Self {
        Self {
            rect: Rect::default(),
            columns,
            selected: None,
            sort: None,
            lead_column: None,
            scroll_x: 0.0,
            scroll_y: 0.0,
            hover: None,
            drag: None,
            buf: String::new(),
        }
    }

    fn body(&self) -> Rect {
        Rect::new(self.rect.x, self.rect.y + HEADER_H, self.rect.w, (self.rect.h - HEADER_H).max(0.0))
    }

    fn content_w(&self) -> f32 {
        2.0 * ROW_INSET + self.columns.iter().filter(|c| c.visible).map(|c| c.width).sum::<f32>()
    }

    /// Height that shows `rows` rows without scrolling.
    pub fn height_for(rows: usize) -> f32 {
        HEADER_H + rows as f32 * ROW_H
    }

    fn max_scroll_y(&self, rows: usize) -> f32 {
        (rows as f32 * ROW_H - self.body().h).max(0.0)
    }

    fn clamp_scroll(&mut self, rows: usize) {
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll_y(rows));
        self.scroll_x = self.scroll_x.clamp(0.0, (self.content_w() - self.rect.w).max(0.0));
    }

    /// Visible columns as (index, left edge, width) in window coordinates.
    fn layout(&self) -> impl Iterator<Item = (usize, f32, f32)> + '_ {
        let mut x = self.rect.x + ROW_INSET - self.scroll_x;
        self.columns.iter().enumerate().filter(|(_, c)| c.visible).map(move |(i, c)| {
            let left = x;
            x += c.width;
            (i, left, c.width)
        })
    }

    fn row_at(&self, y: f32, rows: usize) -> Option<usize> {
        let body = self.body();
        if y < body.y || y >= body.bottom() {
            return None;
        }
        let row = ((y - body.y + self.scroll_y) / ROW_H) as usize;
        (row < rows).then_some(row)
    }

    /// Where `row` is drawn, if it is fully inside the visible area.
    pub fn row_rect(&self, row: usize) -> Option<Rect> {
        let body = self.body();
        let y = body.y + row as f32 * ROW_H - self.scroll_y;
        (y >= body.y && y + ROW_H <= body.bottom()).then(|| Rect::new(body.x, y, body.w, ROW_H))
    }

    /// Scrollbar thumb, if the content overflows.
    fn thumb(&self, rows: usize) -> Option<Rect> {
        let body = self.body();
        let total = rows as f32 * ROW_H;
        if total <= body.h || body.h <= 0.0 {
            return None;
        }
        let h = (body.h * body.h / total).max(24.0);
        let y = body.y + (body.h - h) * (self.scroll_y / (total - body.h));
        Some(Rect::new(body.right() - SCROLLBAR_W, y, SCROLLBAR_W, h))
    }

    pub fn ensure_visible(&mut self, row: usize) {
        let top = row as f32 * ROW_H;
        let h = self.body().h;
        if top < self.scroll_y {
            self.scroll_y = top;
        } else if top + ROW_H > self.scroll_y + h {
            self.scroll_y = top + ROW_H - h;
        }
    }

    pub fn paint(&mut self, c: &mut Canvas, m: &dyn ListModel) {
        let t = c.theme;
        let rows = m.rows();
        self.clamp_scroll(rows);
        let (r, body) = (self.rect, self.body());
        let cols: Vec<_> = self.layout().collect();
        let lead_col = self.lead_column.or(cols.first().map(|c| c.0));
        let tree = m.is_tree();

        c.clip(body);
        let first = (self.scroll_y / ROW_H) as usize;
        let last = rows.min(first + (body.h / ROW_H) as usize + 2);
        for row in first..last {
            let y = body.y + row as f32 * ROW_H - self.scroll_y;
            let highlight = Rect::new(r.x + ROW_INSET, y + 1.0, r.w - 2.0 * ROW_INSET, ROW_H - 2.0);
            if self.selected == Some(row) {
                c.fill_round_a(highlight, 12.0, t.milk_hi, 0.11);
            } else if self.hover == Some(row) {
                c.fill_round_a(highlight, 12.0, t.milk_hi, 0.05);
            }
            for &(ci, x, w) in &cols {
                if x + w < r.x || x > r.right() {
                    continue;
                }
                if let Some(on) = m.switch(row, ci) {
                    let track = Rect::new(x + CELL_PAD, y + (ROW_H - 20.0) / 2.0, 34.0, 20.0);
                    if on {
                        c.pill(track, t.milk);
                    } else {
                        c.fill_round_a(track, track.h / 2.0, t.milk_hi, 0.14);
                    }
                    let knob_x = if on { track.right() - 10.0 } else { track.x + 10.0 };
                    c.dot(knob_x, track.y + 10.0, 7.0, if on { t.ink } else { t.text_dim });
                    continue;
                }
                let align = self.columns[ci].align;
                let mut text_rect = Rect::new(x, y, w, ROW_H).pad_x(CELL_PAD);
                let color = m.text_color(row, ci, &t).unwrap_or(t.text);
                let font = if m.bold(row, ci) { Font::BODY_BOLD } else { Font::BODY };

                if lead_col == Some(ci) {
                    let mut lead = 0.0;
                    if tree {
                        let indent = m.depth(row) as f32 * INDENT;
                        if let Some(expanded) = m.expander(row) {
                            let glyph = if expanded { "\u{25BE}" } else { "\u{25B8}" };
                            let gr = Rect::new(text_rect.x + indent, y, INDENT, ROW_H);
                            c.text_font(glyph, gr, t.text_dim, Align::Left, Font::BODY);
                        }
                        lead += indent + INDENT;
                    }
                    if let Some(path) = m.icon(row) {
                        let ir = Rect::new(text_rect.x + lead, y + (ROW_H - ICON) / 2.0, ICON, ICON);
                        if ir.right() <= text_rect.right() && !c.icon(path, ir) {
                            c.fill_round(ir, 6.0, t.cocoa);
                        }
                        lead += ICON + 10.0;
                    }
                    let lead = lead.min(text_rect.w);
                    text_rect.x += lead;
                    text_rect.w -= lead;
                }

                if let Some(level) = m.bar(row, ci)
                    && text_rect.w >= BAR_TEXT_W + BAR_W + 7.0
                {
                    let track =
                        Rect::new(text_rect.right() - BAR_TEXT_W - 7.0 - BAR_W, y + ROW_H / 2.0 - 2.5, BAR_W, 5.0);
                    c.fill_round_a(track, track.h / 2.0, t.milk_hi, 0.10);
                    let level = level.clamp(0.0, 1.0);
                    if level > 0.0 {
                        c.pill(Rect { w: (BAR_W * level).max(5.0), ..track }, t.milk);
                    }
                }

                self.buf.clear();
                m.cell(row, ci, &mut self.buf);
                c.text_font(&self.buf, text_rect, color, align, font);

                if align == Align::Left {
                    let used = c.measure_font(&self.buf, font) + 10.0;
                    self.buf.clear();
                    m.secondary(row, ci, &mut self.buf);
                    if !self.buf.is_empty() && used < text_rect.w {
                        let rest = Rect { x: text_rect.x + used, w: text_rect.w - used, ..text_rect };
                        c.text_font(&self.buf, rest, t.text_dim, Align::Left, Font::BODY);
                    }
                }
            }
        }
        if let Some(thumb) = self.thumb(rows) {
            let grabbed = matches!(self.drag, Some(Drag::Thumb { .. }));
            let bar = Rect::new(thumb.x + 3.0, thumb.y + 2.0, thumb.w - 6.0, thumb.h - 4.0);
            c.fill_round_a(bar, bar.w / 2.0, t.milk_hi, if grabbed { 0.45 } else { 0.20 });
        }
        c.unclip();

        let header = Rect::new(r.x, r.y, r.w, HEADER_H);
        c.clip(header);
        for &(ci, x, w) in &cols {
            let col = &self.columns[ci];
            let text_rect = Rect::new(x, r.y, w, HEADER_H).pad_x(CELL_PAD);
            match self.sort.filter(|s| s.0 == ci) {
                Some((_, desc)) => {
                    self.buf.clear();
                    self.buf.push_str(col.title);
                    self.buf.push_str(if desc { " \u{25BE}" } else { " \u{25B4}" });
                    c.text_font(&self.buf, text_rect, t.text, col.align, Font::LABEL);
                }
                None => c.text_font(col.title, text_rect, t.text_dim, col.align, Font::LABEL),
            }
        }
        c.unclip();
    }

    /// Column whose right edge is under `x`, for resizing.
    fn grip_at(&self, x: f32) -> Option<usize> {
        self.layout().find(|&(_, left, w)| (x - (left + w)).abs() <= RESIZE_GRIP).map(|c| c.0)
    }

    fn select(&mut self, row: Option<usize>, resp: &mut Response) {
        if self.selected != row {
            self.selected = row;
            resp.action = Some(ListAction::Select(row));
        }
        if let Some(row) = row {
            self.ensure_visible(row);
        }
        resp.redraw = true;
    }

    pub fn event(&mut self, ev: &Event, m: &dyn ListModel, win: &Win) -> Response {
        let mut resp = Response::default();
        let rows = m.rows();
        let r = self.rect;
        let in_header = |x: f32, y: f32| r.contains(x, y) && y < r.y + HEADER_H;

        match *ev {
            Event::MouseMove { x, y } => match self.drag {
                Some(Drag::Column { col, start_x, start_w }) => {
                    self.columns[col].width = (start_w + x - start_x).max(MIN_COL_W);
                    resp.redraw = true;
                }
                Some(Drag::Thumb { grab_dy }) => {
                    if let Some(thumb) = self.thumb(rows) {
                        let body = self.body();
                        let track = (body.h - thumb.h).max(1.0);
                        let frac = ((y - grab_dy - body.y) / track).clamp(0.0, 1.0);
                        self.scroll_y = frac * self.max_scroll_y(rows);
                        resp.redraw = true;
                    }
                }
                None => {
                    // The owner resets the cursor to an arrow before handing
                    // the event to its lists, so only the grip needs saying.
                    if in_header(x, y) && self.grip_at(x).is_some() {
                        win.set_cursor(Cursor::ResizeH);
                    }
                    let hover = if r.contains(x, y) { self.row_at(y, rows) } else { None };
                    resp.redraw = std::mem::replace(&mut self.hover, hover) != hover;
                }
            },
            Event::MouseLeave => resp.redraw = self.hover.take().is_some(),
            Event::MouseUp { x, y, button } => {
                if button == MouseButton::Left && self.drag.take().is_some() {
                    resp.redraw = true;
                } else if button == MouseButton::Right && r.contains(x, y) {
                    resp.action = Some(if in_header(x, y) {
                        ListAction::HeaderContext
                    } else {
                        ListAction::Context { row: self.row_at(y, rows) }
                    });
                }
            }
            Event::MouseDown { x, y, button, clicks } if r.contains(x, y) => {
                if in_header(x, y) {
                    if button != MouseButton::Left {
                        return resp;
                    }
                    if let Some(col) = self.grip_at(x) {
                        self.drag = Some(Drag::Column { col, start_x: x, start_w: self.columns[col].width });
                    } else if let Some((col, _, _)) = self.layout().find(|&(_, left, w)| x >= left && x < left + w) {
                        resp.action = Some(ListAction::Sort(col));
                    }
                    return resp;
                }
                if button == MouseButton::Left && x >= r.right() - SCROLLBAR_W {
                    if let Some(thumb) = self.thumb(rows) {
                        // Clicking the track jumps the thumb's centre to the pointer.
                        let grab_dy = if thumb.contains(x, y) { y - thumb.y } else { thumb.h / 2.0 };
                        self.drag = Some(Drag::Thumb { grab_dy });
                        return self.event(&Event::MouseMove { x, y }, m, win);
                    }
                }
                let row = self.row_at(y, rows);
                if button == MouseButton::Left
                    && let Some(row) = row
                    && m.is_tree()
                    && m.expander(row).is_some()
                    && let Some((_, left, _)) = self.layout().next()
                {
                    let gutter = left + CELL_PAD + m.depth(row) as f32 * INDENT;
                    if x >= gutter - 2.0 && x < gutter + INDENT {
                        resp.action = Some(ListAction::Toggle(row));
                        resp.redraw = true;
                        return resp;
                    }
                }
                self.select(row, &mut resp);
                if button == MouseButton::Left
                    && let Some(row) = row
                    && let Some((col, _, _)) = self.layout().find(|&(_, left, w)| x >= left && x < left + w)
                    && m.switch(row, col).is_some()
                {
                    resp.action = Some(ListAction::Switch(row));
                    return resp;
                }
                if button == MouseButton::Left && clicks == 2 && let Some(row) = row {
                    resp.action = Some(ListAction::Activate(row));
                }
            }
            Event::Wheel { x, y, delta, shift } if r.contains(x, y) => {
                if shift {
                    self.scroll_x -= delta * 60.0;
                } else {
                    self.scroll_y -= delta * 3.0 * ROW_H;
                }
                self.clamp_scroll(rows);
                self.hover = self.row_at(y, rows);
                resp.redraw = true;
            }
            Event::Key { vk, .. } if rows > 0 => {
                let page = ((self.body().h / ROW_H) as usize).max(1);
                let cur = self.selected;
                let target = match vk {
                    key::UP => Some(cur.map_or(0, |s| s.saturating_sub(1))),
                    key::DOWN => Some(cur.map_or(0, |s| s + 1)),
                    key::PAGE_UP => Some(cur.map_or(0, |s| s.saturating_sub(page))),
                    key::PAGE_DOWN => Some(cur.map_or(0, |s| s + page)),
                    key::HOME => Some(0),
                    key::END => Some(rows - 1),
                    _ => None,
                };
                if let Some(target) = target {
                    self.select(Some(target.min(rows - 1)), &mut resp);
                } else if vk == key::RETURN && let Some(row) = cur {
                    resp.action = Some(ListAction::Activate(row));
                }
            }
            _ => {}
        }
        resp
    }
}
