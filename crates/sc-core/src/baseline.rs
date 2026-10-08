//! Remembered autostart entries, so a later scan can point out what is new.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::autoruns::Entry;

/// Identity of an entry. The command is part of it, so an existing entry
/// that now launches something else counts as new. Whether it is switched on
/// is not: disabling an entry does not make it news.
pub fn key(e: &Entry) -> String {
    format!("{}\t{}\t{}\t{}", e.category.label(), e.name, e.location, e.command).replace(['\r', '\n'], " ")
}

pub fn keys(entries: &[Entry]) -> HashSet<String> {
    entries.iter().map(key).collect()
}

// Under ProgramData rather than the user profile: a file Flask creates there
// while elevated cannot be rewritten by unelevated programs, so they cannot
// add themselves to the baseline.
// ponytail: relies on the inherited ProgramData ACL. A file planted here
// before Flask first runs stays writable by whoever planted it; set an
// explicit ACL on the folder if that has to be closed.
fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("ProgramData")?).join(r"SysCentral\Flask\startup-baseline.txt"))
}

/// `None` when no baseline has been saved yet.
pub fn load() -> Option<HashSet<String>> {
    Some(std::fs::read_to_string(path()?).ok()?.lines().map(str::to_owned).collect())
}

/// Makes `entries` the baseline.
pub fn save(entries: &[Entry]) -> std::io::Result<()> {
    let path = path().ok_or(std::io::ErrorKind::NotFound)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut lines: Vec<String> = keys(entries).into_iter().collect();
    lines.sort();
    std::fs::write(path, lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoruns::{Category, Source};
    use crate::reg::Hive;

    #[test]
    fn key_follows_the_command_but_not_the_switch() {
        let e = Entry {
            category: Category::Logon,
            name: "Updater".into(),
            command: r"C:\p\updater.exe /silent".into(),
            image: r"C:\p\updater.exe".into(),
            location: r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run".into(),
            enabled: true,
            source: Source::Locked { hive: Hive::Hkcu, key: "x".into() },
            company: String::new(),
        };
        let known = keys(std::slice::from_ref(&e));
        assert!(known.contains(&key(&Entry { enabled: false, ..e.clone() })));
        assert!(!known.contains(&key(&Entry { command: r"C:\Temp\evil.exe".into(), ..e.clone() })));
        // One line per entry, whatever the name contains.
        assert!(!key(&Entry { name: "a\nb".into(), ..e }).contains('\n'));
    }
}
