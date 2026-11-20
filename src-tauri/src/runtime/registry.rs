//! The only owner of live runtime handles. Callers receive clones, never a second registry.
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

pub struct Registry<T> {
    next: AtomicU64,
    entries: Mutex<Vec<(String, u64, T)>>,
}
impl<T: Clone> Default for Registry<T> {
    fn default() -> Self {
        Self { next: AtomicU64::new(1), entries: Mutex::new(Vec::new()) }
    }
}
impl<T: Clone> Registry<T> {
    pub fn next_generation(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
    pub fn insert(&self, id: String, generation: u64, value: T) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.iter_mut().find(|e| e.0 == id) {
            *entry = (id, generation, value);
        } else {
            entries.push((id, generation, value));
        }
    }
    pub fn remove(&self, id: &str) {
        self.entries.lock().unwrap().retain(|e| e.0 != id);
    }
    pub fn get(&self, id: &str) -> Option<(u64, T)> {
        self.entries.lock().unwrap().iter().find(|e| e.0 == id).map(|e| (e.1, e.2.clone()))
    }
    pub fn all(&self) -> Vec<T> {
        self.entries.lock().unwrap().iter().map(|e| e.2.clone()).collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacing_or_removing_a_runtime_invalidates_old_handles() {
        let r = Registry::default();
        let first = r.next_generation();
        r.insert("bot".into(), first, "old");
        let second = r.next_generation();
        r.insert("bot".into(), second, "new");
        assert_ne!(first, second);
        assert_eq!(r.all(), vec!["new"]);
        assert_eq!(r.get("bot"), Some((second, "new")));
        r.remove("bot");
        assert_eq!(r.get("bot"), None);
    }
}
