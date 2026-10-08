use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, HMENU, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
};
use windows::core::PCWSTR;

use crate::wide;

/// Native popup menu. Show it with [`crate::Win::popup`].
pub struct Menu {
    pub(crate) handle: HMENU,
}

impl Default for Menu {
    fn default() -> Self {
        Self::new()
    }
}

impl Menu {
    pub fn new() -> Self {
        Self { handle: unsafe { CreatePopupMenu() }.unwrap_or_default() }
    }

    /// Adds an item. `id` must be non-zero; it is what `popup` returns.
    pub fn item(&mut self, id: u32, label: &str) -> &mut Self {
        self.item_with(id, label, false, true)
    }

    pub fn item_with(&mut self, id: u32, label: &str, checked: bool, enabled: bool) -> &mut Self {
        let mut flags = MF_STRING;
        if checked {
            flags |= MF_CHECKED;
        }
        if !enabled {
            flags |= MF_GRAYED;
        }
        let label = wide(label);
        unsafe {
            let _ = AppendMenuW(self.handle, flags, id as usize, PCWSTR(label.as_ptr()));
        }
        self
    }

    pub fn separator(&mut self) -> &mut Self {
        unsafe {
            let _ = AppendMenuW(self.handle, MF_SEPARATOR, 0, PCWSTR::null());
        }
        self
    }

    pub fn submenu(&mut self, label: &str, sub: Menu) -> &mut Self {
        let label = wide(label);
        unsafe {
            let _ = AppendMenuW(self.handle, MF_POPUP, sub.handle.0 as usize, PCWSTR(label.as_ptr()));
        }
        // The parent now owns the submenu and destroys it.
        std::mem::forget(sub);
        self
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.handle);
        }
    }
}
