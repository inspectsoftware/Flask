use crate::{Align, Anim, Canvas, Event, Font, MouseButton, Rect, key};

/// Pill-shaped tab switcher.
pub struct TabBar {
    /// Left edge and vertical band to draw in; the width is computed.
    pub rect: Rect,
    pub labels: Vec<&'static str>,
    pub active: usize,
    hover: Option<usize>,
    // Each tab's rectangle as last painted, for hit testing.
    tabs: Vec<Rect>,
}

const TAB_PAD: f32 = 16.0;
const WELL_PAD: f32 = 4.0;

impl TabBar {
    pub fn new(labels: Vec<&'static str>) -> Self {
        Self { rect: Rect::default(), labels, active: 0, hover: None, tabs: Vec::new() }
    }

    /// Area the switcher occupied when last painted.
    pub fn bounds(&self) -> Rect {
        match (self.tabs.first(), self.tabs.last()) {
            (Some(a), Some(b)) => Rect::new(a.x - WELL_PAD, a.y - WELL_PAD, b.right() - a.x + 2.0 * WELL_PAD, a.h + 2.0 * WELL_PAD),
            _ => Rect::default(),
        }
    }

    pub fn paint(&mut self, c: &mut Canvas) {
        let t = c.theme;
        let h = 28.0;
        let y = self.rect.y + (self.rect.h - h) / 2.0;
        let mut x = self.rect.x + WELL_PAD;
        self.tabs.clear();
        for label in &self.labels {
            let w = c.measure_font(label, Font::BODY_BOLD) + 2.0 * TAB_PAD;
            self.tabs.push(Rect::new(x, y, w, h));
            x += w + 4.0;
        }
        c.pill(self.bounds(), t.well);
        for (i, (r, label)) in self.tabs.iter().zip(&self.labels).enumerate() {
            if i == self.active {
                c.button_body(*r, t.milk_hi, t.milk);
                c.text_font(label, Rect { h: r.h - 2.0, ..*r }, t.ink, Align::Center, Font::BODY_BOLD);
            } else {
                if self.hover == Some(i) {
                    c.pill(*r, t.card);
                }
                c.text_font(label, *r, t.text_dim, Align::Center, Font::BODY_BOLD);
            }
        }
    }

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.tabs.iter().position(|r| r.contains(x, y))
    }

    /// Returns (needs redraw, newly activated tab).
    pub fn event(&mut self, ev: &Event) -> (bool, Option<usize>) {
        match *ev {
            Event::MouseMove { x, y } => {
                let h = self.hit(x, y);
                (std::mem::replace(&mut self.hover, h) != h, None)
            }
            Event::MouseLeave => (self.hover.take().is_some(), None),
            Event::MouseDown { x, y, button: MouseButton::Left, .. } => match self.hit(x, y) {
                Some(i) if i != self.active => {
                    self.active = i;
                    (true, Some(i))
                }
                _ => (false, None),
            },
            _ => (false, None),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Primary: milk-white.
    Milk,
    /// Secondary: dark cocoa.
    Cocoa,
    /// Destructive.
    Peach,
}

/// Pill button with a pressable lip.
pub struct Button {
    pub rect: Rect,
    pub label: &'static str,
    pub kind: ButtonKind,
    /// A toggled-on Cocoa button is drawn as Milk.
    pub active: bool,
    pub small: bool,
    hover: bool,
    /// How far the hover highlight has faded in, 0..=1.
    glow: Anim,
}

impl Button {
    pub fn new(label: &'static str, kind: ButtonKind) -> Self {
        Self { rect: Rect::default(), label, kind, active: false, small: false, hover: false, glow: Anim::default() }
    }

    pub fn small(label: &'static str, kind: ButtonKind) -> Self {
        Self { small: true, ..Self::new(label, kind) }
    }

    fn font(&self) -> Font {
        if self.small { Font::LABEL } else { Font::BODY_BOLD }
    }

    pub fn width(&self, c: &mut Canvas) -> f32 {
        c.measure_font(self.label, self.font()) + if self.small { 24.0 } else { 32.0 }
    }

    pub fn paint(&self, c: &mut Canvas) {
        let t = c.theme;
        let kind = if self.active { ButtonKind::Milk } else { self.kind };
        let glow = self.glow.get(self.hover as u8 as f32, 0.12);
        let (top, bottom, ink) = match kind {
            ButtonKind::Milk => (t.milk_hi, t.milk, t.ink),
            // Secondary buttons are glass, like the panels they sit on.
            ButtonKind::Cocoa => {
                let radius = self.rect.h / 2.0;
                c.fill_round_a(self.rect, radius, t.milk_hi, 0.07 + 0.07 * glow);
                c.stroke_round_a(self.rect, radius, t.milk_hi, 0.12);
                c.text_font(self.label, self.rect, t.text, Align::Center, self.font());
                return;
            }
            ButtonKind::Peach => (t.peach, t.peach, t.peach_ink),
        };
        // Hover lifts the face slightly towards white.
        let lift = 0.10 * glow;
        c.button_body(self.rect, top.mix(t.milk_hi, lift), bottom.mix(t.milk_hi, lift));
        c.text_font(self.label, Rect { h: self.rect.h - 2.0, ..self.rect }, ink, Align::Center, self.font());
    }

    /// Returns (needs redraw, clicked).
    pub fn event(&mut self, ev: &Event) -> (bool, bool) {
        match *ev {
            Event::MouseMove { x, y } => {
                let h = self.rect.contains(x, y);
                (std::mem::replace(&mut self.hover, h) != h, false)
            }
            Event::MouseLeave => (std::mem::replace(&mut self.hover, false), false),
            Event::MouseDown { x, y, button: MouseButton::Left, .. } if self.rect.contains(x, y) => (true, true),
            _ => (false, false),
        }
    }
}

/// Single-line search field that edits at the end only: enough for a filter
/// box that receives every typed character.
pub struct TextInput {
    pub rect: Rect,
    pub text: String,
    pub placeholder: &'static str,
}

impl TextInput {
    pub fn new(placeholder: &'static str) -> Self {
        Self { rect: Rect::default(), text: String::new(), placeholder }
    }

    pub fn paint(&self, c: &mut Canvas) {
        let t = c.theme;
        let r = self.rect;
        c.fill_round_a(r, r.h / 2.0, t.milk_hi, 0.06);
        c.stroke_round_a(r, r.h / 2.0, t.milk_hi, if self.text.is_empty() { 0.10 } else { 0.34 });
        // Magnifier.
        let (gx, gy) = (r.x + 19.0, r.y + r.h / 2.0 - 1.0);
        c.ring(gx, gy, 4.5, 1.6, t.text_dim);
        c.line(gx + 3.5, gy + 3.5, gx + 6.5, gy + 6.5, 1.6, t.text_dim);

        let inner = Rect::new(r.x + 34.0, r.y, (r.w - 48.0).max(0.0), r.h);
        if self.text.is_empty() {
            c.text(self.placeholder, inner, t.text_dim, Align::Left, false);
        } else {
            c.text(&self.text, inner, t.text, Align::Left, false);
            let caret_x = (inner.x + c.measure(&self.text, false) + 1.0).min(inner.right());
            c.fill(Rect::new(caret_x, r.y + 9.0, 1.0, r.h - 18.0), t.text);
        }
    }

    /// Returns true when the text changed.
    pub fn event(&mut self, ev: &Event) -> bool {
        match *ev {
            Event::Char(ch) => {
                self.text.push(ch);
                true
            }
            Event::Key { vk: key::BACK, ctrl, .. } if !self.text.is_empty() => {
                if ctrl {
                    // Delete the last word.
                    let trimmed = self.text.trim_end();
                    let cut = trimmed.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                    self.text.truncate(cut);
                } else {
                    self.text.pop();
                }
                true
            }
            Event::Key { vk: key::V, ctrl: true, .. } => {
                // One line only, and without the quotes Explorer's "Copy as path" adds.
                let pasted = crate::clipboard_text().unwrap_or_default();
                let line = pasted.lines().next().unwrap_or_default().trim().trim_matches('"');
                self.text.push_str(line);
                !line.is_empty()
            }
            Event::Key { vk: key::ESCAPE, .. } if !self.text.is_empty() => {
                self.text.clear();
                true
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowAction {
    Minimize,
    ToggleMaximize,
    Close,
}

/// Minimize / maximize / close, for windows that draw their own title bar.
pub struct WindowButtons {
    /// Right-aligned band the three buttons fill.
    pub rect: Rect,
    hover: Option<usize>,
}

const WINDOW_BUTTON_W: f32 = 46.0;
const WINDOW_ACTIONS: [WindowAction; 3] = [WindowAction::Minimize, WindowAction::ToggleMaximize, WindowAction::Close];

impl Default for WindowButtons {
    fn default() -> Self {
        Self { rect: Rect::default(), hover: None }
    }
}

impl WindowButtons {
    pub const WIDTH: f32 = WINDOW_BUTTON_W * 3.0;

    fn button(&self, i: usize) -> Rect {
        Rect::new(self.rect.x + i as f32 * WINDOW_BUTTON_W, self.rect.y, WINDOW_BUTTON_W, self.rect.h)
    }

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        (0..3).find(|&i| self.button(i).contains(x, y))
    }

    pub fn paint(&self, c: &mut Canvas, maximized: bool) {
        let t = c.theme;
        for (i, action) in WINDOW_ACTIONS.into_iter().enumerate() {
            let r = self.button(i);
            let hot = self.hover == Some(i);
            let closing = hot && action == WindowAction::Close;
            if hot {
                let pad = Rect::new(r.x + 3.0, r.y + 6.0, r.w - 6.0, r.h - 12.0);
                if closing {
                    c.fill_round(pad, 10.0, t.peach);
                } else {
                    c.fill_round_a(pad, 10.0, t.milk_hi, 0.10);
                }
            }
            let ink = if closing { t.peach_ink } else if hot { t.text } else { t.text_dim };
            let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
            match action {
                WindowAction::Minimize => c.line(cx - 5.0, cy, cx + 5.0, cy, 1.3, ink),
                WindowAction::ToggleMaximize => {
                    let d = if maximized { 3.5 } else { 4.5 };
                    c.stroke_round(Rect::new(cx - d - 0.5, cy - d - 0.5, 2.0 * d + 1.0, 2.0 * d + 1.0), 2.0, ink);
                }
                WindowAction::Close => {
                    c.line(cx - 4.5, cy - 4.5, cx + 4.5, cy + 4.5, 1.3, ink);
                    c.line(cx + 4.5, cy - 4.5, cx - 4.5, cy + 4.5, 1.3, ink);
                }
            }
        }
    }

    /// Returns (needs redraw, action clicked).
    pub fn event(&mut self, ev: &Event) -> (bool, Option<WindowAction>) {
        match *ev {
            Event::MouseMove { x, y } => {
                let h = self.hit(x, y);
                (std::mem::replace(&mut self.hover, h) != h, None)
            }
            Event::MouseLeave => (self.hover.take().is_some(), None),
            // Acting on release lets the user slide off a button to cancel.
            Event::MouseUp { x, y, button: MouseButton::Left } => (false, self.hit(x, y).map(|i| WINDOW_ACTIONS[i])),
            _ => (false, None),
        }
    }
}
