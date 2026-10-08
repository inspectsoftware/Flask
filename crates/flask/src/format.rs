use std::fmt::Write;

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;

/// Appends a byte count the way Task Manager shows memory.
pub fn bytes(out: &mut String, n: u64) {
    let _ = if n < MB {
        write!(out, "{} K", n.div_ceil(KB))
    } else if n < GB {
        write!(out, "{:.1} MB", n as f64 / MB as f64)
    } else {
        write!(out, "{:.2} GB", n as f64 / GB as f64)
    };
}

/// Appends a transfer rate, or nothing for zero so idle rows stay quiet.
pub fn rate(out: &mut String, bytes_per_sec: u64) {
    if bytes_per_sec > 0 {
        bytes(out, bytes_per_sec);
        out.push_str("/s");
    }
}

pub fn priority(out: &mut String, base: i32) {
    let label = match base {
        4 => "Low",
        6 => "Below normal",
        8 => "Normal",
        10 => "Above normal",
        13 => "High",
        24 => "Realtime",
        _ => {
            let _ = write!(out, "{base}");
            return;
        }
    };
    out.push_str(label);
}

/// Case-insensitive substring test without allocating. `needle` must already
/// be lowercase.
pub fn contains_ci(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    hay.char_indices().any(|(i, _)| {
        let mut h = hay[i..].chars().flat_map(char::to_lowercase);
        needle.chars().all(|n| h.next() == Some(n))
    })
}

/// Case-insensitive ordering without allocating.
pub fn cmp_ci(a: &str, b: &str) -> std::cmp::Ordering {
    a.chars().flat_map(char::to_lowercase).cmp(b.chars().flat_map(char::to_lowercase))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(f: impl Fn(&mut String)) -> String {
        let mut out = String::new();
        f(&mut out);
        out
    }

    #[test]
    fn byte_units() {
        assert_eq!(s(|o| bytes(o, 0)), "0 K");
        assert_eq!(s(|o| bytes(o, 1)), "1 K");
        assert_eq!(s(|o| bytes(o, 500 * KB)), "500 K");
        assert_eq!(s(|o| bytes(o, 3 * MB / 2)), "1.5 MB");
        assert_eq!(s(|o| bytes(o, 5 * GB / 2)), "2.50 GB");
    }

    #[test]
    fn rates_and_priorities() {
        assert_eq!(s(|o| rate(o, 0)), "");
        assert_eq!(s(|o| rate(o, 2 * MB)), "2.0 MB/s");
        assert_eq!(s(|o| priority(o, 8)), "Normal");
        assert_eq!(s(|o| priority(o, 9)), "9");
    }

    #[test]
    fn case_insensitive_helpers() {
        assert!(contains_ci("Chrome.EXE", "chrome"));
        assert!(contains_ci("C:\\Program Files\\App", "files\\app"));
        assert!(contains_ci("ÜBER.exe", "über"));
        assert!(!contains_ci("notepad", "notepad++"));
        assert!(contains_ci("anything", ""));
        assert_eq!(cmp_ci("alpha", "ALPHA"), std::cmp::Ordering::Equal);
        assert_eq!(cmp_ci("Beta", "alpha"), std::cmp::Ordering::Greater);
    }
}
