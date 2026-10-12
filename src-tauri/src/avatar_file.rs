//! A guy's picture as a file in their OpenClaw workspace, which OpenClaw's own
//! `identity.avatar` can point at. A workspace-relative path keeps the image
//! itself out of IDENTITY.md. Only for gateways on this Mac: the gateway API
//! can't receive files.

use std::path::Path;

use base64::Engine;

const DIR: &str = "avatars";
/// Our own file name, so we never overwrite (or delete) a picture someone else put there.
const STEM: &str = "littleguys";
const KINDS: &[(&str, &str)] = &[
    ("data:image/png;base64,", "png"),
    ("data:image/jpeg;base64,", "jpg"),
    ("data:image/webp;base64,", "webp"),
];

/// Writes `face` (a `data:image/…` URL) as `avatars/littleguys.<ext>` and
/// returns that path, relative to the workspace.
pub fn write(workspace: &Path, face: &str) -> anyhow::Result<String> {
    let (prefix, ext) = KINDS
        .iter()
        .find(|(prefix, _)| face.starts_with(prefix))
        .ok_or_else(|| anyhow::anyhow!("That picture can't be used."))?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(&face[prefix.len()..])?;
    remove(workspace)?;
    let dir = workspace.join(DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(format!("{STEM}.{ext}")), bytes)?;
    Ok(format!("{DIR}/{STEM}.{ext}"))
}

/// Deletes the pictures littleguys wrote, and nothing else.
pub fn remove(workspace: &Path) -> anyhow::Result<()> {
    for (_, ext) in KINDS {
        match std::fs::remove_file(workspace.join(DIR).join(format!("{STEM}.{ext}"))) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_our_own_file_and_only_ever_deletes_that() {
        let ws = std::env::temp_dir().join(format!("lg-avatar-{}", std::process::id()));
        std::fs::create_dir_all(ws.join(DIR)).unwrap();
        std::fs::write(ws.join(DIR).join("fox.png"), b"theirs").unwrap();

        let png = write(&ws, "data:image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(png, "avatars/littleguys.png");
        assert_eq!(std::fs::read(ws.join(&png)).unwrap(), b"\x89PNG\r\n\x1a\n");
        // A new picture in another format replaces ours.
        assert_eq!(write(&ws, "data:image/jpeg;base64,/9j/").unwrap(), "avatars/littleguys.jpg");
        assert!(!ws.join(&png).exists());

        remove(&ws).unwrap();
        assert!(!ws.join("avatars/littleguys.jpg").exists());
        assert!(ws.join(DIR).join("fox.png").exists(), "someone else's picture stays");
        assert!(write(&ws, "data:image/svg+xml;base64,PHN2Zz4=").is_err());
        std::fs::remove_dir_all(&ws).unwrap();
    }
}
