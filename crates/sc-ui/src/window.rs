use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use windows::Win32::Foundation::{
    COLORREF, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct2D::{D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1CreateFactory, ID2D1Factory};
use windows::Win32::Graphics::DirectWrite::{DWRITE_FACTORY_TYPE_SHARED, DWriteCreateFactory, IDWriteFactory};
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{InvalidateRect, ScreenToClient, ValidateRect};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, GetSystemMetricsForDpi,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_CONTROL, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCSTR, PCWSTR, Result, w};

use crate::{Canvas, Menu, Theme, wide};

const WM_MOUSELEAVE: u32 = 0x02A3;
const CF_UNICODETEXT: u32 = 13;
const DWMWA_USE_IMMERSIVE_DARK_MODE: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(20);
const DWMWA_BORDER_COLOR: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(34);
/// Height in DIPs of the strip along the top edge that resizes the window
/// when the app draws its own title bar.
const TOP_RESIZE: f32 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Arrow,
    ResizeH,
    ResizeV,
}

/// Input delivered to the app. Coordinates are in DIPs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    MouseMove { x: f32, y: f32 },
    MouseDown { x: f32, y: f32, button: MouseButton, clicks: u8 },
    MouseUp { x: f32, y: f32, button: MouseButton },
    MouseLeave,
    /// `delta` is in wheel notches, positive away from the user.
    Wheel { x: f32, y: f32, delta: f32, shift: bool },
    Key { vk: u32, ctrl: bool, shift: bool },
    Char(char),
    Resized,
    Minimized(bool),
    /// The window is about to close; the last chance to save state.
    Closing,
    /// Posted from another thread with [`Win::post`].
    User(usize),
}

pub trait App {
    /// Draws the whole window. `w` and `h` are the client size in DIPs.
    fn paint(&mut self, c: &mut Canvas, w: f32, h: f32);
    fn event(&mut self, ev: &Event, win: &Win);
    /// With `WindowOptions::custom_frame`: whether the point is empty title
    /// bar, which drags the window. Points over the app's own controls must
    /// answer false so they receive clicks.
    fn is_caption(&self, _x: f32, _y: f32) -> bool {
        false
    }
}

pub struct WindowOptions {
    pub title: &'static str,
    /// Also used to find the window of an already running instance.
    pub class: &'static str,
    pub size: (f32, f32),
    pub min_size: (f32, f32),
    /// Removes the system title bar; the app draws its own in the client area.
    pub custom_frame: bool,
    /// Where the window was last time; overrides `size`.
    pub placement: Option<Placement>,
}

/// Position of the restored window in screen pixels (left, top, right,
/// bottom), and whether it is maximized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub rect: [i32; 4],
    pub maximized: bool,
}

thread_local! {
    static CURSOR: Cell<Cursor> = const { Cell::new(Cursor::Arrow) };
    /// Smallest client size the user can drag the window to, in DIPs.
    static MIN_SIZE: Cell<(f32, f32)> = const { Cell::new((0.0, 0.0)) };
}

/// Handle to the window, given to the app with every event.
#[derive(Clone, Copy)]
pub struct Win {
    hwnd: HWND,
}

