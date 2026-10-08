//! Small Direct2D toolkit shared by SysCentral modules: one window, custom
//! drawn widgets, repaint only when something changed.

pub mod anim;
pub mod canvas;
pub mod list;
pub mod menu;
pub mod shell;
pub mod theme;
pub mod widgets;
pub mod window;

pub use anim::Anim;
pub use canvas::{Align, Canvas, Color, Font, Rect, Weight};
pub use list::{Column, ListAction, ListModel, ListView};
pub use menu::Menu;
pub use theme::Theme;
pub use widgets::{Button, ButtonKind, TabBar, TextInput, WindowAction, WindowButtons};
pub use window::{
    App, Cursor, Event, MouseButton, Placement, Win, WindowOptions, clipboard_text, run, single_instance,
};

/// Virtual-key codes used by the widgets and apps.
pub mod key {
    pub const BACK: u32 = 0x08;
    pub const TAB: u32 = 0x09;
    pub const RETURN: u32 = 0x0D;
    pub const ESCAPE: u32 = 0x1B;
    pub const PAGE_UP: u32 = 0x21;
    pub const PAGE_DOWN: u32 = 0x22;
    pub const END: u32 = 0x23;
    pub const HOME: u32 = 0x24;
    pub const UP: u32 = 0x26;
    pub const DOWN: u32 = 0x28;
    pub const DELETE: u32 = 0x2E;
    pub const N: u32 = 0x4E;
    pub const V: u32 = 0x56;
    pub const F5: u32 = 0x74;
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
