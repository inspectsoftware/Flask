//! What is stopping the PC from sleeping or turning the screen off, read
//! from `powercfg /requests`. The system call behind it is undocumented and
//! its layout changes between Windows versions; the tool's output has not.

use std::os::windows::process::CommandExt;
use std::process::Command;

use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

/// One program, driver or service holding the PC awake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// What is held: `DISPLAY`, `SYSTEM`, `AWAYMODE`, `EXECUTION`, ...
    pub category: String,
    /// `PROCESS`, `DRIVER` or `SERVICE`.
    pub kind: String,
    /// For a process, its image as an NT device path.
    pub who: String,
    pub reason: String,
}

impl Request {
    /// File name of a requesting process, e.g. `chrome.exe`.
    pub fn process_name(&self) -> Option<&str> {
        (self.kind == "PROCESS").then(|| self.who.rsplit('\\').next().unwrap_or(&self.who))
    }
}

/// Parses the tool's output: a `CATEGORY:` heading, then for each requester
/// a `[KIND] who` line followed by its reason. A category with nothing in it
/// holds one line of localized "None." text, which is skipped.
pub fn parse(text: &str) -> Vec<Request> {
    let mut out: Vec<Request> = Vec::new();
    let mut category = "";
    // Whether reason lines belong to the last request pushed.
    let mut open = false;
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            open = false;
        } else if let Some(heading) = line.strip_suffix(':').filter(|h| h.chars().all(|c| c.is_ascii_uppercase())) {
            category = heading;
            open = false;
        } else if let Some((kind, who)) = line.strip_prefix('[').and_then(|l| l.split_once(']')) {
            out.push(Request {
                category: category.to_owned(),
                kind: kind.to_owned(),
                who: who.trim().to_owned(),
                reason: String::new(),
            });
            open = true;
        } else if open && let Some(last) = out.last_mut() {
            if !last.reason.is_empty() {
                last.reason.push(' ');
            }
            last.reason.push_str(line);
        }
    }
    out
}

/// Runs the tool. Needs administrator rights; without them the tool's own
/// refusal is the error text.
pub fn requests() -> Result<Vec<Request>, String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // Named by full path: an elevated process must not pick up some other
    // powercfg.exe from the search path.
    let mut dir = [0u16; 260];
    let len = unsafe { GetSystemDirectoryW(Some(&mut dir)) } as usize;
    let exe = format!(r"{}\powercfg.exe", String::from_utf16_lossy(&dir[..len.min(dir.len())]));
    let output =
        Command::new(exe).arg("/requests").creation_flags(CREATE_NO_WINDOW).output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        // The refusal for a caller without administrator rights goes to either stream.
        let errors = String::from_utf8_lossy(&output.stderr);
        let why = [text.trim(), errors.trim()].into_iter().find(|s| !s.is_empty());
        return Err(why.map_or_else(|| "it needs administrator rights".to_owned(), str::to_owned));
    }
    Ok(parse(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "DISPLAY:\r\n[PROCESS] \\Device\\HarddiskVolume3\\Program Files\\Browser\\browser.exe\r\nVideo Wake Lock\r\n\r\nSYSTEM:\r\n[DRIVER] High Definition Audio Device (HDAUDIO\\FUNC_01)\r\nAn audio stream is currently in use.\r\n[PROCESS] \\Device\\HarddiskVolume3\\Program Files\\Browser\\browser.exe\r\nPlaying audio\r\n\r\nAWAYMODE:\r\nNone.\r\n\r\nEXECUTION:\r\n[SERVICE] \\Device\\HarddiskVolume3\\Windows\\System32\\svchost.exe (UsoSvc)\r\nUniversal Orchestrator\r\n\r\nPERFBOOST:\r\nKeine.\r\n\r\nACTIVELOCKSCREEN:\r\nNone.\r\n";

    #[test]
    fn parses_requesters_and_skips_empty_categories() {
        let got = parse(SAMPLE);
        let brief: Vec<_> = got.iter().map(|r| (r.category.as_str(), r.kind.as_str(), r.reason.as_str())).collect();
        assert_eq!(
            brief,
            [
                ("DISPLAY", "PROCESS", "Video Wake Lock"),
                ("SYSTEM", "DRIVER", "An audio stream is currently in use."),
                ("SYSTEM", "PROCESS", "Playing audio"),
                ("EXECUTION", "SERVICE", "Universal Orchestrator"),
            ]
        );
        assert_eq!(got[0].process_name(), Some("browser.exe"));
        assert_eq!(got[1].process_name(), None);
        assert_eq!(got[1].who, "High Definition Audio Device (HDAUDIO\\FUNC_01)");
    }

    #[test]
    fn refusal_text_is_not_mistaken_for_requests() {
        assert!(parse("This command requires administrator privileges and must be executed from an elevated command prompt.").is_empty());
        assert!(parse("").is_empty());
    }
}
