//! Everything that can make code start without the user asking: logon
//! entries, scheduled tasks, services, drivers, shell add-ons, image hijacks
//! and the system DLL hooks. Modelled on Sysinternals Autoruns.

use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

use crate::origin::{command_exe, decode_task_file, expand_env, shortcut_target, task_commands};
use crate::procinfo;
use crate::reg::{Hive, Key, Value, View};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Subkey Autoruns (and Flask) park disabled entries in.
const DISABLED: &str = "AutorunsDisabled";
const CURRENT_VERSION: &str = r"Software\Microsoft\Windows\CurrentVersion";
const NT_CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
const CONTROL: &str = r"SYSTEM\CurrentControlSet\Control";
const SERVICES: &str = r"SYSTEM\CurrentControlSet\Services";
/// Where Flask remembers a service's start type before disabling it.
const SAVED_SERVICE_STARTS: &str = r"SOFTWARE\SysCentral\Flask\DisabledServices";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Category {
    Logon,
    Tasks,
    Services,
    Drivers,
    Explorer,
    Hijacks,
    Dlls,
    Boot,
    Winlogon,
    Providers,
    Print,
    Codecs,
    Packaged,
}

impl Category {
    pub const ALL: [Category; 13] = [
        Category::Logon,
        Category::Tasks,
        Category::Services,
        Category::Drivers,
        Category::Explorer,
        Category::Hijacks,
        Category::Dlls,
        Category::Boot,
        Category::Winlogon,
        Category::Providers,
        Category::Print,
        Category::Codecs,
        Category::Packaged,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Logon => "Logon",
            Category::Tasks => "Scheduled tasks",
            Category::Services => "Services",
            Category::Drivers => "Drivers",
            Category::Explorer => "Explorer add-ons",
            Category::Hijacks => "Image hijacks",
            Category::Dlls => "Injected DLLs",
            Category::Boot => "Boot execute",
            Category::Winlogon => "Winlogon",
            Category::Providers => "Security providers",
            Category::Print => "Print monitors",
            Category::Codecs => "Codecs",
            Category::Packaged => "Packaged apps",
        }
    }
}

/// Where an entry lives, which decides how it is switched off or removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A Run value. Windows itself tracks on/off for these in StartupApproved,
    /// which keeps Settings and Task Manager in agreement with Flask.
    Approved { hive: Hive, key: String, view: View, value: String },
    /// A file in a Startup folder, also governed by StartupApproved.
    StartupFile { path: String, hive: Hive },
    /// Any other registry value; disabled by moving it to `AutorunsDisabled`.
    RegValue { hive: Hive, key: String, view: View, value: String },
    /// A whole key under `parent`; disabled by moving it to `parent\AutorunsDisabled`.
    RegKey { hive: Hive, parent: String, view: View, name: String },
    Task { name: String, file: String },
    Service { name: String },
    PackagedTask { key: String },
    /// Shown for inspection only: disabling these can stop Windows booting.
    Locked { hive: Hive, key: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub category: Category,
    pub name: String,
    /// The launch string exactly as stored.
    pub command: String,
    /// The file that would run, resolved to a full path when possible.
    pub image: String,
    /// Human-readable place the entry was found.
    pub location: String,
    pub enabled: bool,
    pub source: Source,
    /// Company from the image's version resource (unverified).
    pub company: String,
}

impl Entry {
    pub fn can_toggle(&self) -> bool {
        !matches!(self.source, Source::Locked { .. })
    }

    pub fn can_delete(&self) -> bool {
        !matches!(self.source, Source::Locked { .. } | Source::PackagedTask { .. })
    }

    /// Registry path for regedit's address bar, when the entry lives there.
    pub fn registry_path(&self) -> Option<String> {
        let (hive, key) = match &self.source {
            Source::Approved { hive, key, .. } | Source::RegValue { hive, key, .. } | Source::Locked { hive, key } => {
                (*hive, key.clone())
            }
            Source::RegKey { hive, parent, name, .. } => (*hive, format!(r"{parent}\{name}")),
            Source::Service { name } => (Hive::Hklm, format!(r"{SERVICES}\{name}")),
            Source::PackagedTask { key } => (Hive::Hkcu, key.clone()),
            Source::StartupFile { .. } | Source::Task { .. } => return None,
        };
        Some(format!(r"Computer\{}\{key}", hive.long()))
    }
}

// ---------------------------------------------------------------------------
// Helpers

