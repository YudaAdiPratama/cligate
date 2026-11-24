use super::memory::append_memory;
use super::skills::read_skill_content;
use crate::config::{is_cancelled, ActivePidGuard};
use crate::json::Json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn resolve_path(cwd: &str, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

pub(super) fn clip_output(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... [truncated, {} bytes total]", &s[..end], s.len())
}

pub fn execute_local_tool(cwd: &str, name: &str, args: &Json) -> (bool, String) {
    match name {
        "read_file" => {
            let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if raw.is_empty() {
                return (false, "Missing required argument 'path'".to_string());
            }
            let path = resolve_path(cwd, raw);
            let Ok(content) = fs::read_to_string(&path) else {
                return (false, format!("Failed to read {}", path.display()));
            };
            let start = args
                .get("start_line")
                .and_then(|v| v.as_i64())
                .unwrap_or(1)
                .max(1) as usize;
            let end = args
                .get("end_line")
                .and_then(|v| v.as_i64())
                .map(|v| v.max(1) as usize)
                .unwrap_or(usize::MAX);
            let mut out = String::new();
            for (idx, line) in content.lines().enumerate() {
                let ln = idx + 1;
                if ln < start {
                    continue;
                }
                if ln > end {
                    break;
                }
                out.push_str(&format!("{ln}: {line}\n"));
            }
            (true, clip_output(&out, 16000))
        }
        "list_files" => {
            let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let path = resolve_path(cwd, raw);
            let Ok(rd) = fs::read_dir(&path) else {
                return (false, format!("Failed to list {}", path.display()));
            };
            let mut items = Vec::new();
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let suffix = if entry.path().is_dir() { "/" } else { "" };
                items.push(format!("{name}{suffix}"));
            }
            items.sort();
            if items.len() > 200 {
                let total = items.len();
                items.truncate(200);
                items.push(format!("... ({total} entries total)"));
            }
            (true, items.join("\n"))
        }
        "grep_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            if query.is_empty() {
                return (false, "Missing required argument 'query'".to_string());
            }
            let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let root = resolve_path(cwd, raw);
            let q_low = query.to_lowercase();
            let mut matches = Vec::new();
            grep_walk(&root, &root, &q_low, &mut matches, 100);
            if matches.is_empty() {
                (true, "No matches found.".to_string())
            } else {
                (true, clip_output(&matches.join("\n"), 16000))
            }
        }
        "find_files" => {
            let pat = args
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let root = resolve_path(cwd, raw);
            let mut found = Vec::new();
            find_walk(&root, &root, &pat, &mut found, 200);
            found.sort();
            if found.is_empty() {
                (true, "No files matched.".to_string())
            } else {
                (true, found.join("\n"))
            }
        }
        "read_skill" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.trim().is_empty() {
                return (false, "Missing required argument 'name'".to_string());
            }
            match read_skill_content(name) {
                Some(txt) => (true, clip_output(&txt, 12000)),
                None => (false, format!("Skill '{name}' not found")),
            }
        }
        "save_memory" => {
            let target = args
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("memory");
            let entry = args.get("entry").and_then(|v| v.as_str()).unwrap_or("");
            match append_memory(target, entry) {
                Ok(path) => (true, format!("Saved to {}", path.display())),
                Err(e) => (false, e),
            }
        }
        "write_file" => {
            let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                return (false, "Missing required argument 'content'".to_string());
            };
            if raw.is_empty() {
                return (false, "Missing required argument 'path'".to_string());
            }
            let path = resolve_path(cwd, raw);
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::write(&path, content) {
                Ok(()) => (
                    true,
                    format!("Wrote {} bytes to {}", content.len(), path.display()),
                ),
                Err(e) => (false, format!("Failed to write {}: {e}", path.display())),
            }
        }
        "edit_file" => {
            let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let Some(target) = args.get("target").and_then(|v| v.as_str()) else {
                return (false, "Missing required argument 'target'".to_string());
            };
            let Some(replacement) = args.get("replacement").and_then(|v| v.as_str()) else {
                return (false, "Missing required argument 'replacement'".to_string());
            };
            if raw.is_empty() || target.is_empty() {
                return (
                    false,
                    "Arguments 'path' and 'target' must be non-empty".to_string(),
                );
            }
            let path = resolve_path(cwd, raw);
            let Ok(existing) = fs::read_to_string(&path) else {
                return (false, format!("Failed to read {}", path.display()));
            };
            if !existing.contains(target) {
                return (
                    false,
                    format!("Target text not found in {}", path.display()),
                );
            }
            let updated = existing.replacen(target, replacement, 1);
            match fs::write(&path, &updated) {
                Ok(()) => (true, format!("Updated {}", path.display())),
                Err(e) => (false, format!("Failed to write {}: {e}", path.display())),
            }
        }
        "run_command" => {
            let cmd_str = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if cmd_str.trim().is_empty() {
                return (false, "Missing required argument 'command'".to_string());
            }
            let child = match Command::new("bash")
                .args(["-c", cmd_str])
                .current_dir(cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => return (false, format!("Failed to spawn command: {e}")),
            };
            let _pid_guard = ActivePidGuard::set(child.id());
            let out = match child.wait_with_output() {
                Ok(o) => o,
                Err(e) => return (false, format!("Command failed: {e}")),
            };
            if is_cancelled() {
                return (false, "Command interrupted by user".to_string());
            }
            let code = out.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let mut combined = String::new();
            if !stdout.is_empty() {
                combined.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&stderr);
            }
            if combined.trim().is_empty() {
                combined = format!("(exit {code})");
            } else {
                combined = format!("{}\n(exit {code})", combined.trim_end());
            }
            (out.status.success(), clip_output(&combined, 16000))
        }
        other => (false, format!("Unknown tool '{other}'")),
    }
}

fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | ".venv" | "__pycache__" | "dist" | "build"
    )
}

fn grep_walk(root: &Path, cur: &Path, q_low: &str, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    if cur.is_file() {
        let Ok(meta) = fs::metadata(cur) else {
            return;
        };
        if meta.len() > 512_000 {
            return;
        }
        let Ok(content) = fs::read_to_string(cur) else {
            return;
        };
        let rel = cur.strip_prefix(root).unwrap_or(cur);
        let rel_str = rel.to_string_lossy();
        for (idx, line) in content.lines().enumerate() {
            if out.len() >= limit {
                break;
            }
            if line.to_lowercase().contains(q_low) {
                let preview = if line.len() > 200 {
                    clip_output(line, 200)
                } else {
                    line.to_string()
                };
                out.push(format!("{}:{}: {}", rel_str, idx + 1, preview.trim_end()));
            }
        }
        return;
    }
    let Ok(rd) = fs::read_dir(cur) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if out.len() >= limit {
            break;
        }
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if should_skip_dir(name) {
                continue;
            }
        }
        grep_walk(root, &p, q_low, out, limit);
    }
}

fn find_walk(root: &Path, cur: &Path, pat: &str, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let Ok(rd) = fs::read_dir(cur) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if out.len() >= limit {
            break;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if should_skip_dir(name) {
                continue;
            }
            find_walk(root, &p, pat, out, limit);
        } else {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .into_owned();
            if pat.is_empty() || rel.to_lowercase().contains(pat) {
                out.push(rel);
            }
        }
    }
}
