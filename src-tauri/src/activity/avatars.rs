//! Local avatar validation, storage and roster decoration.
use super::preferences::Persisted;
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path};
/// Photos are small images the app made itself (see src/lib/avatarImage.ts).
pub(super) const MAX_FACE_CHARS: usize = 100_000;

pub(crate) fn is_face(url: &str) -> bool {
    url.len() <= MAX_FACE_CHARS
        && ["data:image/png;base64,", "data:image/jpeg;base64,", "data:image/webp;base64,"].iter().any(|p| url.starts_with(p))
}

/// A photo picked in littleguys wins over the face the bot gives a guy.
pub(super) fn apply_faces(agents: &mut [Value], gateway: &str, faces: &HashMap<String, String>) {
    if faces.is_empty() {
        return;
    }
    for agent in agents.iter_mut().filter(|a| a.is_object()) {
        let Some(face) = agent.get("id").and_then(Value::as_str).and_then(|id| faces.get(&format!("{gateway}/{id}"))) else {
            continue;
        };
        if !agent.get("identity").is_some_and(Value::is_object) {
            agent["identity"] = json!({});
        }
        agent["identity"]["avatarUrl"] = json!(face);
    }
}

/// Which guys' faces are photos picked in littleguys (`apply_faces` put them there).
pub(super) fn mark_faces(agents: &mut [Value], gateway: &str, faces: &HashMap<String, String>) {
    for agent in agents.iter_mut().filter(|a| a.is_object()) {
        if agent.get("id").and_then(Value::as_str).is_some_and(|id| faces.contains_key(&format!("{gateway}/{id}"))) {
            agent["littleguysFace"] = json!(true);
        }
    }
}

pub(super) struct AvatarStore {
    store: Persisted<HashMap<String, String>>,
}
impl AvatarStore {
    pub fn new(dir: Option<&Path>) -> Self {
        Self { store: Persisted::new(dir.map(|d| d.join("faces.json"))) }
    }
    pub fn snapshot(&self) -> HashMap<String, String> {
        self.store.snapshot().into_iter().filter(|(_, face)| is_face(face)).collect()
    }
    pub fn set(&self, uid: &str, face: Option<String>) -> anyhow::Result<()> {
        if let Some(face) = &face {
            anyhow::ensure!(is_face(face), "That photo can't be used.");
        }
        self.store.edit(|faces| {
            match face {
                Some(face) => faces.insert(uid.to_string(), face),
                None => faces.remove(uid),
            };
        })
    }
}
