//! Files a guy points at by path on its own machine (`MEDIA:/…` in a reply,
//! or a path it mentions), and saving chat attachments to ~/Downloads.
//!
//! OpenClaw serves them from `/__openclaw__/assistant-media`: an authorized
//! `meta=1` request says whether the file can be shown and mints a short-lived
//! media ticket; the ticketed URL then works without credentials, so the
//! webview can put it straight into an `<img>` or `<video>`.
//!
//! Guys that run on this Mac (Claude Code, local ACP and Hermes) have their
//! files right here: an image, video or audio file is read from disk, through
//! the asset protocol for the webview. Only files asked for this way are in
//! its scope, and only media: a path to anything else is never served.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{Manager, State};

use crate::gateway::client::RpcError;
use crate::hub::Hub;

const ROUTE: &str = "/__openclaw__/assistant-media";

/// `{available, url?, path?, mimeType?, sizeBytes?, expiresAt?, code?, reason?}`;
/// `path` (no `url`) for a file on this Mac, which the webview loads with `convertFileSrc`.
#[tauri::command]
pub async fn gw_media(
    app: tauri::AppHandle,
    hub: State<'_, Arc<Hub>>,
    gateway: String,
    source: String,
    session_key: Option<String>,
    agent_id: Option<String>,
) -> Result<Value, RpcError> {
    let runtime = hub.client(&gateway).ok_or_else(|| RpcError::new("UNKNOWN_GATEWAY", format!("no gateway {gateway}")))?;
    let info = media_url(&runtime, source, session_key, agent_id).await?;
    if let Some(path) = info["path"].as_str() {
        app.asset_protocol_scope().allow_file(path).map_err(|e| RpcError::new("UNAVAILABLE", e.to_string()))?;
    }
    Ok(info)
}

/// The meta request and ticket behind `gw_media` (or the file on this Mac);
/// the bridge downloads the URL, or reads the file, for phones.
pub(crate) async fn media_url(
    runtime: &crate::runtime::Runtime,
    source: String,
    session_key: Option<String>,
    agent_id: Option<String>,
) -> Result<Value, RpcError> {
    let Some(client) = runtime.as_openclaw() else {
        let folder = runtime
            .local_folder(session_key.as_deref(), agent_id.as_deref())
            .await
            .ok_or_else(|| RpcError::new("UNSUPPORTED", "this guy's files are on another computer"))?;
        return crate::runtime::blocking(move || Ok(local_media(&folder, &source))).await;
    };
    let origin = client.http_origin().ok_or_else(|| RpcError::new("NOT_CONNECTED", "not connected to the gateway"))?;

    let mut query: Vec<(&str, String)> = vec![("source", source)];
    if let Some(key) = session_key.filter(|k| !k.is_empty()) {
        query.push(("sessionKey", key));
        if let Some(agent) = agent_id.filter(|a| !a.is_empty()) {
            query.push(("agentId", agent));
        }
    }

    let mut meta = reqwest::Client::new()
        .get(format!("{origin}{ROUTE}"))
        .query(&query)
        .query(&[("meta", "1")])
        .timeout(Duration::from_secs(15));
    if let Some(token) = client.http_bearer() {
        meta = meta.bearer_auth(token);
    }
    let res = meta.send().await.map_err(|e| RpcError::new("UNAVAILABLE", e.to_string()))?;
    if !res.status().is_success() {
        return Ok(json!({ "available": false, "code": "http", "reason": format!("the gateway answered {}", res.status()) }));
    }
    let info: Value = res.json().await.map_err(|e| RpcError::new("BAD_RESPONSE", e.to_string()))?;
    let Some(ticket) = info["mediaTicket"].as_str().filter(|_| info["available"].as_bool() == Some(true)) else {
        return Ok(json!({ "available": false, "code": info["code"], "reason": info["reason"] }));
    };

    query.push(("mediaTicket", ticket.to_string()));
    // Formats the webview can't play get transcoded by the gateway on request.
    if info["playback"].as_str().is_some_and(|p| p != "native") {
        query.push(("playback", "1".to_string()));
    }
    let url = reqwest::Url::parse_with_params(&format!("{origin}{ROUTE}"), &query).map_err(|e| RpcError::new("BAD_RESPONSE", e.to_string()))?;
    Ok(json!({
        "available": true,
        "url": url.as_str(),
        "mimeType": info["mimeType"],
        "sizeBytes": info["sizeBytes"],
        "expiresAt": info["mediaTicketExpiresAt"],
    }))
}

