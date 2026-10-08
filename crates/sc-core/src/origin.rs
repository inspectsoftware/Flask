//! Where a process came from: who signed its file, where the file was
//! downloaded from, how it is hosted, and what makes it start. Everything
//! here can block on disk or the registry, so callers run it off the UI thread.

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::Cryptography::Catalog::{
    CATALOG_INFO, CryptCATAdminAcquireContext2, CryptCATAdminCalcHashFromFileHandle2,
    CryptCATAdminEnumCatalogFromHash, CryptCATAdminReleaseCatalogContext, CryptCATAdminReleaseContext,
    CryptCATCatalogInfoFromContext,
};
use windows::Win32::Security::Cryptography::{CERT_NAME_SIMPLE_DISPLAY_TYPE, CertGetNameStringW};
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_CATALOG_INFO, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_CATALOG, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData,
    WinVerifyTrust,
};
use windows::Win32::Security::{GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Services::{
    ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW, OpenSCManagerW, SC_ENUM_PROCESS_INFO,
    SC_MANAGER_ENUMERATE_SERVICE, SERVICE_ACTIVE, SERVICE_WIN32, CloseServiceHandle,
};
use windows::Win32::System::Threading::{OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{GUID, Interface, PCWSTR, w};

use crate::autoruns::{self, Entry};
use crate::nt::Process;
use crate::sha256;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

// ---------------------------------------------------------------------------
// Signature

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signature {
    /// Authenticode signature that chains to a trusted root.
    Valid { signer: String },
    Unsigned,
    /// Signed, but verification failed (tampered file, untrusted or expired chain).
    Invalid { code: u32 },
    /// The file could not be opened to check.
    Unknown,
}

const TRUST_E_NOSIGNATURE: i32 = 0x800B_0100_u32 as i32;

/// Runs WinVerifyTrust on `data`, reads the signer if it verified, and
/// releases the verification state.
unsafe fn verify(data: &mut WINTRUST_DATA) -> (i32, Option<String>) {
    unsafe {
        let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let window = HWND(-1isize as *mut c_void); // INVALID_HANDLE_VALUE: no UI
        data.dwStateAction = WTD_STATEACTION_VERIFY;
        let status = WinVerifyTrust(window, &mut action, data as *mut _ as *mut c_void);

        let mut signer = None;
        if status == 0 {
            let prov = WTHelperProvDataFromStateData(data.hWVTStateData);
            if !prov.is_null() {
                let sgnr = WTHelperGetProvSignerFromChain(prov, 0, false, 0);
                if !sgnr.is_null() && !(*sgnr).pasCertChain.is_null() {
                    let cert = (*(*sgnr).pasCertChain).pCert;
                    let mut name = [0u16; 256];
                    let len = CertGetNameStringW(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, Some(&mut name));
                    if len > 1 {
                        signer = Some(String::from_utf16_lossy(&name[..len as usize - 1]));
                    }
                }
            }
        }
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        WinVerifyTrust(window, &mut action, data as *mut _ as *mut c_void);
        (status, signer)
    }
}

fn base_data() -> WINTRUST_DATA {
    WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        // No network: revocation is not checked and nothing is downloaded.
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
        ..Default::default()
    }
}

/// Looks the file's hash up in the system's security catalogs, which is how
/// most files that ship with Windows are signed.
unsafe fn verify_by_catalog(wpath: &[u16], file: HANDLE) -> Option<(i32, Option<String>)> {
    unsafe {
        let mut admin = 0isize;
        CryptCATAdminAcquireContext2(&mut admin, None, w!("SHA256"), None, None).ok()?;
        let result = (|| {
            let mut hash = [0u8; 64];
            let mut hash_len = hash.len() as u32;
            CryptCATAdminCalcHashFromFileHandle2(admin, file, &mut hash_len, Some(hash.as_mut_ptr()), None).ok()?;
            let hash = &hash[..hash_len as usize];
            let cat = CryptCATAdminEnumCatalogFromHash(admin, hash, None, None);
            if cat == 0 {
                return None;
            }
            let mut info = CATALOG_INFO { cbStruct: size_of::<CATALOG_INFO>() as u32, ..Default::default() };
            let found = CryptCATCatalogInfoFromContext(cat, &mut info, 0).is_ok();
            let verdict = found.then(|| {
                let tag = wide(&hash.iter().map(|b| format!("{b:02X}")).collect::<String>());
                let mut cat_info = WINTRUST_CATALOG_INFO {
                    cbStruct: size_of::<WINTRUST_CATALOG_INFO>() as u32,
                    pcwszCatalogFilePath: PCWSTR(info.wszCatalogFile.as_ptr()),
                    pcwszMemberTag: PCWSTR(tag.as_ptr()),
                    pcwszMemberFilePath: PCWSTR(wpath.as_ptr()),
                    hMemberFile: file,
                    pbCalculatedFileHash: hash.as_ptr() as *mut u8,
                    cbCalculatedFileHash: hash.len() as u32,
                    hCatAdmin: admin,
                    ..Default::default()
                };
                let mut data = base_data();
                data.dwUnionChoice = WTD_CHOICE_CATALOG;
                data.Anonymous = WINTRUST_DATA_0 { pCatalog: &mut cat_info };
                verify(&mut data)
            });
            let _ = CryptCATAdminReleaseCatalogContext(admin, cat, 0);
            verdict
        })();
        let _ = CryptCATAdminReleaseContext(admin, 0);
        result
    }
}

/// Checks the file's Authenticode signature, embedded or by catalog.
pub fn signature(path: &str) -> Signature {
    let Ok(file) = std::fs::File::open(path) else { return Signature::Unknown };
    let handle = HANDLE(file.as_raw_handle());
    let wpath = wide(path);
    unsafe {
        let mut file_info = WINTRUST_FILE_INFO {
            cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: PCWSTR(wpath.as_ptr()),
            hFile: handle,
            ..Default::default()
        };
        let mut data = base_data();
        data.dwUnionChoice = WTD_CHOICE_FILE;
        data.Anonymous = WINTRUST_DATA_0 { pFile: &mut file_info };
        let (mut status, mut signer) = verify(&mut data);
        if status == TRUST_E_NOSIGNATURE
            && let Some((cat_status, cat_signer)) = verify_by_catalog(&wpath, handle)
        {
            (status, signer) = (cat_status, cat_signer);
        }
        match status {
            0 => Signature::Valid { signer: signer.unwrap_or_default() },
            TRUST_E_NOSIGNATURE => Signature::Unsigned,
            code => Signature::Invalid { code: code as u32 },
        }
    }
}

// ---------------------------------------------------------------------------
// Download origin (Mark of the Web)

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Zone {
    pub id: u32,
    pub referrer: String,
    pub host: String,
}

impl Zone {
    pub fn label(&self) -> &'static str {
        match self.id {
            0 => "This computer",
            1 => "Local network",
            2 => "Trusted sites",
            3 => "Internet",
            4 => "Restricted sites",
            _ => "Unknown zone",
        }
    }
}

