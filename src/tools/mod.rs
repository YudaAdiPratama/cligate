mod fs;
mod memory;
mod skills;

pub use fs::execute_local_tool;
pub use memory::{append_memory, clear_memory, load_project_context, read_memory_file};
pub use skills::list_skills;

use crate::json::Json;
use fs::clip_output;
use skills::read_skill_content;

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

pub fn build_tui_system_prompt(work_dir: &str, active_skills: &[String]) -> String {
    let mut parts = vec![format!(
        "You are an expert senior software engineer working in {work_dir}.\n\
         Guidelines:\n\
         - Always inspect relevant files with read_file, grep_search, or find_files before proposing or making edits; never guess file contents.\n\
         - Make minimal, surgical edits with edit_file (or write_file for new files) and preserve existing style.\n\
         - After making code changes, verify them with build/lint/test commands via run_command and fix any errors before replying.\n\
         - You may emit multiple <tool_call> blocks in a single turn to gather independent information in parallel.\n\
         - Be concise, factual, and never fabricate file contents, line numbers, or command outputs."
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