/// A file on this Mac, if it's media the chat can show. Relative paths start at `folder`.
fn local_media(folder: &Path, source: &str) -> Value {
    let unavailable = |code: &str, reason: &str| json!({ "available": false, "code": code, "reason": reason });
    let Some(path) = local_path(folder, source) else {
        return unavailable("invalid", "that isn't a file path");
    };
    // The real file (not a link to it) is what's served, so that's what must be media.
    let Some(path) = std::fs::canonicalize(&path).ok().filter(|p| p.is_file()) else {
        return unavailable("not_found", "the file isn't there");
    };
    let Some(mime) = media_mime(&path) else {
        return unavailable("unsupported", "only images, videos and audio show in the chat");
    };
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or_default();
    json!({ "available": true, "path": path.display().to_string(), "mimeType": mime, "sizeBytes": size })
}

/// `~/…`, `file://…`, absolute, or relative to `folder`.
fn local_path(folder: &Path, source: &str) -> Option<PathBuf> {
    let source = source.trim();
    let source = match source.strip_prefix("file://") {
        Some(rest) => percent_decode(rest.strip_prefix("localhost").unwrap_or(rest))?,
        None => source.to_string(),
    };
    if source.is_empty() || source.contains('\0') {
        return None;
    }
    if let Some(rest) = source.strip_prefix("~/") {
        return dirs::home_dir().map(|home| home.join(rest));
    }
    let path = Path::new(&source);
    Some(if path.is_absolute() { path.to_path_buf() } else { folder.join(path) })
}

/// Images, videos and audio, by extension.
fn media_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        _ => return None,
    })
}

/// Save a chat attachment to ~/Downloads; resolves to the saved path. The
/// webview can't do this itself: WKWebView's own "Download Video" needs a
/// delegate hook wry doesn't implement, and a cross-origin fetch is blocked.
/// `url` is an http(s) URL (ticketed media URLs need no credentials; the
/// gateway's own origin gets its bearer token anyway) or a `data:` URL.
#[tauri::command]
pub async fn save_media(
    hub: State<'_, Arc<Hub>>,
    url: String,
    file_name: Option<String>,
    gateway: Option<String>,
) -> Result<String, RpcError> {
    let dir = dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
        .ok_or_else(|| RpcError::new("NO_DIR", "couldn't find the Downloads folder"))?;
    let name = file_name.as_deref().map(sanitize).filter(|n| !n.is_empty());

    let dest = if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',').ok_or_else(|| RpcError::new("BAD_URL", "malformed data URL"))?;
        let mime = meta.split(';').next().unwrap_or_default();
        let bytes = if meta.ends_with(";base64") {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| RpcError::new("BAD_URL", e.to_string()))?
        } else {
            data.as_bytes().to_vec()
        };
        let dest = unique_path(&dir, &with_extension(name.unwrap_or_else(|| "download".into()), mime));
        tokio::fs::write(&dest, bytes).await.map_err(|e| RpcError::new("WRITE_FAILED", e.to_string()))?;
        dest
    } else {
        let parsed = reqwest::Url::parse(&url).map_err(|e| RpcError::new("BAD_URL", e.to_string()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(RpcError::new("BAD_URL", "only http(s) and data URLs can be saved"));
        }
        let mut req = reqwest::Client::new().get(parsed.clone()).timeout(Duration::from_secs(600));
        let client = gateway.and_then(|g| hub.client(&g));
        if let Some(gw) = client.as_ref().and_then(|c| c.as_openclaw()) {
            let same_origin = gw
                .http_origin()
                .and_then(|o| reqwest::Url::parse(&o).ok())
                .is_some_and(|o| o.origin() == parsed.origin());
            if let Some(token) = gw.http_bearer().filter(|_| same_origin) {
                req = req.bearer_auth(token);
            }
        }
        let res = req.send().await.map_err(|e| RpcError::new("UNAVAILABLE", e.to_string()))?;
        if !res.status().is_success() {
            return Err(RpcError::new("HTTP", format!("the gateway answered {}", res.status())));
        }
        let header = |h| res.headers().get(h).and_then(|v| v.to_str().ok()).map(str::to_string);
        let mime = header(reqwest::header::CONTENT_TYPE).unwrap_or_default();
        let mime = mime.split(';').next().unwrap_or_default().trim().to_string();
        // Our name ("Video") unless it lacks an extension and the server has a better one.
        let name = match name {
            Some(n) if n.contains('.') => n,
            n => header(reqwest::header::CONTENT_DISPOSITION)
                .as_deref()
                .and_then(disposition_name)
                .or(n)
                .unwrap_or_else(|| "download".into()),
        };
        let dest = unique_path(&dir, &with_extension(name, &mime));

        // Stream into a hidden partial file so a failed download leaves nothing behind.
        let partial = dir.join(format!(".{}.download", dest.file_name().unwrap_or_default().to_string_lossy()));
        let write = async {
            use futures_util::StreamExt;
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::File::create(&partial).await.map_err(|e| e.to_string())?;
            let mut body = res.bytes_stream();
            while let Some(chunk) = body.next().await {
                file.write_all(&chunk.map_err(|e| e.to_string())?).await.map_err(|e| e.to_string())?;
            }
            file.flush().await.map_err(|e| e.to_string())?;
            tokio::fs::rename(&partial, &dest).await.map_err(|e| e.to_string())
        };
        if let Err(e) = write.await {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(RpcError::new("WRITE_FAILED", e));
        }
        dest
    };

    Ok(dest.display().to_string())
}