impl Win {
    /// Opaque value for [`Win::post`], safe to move to another thread.
    pub fn id(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// Queues `Event::User(code)` for the window `id` belongs to. Callable
    /// from any thread.
    pub fn post(id: isize, code: usize) {
        unsafe {
            let _ = PostMessageW(Some(HWND(id as *mut c_void)), WM_APP, WPARAM(code), LPARAM(0));
        }
    }

    pub fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    pub fn close(&self) {
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    pub fn minimize(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
        }
    }

    pub fn is_maximized(&self) -> bool {
        unsafe { IsZoomed(self.hwnd).as_bool() }
    }

    pub fn placement(&self) -> Placement {
        let mut wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        unsafe {
            let _ = GetWindowPlacement(self.hwnd, &mut wp);
        }
        let r = wp.rcNormalPosition;
        Placement {
            rect: [r.left, r.top, r.right, r.bottom],
            // A minimized window remembers whether it restores to maximized.
            maximized: self.is_maximized() || wp.flags.contains(WPF_RESTORETOMAXIMIZED),
        }
    }

    /// Moves and sizes the window; see [`Placement`].
    pub fn set_placement(&self, placement: Placement) {
        apply_placement(self.hwnd, placement);
    }

    /// Keeps the window above all others, or stops doing so.
    pub fn set_topmost(&self, on: bool) {
        let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// Changes how small the window may be made, in DIPs.
    pub fn set_min_size(&self, size: (f32, f32)) {
        MIN_SIZE.set(size);
    }

    /// Physical pixels per DIP on the monitor the window is on.
    pub fn scale(&self) -> f32 {
        scale(self.hwnd)
    }

    pub fn toggle_maximize(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, if self.is_maximized() { SW_RESTORE } else { SW_MAXIMIZE });
        }
    }

    pub fn set_cursor(&self, cursor: Cursor) {
        if CURSOR.replace(cursor) != cursor {
            apply_cursor(cursor);
        }
    }

    /// Shows `menu` at the pointer and returns the chosen item id.
    pub fn popup(&self, menu: &Menu) -> Option<u32> {
        unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.hwnd);
            let id = TrackPopupMenu(menu.handle, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, None, self.hwnd, None);
            (id.0 != 0).then_some(id.0 as u32)
        }
    }

    pub fn confirm(&self, title: &str, text: &str) -> bool {
        self.message(title, text, MB_OKCANCEL | MB_ICONWARNING | MB_DEFBUTTON2) == IDOK
    }

    pub fn error(&self, title: &str, text: &str) {
        self.message(title, text, MB_OK | MB_ICONERROR);
    }

    fn message(&self, title: &str, text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
        let (title, text) = (wide(title), wide(text));
        unsafe { MessageBoxW(Some(self.hwnd), PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), style) }
    }

    /// Shows the system Run dialog, the one Win+R opens. Whatever the user
    /// starts from it inherits this process's privileges. The shell32 export
    /// is undocumented (ordinal 61), so every step is allowed to fail.
    pub fn run_dialog(&self, title: &str, description: &str) {
        const RFF_CALCDIRECTORY: u32 = 4;
        let (title, description) = (wide(title), wide(description));
        unsafe {
            let Ok(shell32) = LoadLibraryW(w!("shell32.dll")) else { return };
            let Some(run_file_dlg) = GetProcAddress(shell32, PCSTR(61 as *const u8)) else { return };
            let run_file_dlg: unsafe extern "system" fn(HWND, *mut c_void, PCWSTR, PCWSTR, PCWSTR, u32) =
                std::mem::transmute(run_file_dlg);
            run_file_dlg(
                self.hwnd,
                std::ptr::null_mut(),
                PCWSTR::null(),
                PCWSTR(title.as_ptr()),
                PCWSTR(description.as_ptr()),
                RFF_CALCDIRECTORY,
            );
        }
    }

    pub fn copy_text(&self, text: &str) {
        let data = wide(text);
        unsafe {
            if OpenClipboard(Some(self.hwnd)).is_err() {
                return;
            }
            let _ = EmptyClipboard();
            if let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2) {
                let dst = GlobalLock(mem) as *mut u16;
                if !dst.is_null() {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
                    let _ = GlobalUnlock(mem);
                    // On success the clipboard owns the memory.
                    let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0)));
                }
            }
            let _ = CloseClipboard();
        }
    }
}

/// Text on the clipboard, if it holds any.
pub fn clipboard_text() -> Option<String> {
    unsafe {
        OpenClipboard(None).ok()?;
        let text = GetClipboardData(CF_UNICODETEXT).ok().and_then(|data| {
            let mem = windows::Win32::Foundation::HGLOBAL(data.0);
            let ptr = GlobalLock(mem) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let len = (0..).take_while(|&i| *ptr.add(i) != 0).count();
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(mem);
            Some(text)
        });
        let _ = CloseClipboard();
        text
    }
}

