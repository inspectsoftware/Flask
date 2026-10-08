//! Thin, owned wrapper over the registry calls the scanners need.

use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY,
    KEY_WRITE, REG_DWORD, REG_EXPAND_SZ, REG_MULTI_SZ, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ,
    REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW,
    RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::{PCWSTR, PWSTR};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Hive {
    Hkcu,
    Hklm,
    Hkcr,
}

impl Hive {
    fn handle(self) -> HKEY {
        match self {
            Hive::Hkcu => HKEY_CURRENT_USER,
            Hive::Hklm => HKEY_LOCAL_MACHINE,
            Hive::Hkcr => HKEY_CLASSES_ROOT,
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Hive::Hkcu => "HKCU",
            Hive::Hklm => "HKLM",
            Hive::Hkcr => "HKCR",
        }
    }

    /// Name as regedit's address bar wants it.
    pub fn long(self) -> &'static str {
        match self {
            Hive::Hkcu => "HKEY_CURRENT_USER",
            Hive::Hklm => "HKEY_LOCAL_MACHINE",
            Hive::Hkcr => "HKEY_CLASSES_ROOT",
        }
    }
}

/// Which registry view to use on 64-bit Windows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum View {
    Native,
    Wow32,
}

impl View {
    fn flag(self) -> REG_SAM_FLAGS {
        match self {
            View::Native => KEY_WOW64_64KEY,
            View::Wow32 => KEY_WOW64_32KEY,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Multi(Vec<String>),
    Dword(u32),
    Other,
}

impl Value {
    /// Text form: strings as they are, multi-strings joined with "; ".
    pub fn text(&self) -> Option<String> {
        match self {
            Value::Str(s) => Some(s.clone()),
            Value::Multi(v) => Some(v.join("; ")),
            _ => None,
        }
    }
}

fn utf16_from_bytes(data: &[u8]) -> Vec<u16> {
    data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}

pub fn decode(kind: u32, data: &[u8]) -> Value {
    if kind == REG_SZ.0 || kind == REG_EXPAND_SZ.0 {
        let units = utf16_from_bytes(data);
        let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
        Value::Str(String::from_utf16_lossy(&units[..end]))
    } else if kind == REG_MULTI_SZ.0 {
        let units = utf16_from_bytes(data);
        Value::Multi(units.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect())
    } else if kind == REG_DWORD.0 && data.len() >= 4 {
        Value::Dword(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
    } else {
        Value::Other
    }
}

/// Open registry key, closed on drop.
pub struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

impl Key {
    fn open_with(hive: Hive, path: &str, access: REG_SAM_FLAGS) -> Option<Key> {
        let mut key = HKEY::default();
        let wpath = wide(path);
        unsafe { RegOpenKeyExW(hive.handle(), PCWSTR(wpath.as_ptr()), None, access, &mut key).ok().ok()? };
        Some(Key(key))
    }

    pub fn open(hive: Hive, path: &str, view: View) -> Option<Key> {
        Self::open_with(hive, path, KEY_READ | view.flag())
    }

    pub fn open_write(hive: Hive, path: &str, view: View) -> Option<Key> {
        Self::open_with(hive, path, KEY_READ | KEY_WRITE | view.flag())
    }

    /// Opens the key for writing, creating it and any missing parents.
    pub fn create(hive: Hive, path: &str, view: View) -> Option<Key> {
        let mut key = HKEY::default();
        let wpath = wide(path);
        unsafe {
            RegCreateKeyExW(
                hive.handle(),
                PCWSTR(wpath.as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE | view.flag(),
                None,
                &mut key,
                None,
            )
            .ok()
            .ok()?
        };
        Some(Key(key))
    }

    pub fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            let status =
                unsafe { RegEnumKeyExW(self.0, index, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) };
            if status.is_err() {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..len as usize]));
        }
        out
    }

    /// Raw type and bytes of a value.
    pub fn raw(&self, name: &str) -> Option<(u32, Vec<u8>)> {
        let wname = wide(name);
        let mut kind = REG_VALUE_TYPE::default();
        let mut len = 0u32;
        unsafe {
            RegQueryValueExW(self.0, PCWSTR(wname.as_ptr()), None, Some(&mut kind), None, Some(&mut len)).ok().ok()?;
            let mut data = vec![0u8; len as usize];
            RegQueryValueExW(self.0, PCWSTR(wname.as_ptr()), None, Some(&mut kind), Some(data.as_mut_ptr()), Some(&mut len))
                .ok()
                .ok()?;
            data.truncate(len as usize);
            Some((kind.0, data))
        }
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        self.raw(name).map(|(kind, data)| decode(kind, &data))
    }

    pub fn string(&self, name: &str) -> Option<String> {
        self.get(name)?.text()
    }

    pub fn dword(&self, name: &str) -> Option<u32> {
        match self.get(name)? {
            Value::Dword(d) => Some(d),
            _ => None,
        }
    }