/// Parses the `Zone.Identifier` stream Windows attaches to downloaded files.
pub fn parse_zone(text: &str) -> Option<Zone> {
    let mut zone = Zone::default();
    let mut seen = false;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "zoneid" => {
                zone.id = value.parse().ok()?;
                seen = true;
            }
            "referrerurl" => zone.referrer = value.to_owned(),
            "hosturl" => zone.host = value.to_owned(),
            _ => {}
        }
    }
    seen.then_some(zone)
}

pub fn zone(path: &str) -> Option<Zone> {
    let bytes = std::fs::read(format!("{path}:Zone.Identifier")).ok()?;
    parse_zone(&String::from_utf8_lossy(&bytes))
}

// ---------------------------------------------------------------------------
// Runtime facts

/// Mandatory integrity level of the process: "Low", "Medium", "High", "System".
pub fn integrity(pid: u32) -> Option<&'static str> {
    let p = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(p.0, TOKEN_QUERY, &mut token).ok()?;
        let mut buf = [0u64; 16];
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 8) as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        ok.ok()?;
        let sid = (*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL)).Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        let rid = *GetSidSubAuthority(sid, count.checked_sub(1)? as u32);
        Some(match rid {
            0..0x1000 => "Untrusted",
            0x1000..0x2000 => "Low",
            0x2000..0x3000 => "Medium",
            0x3000..0x4000 => "High",
            _ => "System",
        })
    }
}

