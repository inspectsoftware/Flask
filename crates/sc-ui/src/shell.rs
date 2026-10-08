//! Explorer integration.

use windows::Win32::UI::Shell::{SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use crate::wide;

/// Opens Explorer with `path` selected.
pub fn reveal_in_explorer(path: &str) {
    let args = wide(&format!("/select,\"{path}\""));
    unsafe {
        ShellExecuteW(None, w!("open"), w!("explorer.exe"), PCWSTR(args.as_ptr()), None, SW_SHOWNORMAL);
    }
}

/// Starts `program` with `args` through the shell.
pub fn run(program: &str, args: &str) {
    let (program, args) = (wide(program), wide(args));
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(program.as_ptr()), PCWSTR(args.as_ptr()), None, SW_SHOWNORMAL);
    }
}

/// Opens a web address in the default browser. Going through Explorer hands
/// the launch to the desktop shell, so the browser does not inherit this
/// process's elevation.
pub fn open_url(url: &str) {
    run("explorer.exe", &format!("\"{url}\""));
}

/// Shows the Explorer properties sheet for `path`.
pub fn show_properties(path: &str) {
    let path = wide(path);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST,
        lpVerb: w!("properties"),
        lpFile: PCWSTR(path.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        let _ = ShellExecuteExW(&mut info);
    }
}