fn apply_placement(hwnd: HWND, Placement { rect: [left, top, right, bottom], maximized }: Placement) {
    // The system moves a placement that is off every screen back into view.
    let wp = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        showCmd: (if maximized { SW_SHOWMAXIMIZED } else { SW_SHOWNORMAL }).0 as u32,
        rcNormalPosition: RECT { left, top, right, bottom },
        ..Default::default()
    };
    unsafe {
        let _ = SetWindowPlacement(hwnd, &wp);
    }
}

fn apply_cursor(cursor: Cursor) {
    let id = match cursor {
        Cursor::Arrow => IDC_ARROW,
        Cursor::ResizeH => IDC_SIZEWE,
        Cursor::ResizeV => IDC_SIZENS,
    };
    unsafe {
        if let Ok(h) = LoadCursorW(None, id) {
            SetCursor(Some(h));
        }
    }
}

/// Returns true when this is the only instance. Otherwise brings the running
/// instance's window (found by `class`) to the front and returns false.
pub fn single_instance(class: &str) -> bool {
    let name = wide(&format!("Local\\{class}.SingleInstance"));
    unsafe {
        // Deliberately leaked: the mutex must live as long as the process.
        let _ = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return true;
        }
        let class = wide(class);
        if let Ok(hwnd) = FindWindowW(PCWSTR(class.as_ptr()), None) {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(hwnd);
        }
        false
    }
}

/// Makes the process's native menus dark. These uxtheme exports are
/// undocumented (ordinals 135 and 136, Windows 10 1903+), so every step is
/// allowed to fail.
fn force_dark_menus() {
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(set_mode) = GetProcAddress(uxtheme, PCSTR(135 as *const u8)) {
            let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
            const FORCE_DARK: i32 = 2;
            set_mode(FORCE_DARK);
        }
        if let Some(flush) = GetProcAddress(uxtheme, PCSTR(136 as *const u8)) {
            let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
            flush();
        }
    }
}

struct State<A> {
    // `None` until the app is constructed; borrowed for the duration of each
    // callback, so re-entrant messages (modal menus, message boxes) see it busy.
    app: RefCell<Option<A>>,
    canvas: RefCell<Option<Canvas>>,
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    theme: Theme,
    tracking_leave: Cell<bool>,
    /// A saved placement is being applied; its size is already right for
    /// the monitor it lands on.
    restoring: Cell<bool>,
    custom_frame: bool,
}

fn scale(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }
}

fn client_px(hwnd: HWND) -> (u32, u32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32)
}

fn style_frame(hwnd: HWND, theme: Theme) {
    let dark = BOOL::from(true);
    // COLORREF is 0x00BBGGRR.
    let c = theme.line.0;
    let border = COLORREF(((c & 0xFF) << 16) | (c & 0xFF00) | (c >> 16));
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const BOOL as *const c_void,
            size_of::<BOOL>() as u32,
        );
        // Windows 11 only; ignored elsewhere.
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const COLORREF as *const c_void,
            size_of::<COLORREF>() as u32,
        );
    }
}

impl<A: App> State<A> {
    fn dispatch(&self, hwnd: HWND, ev: Event) {
        if let Ok(mut app) = self.app.try_borrow_mut()
            && let Some(app) = app.as_mut()
        {
            app.event(&ev, &Win { hwnd });
        }
    }