/// Display names of the Windows services running inside `pid`.
pub fn services_in(pid: u32) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_ENUMERATE_SERVICE) else { return out };
        let (mut needed, mut count, mut resume) = (0u32, 0u32, 0u32);
        let _ = EnumServicesStatusExW(
            scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_ACTIVE, None, &mut needed, &mut count, Some(&mut resume), None,
        );
        let mut buf = vec![0u64; (needed as usize).div_ceil(8) + 64];
        resume = 0;
        let bytes = std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), buf.len() * 8);
        if EnumServicesStatusExW(
            scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_ACTIVE, Some(bytes), &mut needed, &mut count, Some(&mut resume), None,
        )
        .is_ok()
        {
            let items = std::slice::from_raw_parts(buf.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(), count as usize);
            for s in items.iter().filter(|s| s.ServiceStatusProcess.dwProcessId == pid) {
                if let Ok(name) = s.lpDisplayName.to_string() {
                    out.push(name);
                }
            }
        }
        let _ = CloseServiceHandle(scm);
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// What makes it start

pub fn expand_env(s: &str) -> String {
    let src = wide(s);
    let mut buf = vec![0u16; 2048];
    let len = unsafe { ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) } as usize;
    if len == 0 || len > buf.len() { s.to_owned() } else { String::from_utf16_lossy(&buf[..len - 1]) }
}

/// Extracts the program from a launch string such as
/// `"C:\Program Files\App\app.exe" --flag` or `C:\Tools\x.exe /s`.
pub fn command_exe(command: &str) -> String {
    let c = command.trim();
    if let Some(rest) = c.strip_prefix('"') {
        return rest.split('"').next().unwrap_or("").to_owned();
    }
    // Unquoted paths may contain spaces; cut after the first ".exe"-style
    // extension that is followed by a space or the end.
    let lower = c.to_ascii_lowercase();
    for ext in [".exe", ".com", ".bat", ".cmd", ".scr"] {
        let mut from = 0;
        while let Some(i) = lower[from..].find(ext) {
            let end = from + i + ext.len();
            if lower[end..].chars().next().is_none_or(char::is_whitespace) {
                return c[..end].to_owned();
            }
            from = end;
        }
    }
    c.split_whitespace().next().unwrap_or("").to_owned()
}

fn same_file(a: &str, b: &str) -> bool {
    !a.is_empty() && a.eq_ignore_ascii_case(b)
}

/// The `<Command>` of every `<Exec>` action in a Task Scheduler XML definition.
pub fn task_commands(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Command>") {
        rest = &rest[start + "<Command>".len()..];
        let Some(end) = rest.find("</Command>") else { break };
        out.push(rest[..end].trim().replace("&amp;", "&").replace("&quot;", "\""));
        rest = &rest[end..];
    }
    out
}

