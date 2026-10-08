use std::path::{Path, PathBuf};
use std::process::Command;

/// Newest resource compiler from the installed Windows SDKs.
fn find_rc() -> Option<PathBuf> {
    let kits = Path::new(&std::env::var("ProgramFiles(x86)").ok()?).join(r"Windows Kits\10\bin");
    let mut found: Vec<PathBuf> = std::fs::read_dir(kits)
        .ok()?
        .flatten()
        .map(|e| e.path().join(r"x64\rc.exe"))
        .filter(|p| p.exists())
        .collect();
    found.sort();
    found.pop()
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // Embed the application manifest with the MSVC linker. The `unelevated`
    // feature swaps the execution level so development builds can be started
    // and driven by unelevated tooling; shipped builds always require admin.
    println!("cargo:rerun-if-changed=app.manifest");
    let mut xml = std::fs::read_to_string(dir.join("app.manifest")).unwrap();
    if std::env::var_os("CARGO_FEATURE_UNELEVATED").is_some() {
        xml = xml.replace("requireAdministrator", "asInvoker");
    }
    let manifest = out.join("app.manifest");
    std::fs::write(&manifest, xml).unwrap();
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    // The manifest carries its own requestedExecutionLevel.
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");

    // Icon (resource id 1, which sc-ui loads for the window) and version info.
    let icon = dir.join(r"..\..\assets\flask.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let version = env!("CARGO_PKG_VERSION");
    let commas = version.replace('.', ",");
    let rc = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {commas},0
PRODUCTVERSION {commas},0
{{
  BLOCK "StringFileInfo"
  {{
    BLOCK "040904B0"
    {{
      VALUE "CompanyName", "SysCentral\0"
      VALUE "FileDescription", "Flask\0"
      VALUE "ProductName", "Flask\0"
      VALUE "FileVersion", "{version}\0"
      VALUE "ProductVersion", "{version}\0"
      VALUE "OriginalFilename", "flask.exe\0"
    }}
  }}
  BLOCK "VarFileInfo"
  {{
    VALUE "Translation", 0x409, 1200
  }}
}}
"#,
        icon = icon.display().to_string().replace('\\', "/"),
    );
    let rc_path = out.join("flask.rc");
    let res_path = out.join("flask.res");
    std::fs::write(&rc_path, rc).unwrap();
    let rc_exe = find_rc().expect("rc.exe not found; install the Windows 10/11 SDK");
    let status = Command::new(rc_exe).arg("/nologo").arg("/fo").arg(&res_path).arg(&rc_path).status().unwrap();
    assert!(status.success(), "rc.exe failed");
    println!("cargo:rustc-link-arg-bins={}", res_path.display());
}