    fn paint(&self, hwnd: HWND) {
        // Marks the window clean whatever happens below; anything that needs
        // another frame invalidates again.
        unsafe {
            let _ = ValidateRect(Some(hwnd), None);
        }
        let Ok(mut app) = self.app.try_borrow_mut() else { return };
        let Some(app) = app.as_mut() else { return };
        let mut slot = self.canvas.borrow_mut();
        if slot.is_none() {
            *slot = Canvas::new(&self.d2d, &self.dwrite, hwnd, client_px(hwnd), scale(hwnd) * 96.0, self.theme).ok();
        }
        let Some(canvas) = slot.as_mut() else { return };
        let lost = unsafe {
            canvas.rt.BeginDraw();
            canvas.clear(canvas.theme.ground);
            let size = canvas.rt.GetSize();
            crate::anim::begin_frame();
            app.paint(canvas, size.width, size.height);
            canvas.rt.EndDraw(None, None).is_err()
        };
        if lost {
            // Device lost: rebuild the render target on the next frame.
            *slot = None;
        }
        // Something is still moving: paint again. Presenting waits for the
        // display, which paces this.
        if lost || crate::anim::busy() {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }

    /// Hit test for a window without a system title bar.
    unsafe fn hit_test(&self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            // The side and bottom resize borders are still the system's.
            let hit = DefWindowProcW(hwnd, msg, wp, lp);
            if hit.0 as u32 != HTCLIENT {
                return hit;
            }
            let mut pt = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 };
            let _ = ScreenToClient(hwnd, &mut pt);
            let s = scale(hwnd);
            let (x, y) = (pt.x as f32 / s, pt.y as f32 / s);
            if !IsZoomed(hwnd).as_bool() && y < TOP_RESIZE {
                let w = client_px(hwnd).0 as f32 / s;
                let code = if x < 10.0 {
                    HTTOPLEFT
                } else if x >= w - 10.0 {
                    HTTOPRIGHT
                } else {
                    HTTOP
                };
                return LRESULT(code as isize);
            }
            let caption = match self.app.try_borrow() {
                Ok(app) => app.as_ref().is_some_and(|a| a.is_caption(x, y)),
                Err(_) => false,
            };
            LRESULT(if caption { HTCAPTION } else { HTCLIENT } as isize)
        }
    }
}

fn mouse_pos(hwnd: HWND, lp: LPARAM) -> (f32, f32) {
    let s = scale(hwnd);
    let x = (lp.0 & 0xFFFF) as i16 as f32;
    let y = ((lp.0 >> 16) & 0xFFFF) as i16 as f32;
    (x / s, y / s)
}