/// Turns a launch string into the file it runs. Handles the kernel-style
/// prefixes services use and bare names that live in System32.
pub fn resolve_image(command: &str) -> String {
    let mut exe = expand_env(&command_exe(command));
    let lower = exe.to_ascii_lowercase();
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    if let Some(rest) = lower.strip_prefix(r"\systemroot\") {
        exe = format!(r"{system_root}\{}", &exe[exe.len() - rest.len()..]);
    } else if lower.starts_with(r"\??\") {
        exe = exe[4..].to_owned();
    } else if lower.starts_with(r"system32\") || lower.starts_with(r"syswow64\") {
        exe = format!(r"{system_root}\{exe}");
    } else if !exe.is_empty() && !exe.contains('\\') && !exe.contains('/') {
        // A bare name: Windows would search System32 first.
        let with_ext = if Path::new(&exe).extension().is_some() { exe.clone() } else { format!("{exe}.exe") };
        let candidate = format!(r"{system_root}\System32\{with_ext}");
        if Path::new(&candidate).exists() {
            exe = candidate;
        }
    }
    exe
}

/// StartupApproved stores a small binary blob per entry; an odd first byte
/// means the user switched it off.
pub fn approved_blob_disabled(blob: &[u8]) -> bool {
    blob.first().is_some_and(|b| b & 1 == 1)
}

fn approved_key(view: View, folder: bool) -> String {
    let leaf = match (folder, view) {
        (true, _) => "StartupFolder",
        (false, View::Native) => "Run",
        (false, View::Wow32) => "Run32",
    };
    format!(r"{CURRENT_VERSION}\Explorer\StartupApproved\{leaf}")
}

fn approved_enabled(hive: Hive, view: View, folder: bool, name: &str) -> bool {
    Key::open(hive, &approved_key(view, folder), View::Native)
        .and_then(|k| k.raw(name))
        .is_none_or(|(_, blob)| !approved_blob_disabled(&blob))
}

fn set_approved(hive: Hive, view: View, folder: bool, name: &str, enabled: bool) -> Result<(), String> {
    let key = Key::create(hive, &approved_key(view, folder), View::Native).ok_or("Could not open StartupApproved")?;
    let mut blob = [0u8; 12];
    blob[0] = if enabled { 2 } else { 3 };
    const REG_BINARY: u32 = 3;
    key.set_raw(name, REG_BINARY, &blob).then_some(()).ok_or_else(|| "Could not write StartupApproved".into())
}

/// Moves a value between `key` and `key\AutorunsDisabled`.
pub fn toggle_reg_value(hive: Hive, key: &str, view: View, value: &str, enable: bool) -> Result<(), String> {
    let disabled_path = format!(r"{key}\{DISABLED}");
    let (from, to) = if enable { (disabled_path.as_str(), key) } else { (key, disabled_path.as_str()) };
    let src = Key::open_write(hive, from, view).ok_or("Could not open the key for writing")?;
    let (kind, data) = src.raw(value).ok_or("The value is no longer there")?;
    let dst = Key::create(hive, to, view).ok_or("Could not create the destination key")?;
    if !dst.set_raw(value, kind, &data) {
        return Err("Could not write the value".into());
    }
    src.delete_value(value).then_some(()).ok_or_else(|| "Copied the value but could not remove the original".into())
}

/// Moves a subkey between `parent` and `parent\AutorunsDisabled`.
pub fn toggle_reg_key(hive: Hive, parent: &str, view: View, name: &str, enable: bool) -> Result<(), String> {
    let disabled_path = format!(r"{parent}\{DISABLED}");
    let (from, to) = if enable { (disabled_path.as_str(), parent) } else { (parent, disabled_path.as_str()) };
    let src = Key::open_write(hive, from, view).ok_or("Could not open the key for writing")?;
    let dst = Key::create(hive, to, view).ok_or("Could not create the destination key")?;
    if !src.copy_tree_to(name, &dst) {
        return Err("Could not copy the key".into());
    }
    src.delete_tree(name).then_some(()).ok_or_else(|| "Copied the key but could not remove the original".into())
}

fn run_tool(program: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Could not run {program}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr });
    Err(text.lines().find(|l| !l.trim().is_empty()).unwrap_or("The command failed").trim().to_owned())
}

