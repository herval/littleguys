use serde::Serialize;
use std::collections::BTreeMap;
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Prefs {
    #[serde(default)]
    pub hidden_guys: Vec<String>,
    #[serde(default)]
    pub notifications: Notifications,
    /// Settings → Coder mode (code.rs).
    #[serde(default)]
    pub coder: crate::code::CoderPrefs,
}

/// What notifies you: everyone's setting, and the guys (`gatewayId/agentId`) set otherwise.
#[derive(Debug, Clone, Default, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notifications {
    #[serde(default)]
    pub everyone: NotifyPrefs,
    #[serde(default)]
    pub guys: BTreeMap<String, NotifyPrefs>,
}

impl Notifications {
    pub fn for_guy(&self, uid: &str) -> NotifyPrefs {
        self.guys.get(uid).copied().unwrap_or(self.everyone)
    }

    /// Set one guy, or (`None`) everyone, which resets every guy to it.
    pub(super) fn set(&mut self, uid: Option<&str>, prefs: NotifyPrefs) {
        match uid {
            None => {
                self.everyone = prefs;
                self.guys.clear();
            }
            Some(uid) if prefs == self.everyone => {
                self.guys.remove(uid);
            }
            Some(uid) => {
                self.guys.insert(uid.to_string(), prefs);
            }
        }
    }
}

/// What notifies you about one guy. Approvals always do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyPrefs {
    /// Their replies in the main chat and littleguys threads.
    pub replies: bool,
    pub routines: RoutineNotifications,
}

impl Default for NotifyPrefs {
    fn default() -> Self {
        Self { replies: true, routines: RoutineNotifications::All }
    }
}

/// Which finished routine runs notify.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RoutineNotifications {
    #[default]
    All,
    Failing,
    Off,
}

/// Keep the last readable state on disk and commit to memory only after a successful write.
pub(super) struct Persisted<T> {
    path: Option<PathBuf>,
    state: Mutex<(T, Option<String>)>,
}
impl<T: Clone + Default + Serialize + serde::de::DeserializeOwned> Persisted<T> {
    pub fn new(path: Option<PathBuf>) -> Self {
        let loaded = path
            .as_ref()
            .map(|path| match std::fs::read(path) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
                Err(e) => Err(e.to_string()),
            })
            .unwrap_or_else(|| Err("no app data dir".into()));
        let (value, error) = match loaded {
            Ok(value) => (value, None),
            Err(e) => (T::default(), Some(e)),
        };
        Self { path, state: Mutex::new((value, error)) }
    }
    pub fn snapshot(&self) -> T {
        self.state.lock().unwrap().0.clone()
    }
    pub fn edit(&self, edit: impl FnOnce(&mut T)) -> anyhow::Result<()> {
        let mut saved = self.state.lock().unwrap();
        if let Some(error) = &saved.1 {
            anyhow::bail!("Couldn't read settings safely: {error}");
        }
        let mut next = saved.0.clone();
        edit(&mut next);
        let path = self.path.as_ref().ok_or_else(|| anyhow::anyhow!("no app data dir"))?;
        crate::gateway::identity::write_private(path, &serde_json::to_vec_pretty(&next)?)?;
        saved.0 = next;
        Ok(())
    }
}
pub(super) struct Preferences {
    store: Persisted<Prefs>,
}
impl Preferences {
    pub fn new(dir: Option<&Path>) -> Self {
        Self { store: Persisted::new(dir.map(|d| d.join("prefs.json"))) }
    }
    pub fn snapshot(&self) -> Prefs {
        self.store.snapshot()
    }
    pub fn set_hidden(&self, uid: &str, hidden: bool) -> anyhow::Result<()> {
        self.store.edit(|prefs| {
            prefs.hidden_guys.retain(|id| id != uid);
            if hidden {
                prefs.hidden_guys.push(uid.to_string());
                prefs.hidden_guys.sort();
            }
        })
    }
    pub fn set_notifications(&self, uid: Option<&str>, prefs: NotifyPrefs) -> anyhow::Result<()> {
        self.store.edit(|saved| saved.notifications.set(uid, prefs))
    }
    pub fn set_coder(&self, coder: crate::code::CoderPrefs) -> anyhow::Result<()> {
        self.store.edit(|saved| saved.coder = coder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_write_does_not_commit_preferences_and_can_be_retried() {
        let root = std::env::temp_dir().join(format!("lg-prefs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let store = Preferences::new(Some(&root));
        std::fs::create_dir(root.join("prefs.json")).unwrap();
        assert!(store.set_hidden("g/a", true).is_err());
        assert!(store.snapshot().hidden_guys.is_empty());
        std::fs::remove_dir(root.join("prefs.json")).unwrap();
        store.set_hidden("g/a", true).unwrap();
        store.set_notifications(None, NotifyPrefs { replies: false, routines: RoutineNotifications::Off }).unwrap();
        let saved = Preferences::new(Some(&root)).snapshot();
        assert_eq!(saved.hidden_guys, vec!["g/a"]);
        assert!(!saved.notifications.everyone.replies);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn corrupt_preferences_are_not_overwritten_with_defaults() {
        let root = std::env::temp_dir().join(format!("lg-prefs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("prefs.json"), "{broken").unwrap();
        let store = Preferences::new(Some(&root));
        assert!(store.set_hidden("g/a", true).is_err());
        assert_eq!(std::fs::read_to_string(root.join("prefs.json")).unwrap(), "{broken");
        std::fs::remove_dir_all(root).unwrap();
    }
}