pub(crate) fn decode_task_file(bytes: &[u8]) -> String {
    // Task definitions are UTF-16 LE with a byte order mark.
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Target of a `.lnk` shortcut. COM must be initialized on this thread.
pub(crate) unsafe fn shortcut_target(link: &Path) -> Option<String> {
    unsafe {
        let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = shell.cast().ok()?;
        let wlink = wide(&link.display().to_string());
        file.Load(PCWSTR(wlink.as_ptr()), STGM_READ).ok()?;
        let mut target = [0u16; 1024];
        shell.GetPath(&mut target, std::ptr::null_mut(), 0).ok()?;
        let len = target.iter().position(|&c| c == 0).unwrap_or(target.len());
        Some(String::from_utf16_lossy(&target[..len]))
    }
}

/// Logon entries and scheduled tasks that launch the file at `image`.
/// Services the process hosts are reported separately by [`services_in`].
pub fn start_sources(image: &str) -> Vec<Entry> {
    if image.is_empty() {
        return Vec::new();
    }
    autoruns::scan_launch_points().into_iter().filter(|e| same_file(&e.image, image)).collect()
}

// ---------------------------------------------------------------------------
// Aggregate

#[derive(Debug, Clone, Default)]
pub struct FileFacts {
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub created: Option<u64>,
    pub modified: Option<u64>,
}

pub fn file_facts(path: &str) -> Option<FileFacts> {
    let meta = std::fs::metadata(path).ok()?;
    let secs = |t: std::io::Result<std::time::SystemTime>| {
        t.ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
    };
    Some(FileFacts { size: meta.len(), created: secs(meta.created()), modified: secs(meta.modified()) })
}

/// True when the file lives somewhere installers do not normally put programs.
pub fn in_scratch_location(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    [r"\appdata\local\temp\", r"\windows\temp\", r"\downloads\", r"\$recycle.bin\"].iter().any(|s| p.contains(s))
}

#[derive(Debug, Clone)]
pub struct Origin {
    pub signature: Signature,
    pub zone: Option<Zone>,
    pub file: Option<FileFacts>,
    pub integrity: Option<&'static str>,
    pub services: Vec<String>,
    pub start_sources: Vec<Entry>,
    pub scratch_location: bool,
    /// SHA-256 of the image, for looking the file up elsewhere.
    pub sha256: Option<String>,
}

impl Origin {
    /// Plain-language reasons this process deserves a second look.
    pub fn warnings(&self) -> Vec<&'static str> {
        let mut w = Vec::new();
        match self.signature {
            Signature::Unsigned => w.push("not signed"),
            Signature::Invalid { .. } => w.push("signature does not verify"),
            _ => {}
        }
        if self.scratch_location {
            w.push("runs from Temp or Downloads");
        }
        if self.zone.as_ref().is_some_and(|z| z.id >= 3) {
            w.push("downloaded from the internet");
        }
        w
    }
}

pub fn collect(pid: u32, image: &str) -> Origin {
    Origin {
        signature: if image.is_empty() { Signature::Unknown } else { signature(image) },
        zone: zone(image),
        file: file_facts(image),
        integrity: integrity(pid),
        services: services_in(pid),
        start_sources: start_sources(image),
        scratch_location: in_scratch_location(image),
        sha256: sha256::file(image),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system32(file: &str) -> String {
        format!(r"{}\System32\{file}", std::env::var("SystemRoot").unwrap())
    }

    #[test]
    fn windows_binaries_verify_and_test_binary_is_unsigned() {
        // Catalog-signed.
        match signature(&system32("notepad.exe")) {
            Signature::Valid { signer } => assert!(signer.contains("Microsoft"), "{signer}"),
            other => panic!("notepad: {other:?}"),
        }
        // Embedded signature.
        assert!(matches!(signature(&system32("ntdll.dll")), Signature::Valid { .. }));
        let me = std::env::current_exe().unwrap().display().to_string();
        assert_eq!(signature(&me), Signature::Unsigned);
        assert_eq!(signature(r"C:\no\such\file.exe"), Signature::Unknown);
    }

    #[test]
    fn zone_identifier_round_trip() {
        let text = "[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://example.com/page\r\nHostUrl=https://example.com/f.zip\r\n";
        let z = parse_zone(text).unwrap();
        assert_eq!((z.id, z.label()), (3, "Internet"));
        assert_eq!(z.referrer, "https://example.com/page");
        assert_eq!(z.host, "https://example.com/f.zip");
        assert!(parse_zone("[ZoneTransfer]\r\n").is_none());
        assert!(parse_zone("ZoneId=abc").is_none());

        // The real thing, on a file we mark ourselves.
        let file = std::env::temp_dir().join(format!("sc-origin-zone-{}.txt", std::process::id()));
        std::fs::write(&file, "x").unwrap();
        let path = file.display().to_string();
        assert!(zone(&path).is_none());
        std::fs::write(format!("{path}:Zone.Identifier"), text).unwrap();
        assert_eq!(zone(&path).unwrap().id, 3);
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn command_exe_handles_quotes_and_spaces() {
        assert_eq!(command_exe(r#""C:\Program Files\App\app.exe" --minimized"#), r"C:\Program Files\App\app.exe");
        assert_eq!(command_exe(r"C:\Tools\x.exe /s"), r"C:\Tools\x.exe");
        assert_eq!(command_exe(r"C:\Program Files\My App\run.EXE -a b"), r"C:\Program Files\My App\run.EXE");
        assert_eq!(command_exe(r"C:\a.exe.config\b.exe"), r"C:\a.exe.config\b.exe");
        assert_eq!(command_exe("rundll32 shell32.dll,Control_RunDLL"), "rundll32");
        assert_eq!(command_exe(""), "");
    }

    #[test]
    fn task_xml_commands() {
        let xml = r#"<Task><Actions Context="Author"><Exec><Command>"C:\A &amp; B\tool.exe"</Command><Arguments>/q</Arguments></Exec>
            <Exec><Command>%windir%\system32\sc.exe</Command></Exec></Actions></Task>"#;
        assert_eq!(task_commands(xml), [r#""C:\A & B\tool.exe""#, r"%windir%\system32\sc.exe"]);
        assert!(task_commands("<Task><Command>unterminated").is_empty());
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("<Command>x.exe</Command>".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(task_commands(&decode_task_file(&utf16)), ["x.exe"]);
    }

    #[test]
    fn env_expansion_and_scratch_locations() {
        let windir = std::env::var("windir").unwrap();
        assert_eq!(expand_env(r"%windir%\x.exe").to_lowercase(), format!(r"{windir}\x.exe").to_lowercase());
        assert_eq!(expand_env("plain"), "plain");
        assert!(in_scratch_location(r"C:\Users\n\AppData\Local\Temp\upd\u.exe"));
        assert!(in_scratch_location(r"C:\Users\n\Downloads\setup.exe"));
        assert!(!in_scratch_location(r"C:\Program Files\App\app.exe"));
    }

    #[test]
    fn collect_for_own_process() {
        let me = std::env::current_exe().unwrap().display().to_string();
        let o = collect(std::process::id(), &me);
        assert_eq!(o.signature, Signature::Unsigned);
        assert!(o.file.as_ref().is_some_and(|f| f.size > 0 && f.modified.is_some()));
        assert!(matches!(o.integrity, Some("Medium" | "High")));
        assert!(o.services.is_empty());
        assert!(o.warnings().contains(&"not signed"));
        assert_eq!(o.sha256.as_ref().map(String::len), Some(64));
    }

    #[test]
    fn finds_a_run_key_entry_for_an_image() {
        // A throwaway HKCU Run value pointing at a path nothing else uses.
        let image = r"C:\sc-core-test\origin probe.exe";
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let name = format!("sc-core-test-{}", std::process::id());
        let add = std::process::Command::new("reg")
            .args(["add", key, "/v", &name, "/t", "REG_SZ", "/d", &format!("\"{image}\" --probe"), "/f"])
            .output()
            .unwrap();
        assert!(add.status.success());
        let found = start_sources(image);
        let _ = std::process::Command::new("reg").args(["delete", key, "/v", &name, "/f"]).output();
        let hit = found.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("{found:?}"));
        assert_eq!(hit.category, autoruns::Category::Logon);
        assert!(hit.location.starts_with("HKCU") && hit.enabled);
        assert!(start_sources(image).is_empty());
    }
}