/// A name that's safe as a single path component.
fn sanitize(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '\0') { '_' } else { c }).collect();
    cleaned.trim().trim_start_matches('.').to_string()
}

/// The file name from a Content-Disposition header: `filename*=UTF-8''…`, else `filename="…"`.
fn disposition_name(value: &str) -> Option<String> {
    let param = |key: &str| value.split(';').find_map(|part| part.trim().strip_prefix(key).map(str::to_string));
    let extended = param("filename*=").and_then(|v| v.strip_prefix("UTF-8''").and_then(percent_decode));
    let name = extended.or_else(|| param("filename=").map(|v| v.trim_matches('"').to_string()))?;
    Some(sanitize(&name)).filter(|n| !n.is_empty())
}

fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

/// Add an extension for `mime` if the name has none.
fn with_extension(name: String, mime: &str) -> String {
    if std::path::Path::new(&name).extension().is_some() {
        return name;
    }
    let ext = match mime {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => return name,
    };
    format!("{name}.{ext}")
}

/// `dir/name`, or `dir/name (2).ext`… if that's taken.
fn unique_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let p = std::path::Path::new(name);
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_content_disposition() {
        // What OpenClaw sends for chat media.
        let v = r#"inline; filename="clip_1.mp4"; filename*=UTF-8''clip%20%C3%A9%201.mp4"#;
        assert_eq!(disposition_name(v).as_deref(), Some("clip é 1.mp4"));
        assert_eq!(disposition_name(r#"attachment; filename="report.pdf""#).as_deref(), Some("report.pdf"));
        assert_eq!(disposition_name(r#"inline; filename="../../.ssh/x""#).as_deref(), Some("_.._.ssh_x"));
        assert_eq!(disposition_name("inline"), None);
    }

    #[test]
    fn local_paths() {
        let folder = Path::new("/work/thread");
        let home = dirs::home_dir().unwrap();
        assert_eq!(local_path(folder, "out/clip.mp4"), Some(folder.join("out/clip.mp4")));
        assert_eq!(local_path(folder, "/tmp/a.png"), Some(PathBuf::from("/tmp/a.png")));
        assert_eq!(local_path(folder, "~/dev/a b.mov"), Some(home.join("dev/a b.mov")));
        assert_eq!(local_path(folder, "file:///tmp/a%20b.png"), Some(PathBuf::from("/tmp/a b.png")));
        assert_eq!(local_path(folder, "file://localhost/tmp/a.png"), Some(PathBuf::from("/tmp/a.png")));
        assert_eq!(local_path(folder, "  "), None);
    }

    #[test]
    fn only_media_is_served() {
        let dir = std::env::temp_dir().join(format!("littleguys-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("clip.MP4"), b"not really a video").unwrap();
        std::fs::write(dir.join("secret"), b"hush").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("secret"), dir.join("looks.png")).unwrap();

        let clip = local_media(&dir, "clip.MP4");
        assert_eq!(clip["available"], true);
        assert_eq!(clip["mimeType"], "video/mp4");
        assert_eq!(clip["sizeBytes"], 18);
        assert_eq!(local_media(&dir, "secret")["code"], "unsupported");
        #[cfg(unix)]
        assert_eq!(local_media(&dir, "looks.png")["code"], "unsupported");
        assert_eq!(local_media(&dir, "gone.png")["code"], "not_found");
        assert_eq!(local_media(&dir, ".")["code"], "not_found");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extensions_from_mime() {
        assert_eq!(with_extension("Video".into(), "video/mp4"), "Video.mp4");
        assert_eq!(with_extension("a.mov".into(), "video/mp4"), "a.mov");
        assert_eq!(with_extension("blob".into(), "application/x-thing"), "blob");
    }
}