    /// Names of all values, in registry order. The default value is "".
    pub fn value_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut name = vec![0u16; 16384];
            let mut len = name.len() as u32;
            let status =
                unsafe { RegEnumValueW(self.0, index, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) };
            if status.is_err() {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..len as usize]));
        }
        out
    }

    pub fn values(&self) -> Vec<(String, Value)> {
        self.value_names().into_iter().filter_map(|n| self.get(&n).map(|v| (n, v))).collect()
    }

    pub fn set_raw(&self, name: &str, kind: u32, data: &[u8]) -> bool {
        let wname = wide(name);
        unsafe { RegSetValueExW(self.0, PCWSTR(wname.as_ptr()), None, REG_VALUE_TYPE(kind), Some(data)).is_ok() }
    }

    pub fn set_string(&self, name: &str, value: &str) -> bool {
        let bytes: Vec<u8> = wide(value).into_iter().flat_map(u16::to_le_bytes).collect();
        self.set_raw(name, REG_SZ.0, &bytes)
    }

    pub fn set_dword(&self, name: &str, value: u32) -> bool {
        self.set_raw(name, REG_DWORD.0, &value.to_le_bytes())
    }

    pub fn delete_value(&self, name: &str) -> bool {
        let wname = wide(name);
        unsafe { RegDeleteValueW(self.0, PCWSTR(wname.as_ptr())).is_ok() }
    }

    /// Deletes the subkey `sub` and everything under it.
    pub fn delete_tree(&self, sub: &str) -> bool {
        let wsub = wide(sub);
        unsafe { RegDeleteTreeW(self.0, PCWSTR(wsub.as_ptr())).is_ok() }
    }

    fn sub(&self, name: &str, create: bool) -> Option<Key> {
        let wname = wide(name);
        let mut key = HKEY::default();
        unsafe {
            if create {
                RegCreateKeyExW(
                    self.0,
                    PCWSTR(wname.as_ptr()),
                    None,
                    None,
                    REG_OPTION_NON_VOLATILE,
                    KEY_READ | KEY_WRITE,
                    None,
                    &mut key,
                    None,
                )
                .ok()
                .ok()?;
            } else {
                RegOpenKeyExW(self.0, PCWSTR(wname.as_ptr()), None, KEY_READ, &mut key).ok().ok()?;
            }
        }
        Some(Key(key))
    }

    /// Copies this key's values and subkeys into `dest`. Done by hand because
    /// `RegCopyTreeW` refuses handles opened with an explicit registry view.
    pub fn copy_into(&self, dest: &Key) -> bool {
        let mut ok = true;
        for name in self.value_names() {
            ok &= self.raw(&name).is_some_and(|(kind, data)| dest.set_raw(&name, kind, &data));
        }
        for name in self.subkeys() {
            ok &= match (self.sub(&name, false), dest.sub(&name, true)) {
                (Some(from), Some(to)) => from.copy_into(&to),
                _ => false,
            };
        }
        ok
    }

    /// Copies the subkey `sub` (values and children) to a subkey of the same
    /// name under `dest`.
    pub fn copy_tree_to(&self, sub: &str, dest: &Key) -> bool {
        match (self.sub(sub, false), dest.sub(sub, true)) {
            (Some(from), Some(to)) => from.copy_into(&to),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_value_kinds() {
        let sz: Vec<u8> = "hi\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode(REG_SZ.0, &sz), Value::Str("hi".into()));
        let multi: Vec<u8> = "a\0bc\0\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode(REG_MULTI_SZ.0, &multi), Value::Multi(vec!["a".into(), "bc".into()]));
        assert_eq!(decode(REG_MULTI_SZ.0, &multi).text().unwrap(), "a; bc");
        assert_eq!(decode(REG_DWORD.0, &[2, 0, 0, 0]), Value::Dword(2));
        assert_eq!(decode(3, &[1, 2, 3]), Value::Other);
    }

    #[test]
    fn create_write_enumerate_copy_delete() {
        let base = format!(r"Software\SysCentral-test-{}", std::process::id());
        let key = Key::create(Hive::Hkcu, &format!(r"{base}\src\child"), View::Native).unwrap();
        assert!(key.set_string("name", "value") && key.set_dword("n", 7));
        assert_eq!(key.string("name").as_deref(), Some("value"));
        assert_eq!(key.dword("n"), Some(7));
        let mut names = key.value_names();
        names.sort();
        assert_eq!(names, ["n", "name"]);
        assert!(key.delete_value("n") && key.dword("n").is_none());

        let root = Key::open_write(Hive::Hkcu, &base, View::Native).unwrap();
        assert_eq!(root.subkeys(), ["src"]);
        let dest = Key::create(Hive::Hkcu, &format!(r"{base}\dst"), View::Native).unwrap();
        assert!(root.copy_tree_to("src", &dest));
        let copied = Key::open(Hive::Hkcu, &format!(r"{base}\dst\src\child"), View::Native).unwrap();
        assert_eq!(copied.string("name").as_deref(), Some("value"));

        drop((key, dest, copied, root));
        let software = Key::open_write(Hive::Hkcu, "Software", View::Native).unwrap();
        assert!(software.delete_tree(base.strip_prefix(r"Software\").unwrap()));
        assert!(Key::open(Hive::Hkcu, &base, View::Native).is_none());
    }
}
