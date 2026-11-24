use super::TuiState;
use crate::config::{fnv1a_hex, gen_hex_id, home_dir};
use crate::json::{parse_json, Json};
use crate::tools::PermissionMode;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub model: String,
    pub updated_at: u64,
}

pub(super) fn sessions_dir() -> PathBuf {
    if let Ok(v) = std::env::var("CLIGATE_SESSIONS_DIR") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    home_dir()
        .join(".local")
        .join("share")
        .join("cligate")
        .join("sessions")
}

fn prefs_path() -> PathBuf {
    sessions_dir()
        .parent()
        .map(|p| p.join("prefs.json"))
        .unwrap_or_else(|| home_dir().join(".local/share/cligate/prefs.json"))
}

pub(super) const MAX_WINDOW_PRESETS: &[(usize, &str)] = &[
    (0, "off     Disable automatic compaction"),
    (32_000, "32k     Auto-compact around 32,000 tokens"),
    (64_000, "64k     Auto-compact around 64,000 tokens"),
    (128_000, "128k    Auto-compact around 128,000 tokens"),
    (200_000, "200k    Auto-compact around 200,000 tokens"),
];

pub fn parse_token_limit(raw: &str) -> Option<usize> {
    let s = raw.trim().to_lowercase();
    if matches!(s.as_str(), "off" | "0" | "none" | "disable" | "disabled") {
        return Some(0);
    }
    let parsed = if let Some(n) = s.strip_suffix('k') {
        n.trim()
            .parse::<usize>()
            .ok()
            .map(|v| v.saturating_mul(1_000))
    } else if let Some(n) = s.strip_suffix('m') {
        n.trim()
            .parse::<usize>()
            .ok()
            .map(|v| v.saturating_mul(1_000_000))
    } else {
        s.parse::<usize>().ok()
    }?;
    Some(parsed.max(4_000))
}

pub(super) fn format_tokens_short(n: usize) -> String {
    if n == 0 {
        "off".to_string()
    } else if n >= 1_000_000 && n.is_multiple_of(1_000_000) {
        format!("{}m", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

pub(super) fn estimate_history_tokens(history: &[Json]) -> usize {
    history.iter().map(|m| m.dump().len() / 4).sum()
}

pub(super) struct TuiPrefs {
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) fallback_chain: Vec<String>,
    pub(super) perm_mode: PermissionMode,
    pub(super) always_allow_tools: Vec<String>,
    pub(super) auto_compact_tokens: usize,
}

pub(super) fn load_prefs() -> TuiPrefs {
    let mut out = TuiPrefs {
        model: None,
        effort: None,
        fallback_chain: Vec::new(),
        perm_mode: PermissionMode::Ask,
        always_allow_tools: Vec::new(),
        auto_compact_tokens: 0,
    };
    let Ok(raw) = fs::read_to_string(prefs_path()) else {
        return out;
    };
    let Ok(doc) = parse_json(&raw) else {
        return out;
    };
    out.model = doc
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    out.effort = doc
        .get("effort")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    out.fallback_chain = doc
        .get("fallback_chain")
        .and_then(|v| v.as_arr())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if let Some(m) = doc
        .get("permission_mode")
        .and_then(|v| v.as_str())
        .and_then(PermissionMode::parse)
    {
        out.perm_mode = m;
    }
    out.always_allow_tools = doc
        .get("always_allow_tools")
        .and_then(|v| v.as_arr())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    out.auto_compact_tokens = doc
        .get("auto_compact_tokens")
        .and_then(|v| v.as_i64())
        .map(|v| v.max(0) as usize)
        .unwrap_or(0);
    out
}

pub(super) fn write_prefs(st: &TuiState) {
    let path = prefs_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let fb_json: Vec<Json> = st.tui_fallback.iter().cloned().map(Json::Str).collect();
    let allow_json: Vec<Json> = st
        .always_allow_tools
        .iter()
        .cloned()
        .map(Json::Str)
        .collect();
    let doc = Json::obj(vec![
        ("model", Json::Str(st.model.clone())),
        ("effort", Json::Str(st.effort.clone())),
        ("fallback_chain", Json::Arr(fb_json)),
        (
            "permission_mode",
            Json::Str(st.perm_mode.as_str().to_string()),
        ),
        ("always_allow_tools", Json::Arr(allow_json)),
        (
            "auto_compact_tokens",
            Json::Num(st.auto_compact_tokens as f64),
        ),
    ]);
    let _ = fs::write(path, format!("{}\n", doc.dump_pretty()));
}

pub fn new_session_id() -> String {
    let mut b = [0u8; 16];
    let filled = fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok();
    if !filled {
        let seed = format!("{}-{}", gen_hex_id(""), std::process::id());
        let h = fnv1a_hex(&seed);
        for (i, chunk) in h.as_bytes().chunks(2).take(16).enumerate() {
            let s = std::str::from_utf8(chunk).unwrap_or("00");
            b[i] = u8::from_str_radix(s, 16).unwrap_or(0);
        }
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

pub fn list_saved_sessions() -> Vec<SessionSummary> {
    let dir = sessions_dir();
    let Ok(rd) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = parse_json(&raw) else {
            continue;
        };
        let id = doc
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        let title = doc
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("(untitled)")
            .to_string();
        let model = doc
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let updated_at = doc.get("updated_at").and_then(|v| v.as_i64()).unwrap_or(0) as u64;
        out.push(SessionSummary {
            id,
            title,
            model,
            updated_at,
        });
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
    out
}
