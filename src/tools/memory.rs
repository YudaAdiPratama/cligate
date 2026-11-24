use super::fs::clip_output;
use crate::config::home_dir;
use std::fs;
use std::path::{Path, PathBuf};

pub fn memory_dir() -> PathBuf {
    if let Ok(v) = std::env::var("CLIGATE_MEMORY_DIR") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    home_dir()
        .join(".local")
        .join("share")
        .join("cligate")
        .join("memory")
}

fn memory_file_path(target: &str) -> PathBuf {
    let fname = if target.trim().eq_ignore_ascii_case("user") {
        "USER.md"
    } else {
        "MEMORY.md"
    };
    memory_dir().join(fname)
}

pub fn read_memory_file(target: &str) -> String {
    fs::read_to_string(memory_file_path(target)).unwrap_or_default()
}

pub fn append_memory(target: &str, entry: &str) -> Result<PathBuf, String> {
    let clean = entry.trim();
    if clean.is_empty() {
        return Err("Memory entry cannot be empty".to_string());
    }
    let path = memory_file_path(target);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut existing = fs::read_to_string(&path).unwrap_or_default();
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    let line = if clean.starts_with("- ") {
        format!("{clean}\n")
    } else {
        format!("- {clean}\n")
    };
    existing.push_str(&line);
    fs::write(&path, existing).map_err(|e| e.to_string())?;
    Ok(path)
}

pub fn clear_memory(target: &str) -> Result<(), String> {
    let path = memory_file_path(target);
    if path.exists() {
        fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn load_project_context(cwd: &str) -> Option<(String, String)> {
    for fname in ["AGENTS.md", "CLAUDE.md"] {
        let path = Path::new(cwd).join(fname);
        if let Ok(raw) = fs::read_to_string(&path) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Some((fname.to_string(), clip_output(trimmed, 6000)));
            }
        }
    }
    None
}
