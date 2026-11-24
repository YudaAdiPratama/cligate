use crate::config::home_dir;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

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
