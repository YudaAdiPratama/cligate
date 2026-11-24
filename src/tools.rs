use crate::config::{home_dir, is_cancelled, ActivePidGuard};
use crate::json::Json;
use std::collections::HashSet;
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
    matches!(
        name,
        "read_file" | "list_files" | "grep_search" | "find_files" | "read_skill" | "save_memory"
    )
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
            "grep_search",
            "Search for a case-insensitive text query across files in a directory.",
            vec![
                ("query", "Text substring to search for."),
                ("path", "Optional directory or file path (defaults to '.')."),
            ],
            &["query"],
        ),
        tool_schema(
            "find_files",
            "Recursively find files whose path contains a case-insensitive pattern.",
            vec![
                ("pattern", "Optional filename/path substring filter."),
                ("path", "Optional root directory (defaults to '.')."),
            ],
            &[],
        ),
        tool_schema(
            "read_skill",
            "Read the full SKILL.md instructions for an available local skill by name.",
            vec![("name", "Skill name from the available skills list.")],
            &["name"],
        ),
        tool_schema(
            "save_memory",
            "Append a concise persistent note to TUI memory ('memory' or 'user').",
            vec![
                (
                    "target",
                    "Memory file: 'memory' (facts/lessons) or 'user' (preferences).",
                ),
                (
                    "entry",
                    "Concise single-line fact or preference to remember across sessions.",
                ),
            ],
            &["entry"],
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
        "grep_search" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            format!("{q} in {p}")
        }
        "find_files" => {
            let pat = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("*");
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            format!("{pat} in {p}")
        }
        "read_skill" => args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "save_memory" => {
            let t = args
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("memory");
            let e = args.get("entry").and_then(|v| v.as_str()).unwrap_or("");
            format!("[{t}] {e}")
        }
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

#[derive(Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

fn skill_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(v) = std::env::var("CLIGATE_SKILLS_DIR") {
        if !v.trim().is_empty() {
            dirs.push(PathBuf::from(v));
        }
    }
    dirs.push(
        home_dir()
            .join(".local")
            .join("share")
            .join("cligate")
            .join("skills"),
    );
    dirs.push(home_dir().join(".agents").join("skills"));
    dirs
}

fn parse_skill_description(raw: &str) -> String {
    let mut in_fm = false;
    for (idx, line) in raw.lines().enumerate() {
        let t = line.trim();
        if idx == 0 && t == "---" {
            in_fm = true;
            continue;
        }
        if in_fm {
            if t == "---" {
                break;
            }
            if let Some(rest) = t.strip_prefix("description:") {
                return rest.trim().trim_matches('"').trim_matches('\'').to_string();
            }
        } else if !t.is_empty() && !t.starts_with('#') {
            return t.to_string();
        }
    }
    "Local skill instructions".to_string()
}

pub fn list_skills() -> Vec<SkillInfo> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for dir in skill_dirs() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let skill_file = p.join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() || !seen.insert(name.clone()) {
                continue;
            }
            let raw = fs::read_to_string(&skill_file).unwrap_or_default();
            let description = parse_skill_description(&raw);
            out.push(SkillInfo {
                name,
                description,
                path: skill_file,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn read_skill_content(name: &str) -> Option<String> {
    let target = name.trim().to_lowercase();
    let skill = list_skills()
        .into_iter()
        .find(|s| s.name.to_lowercase() == target)?;
    fs::read_to_string(skill.path).ok()
}

pub fn build_tui_system_prompt(work_dir: &str, active_skills: &[String]) -> String {
    let mut parts = vec![format!(
        "You are a coding assistant in working directory {work_dir}. Use the provided caller tools when you need to inspect files, search code, edit files, save persistent memory, read skills, or run shell commands."
    )];

    if let Some((fname, content)) = load_project_context(work_dir) {
        parts.push(format!("[Project Instructions ({fname})]\n{content}"));
    }

    let user_mem = read_memory_file("user");
    if !user_mem.trim().is_empty() {
        parts.push(format!(
            "[User Preferences (USER.md)]\n{}",
            clip_output(user_mem.trim(), 3000)
        ));
    }

    let proj_mem = read_memory_file("memory");
    if !proj_mem.trim().is_empty() {
        parts.push(format!(
            "[Persistent Memory (MEMORY.md)]\n{}",
            clip_output(proj_mem.trim(), 4000)
        ));
    }

    let skills = list_skills();
    if !skills.is_empty() {
        let mut lines = Vec::new();
        for s in skills.iter().take(24) {
            let desc = if s.description.len() > 100 {
                clip_output(&s.description, 100)
            } else {
                s.description.clone()
            };
            lines.push(format!("- {}: {}", s.name, desc));
        }
        parts.push(format!(
            "[Available Local Skills — call read_skill(name) when relevant]\n{}",
            lines.join("\n")
        ));
    }

    for sk in active_skills {
        if let Some(content) = read_skill_content(sk) {
            parts.push(format!(
                "[Active Skill: {sk}]\n{}",
                clip_output(content.trim(), 5000)
            ));
        }
    }

    parts.join("\n\n")
}