/// Whether the `<Settings>` block of a task definition leaves it enabled.
pub fn task_enabled(xml: &str) -> bool {
    let settings = xml.find("<Settings>").and_then(|a| xml[a..].find("</Settings>").map(|b| &xml[a..a + b]));
    !settings.is_some_and(|s| s.contains("<Enabled>false</Enabled>"))
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ServiceKind {
    Driver,
    Service,
    Other,
}

/// Classifies a service registry `Type` value.
pub fn service_kind(kind: u32) -> ServiceKind {
    const KERNEL_OR_FS_DRIVER: u32 = 0x1 | 0x2 | 0x8;
    const WIN32: u32 = 0x10 | 0x20;
    if kind & WIN32 != 0 {
        ServiceKind::Service
    } else if kind & KERNEL_OR_FS_DRIVER != 0 {
        ServiceKind::Driver
    } else {
        ServiceKind::Other
    }
}

struct Scan {
    out: Vec<Entry>,
    companies: HashMap<String, String>,
}

impl Scan {
    fn push(&mut self, category: Category, name: &str, command: &str, image: Option<String>, location: String, enabled: bool, source: Source) {
        let image = image.unwrap_or_else(|| resolve_image(command));
        let company = match self.companies.get(&image) {
            Some(c) => c.clone(),
            None => {
                let c = procinfo::company(&image).unwrap_or_default();
                self.companies.insert(image.clone(), c.clone());
                c
            }
        };
        self.out.push(Entry {
            category,
            name: name.to_owned(),
            command: command.to_owned(),
            image,
            location,
            enabled,
            source,
            company,
        });
    }

    /// Values of `key` as entries, plus the ones parked in `AutorunsDisabled`.
    fn reg_values(&mut self, category: Category, hive: Hive, key: &str, view: View) {
        let wow = if view == View::Wow32 { " (32-bit)" } else { "" };
        for (path, enabled) in [(key.to_owned(), true), (format!(r"{key}\{DISABLED}"), false)] {
            let Some(k) = Key::open(hive, &path, view) else { continue };
            for (name, value) in k.values() {
                let Some(command) = value.text().filter(|c| !c.is_empty()) else { continue };
                let source = Source::RegValue { hive, key: key.to_owned(), view, value: name.clone() };
                self.push(category, &name, &command, None, format!(r"{}\{key}{wow}", hive.short()), enabled, source);
            }
        }
    }

    fn logon(&mut self) {
        let views = [(Hive::Hkcu, View::Native), (Hive::Hklm, View::Native), (Hive::Hklm, View::Wow32)];
        for (hive, view) in views {
            let wow = if view == View::Wow32 { " (32-bit)" } else { "" };
            let run = format!(r"{CURRENT_VERSION}\Run");
            if let Some(k) = Key::open(hive, &run, view) {
                for (name, value) in k.values() {
                    let Some(command) = value.text().filter(|c| !c.is_empty()) else { continue };
                    let enabled = approved_enabled(hive, view, false, &name);
                    let source = Source::Approved { hive, key: run.clone(), view, value: name.clone() };
                    self.push(Category::Logon, &name, &command, None, format!(r"{}\{run}{wow}", hive.short()), enabled, source);
                }
            }
            for leaf in ["RunOnce", r"Policies\Explorer\Run"] {
                self.reg_values(Category::Logon, hive, &format!(r"{CURRENT_VERSION}\{leaf}"), view);
            }
        }

        let startup = r"Microsoft\Windows\Start Menu\Programs\Startup";
        let folders = [("APPDATA", Hive::Hkcu, "Startup folder (you)"), ("ProgramData", Hive::Hklm, "Startup folder (all users)")];
        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() };
        for (var, hive, label) in folders {
            let Ok(base) = std::env::var(var) else { continue };
            let Ok(entries) = std::fs::read_dir(Path::new(&base).join(startup)) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let file = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                // Subfolders (such as the one Autoruns parks disabled items in) do not run.
                if path.is_dir() || file.eq_ignore_ascii_case("desktop.ini") {
                    continue;
                }
                let is_link = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk"));
                let target = if is_link { unsafe { shortcut_target(&path) } } else { None };
                let image = target.unwrap_or_else(|| path.display().to_string());
                let enabled = approved_enabled(hive, View::Native, true, &file);
                let source = Source::StartupFile { path: path.display().to_string(), hive };
                self.push(Category::Logon, &file, &image.clone(), Some(image), label.to_owned(), enabled, source);
            }
        }
        if com {
            unsafe { CoUninitialize() };
        }

        // Active Setup runs each component's StubPath once per user at logon.
        for view in [View::Native, View::Wow32] {
            let parent = r"SOFTWARE\Microsoft\Active Setup\Installed Components";
            for (path, enabled) in [(parent.to_owned(), true), (format!(r"{parent}\{DISABLED}"), false)] {
                let Some(root) = Key::open(Hive::Hklm, &path, view) else { continue };
                for sub in root.subkeys().into_iter().filter(|s| s != DISABLED) {
                    let Some(k) = Key::open(Hive::Hklm, &format!(r"{path}\{sub}"), view) else { continue };
                    let Some(stub) = k.string("StubPath").filter(|s| !s.is_empty()) else { continue };
                    let name = k.string("").filter(|s| !s.is_empty()).unwrap_or_else(|| sub.clone());
                    let source = Source::RegKey { hive: Hive::Hklm, parent: parent.to_owned(), view, name: sub };
                    self.push(Category::Logon, &name, &stub, None, "Active Setup".into(), enabled, source);
                }
            }
        }
    }

    fn tasks(&mut self) {
        let Ok(system) = std::env::var("SystemRoot") else { return };
        let root = Path::new(&system).join(r"System32\Tasks");
        let mut stack: Vec<PathBuf> = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else { continue };
                let xml = decode_task_file(&bytes);
                let name = format!(r"\{}", path.strip_prefix(&root).unwrap_or(&path).display());
                let command = match task_commands(&xml).into_iter().next() {
                    Some(c) => c,
                    None if xml.contains("<ComHandler>") => "COM handler".to_owned(),
                    None => continue,
                };
                let image = if command == "COM handler" { String::new() } else { resolve_image(&command) };
                let source = Source::Task { name: name.clone(), file: path.display().to_string() };
                self.push(Category::Tasks, &name, &command, Some(image), "Task Scheduler".into(), task_enabled(&xml), source);
            }
        }
    }

    fn services(&mut self) {
        let Some(root) = Key::open(Hive::Hklm, SERVICES, View::Native) else { return };
        let saved = Key::open(Hive::Hklm, SAVED_SERVICE_STARTS, View::Native);
        for name in root.subkeys() {
            let Some(k) = Key::open(Hive::Hklm, &format!(r"{SERVICES}\{name}"), View::Native) else { continue };
            let (Some(kind), Some(start)) = (k.dword("Type"), k.dword("Start")) else { continue };
            let kind = service_kind(kind);
            // Boot, system and automatic start; plus services Flask itself switched off.
            let ours = saved.as_ref().is_some_and(|s| s.dword(&name).is_some());
            if kind == ServiceKind::Other || !(start <= 2 || (start == 4 && ours)) {
                continue;
            }
            let command = k.string("ImagePath").unwrap_or_default();
            // Services hosted by svchost keep their real code in a DLL.
            let dll = Key::open(Hive::Hklm, &format!(r"{SERVICES}\{name}\Parameters"), View::Native)
                .and_then(|p| p.string("ServiceDll"))
                .map(|d| expand_env(&d));
            let image = dll.unwrap_or_else(|| resolve_image(&command));
            let display = k.string("DisplayName").filter(|d| !d.is_empty() && !d.starts_with('@')).unwrap_or_else(|| name.clone());
            let location = format!(r"HKLM\{SERVICES}\{name}");
            match kind {
                ServiceKind::Service => {
                    let source = Source::Service { name: name.clone() };
                    self.push(Category::Services, &display, &command, Some(image), location, start != 4, source);
                }
                _ => {
                    let source = Source::Locked { hive: Hive::Hklm, key: format!(r"{SERVICES}\{name}") };
                    self.push(Category::Drivers, &display, &command, Some(image), location, true, source);
                }
            }
        }
    }

    fn com_server(clsid: &str) -> Option<String> {
        let k = Key::open(Hive::Hkcr, &format!(r"CLSID\{clsid}\InprocServer32"), View::Native)?;
        k.string("").map(|s| expand_env(&s)).filter(|s| !s.is_empty())
    }

    fn explorer(&mut self) {
        let approved = format!(r"{CURRENT_VERSION}\Shell Extensions\Approved");
        for (path, enabled) in [(approved.clone(), true), (format!(r"{approved}\{DISABLED}"), false)] {
            let Some(k) = Key::open(Hive::Hklm, &path, View::Native) else { continue };
            for (clsid, value) in k.values() {
                let Some(image) = Self::com_server(&clsid) else { continue };
                let name = value.text().filter(|t| !t.is_empty()).unwrap_or_else(|| clsid.clone());
                let source = Source::RegValue { hive: Hive::Hklm, key: approved.clone(), view: View::Native, value: clsid };
                self.push(Category::Explorer, &name, &image.clone(), Some(image), "Approved shell extensions".into(), enabled, source);
            }
        }
        for class in ["*", "Directory", r"Directory\Background", "Folder", "Drive"] {
            let parent = format!(r"{class}\shellex\ContextMenuHandlers");
            for (path, enabled) in [(parent.clone(), true), (format!(r"{parent}\{DISABLED}"), false)] {
                let Some(root) = Key::open(Hive::Hkcr, &path, View::Native) else { continue };
                for sub in root.subkeys().into_iter().filter(|s| s != DISABLED) {
                    let Some(k) = Key::open(Hive::Hkcr, &format!(r"{path}\{sub}"), View::Native) else { continue };
                    // Either the key is named after the CLSID or its default value holds it.
                    let clsid = k.string("").filter(|v| v.starts_with('{')).unwrap_or_else(|| sub.clone());
                    let Some(image) = Self::com_server(&clsid) else { continue };
                    let source = Source::RegKey { hive: Hive::Hkcr, parent: parent.clone(), view: View::Native, name: sub.clone() };
                    self.push(Category::Explorer, &sub, &image.clone(), Some(image), format!("Context menu: {class}"), enabled, source);
                }
            }
        }
    }

    fn hijacks(&mut self) {
        for view in [View::Native, View::Wow32] {
            let ifeo = format!(r"{NT_CURRENT_VERSION}\Image File Execution Options");
            let Some(root) = Key::open(Hive::Hklm, &ifeo, view) else { continue };
            for exe in root.subkeys() {
                let key = format!(r"{ifeo}\{exe}");
                for (path, enabled) in [(key.clone(), true), (format!(r"{key}\{DISABLED}"), false)] {
                    let Some(debugger) = Key::open(Hive::Hklm, &path, view).and_then(|k| k.string("Debugger")) else { continue };
                    let source = Source::RegValue { hive: Hive::Hklm, key: key.clone(), view, value: "Debugger".into() };
                    self.push(Category::Hijacks, &exe, &debugger, None, "Debugger that replaces this program".into(), enabled, source);
                }
            }
        }
        for hive in [Hive::Hklm, Hive::Hkcu] {
            let key = r"Software\Microsoft\Command Processor";
            for (path, enabled) in [(key.to_owned(), true), (format!(r"{key}\{DISABLED}"), false)] {
                let Some(cmd) = Key::open(hive, &path, View::Native).and_then(|k| k.string("AutoRun")).filter(|c| !c.is_empty()) else { continue };
                let source = Source::RegValue { hive, key: key.to_owned(), view: View::Native, value: "AutoRun".into() };
                self.push(Category::Hijacks, "Command Prompt AutoRun", &cmd, None, format!(r"{}\{key}", hive.short()), enabled, source);
            }
        }
    }

    fn locked_values(&mut self, category: Category, key: &str, view: View, only: Option<&[&str]>, split: char) {
        let Some(k) = Key::open(Hive::Hklm, key, view) else { return };
        for (name, value) in k.values() {
            if only.is_some_and(|names| !names.iter().any(|n| n.eq_ignore_ascii_case(&name))) {
                continue;
            }
            let items: Vec<String> = match value {
                Value::Str(s) => s.split(split).map(|p| p.trim().to_owned()).collect(),
                Value::Multi(v) => v,
                _ => continue,
            };
            for item in items.into_iter().filter(|i| !i.is_empty()) {
                let source = Source::Locked { hive: Hive::Hklm, key: key.to_owned() };
                self.push(category, &item, &item.clone(), None, format!(r"HKLM\{key}: {name}"), true, source);
            }
        }
    }

    fn system_hooks(&mut self) {
        for view in [View::Native, View::Wow32] {
            self.locked_values(Category::Dlls, &format!(r"{NT_CURRENT_VERSION}\Windows"), view, Some(&["AppInit_DLLs"]), ',');
            self.locked_values(Category::Codecs, &format!(r"{NT_CURRENT_VERSION}\Drivers32"), view, None, '\0');
        }
        self.locked_values(Category::Dlls, &format!(r"{CONTROL}\Session Manager\KnownDLLs"), View::Native, None, '\0');
        self.locked_values(Category::Dlls, &format!(r"{CONTROL}\Session Manager\AppCertDlls"), View::Native, None, '\0');
        self.locked_values(Category::Boot, &format!(r"{CONTROL}\Session Manager"), View::Native, Some(&["BootExecute", "SetupExecute"]), '\0');
        self.locked_values(Category::Winlogon, &format!(r"{NT_CURRENT_VERSION}\Winlogon"), View::Native, Some(&["Userinit", "Shell"]), ',');
        let packages = ["Authentication Packages", "Notification Packages", "Security Packages"];
        self.locked_values(Category::Providers, &format!(r"{CONTROL}\Lsa"), View::Native, Some(&packages), '\0');
        self.locked_values(Category::Providers, &format!(r"{CONTROL}\NetworkProvider\Order"), View::Native, Some(&["ProviderOrder"]), ',');

        let monitors = format!(r"{CONTROL}\Print\Monitors");
        if let Some(root) = Key::open(Hive::Hklm, &monitors, View::Native) {
            for sub in root.subkeys() {
                let key = format!(r"{monitors}\{sub}");
                let Some(driver) = Key::open(Hive::Hklm, &key, View::Native).and_then(|k| k.string("Driver")) else { continue };
                let source = Source::Locked { hive: Hive::Hklm, key: key.clone() };
                self.push(Category::Print, &sub, &driver, None, format!(r"HKLM\{key}"), true, source);
            }
        }
    }

    fn packaged(&mut self) {
        let base = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\SystemAppData";
        let Some(root) = Key::open(Hive::Hkcu, base, View::Native) else { return };
        for package in root.subkeys() {
            let Some(pk) = Key::open(Hive::Hkcu, &format!(r"{base}\{package}"), View::Native) else { continue };
            for task in pk.subkeys() {
                let key = format!(r"{base}\{package}\{task}");
                let Some(state) = Key::open(Hive::Hkcu, &key, View::Native).and_then(|k| k.dword("State")) else { continue };
                // 2 and 4 are the enabled states; 0, 1 and 3 are variations of off.
                let enabled = state == 2 || state == 4;
                let source = Source::PackagedTask { key };
                self.push(Category::Packaged, &package, &task, Some(String::new()), "Packaged app startup task".into(), enabled, source);
            }
        }
    }
}

