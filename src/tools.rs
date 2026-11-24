use crate::json::Json;
use crate::{is_cancelled, ActivePidGuard};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PermissionMode {
    Ask,
    AlwaysAllow,
    ReadOnly,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AlwaysAllow => "always-allow",
            Self::ReadOnly => "read-only",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "ask" | "prompt" | "default" => Some(Self::Ask),
            "always-allow" | "always" | "allow" | "allow-all" | "yolo" | "auto" => {
                Some(Self::AlwaysAllow)
            }
            "read-only" | "readonly" | "read" | "ro" => Some(Self::ReadOnly),
            _ => None,
        }
    }
}

pub fn is_read_only_tool(name: &str) -> bool {
    matches!(name, "read_file" | "list_files")
}

pub fn tui_tool_definitions() -> Vec<Json> {
    vec![
        tool_schema(
            "read_file",
            "Read a text file from disk with optional 1-based line range.",
            vec![
                (
                    "path",
                    "File path relative to working directory or absolute.",
                ),
                ("start_line", "Optional 1-based start line."),
                ("end_line", "Optional 1-based inclusive end line."),
            ],
            &["path"],
        ),
        tool_schema(
            "list_files",
            "List files and directories inside a directory.",
            vec![(
                "path",
                "Directory path (defaults to current working directory).",
            )],
            &[],
        ),
        tool_schema(
            "write_file",
            "Create or overwrite a file with full text content.",
            vec![
                ("path", "File path to create or overwrite."),
                ("content", "Full file content to write."),
            ],
            &["path", "content"],
        ),
        tool_schema(
            "edit_file",
            "Replace an exact target text block inside an existing file.",
            vec![
                ("path", "File path to edit."),
                ("target", "Exact existing text to replace."),
                ("replacement", "Replacement text."),
            ],
            &["path", "target", "replacement"],
        ),
        tool_schema(
            "run_command",
            "Run a shell command in the working directory and return stdout/stderr.",
            vec![("command", "Shell command string to execute via bash -c.")],
            &["command"],
        ),
    ]
}

fn tool_schema(name: &str, description: &str, props: Vec<(&str, &str)>, required: &[&str]) -> Json {
    let mut prop_entries = Vec::new();
    for (k, desc) in props {
        let ty = if matches!(k, "start_line" | "end_line") {
            "integer"
        } else {
            "string"
        };
        prop_entries.push((
            k,
            Json::obj(vec![
                ("type", Json::Str(ty.to_string())),
                ("description", Json::Str(desc.to_string())),
            ]),
        ));
    }
    let req_arr = required
        .iter()
        .map(|s| Json::Str((*s).to_string()))
        .collect();
    Json::obj(vec![
        ("type", Json::Str("function".to_string())),
        (
            "function",
            Json::obj(vec![
                ("name", Json::Str(name.to_string())),
                ("description", Json::Str(description.to_string())),
                (
                    "parameters",
                    Json::obj(vec![
                        ("type", Json::Str("object".to_string())),
                        ("properties", Json::obj(prop_entries)),
                        ("required", Json::Arr(req_arr)),
                    ]),
                ),
            ]),
        ),
    ])
}

pub fn summarize_tool_call(name: &str, args: &Json) -> String {
    match name {
        "read_file" => args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "list_files" => args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string(),
        "write_file" => {
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let len = args
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            format!("{p} ({len} bytes)")
        }
        "edit_file" => args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "run_command" => args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => args.dump(),
    }
}

fn resolve_path(cwd: &str, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

fn clip_output(s: &str, max_bytes: usize) -> String {
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
