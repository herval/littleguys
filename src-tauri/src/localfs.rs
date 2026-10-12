//! Read-only, root-confined browsing of a folder on this Mac, in the same
//! shapes as OpenClaw's `agents.workspace.list|get` (see runtime/mod.rs).
//!
//! - Paths are workspace-relative with forward slashes; the root is `""`.
//! - Absolute paths and `..` are refused up front; everything is then resolved
//!   against the canonical root, so symlinks can't lead outside it.
//! - Listings skip symlinks and anything that isn't a plain file or folder.
//! - Previews: UTF-8 text up to 256 KB, images up to 5 MB (base64). Nothing is
//!   ever written.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use base64::Engine;
use serde_json::{json, Value};

use crate::gateway::client::RpcError;

pub const DEFAULT_LIST_LIMIT: usize = 250;
pub const MAX_LIST_LIMIT: usize = 500;
/// Text previews (OpenClaw's `WORKSPACE_PREVIEW_MAX_BYTES`).
pub const MAX_TEXT_BYTES: u64 = 262_144;
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

const IMAGE_EXTENSIONS: &[(&str, &str)] = &[
    ("avif", "image/avif"),
    ("bmp", "image/bmp"),
    ("gif", "image/gif"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("jpeg", "image/jpeg"),
    ("jpg", "image/jpeg"),
    ("png", "image/png"),
    ("webp", "image/webp"),
];

/// A listing that says which folder it's of (for bots whose threads each work in their own).
pub fn with_root(mut out: Value, root: &Path) -> Value {
    if out.get("entries").is_some() {
        out["workspaceDir"] = json!(root.display().to_string());
    }
    out
}

/// `agents.workspace.list|get` with OpenClaw's params, rooted at `root`.
pub fn handle(root: &Path, agent_id: &str, method: &str, params: &Value) -> Result<Value, RpcError> {
    let rel = params.get("path").and_then(Value::as_str).unwrap_or_default();
    match method {
        "agents.workspace.list" => {
            let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            let limit = params.get("limit").and_then(Value::as_u64).map(|l| l as usize).unwrap_or(DEFAULT_LIST_LIMIT);
            list(root, agent_id, rel, offset, limit)
        }
        "agents.workspace.get" => get(root, agent_id, rel),
        other => Err(crate::runtime::unsupported(other)),
    }
}

/// One page of a folder: directories first, then by name.
pub fn list(root: &Path, agent_id: &str, rel: &str, offset: usize, limit: usize) -> Result<Value, RpcError> {
    let path = normalize(rel)?;
    let not_found = || error("workspace_path_not_found", "workspace directory not found", json!({ "path": path }));
    let dir = match resolve(root, &path)? {
        Some((target, meta)) if meta.is_dir() => target,
        _ => return Err(not_found()),
    };
    let reader = std::fs::read_dir(&dir).map_err(|_| not_found())?;
    let mut entries: Vec<(bool, String, Value)> = Vec::new();
    for entry in reader.flatten() {
        let Ok(name) = entry.file_name().into_string() else { continue };
        // `file_type`/`metadata` don't follow symlinks.
        let Ok(kind) = entry.file_type() else { continue };
        if !kind.is_dir() && !kind.is_file() {
            continue;
        }
        let meta = entry.metadata().ok();
        let mut row = json!({
            "path": if path.is_empty() { name.clone() } else { format!("{path}/{name}") },
            "name": name,
            "kind": if kind.is_dir() { "directory" } else { "file" },
        });
        if kind.is_file() {
            row["size"] = json!(meta.as_ref().map(|m| m.len()).unwrap_or(0));
        }
        if let Some(ms) = meta.as_ref().and_then(updated_at_ms) {
            row["updatedAtMs"] = json!(ms);
        }
        entries.push((kind.is_dir(), name, row));
    }
    entries.sort_by(|(a_dir, a, _), (b_dir, b, _)| b_dir.cmp(a_dir).then_with(|| a.to_lowercase().cmp(&b.to_lowercase())).then_with(|| a.cmp(b)));
    let total = entries.len();
    let offset = offset.min(total);
    let limit = limit.min(MAX_LIST_LIMIT);
    let page: Vec<Value> = entries.into_iter().skip(offset).take(limit).map(|(_, _, row)| row).collect();
    let mut result = json!({
        "agentId": agent_id,
        "path": path,
        "entries": page,
        "totalEntries": total,
        "offset": offset,
    });
    if !path.is_empty() {
        result["parentPath"] = json!(path.rsplit_once('/').map(|(parent, _)| parent).unwrap_or(""));
    }
    Ok(result)
}

/// One file's preview: UTF-8 text or a supported image.
pub fn get(root: &Path, agent_id: &str, rel: &str) -> Result<Value, RpcError> {
    let path = normalize(rel)?;
    let not_found = || error("workspace_file_not_found", "workspace file not found", json!({ "path": path }));
    if path.is_empty() {
        return Err(not_found());
    }
    let (target, meta) = match resolve(root, &path)? {
        Some((target, meta)) if meta.is_file() => (target, meta),
        _ => return Err(not_found()),
    };
    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
    let ext_mime = image_mime_for(&name);
    let max = if ext_mime.is_some() { MAX_IMAGE_BYTES } else { MAX_TEXT_BYTES };
    let too_large = |size: u64| {
        error(
            "workspace_file_too_large",
            "workspace file is too large to preview",
            json!({ "maxBytes": max, "path": path, "size": size }),
        )
    };
    if meta.len() > max {
        return Err(too_large(meta.len()));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    let file = std::fs::File::open(&target).map_err(|_| not_found())?;
    // The file may have grown since the stat.
    file.take(max + 1).read_to_end(&mut bytes).map_err(|_| not_found())?;
    if bytes.len() as u64 > max {
        return Err(too_large(bytes.len() as u64));
    }
    let unsupported =
        || error("workspace_file_unsupported", "workspace file is not UTF-8 text or a supported image", json!({ "path": path }));
    let (mime, encoding, content) = match ext_mime {
        Some(by_ext) => {
            let mime = sniff_image(&bytes).unwrap_or(by_ext);
            (mime, "base64", base64::engine::general_purpose::STANDARD.encode(&bytes))
        }
        None => {
            if bytes.contains(&0) {
                return Err(unsupported());
            }
            let text = String::from_utf8(bytes).map_err(|_| unsupported())?;
            ("text/plain", "utf8", text)
        }
    };
    Ok(json!({
        "agentId": agent_id,
        "file": {
            "path": path,
            "name": name,
            "size": meta.len(),
            "updatedAtMs": updated_at_ms(&meta),
            "mimeType": mime,
            "encoding": encoding,
            "content": content,
        },
    }))
}

// ---- helpers --------------------------------------------------------------------

fn error(kind: &str, message: &str, details: Value) -> RpcError {
    let mut e = RpcError::new("INVALID_REQUEST", message);
    let mut fields = json!({ "type": kind });
    if let (Some(f), Value::Object(extra)) = (fields.as_object_mut(), details) {
        f.extend(extra);
    }
    e.details = Some(fields);
    e
}

/// `rel` → canonical workspace-relative form (`a/b`, root = `""`), refusing
/// absolute paths and `..`.
fn normalize(rel: &str) -> Result<String, RpcError> {
    let portable = rel.replace('\\', "/");
    let bytes = rel.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if portable.starts_with('/') || drive {
        return Err(error("workspace_path_invalid", "path must be workspace-relative", json!({ "path": rel })));
    }
    let parts: Vec<&str> = portable.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    if parts.contains(&"..") {
        return Err(error("workspace_path_invalid", "path escapes the agent workspace", json!({ "path": rel })));
    }
    Ok(parts.join("/"))
}

/// `~` / `~/…` → the home folder.
fn expand_root(root: &Path) -> PathBuf {
    let Some(s) = root.to_str() else { return root.to_path_buf() };
    match (s, dirs::home_dir()) {
        ("~", Some(home)) => home,
        (s, Some(home)) if s.starts_with("~/") => home.join(&s[2..]),
        _ => root.to_path_buf(),
    }
}

/// The real location of `path` inside `root`, with its (non-followed)
/// metadata. `None` = doesn't exist, is a symlink, or the root is missing.
fn resolve(root: &Path, path: &str) -> Result<Option<(PathBuf, std::fs::Metadata)>, RpcError> {
    let Ok(root) = std::fs::canonicalize(expand_root(root)) else { return Ok(None) };
    if path.is_empty() {
        return Ok(std::fs::metadata(&root).ok().map(|m| (root, m)));
    }
    let joined = root.join(path);
    let Ok(own) = std::fs::symlink_metadata(&joined) else { return Ok(None) };
    let Ok(real) = std::fs::canonicalize(&joined) else { return Ok(None) };
    if !real.starts_with(&root) {
        return Err(error("workspace_path_invalid", "path escapes the agent workspace", json!({ "path": path })));
    }
    if own.file_type().is_symlink() {
        return Ok(None);
    }
    Ok(Some((real, own)))
}

fn updated_at_ms(meta: &std::fs::Metadata) -> Option<u64> {
    let modified = meta.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// The image mime type implied by a file name (SVG is text).
pub(crate) fn image_mime_for(name: &str) -> Option<&'static str> {
    let (_, ext) = name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    IMAGE_EXTENSIONS.iter().find(|(e, _)| *e == ext).map(|(_, mime)| *mime)
}

/// The image type from its magic bytes.
pub(crate) fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") && bytes.len() >= 14 {
        Some("image/bmp")
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        match &bytes[8..12] {
            b"avif" | b"avis" => Some("image/avif"),
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" => Some("image/heic"),
            b"mif1" | b"msf1" => Some("image/heif"),
            _ => None,
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("littleguys-localfs-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn detail_type(e: &RpcError) -> String {
        e.details.as_ref().and_then(|d| d["type"].as_str()).unwrap_or_default().to_string()
    }

    fn names(listing: &Value) -> Vec<String> {
        listing["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn lists_dirs_first_then_by_name() {
        let ws = Scratch::new();
        ws.write("b.txt", b"bee");
        ws.write("A.md", b"# a");
        ws.write(".hidden", b"h");
        ws.write("zeta/inner.txt", b"x");
        ws.write("Alpha/inner.txt", b"x");
        let out = list(&ws.0, "bot", "", 0, DEFAULT_LIST_LIMIT).unwrap();
        assert_eq!(names(&out), vec!["Alpha", "zeta", ".hidden", "A.md", "b.txt"]);
        assert_eq!(out["agentId"], "bot");
        assert_eq!(out["path"], "");
        assert!(out.get("parentPath").is_none());
        assert_eq!(out["totalEntries"], 5);
        assert_eq!(out["offset"], 0);
        let dir = &out["entries"][0];
        assert_eq!(dir["path"], "Alpha");
        assert_eq!(dir["kind"], "directory");
        assert!(dir.get("size").is_none());
        assert!(dir["updatedAtMs"].as_u64().is_some());
        let file = &out["entries"][4];
        assert_eq!(file["kind"], "file");
        assert_eq!(file["size"], 3);
    }

    #[test]
    fn nested_paths_and_parent_paths() {
        let ws = Scratch::new();
        ws.write("a/b/c.txt", b"c");
        let top = list(&ws.0, "bot", "a", 0, 10).unwrap();
        assert_eq!(top["path"], "a");
        assert_eq!(top["parentPath"], "");
        assert_eq!(top["entries"][0]["path"], "a/b");
        let nested = list(&ws.0, "bot", "./a//b/", 0, 10).unwrap();
        assert_eq!(nested["path"], "a/b");
        assert_eq!(nested["parentPath"], "a");
        assert_eq!(nested["entries"][0]["path"], "a/b/c.txt");
        let err = list(&ws.0, "bot", "a/b/c.txt", 0, 10).unwrap_err();
        assert_eq!(detail_type(&err), "workspace_path_not_found");
        let err = list(&ws.0, "bot", "nope", 0, 10).unwrap_err();
        assert_eq!(err.code, "INVALID_REQUEST");
        assert_eq!(detail_type(&err), "workspace_path_not_found");
        assert_eq!(err.details.as_ref().unwrap()["path"], "nope");
    }

    #[test]
    fn pages_through_entries() {
        let ws = Scratch::new();
        for i in 0..7 {
            ws.write(&format!("f{i}.txt"), b"x");
        }
        let page = list(&ws.0, "bot", "", 2, 3).unwrap();
        assert_eq!(names(&page), vec!["f2.txt", "f3.txt", "f4.txt"]);
        assert_eq!(page["totalEntries"], 7);
        assert_eq!(page["offset"], 2);
        let past = list(&ws.0, "bot", "", 99, 3).unwrap();
        assert_eq!(past["offset"], 7);
        assert!(past["entries"].as_array().unwrap().is_empty());
        let capped = handle(&ws.0, "bot", "agents.workspace.list", &json!({ "limit": 10_000 })).unwrap();
        assert_eq!(capped["entries"].as_array().unwrap().len(), 7);
    }

    #[test]
    fn refuses_absolute_paths_and_dotdot() {
        let ws = Scratch::new();
        ws.write("a/x.txt", b"x");
        for bad in ["/etc", "\\etc", "C:\\Windows", "..", "a/../..", "a/../a"] {
            let err = list(&ws.0, "bot", bad, 0, 10).unwrap_err();
            assert_eq!(detail_type(&err), "workspace_path_invalid", "{bad}");
            let err = get(&ws.0, "bot", bad).unwrap_err();
            assert_eq!(detail_type(&err), "workspace_path_invalid", "{bad}");
        }
    }

    #[test]
    fn symlinks_are_skipped_and_cant_escape() {
        let ws = Scratch::new();
        let outside = Scratch::new();
        outside.write("secret.txt", b"shh");
        ws.write("real.txt", b"ok");
        std::os::unix::fs::symlink(&outside.0, ws.0.join("out")).unwrap();
        std::os::unix::fs::symlink(outside.0.join("secret.txt"), ws.0.join("secret.txt")).unwrap();
        std::os::unix::fs::symlink(ws.0.join("real.txt"), ws.0.join("inside-link.txt")).unwrap();
        let out = list(&ws.0, "bot", "", 0, 10).unwrap();
        assert_eq!(names(&out), vec!["real.txt"]);
        assert_eq!(detail_type(&list(&ws.0, "bot", "out", 0, 10).unwrap_err()), "workspace_path_invalid");
        assert_eq!(detail_type(&get(&ws.0, "bot", "out/secret.txt").unwrap_err()), "workspace_path_invalid");
        assert_eq!(detail_type(&get(&ws.0, "bot", "secret.txt").unwrap_err()), "workspace_path_invalid");
        assert_eq!(detail_type(&get(&ws.0, "bot", "inside-link.txt").unwrap_err()), "workspace_file_not_found");
        // A symlinked root is fine: it's resolved first.
        let link_root = outside.0.join("ws-link");
        std::os::unix::fs::symlink(&ws.0, &link_root).unwrap();
        assert_eq!(get(&link_root, "bot", "real.txt").unwrap()["file"]["content"], "ok");
    }

    #[test]
    fn reads_text_images_and_refuses_the_rest() {
        let ws = Scratch::new();
        ws.write("notes/readme.md", "héllo".as_bytes());
        ws.write("logo.svg", b"<svg/>");
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        ws.write("pic.png", png);
        ws.write("photo.JPG", &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]);
        ws.write("mislabeled.gif", png);
        ws.write("blob.bin", &[0, 159, 146, 150]);
        ws.write("latin1.txt", &[0x68, 0xE9]);
        ws.write("big.txt", &vec![b'a'; MAX_TEXT_BYTES as usize + 1]);
        ws.write("ok.txt", &vec![b'a'; MAX_TEXT_BYTES as usize]);

        let text = get(&ws.0, "bot", "notes/readme.md").unwrap();
        let file = &text["file"];
        assert_eq!(text["agentId"], "bot");
        assert_eq!(file["path"], "notes/readme.md");
        assert_eq!(file["name"], "readme.md");
        assert_eq!(file["size"], 6);
        assert_eq!(file["mimeType"], "text/plain");
        assert_eq!(file["encoding"], "utf8");
        assert_eq!(file["content"], "héllo");
        assert!(file["updatedAtMs"].as_u64().is_some());
        assert_eq!(get(&ws.0, "bot", "logo.svg").unwrap()["file"]["mimeType"], "text/plain");

        let image = get(&ws.0, "bot", "pic.png").unwrap();
        assert_eq!(image["file"]["mimeType"], "image/png");
        assert_eq!(image["file"]["encoding"], "base64");
        assert_eq!(image["file"]["content"], base64::engine::general_purpose::STANDARD.encode(png));
        assert_eq!(get(&ws.0, "bot", "photo.JPG").unwrap()["file"]["mimeType"], "image/jpeg");
        assert_eq!(get(&ws.0, "bot", "mislabeled.gif").unwrap()["file"]["mimeType"], "image/png");

        for bad in ["blob.bin", "latin1.txt"] {
            assert_eq!(detail_type(&get(&ws.0, "bot", bad).unwrap_err()), "workspace_file_unsupported", "{bad}");
        }
        let err = get(&ws.0, "bot", "big.txt").unwrap_err();
        assert_eq!(detail_type(&err), "workspace_file_too_large");
        assert_eq!(err.details.as_ref().unwrap()["maxBytes"], MAX_TEXT_BYTES);
        assert!(get(&ws.0, "bot", "ok.txt").is_ok());
        assert_eq!(detail_type(&get(&ws.0, "bot", "notes").unwrap_err()), "workspace_file_not_found");
        assert_eq!(detail_type(&get(&ws.0, "bot", "").unwrap_err()), "workspace_file_not_found");
        assert_eq!(detail_type(&get(&ws.0, "bot", "missing.txt").unwrap_err()), "workspace_file_not_found");
    }

    #[test]
    fn missing_root_is_not_found() {
        let missing = std::env::temp_dir().join(format!("littleguys-missing-{}", uuid::Uuid::new_v4()));
        assert_eq!(detail_type(&list(&missing, "bot", "", 0, 10).unwrap_err()), "workspace_path_not_found");
    }

    #[test]
    fn sniffs_image_types() {
        assert_eq!(sniff_image(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image(b"\0\0\0\x18ftypheic\0\0"), Some("image/heic"));
        assert_eq!(sniff_image(b"\0\0\0\x18ftypavif\0\0"), Some("image/avif"));
        assert_eq!(sniff_image(b"hello"), None);
        assert_eq!(image_mime_for("x.HEIF"), Some("image/heif"));
        assert_eq!(image_mime_for("x.svg"), None);
        assert_eq!(image_mime_for("png"), None);
    }
}
