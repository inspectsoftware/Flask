//! What Flask remembers between runs, kept in the registry under HKCU.

use sc_core::reg::{Hive, Key, View};
use sc_ui::Placement;

const KEY: &str = r"Software\SysCentral\Flask";
const PANE_VALUES: [&str; 2] = ["AppsColumns", "BackgroundColumns"];

/// Refresh intervals on offer: milliseconds and the label shown for them.
pub const INTERVALS: [(u32, &str); 4] = [(500, "Every 0.5 s"), (1000, "Every 1 s"), (2000, "Every 2 s"), (5000, "Every 5 s")];
const DEFAULT_INTERVAL: u32 = 1000;

pub struct Settings {
    pub window: Option<Placement>,
    pub tab: usize,
    pub refresh_ms: u32,
    pub tree: bool,
    /// Keep the window above all others.
    pub topmost: bool,
    /// Nothing eases or slides; everything is where it belongs at once.
    pub no_animations: bool,
    /// Column layout of the two process panes, in the form the Processes
    /// page writes and reads.
    pub panes: [String; 2],
    /// Which panel of the Processes page sits where; that page's own format.
    pub panels: String,
    /// How those panels share the page; that page's own format.
    pub split: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self { window: None, tab: 0, refresh_ms: DEFAULT_INTERVAL, tree: false, topmost: false, no_animations: false, panes: Default::default(), panels: String::new(), split: String::new() }
    }
}

fn window_text(p: Placement) -> String {
    let [l, t, r, b] = p.rect;
    format!("{l},{t},{r},{b},{}", p.maximized as u8)
}

/// Anything that is not five numbers describing a usable rectangle is ignored.
fn parse_window(text: &str) -> Option<Placement> {
    let n: Vec<i32> = text.split(',').map(|f| f.trim().parse().ok()).collect::<Option<_>>()?;
    let &[l, t, r, b, max] = n.as_slice() else { return None };
    (r.checked_sub(l)? >= 200 && b.checked_sub(t)? >= 200).then_some(Placement { rect: [l, t, r, b], maximized: max != 0 })
}

impl Settings {
    /// Missing or malformed values fall back to their defaults.
    pub fn load() -> Self {
        // Until the user says otherwise, do as Windows is set to.
        let mut s = Self { no_animations: !sc_ui::anim::system_enabled(), ..Default::default() };
        let Some(k) = Key::open(Hive::Hkcu, KEY, View::Native) else { return s };
        s.window = k.string("Window").as_deref().and_then(parse_window);
        s.tab = k.dword("Tab").unwrap_or(0) as usize;
        s.refresh_ms = k.dword("RefreshMs").filter(|ms| INTERVALS.iter().any(|i| i.0 == *ms)).unwrap_or(DEFAULT_INTERVAL);
        s.tree = k.dword("TreeView") == Some(1);
        s.topmost = k.dword("OnTop") == Some(1);
        s.no_animations = k.dword("NoAnimations").map_or(s.no_animations, |v| v == 1);
        s.panes = PANE_VALUES.map(|name| k.string(name).unwrap_or_default());
        s.panels = k.string("PanelOrder").unwrap_or_default();
        s.split = k.string("PanelSplit").unwrap_or_default();
        s
    }

    pub fn save(&self) {
        let Some(k) = Key::create(Hive::Hkcu, KEY, View::Native) else { return };
        if let Some(window) = self.window {
            k.set_string("Window", &window_text(window));
        }
        k.set_dword("Tab", self.tab as u32);
        k.set_dword("RefreshMs", self.refresh_ms);
        k.set_dword("TreeView", self.tree as u32);
        k.set_dword("OnTop", self.topmost as u32);
        k.set_dword("NoAnimations", self.no_animations as u32);
        k.set_string("PanelOrder", &self.panels);
        k.set_string("PanelSplit", &self.split);
        for (name, layout) in PANE_VALUES.iter().zip(&self.panes) {
            k.set_string(name, layout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_round_trips_and_rejects_nonsense() {
        let p = Placement { rect: [-1200, 40, -100, 740], maximized: true };
        assert_eq!(parse_window(&window_text(p)), Some(p));
        for bad in ["", "1,2,3,4", "0,0,100,100,0", "a,b,c,d,e", "0,0,2147483647,900,0,7", "-2147483648,0,2147483647,900,0"] {
            assert_eq!(parse_window(bad), None, "{bad}");
        }
    }
}