/// Entries that launch a program directly at logon or on a schedule. Cheap
/// enough to run whenever a process's origin is opened.
pub fn scan_launch_points() -> Vec<Entry> {
    let mut scan = Scan { out: Vec::new(), companies: HashMap::new() };
    scan.logon();
    scan.tasks();
    scan.out
}

/// Every autostart location Flask knows. Takes a second or two.
pub fn scan() -> Vec<Entry> {
    let mut scan = Scan { out: Vec::new(), companies: HashMap::new() };
    scan.logon();
    scan.tasks();
    scan.services();
    scan.explorer();
    scan.hijacks();
    scan.system_hooks();
    scan.packaged();
    scan.out
}

/// Switches an entry on or off using the mechanism its location calls for.
/// Every route is reversible.
pub fn set_enabled(entry: &Entry, enable: bool) -> Result<(), String> {
    match &entry.source {
        Source::Approved { hive, view, value, .. } => set_approved(*hive, *view, false, value, enable),
        Source::StartupFile { path, hive } => {
            let file = Path::new(path).file_name().unwrap_or_default().to_string_lossy();
            set_approved(*hive, View::Native, true, &file, enable)
        }
        Source::RegValue { hive, key, view, value } => toggle_reg_value(*hive, key, *view, value, enable),
        Source::RegKey { hive, parent, view, name } => toggle_reg_key(*hive, parent, *view, name, enable),
        Source::Task { name, .. } => run_tool("schtasks", &["/Change", "/TN", name, if enable { "/ENABLE" } else { "/DISABLE" }]),
        Source::Service { name } => {
            let saved = Key::create(Hive::Hklm, SAVED_SERVICE_STARTS, View::Native);
            if enable {
                // Put back what it was; a service nobody recorded goes to manual start.
                let start = saved.as_ref().and_then(|s| s.dword(name)).unwrap_or(3);
                let keyword = match start {
                    2 => "auto",
                    _ => "demand",
                };
                run_tool("sc", &["config", name, "start=", keyword])?;
                if let Some(s) = saved {
                    s.delete_value(name);
                }
                Ok(())
            } else {
                let current = Key::open(Hive::Hklm, &format!(r"{SERVICES}\{name}"), View::Native).and_then(|k| k.dword("Start"));
                if let (Some(s), Some(current)) = (&saved, current) {
                    s.set_dword(name, current);
                }
                run_tool("sc", &["config", name, "start=", "disabled"])
            }
        }
        Source::PackagedTask { key } => {
            let k = Key::open_write(Hive::Hkcu, key, View::Native).ok_or("Could not open the startup task")?;
            k.set_dword("State", if enable { 2 } else { 1 }).then_some(()).ok_or_else(|| "Could not write the state".into())
        }
        Source::Locked { .. } => Err("This entry is shown for inspection only".into()),
    }
}