unsafe extern "system" fn wndproc<A: App>(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = lp.0 as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State<A>;
        if state.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let st = &*state;

        match msg {
            WM_PAINT => st.paint(hwnd),
            WM_ERASEBKGND => return LRESULT(1),
            WM_NCCALCSIZE if st.custom_frame && wp.0 != 0 => {
                // Let the system size the side and bottom borders, then give
                // the title bar's space back to the client area. A maximized
                // window hangs over the screen edge by the frame thickness.
                let params = lp.0 as *mut NCCALCSIZE_PARAMS;
                let top = (*params).rgrc[0].top;
                DefWindowProcW(hwnd, msg, wp, lp);
                let overhang = if IsZoomed(hwnd).as_bool() {
                    let dpi = GetDpiForWindow(hwnd);
                    GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
                } else {
                    0
                };
                (*params).rgrc[0].top = top + overhang;
                return LRESULT(0);
            }
            WM_NCHITTEST if st.custom_frame => return st.hit_test(hwnd, msg, wp, lp),
            WM_SIZE => {
                let minimized = wp.0 as u32 == SIZE_MINIMIZED;
                if !minimized {
                    if let Some(c) = st.canvas.borrow_mut().as_mut() {
                        c.resize(client_px(hwnd));
                    }
                    st.dispatch(hwnd, Event::Resized);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                st.dispatch(hwnd, Event::Minimized(minimized));
            }
            WM_DPICHANGED => {
                let rc = &*(lp.0 as *const RECT);
                if !st.restoring.get() {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        rc.left,
                        rc.top,
                        rc.right - rc.left,
                        rc.bottom - rc.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                if let Some(c) = st.canvas.borrow_mut().as_mut() {
                    c.set_dpi((wp.0 & 0xFFFF) as f32);
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lp.0 as *mut MINMAXINFO);
                let s = scale(hwnd);
                let min = MIN_SIZE.get();
                info.ptMinTrackSize = POINT { x: (min.0 * s) as i32, y: (min.1 * s) as i32 };
            }
            WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
                apply_cursor(CURSOR.get());
                return LRESULT(1);
            }
            WM_MOUSEMOVE => {
                if !st.tracking_leave.replace(true) {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                }
                let (x, y) = mouse_pos(hwnd, lp);
                st.dispatch(hwnd, Event::MouseMove { x, y });
            }
            WM_MOUSELEAVE => {
                st.tracking_leave.set(false);
                st.dispatch(hwnd, Event::MouseLeave);
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => {
                let (x, y) = mouse_pos(hwnd, lp);
                let left = matches!(msg, WM_LBUTTONDOWN | WM_LBUTTONDBLCLK);
                let clicks = if matches!(msg, WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK) { 2 } else { 1 };
                if left {
                    SetCapture(hwnd);
                }
                let button = if left { MouseButton::Left } else { MouseButton::Right };
                st.dispatch(hwnd, Event::MouseDown { x, y, button, clicks });
            }
            WM_LBUTTONUP | WM_RBUTTONUP => {
                let (x, y) = mouse_pos(hwnd, lp);
                let left = msg == WM_LBUTTONUP;
                if left {
                    let _ = ReleaseCapture();
                }
                let button = if left { MouseButton::Left } else { MouseButton::Right };
                st.dispatch(hwnd, Event::MouseUp { x, y, button });
            }
            WM_MOUSEWHEEL => {
                // Wheel coordinates are in screen space, unlike the other mouse messages.
                let mut pt = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 };
                let _ = ScreenToClient(hwnd, &mut pt);
                let s = scale(hwnd);
                let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as f32 / WHEEL_DELTA as f32;
                let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                st.dispatch(hwnd, Event::Wheel { x: pt.x as f32 / s, y: pt.y as f32 / s, delta, shift });
            }
            WM_KEYDOWN => {
                let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                st.dispatch(hwnd, Event::Key { vk: wp.0 as u32, ctrl, shift });
            }
            WM_CHAR => {
                if let Some(ch) = char::from_u32(wp.0 as u32).filter(|c| !c.is_control()) {
                    st.dispatch(hwnd, Event::Char(ch));
                }
            }
            WM_APP => st.dispatch(hwnd, Event::User(wp.0)),
            WM_CLOSE => {
                st.dispatch(hwnd, Event::Closing);
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            WM_DESTROY => PostQuitMessage(0),
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
        LRESULT(0)
    }
}

/// Creates the window, builds the app with `make` and runs the message loop
/// until the window is closed.
pub fn run<A: App + 'static>(opts: WindowOptions, make: impl FnOnce(&Win) -> A) -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        force_dark_menus();

        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = wide(opts.class);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc::<A>),
            hInstance: instance,
            hIcon: LoadIconW(Some(instance), PCWSTR(1 as *const u16)).unwrap_or_default(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let theme = Theme::COCOA;
        // Lives for the whole process; the window procedure borrows it.
        let state: &'static State<A> = Box::leak(Box::new(State {
            app: RefCell::new(None),
            canvas: RefCell::new(None),
            d2d: D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
            dwrite: DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?,
            theme,
            tracking_leave: Cell::new(false),
            restoring: Cell::new(false),
            custom_frame: opts.custom_frame,
        }));
        MIN_SIZE.set(opts.min_size);

        let title = wide(opts.title);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance),
            Some(state as *const State<A> as *const c_void),
        )?;
        style_frame(hwnd, theme);
        let s = scale(hwnd);
        // SWP_FRAMECHANGED makes the system ask for the client area again now
        // that the window knows whether it draws its own frame.
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            (opts.size.0 * s) as i32,
            (opts.size.1 * s) as i32,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );

        let app = make(&Win { hwnd });
        *state.app.borrow_mut() = Some(app);
        match opts.placement {
            Some(placement) => {
                // Without this, landing on a monitor with another scale
                // would rescale a size that was saved in that monitor's pixels.
                state.restoring.set(true);
                apply_placement(hwnd, placement);
                // Crossing to that monitor nudges the position; now that the
                // window is there, the second call lands exactly.
                apply_placement(hwnd, placement);
                state.restoring.set(false);
            }
            None => {
                let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            }
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}
