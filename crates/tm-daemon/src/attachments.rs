//! Image and file attachments for runs (Item 4.3).
//!
//! Stores mockups, screenshots, and reference files under `.taskmaster/runs/{id}/attachments/`
//! with strict filename sanitization and a 10 MiB size cap, exposing both metadata for the UI
//! and a prompt context section for planners and UI workers.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub bytes: usize,
    pub is_image: bool,
    pub path: String,
    pub added_at: String,
}

fn dir_for(project: &Path, run_id: &str) -> PathBuf {
    project.join(".taskmaster").join("runs").join(run_id).join("attachments")
}

fn manifest_path(project: &Path, run_id: &str) -> PathBuf {
    dir_for(project, run_id).join("manifest.json")
}

pub fn list(project: &Path, run_id: &str) -> Vec<Attachment> {
    let Ok(bytes) = std::fs::read(manifest_path(project, run_id)) else { return Vec::new() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn sanitize_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment.bin");
    let clean: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    if clean.is_empty() || clean.starts_with('.') {
        "attachment.bin".into()
    } else {
        clean
    }
}

/// Save one attachment (from raw bytes) for `run_id` and record it in `manifest.json`.
pub fn save(
    project: &Path,
    run_id: &str,
    name: &str,
    mime: &str,
    bytes: &[u8],
) -> anyhow::Result<Attachment> {
    if bytes.is_empty() {
        anyhow::bail!("attachment is empty");
    }
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        anyhow::bail!("attachment exceeds the {} MiB limit", MAX_ATTACHMENT_BYTES / (1024 * 1024));
    }
    let dir = dir_for(project, run_id);
    std::fs::create_dir_all(&dir)?;
    let id = crate::secret::random_hex(4)?;
    let safe = sanitize_name(name);
    let stored = dir.join(format!("{id}-{safe}"));
    std::fs::write(&stored, bytes)?;

    let mime_norm = if mime.trim().is_empty() {
        infer_mime(&safe).to_string()
    } else {
        mime.trim().to_string()
    };
    let is_image = mime_norm.starts_with("image/");
    let item = Attachment {
        id,
        name: safe,
        mime: mime_norm,
        bytes: bytes.len(),
        is_image,
        path: stored.display().to_string(),
        added_at: crate::time::rfc3339_now(),
    };
    let mut items = list(project, run_id);
    items.push(item.clone());
    let manifest = manifest_path(project, run_id);
    let tmp = manifest.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&items)?)?;
    std::fs::rename(&tmp, &manifest)?;
    Ok(item)
}

fn infer_mime(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".md") || lower.ends_with(".txt") {
        "text/plain"
    } else if lower.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

/// Format a prompt section describing attached mockups and context files for planners / UI workers.
pub fn prompt_section(project: &Path, run_id: &str) -> String {
    let items = list(project, run_id);
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Attached reference files\n\n");
    for item in items {
        let kind = if item.is_image { "image mockup" } else { "context file" };
        out.push_str(&format!("- `{}` ({kind}, {}, {} bytes) at `{}`\n", item.name, item.mime, item.bytes, item.path));
    }
    out
}

/// Minimal standard Base64 decoder (no extra dependency required).
pub fn decode_base64(input: &str) -> anyhow::Result<Vec<u8>> {
    // Strip optional `data:<mime>;base64,` prefix.
    let body = input
        .trim()
        .split_once(";base64,")
        .map(|(_, b)| b)
        .unwrap_or(input.trim());
    let mut buf = Vec::with_capacity(body.len() * 3 / 4);
    let mut val: u32 = 0;
    let mut bits: i32 = -8;
    for byte in body.bytes() {
        if byte.is_ascii_whitespace() || byte == b'=' {
            continue;
        }
        let d = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => anyhow::bail!("invalid base64 character"),
        };
        val = (val << 6) | u32::from(d);
        bits += 6;
        if bits >= 0 {
            buf.push(((val >> bits) & 0xff) as u8);
            bits -= 8;
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_lists_and_injects_image_attachments_into_prompt_section() {
        let dir = std::env::temp_dir().join(format!("tm-att-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let png = decode_base64("data:image/png;base64,iVBORw0KGgo=").unwrap();
        let saved = save(&dir, "run-1", "../mockup screen.png", "", &png).unwrap();
        assert_eq!(saved.name, "mockup_screen.png");
        assert!(saved.is_image);
        assert_eq!(saved.mime, "image/png");
        assert_eq!(list(&dir, "run-1").len(), 1);

        let prompt = prompt_section(&dir, "run-1");
        assert!(prompt.contains("mockup_screen.png"));
        assert!(prompt.contains("image mockup"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
