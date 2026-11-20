//! Completion accounting independent of the tray's running indicator.
use std::collections::{HashMap, VecDeque};
#[derive(Default)]
pub(super) struct Turns {
    active: HashMap<String, Option<String>>,
    completed: VecDeque<(String, String)>,
}
impl Turns {
    pub fn start(&mut self, key: &str, run: Option<&str>) -> bool {
        if run.is_some_and(|id| self.completed.iter().any(|(k, r)| k == key && r == id)) {
            return false;
        }
        if let (Some(Some(active)), Some(run)) = (self.active.get(key), run) {
            if active != run {
                let old = active.clone();
                self.remember(key, &old);
            }
        }
        let current = self.active.entry(key.to_string()).or_default();
        if let Some(run) = run {
            *current = Some(run.to_string());
        }
        true
    }
    pub fn finish(&mut self, key: &str, run: Option<&str>) -> bool {
        if run.is_some_and(|id| self.completed.iter().any(|(k, r)| k == key && r == id)) {
            return false;
        }
        if let (Some(Some(active)), Some(run)) = (self.active.get(key), run) {
            if active != run {
                self.remember(key, run);
                return false;
            }
        }
        let Some(active) = self.active.remove(key) else {
            if let Some(run) = run {
                self.remember(key, run);
            }
            return false;
        };
        if let Some(id) = run.or(active.as_deref()) {
            self.remember(key, id);
        }
        true
    }
    fn remember(&mut self, key: &str, run: &str) {
        self.completed.push_back((key.to_string(), run.to_string()));
        while self.completed.len() > 2048 {
            self.completed.pop_front();
        }
    }
    pub fn disconnect(&mut self) {
        self.active.clear();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_end_orders_notify_once() {
        for first in [true, false] {
            let mut t = Turns::default();
            t.start("s", None);
            t.start("s", Some("r"));
            assert!(t.finish("s", if first { Some("r") } else { None }));
            assert!(!t.finish("s", if first { None } else { Some("r") }));
            assert!(!t.start("s", Some("r")));
        }
    }
    #[test]
    fn old_completion_cannot_end_new_turn() {
        let mut t = Turns::default();
        t.start("s", Some("new"));
        assert!(!t.finish("s", Some("old")));
        assert!(t.finish("s", Some("new")));
    }
    #[test]
    fn reconnect_and_unobserved_completion_do_not_notify() {
        let mut t = Turns::default();
        t.start("s", Some("r"));
        t.disconnect();
        assert!(!t.finish("s", Some("r")));
        assert!(!t.finish("unknown", None));
    }
    #[test]
    fn legacy_turns_can_run_again() {
        let mut t = Turns::default();
        for _ in 0..2 {
            assert!(t.start("s", None));
            assert!(t.finish("s", None));
            assert!(!t.finish("s", None));
        }
    }
}