/// Removes an entry for good.
pub fn delete(entry: &Entry) -> Result<(), String> {
    let gone = |ok: bool| ok.then_some(()).ok_or_else(|| "Could not delete it".to_owned());
    match &entry.source {
        Source::Approved { hive, key, view, value } => {
            gone(Key::open_write(*hive, key, *view).is_some_and(|k| k.delete_value(value)))?;
            if let Some(k) = Key::open_write(*hive, &approved_key(*view, false), View::Native) {
                k.delete_value(value);
            }
            Ok(())
        }
        Source::StartupFile { path, .. } => std::fs::remove_file(path).map_err(|e| e.to_string()),
        Source::RegValue { hive, key, view, value } => {
            let path = if entry.enabled { key.clone() } else { format!(r"{key}\{DISABLED}") };
            gone(Key::open_write(*hive, &path, *view).is_some_and(|k| k.delete_value(value)))
        }
        Source::RegKey { hive, parent, view, name } => {
            let path = if entry.enabled { parent.clone() } else { format!(r"{parent}\{DISABLED}") };
            gone(Key::open_write(*hive, &path, *view).is_some_and(|k| k.delete_tree(name)))
        }
        Source::Task { name, .. } => run_tool("schtasks", &["/Delete", "/TN", name, "/F"]),
        Source::Service { name } => run_tool("sc", &["delete", name]),
        Source::PackagedTask { .. } | Source::Locked { .. } => Err("This entry cannot be deleted from here".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_resolution() {
        let root = std::env::var("SystemRoot").unwrap();
        let eq = |a: String, b: String| assert_eq!(a.to_lowercase(), b.to_lowercase());
        eq(resolve_image(r"\SystemRoot\System32\drivers\ntfs.sys"), format!(r"{root}\System32\drivers\ntfs.sys"));
        eq(resolve_image(r"\??\C:\Tools\drv.sys"), r"C:\Tools\drv.sys".into());
        eq(resolve_image(r"system32\DRIVERS\acpi.sys"), format!(r"{root}\system32\DRIVERS\acpi.sys"));
        eq(resolve_image("rundll32 shell32.dll,Control_RunDLL"), format!(r"{root}\System32\rundll32.exe"));
        eq(resolve_image(r"%SystemRoot%\system32\svchost.exe -k netsvcs -p"), format!(r"{root}\system32\svchost.exe"));
        eq(resolve_image(r#""C:\Program Files\A B\c.exe" /x"#), r"C:\Program Files\A B\c.exe".into());
        assert_eq!(resolve_image("no-such-tool-xyz"), "no-such-tool-xyz");
    }

    #[test]
    fn small_parsers() {
        assert!(!approved_blob_disabled(&[2, 0, 0, 0]));
        assert!(!approved_blob_disabled(&[6, 0]));
        assert!(approved_blob_disabled(&[3, 0, 0, 0, 1, 2]));
        assert!(approved_blob_disabled(&[7]));
        assert!(!approved_blob_disabled(&[]));

        assert_eq!(service_kind(0x10), ServiceKind::Service);
        assert_eq!(service_kind(0x20), ServiceKind::Service);
        assert_eq!(service_kind(0x60), ServiceKind::Service);
        assert_eq!(service_kind(0x1), ServiceKind::Driver);
        assert_eq!(service_kind(0x2), ServiceKind::Driver);
        assert_eq!(service_kind(0x4), ServiceKind::Other);

        let on = "<Task><Triggers><LogonTrigger><Enabled>false</Enabled></LogonTrigger></Triggers><Settings><Enabled>true</Enabled></Settings></Task>";
        let off = "<Task><Settings><Hidden>true</Hidden><Enabled>false</Enabled></Settings></Task>";
        assert!(task_enabled(on) && !task_enabled(off) && task_enabled("<Task/>"));
    }

    #[test]
    fn registry_value_and_key_toggle_round_trip() {
        let base = format!(r"Software\SysCentral-autoruns-test-{}", std::process::id());
        let key = Key::create(Hive::Hkcu, &base, View::Native).unwrap();
        key.set_string("Probe", r"C:\probe.exe /x");
        let sub = Key::create(Hive::Hkcu, &format!(r"{base}\Component\Inner"), View::Native).unwrap();
        sub.set_dword("n", 5);
        drop(sub);

        toggle_reg_value(Hive::Hkcu, &base, View::Native, "Probe", false).unwrap();
        assert!(key.string("Probe").is_none());
        let parked = Key::open(Hive::Hkcu, &format!(r"{base}\{DISABLED}"), View::Native).unwrap();
        assert_eq!(parked.string("Probe").as_deref(), Some(r"C:\probe.exe /x"));
        toggle_reg_value(Hive::Hkcu, &base, View::Native, "Probe", true).unwrap();
        assert_eq!(key.string("Probe").as_deref(), Some(r"C:\probe.exe /x"));
        assert!(parked.string("Probe").is_none());
        assert!(toggle_reg_value(Hive::Hkcu, &base, View::Native, "Missing", false).is_err());

        toggle_reg_key(Hive::Hkcu, &base, View::Native, "Component", false).unwrap();
        assert!(Key::open(Hive::Hkcu, &format!(r"{base}\Component"), View::Native).is_none());
        let inner = Key::open(Hive::Hkcu, &format!(r"{base}\{DISABLED}\Component\Inner"), View::Native).unwrap();
        assert_eq!(inner.dword("n"), Some(5));
        drop(inner);
        toggle_reg_key(Hive::Hkcu, &base, View::Native, "Component", true).unwrap();
        let inner = Key::open(Hive::Hkcu, &format!(r"{base}\Component\Inner"), View::Native).unwrap();
        assert_eq!(inner.dword("n"), Some(5));

        drop((key, parked, inner));
        let software = Key::open_write(Hive::Hkcu, "Software", View::Native).unwrap();
        assert!(software.delete_tree(base.strip_prefix(r"Software\").unwrap()));
    }

    #[test]
    fn run_entry_is_found_toggled_through_startup_approved_and_deleted() {
        let name = format!("sc-autoruns-test-{}", std::process::id());
        let run = Key::open_write(Hive::Hkcu, &format!(r"{CURRENT_VERSION}\Run"), View::Native).unwrap();
        run.set_string(&name, r#""C:\sc-test\probe app.exe" --tray"#);

        let find = || scan_launch_points().into_iter().find(|e| e.name == name);
        let entry = find().expect("entry listed");
        assert_eq!((entry.category, entry.enabled), (Category::Logon, true));
        assert_eq!(entry.image, r"C:\sc-test\probe app.exe");
        assert!(entry.registry_path().unwrap().starts_with(r"Computer\HKEY_CURRENT_USER\Software\Microsoft"));

        set_enabled(&entry, false).unwrap();
        assert!(!find().unwrap().enabled);
        // The Run value itself is untouched; only Windows' approval list changed.
        assert!(run.string(&name).is_some());
        set_enabled(&entry, true).unwrap();
        assert!(find().unwrap().enabled);

        delete(&entry).unwrap();
        assert!(find().is_none() && run.string(&name).is_none());
        let approved = Key::open(Hive::Hkcu, &approved_key(View::Native, false), View::Native);
        assert!(approved.is_none_or(|k| k.raw(&name).is_none()));
    }

    #[test]
    fn full_scan_covers_the_main_categories() {
        let entries = scan();
        let count = |c: Category| entries.iter().filter(|e| e.category == c).count();
        assert!(count(Category::Services) > 20, "services: {}", count(Category::Services));
        assert!(count(Category::Drivers) > 20, "drivers: {}", count(Category::Drivers));
        assert!(count(Category::Dlls) > 5 && count(Category::Winlogon) >= 1);
        // Locked entries can be neither toggled nor deleted.
        let locked = entries.iter().find(|e| e.category == Category::Drivers).unwrap();
        assert!(!locked.can_toggle() && !locked.can_delete() && set_enabled(locked, false).is_err());
        assert!(entries.iter().filter(|e| e.category == Category::Services).any(|e| e.company.contains("Microsoft")));
        let summary: Vec<String> = Category::ALL.iter().map(|c| format!("{} {}", c.label(), count(*c))).collect();
        eprintln!("{}", summary.join(", "));
    }
}
