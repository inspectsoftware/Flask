//! Priority and affinity remembered per program and put back whenever that
//! program starts. Rules are applied only while Flask is running.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sc_core::actions::{self, Priority};
use sc_core::reg::{Hive, Key, View};

// Under HKLM, not HKCU: Flask runs elevated, and a rule that any unelevated
// program could write would let it have Flask raise its priority.
const KEY: &str = r"Software\SysCentral\Flask\Rules";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rule {
    pub priority: Option<Priority>,
    pub affinity: Option<usize>,
}

/// Rules by [`key`], shared with the sampler thread that applies them.
pub type Rules = Arc<Mutex<HashMap<String, Rule>>>;

/// How a program is identified: its full path, case ignored.
pub fn key(image: &str) -> String {
    image.to_lowercase()
}

fn text(rule: Rule) -> String {
    let mut parts = Vec::new();
    if let Some(p) = rule.priority {
        parts.push(format!("priority={}", p.label()));
    }
    if let Some(mask) = rule.affinity {
        parts.push(format!("affinity={mask:x}"));
    }
    parts.join(";")
}

/// Anything unrecognized is dropped, leaving that half of the rule unset.
fn parse(text: &str) -> Rule {
    let mut rule = Rule::default();
    for part in text.split(';') {
        match part.split_once('=') {
            Some(("priority", v)) => rule.priority = Priority::ALL.into_iter().find(|p| p.label() == v),
            Some(("affinity", v)) => rule.affinity = usize::from_str_radix(v, 16).ok().filter(|&mask| mask != 0),
            _ => {}
        }
    }
    rule
}

pub fn load() -> Rules {
    let mut rules = HashMap::new();
    if let Some(k) = Key::open(Hive::Hklm, KEY, View::Native) {
        for (name, value) in k.values() {
            let rule = value.text().as_deref().map(parse).unwrap_or_default();
            if rule != Rule::default() {
                rules.insert(key(&name), rule);
            }
        }
    }
    Arc::new(Mutex::new(rules))
}

/// Saves the rule for `image`, or forgets it for `None`. False when the
/// registry refused, which leaves the rules as they were.
pub fn set(rules: &Rules, image: &str, rule: Option<Rule>) -> bool {
    let name = key(image);
    let Some(k) = Key::create(Hive::Hklm, KEY, View::Native) else { return false };
    match rule {
        Some(rule) if k.set_string(&name, &text(rule)) => {
            rules.lock().unwrap().insert(name, rule);
            true
        }
        Some(_) => false,
        None => {
            k.delete_value(&name);
            rules.lock().unwrap().remove(&name);
            true
        }
    }
}

/// Best effort: a process that has already exited, or refuses, is left alone.
pub fn apply(rule: Rule, pid: u32) {
    if let Some(priority) = rule.priority {
        let _ = actions::set_priority(pid, priority);
    }
    if let Some(mask) = rule.affinity
        && let Some((_, system)) = actions::affinity(pid)
        // Processors the rule names may be gone on this machine.
        && mask & system != 0
    {
        let _ = actions::set_affinity(pid, mask & system);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_round_trip_and_junk_is_dropped() {
        for rule in [
            Rule { priority: Some(Priority::High), affinity: Some(0xF0) },
            Rule { priority: Some(Priority::BelowNormal), affinity: None },
            Rule { priority: None, affinity: Some(1) },
        ] {
            assert_eq!(parse(&text(rule)), rule);
        }
        assert_eq!(parse("priority=Ludicrous;affinity=zz;nonsense;=;affinity=0"), Rule::default());
        assert_eq!(parse(""), Rule::default());
        assert_eq!(key(r"C:\Apps\Game.EXE"), r"c:\apps\game.exe");
    }
}
