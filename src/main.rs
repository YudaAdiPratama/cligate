mod json;
mod tui;

use json::{parse_json, Json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(1);
pub static PROGRESS: Mutex<String> = Mutex::new(String::new());
static CANCELLED: AtomicBool = AtomicBool::new(false);
static ACTIVE_CHILD_PID: AtomicU32 = AtomicU32::new(0);

fn set_progress(msg: &str) {
    if let Ok(mut p) = PROGRESS.lock() {
        *p = msg.to_string();
    }
}

pub fn reset_cancel() {
    CANCELLED.store(false, Ordering::SeqCst);
    ACTIVE_CHILD_PID.store(0, Ordering::SeqCst);
}

pub fn is_cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

pub fn request_cancel() {
    CANCELLED.store(true, Ordering::SeqCst);
    let pid = ACTIVE_CHILD_PID.swap(0, Ordering::SeqCst);
    if pid != 0 {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct ActivePidGuard;

impl ActivePidGuard {
    fn set(pid: u32) -> Self {
        ACTIVE_CHILD_PID.store(pid, Ordering::SeqCst);
        if is_cancelled() {
            request_cancel();
        }
        Self
    }
}

impl Drop for ActivePidGuard {
    fn drop(&mut self) {
        ACTIVE_CHILD_PID.store(0, Ordering::SeqCst);
    }
}

fn now_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs()
}

fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs_f64()
}

fn gen_hex_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{nanos:016x}{seq:08x}")
}

fn fnv1a_hex(data: &str) -> String {
    let mut h1: u64 = 0xcbf29ce484222325;
    let mut h2: u64 = 0x84222325cbf29ce4;
    for (i, b) in data.bytes().enumerate() {
        if i % 2 == 0 {
            h1 ^= b as u64;
            h1 = h1.wrapping_mul(0x100000001b3);
        } else {
            h2 ^= b as u64;
            h2 = h2.wrapping_mul(0x100000001b3);
        }
    }
    format!("{h1:016x}{h2:016x}")
}

fn home_dir() -> PathBuf {
    env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn find_cli_bin(name: &str, env_key: &str) -> String {
    if let Ok(v) = env::var(env_key) {
        if !v.trim().is_empty() {
            return v;
        }
    }
    if let Ok(path_var) = env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = Path::new(dir).join(name);
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    let local = home_dir().join(".local").join("bin").join(name);
    if local.is_file() {
        return local.to_string_lossy().into_owned();
    }
    name.to_string()
}

#[derive(Clone)]
struct Config {
    host: String,
    port: u16,
    agy_bin: String,
    codex_bin: String,
    claude_bin: String,
    default_model: String,
    workdir: String,
    routes_path: String,
    max_prompt_bytes: usize,
    sticky_ttl_sec: f64,
    max_sticky_sessions: usize,
    cooldown_sec: f64,
}

impl Config {
    fn from_env() -> Self {
        let host = env::var("CLIGATE_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = env::var("CLIGATE_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8787);
        let default_model =
            env::var("CLIGATE_DEFAULT_MODEL").unwrap_or_else(|_| "gemini-4-argon".to_string());
        let default_workdir = home_dir()
            .join(".cache")
            .join("cligate")
            .join("workspace")
            .to_string_lossy()
            .into_owned();
        let workdir = env::var("CLIGATE_WORKDIR").unwrap_or(default_workdir);
        let routes_path =
            env::var("CLIGATE_ROUTES_PATH").unwrap_or_else(|_| "agent_routes.json".to_string());
        let max_prompt_bytes = env::var("CLIGATE_MAX_PROMPT_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(44000);
        let sticky_ttl_sec = env::var("CLIGATE_STICKY_TTL_SEC")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1800.0);
        let max_sticky_sessions = env::var("CLIGATE_MAX_STICKY_SESSIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6);
        let cooldown_sec = env::var("CLIGATE_COOLDOWN_SEC")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(900.0);

        Self {
            host,
            port,
            agy_bin: find_cli_bin("agy", "CLIGATE_AGY_BIN"),
            codex_bin: find_cli_bin("codex", "CLIGATE_CODEX_BIN"),
            claude_bin: find_cli_bin("claude", "CLIGATE_CLAUDE_BIN"),
            default_model,
            workdir,
            routes_path,
            max_prompt_bytes,
            sticky_ttl_sec,
            max_sticky_sessions,
            cooldown_sec,
        }
    }
}

const AGY_MODELS: &[&str] = &[
    "gemini-4-neon",
    "gemini-4-carbon",
    "gemini-4-argon",
    "gemini-4-hydrogen",
    "gemini-pro-orchestrator-neon",
    "gemini-pro-orchestrator",
    "gemini-pro-orchestrator-argon",
    "gemini-3.8-flash",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "gpt-oss-120b",
];

const CODEX_MODELS: &[&str] = &[
    "gpt-6.1-sol",
    "gpt-6.1-terra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex-spark",
];

const CLAUDE_MODELS: &[&str] = &[
    "claude-fable-5-2",
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-opus-4-5-20251101",
    "claude-sonnet-5-5",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
    "claude-haiku-5-5",
    "claude-haiku-4-5-20251001",
    "claude-mythos-5-2",
    "claude-mythos-5-1",
];

const CODEX_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

fn agy_effort_tiers(model: &str) -> &'static [&'static str] {
    match model {
        "gemini-4-neon" | "gemini-4-hydrogen" => &["low", "medium", "high", "xhigh"],
        "gemini-4-carbon" | "gemini-4-argon" | "gemini-3.8-flash" | "claude-opus-5-5"
        | "claude-sonnet-5-5" => &["low", "medium", "high"],
        "gpt-oss-120b" => &["medium"],
        _ => &["medium"],
    }
}

fn is_exact_agy_model(model: &str) -> bool {
    if matches!(
        model,
        "gemini-pro-orchestrator-neon"
            | "gemini-pro-orchestrator"
            | "gemini-pro-orchestrator-argon"
            | "gemini-pro-orchestrator-g1-pro"
    ) {
        return true;
    }
    for base in AGY_MODELS {
        for tier in agy_effort_tiers(base) {
            if model == format!("{base}-{tier}") {
                return true;
            }
        }
    }
    false
}

fn all_known_models() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for &m in AGY_MODELS {
        seen.insert(m);
        out.push((m, "agy", agy_effort_tiers(m)));
    }
    for &m in CODEX_MODELS {
        if seen.insert(m) {
            out.push((m, "codex", CODEX_EFFORTS));
        }
    }
    for &m in CLAUDE_MODELS {
        if seen.insert(m) {
            out.push((m, "claude", CLAUDE_EFFORTS));
        }
    }
    out
}

fn is_rate_limit_message(text: &str) -> bool {
    let low = text.to_lowercase();
    let needles = [
        "usage_limit_reached",
        "rate_limit_error",
        "hit your weekly limit",
        "hit your usage limit",
        "out of usage credits",
        "insufficient_quota",
        "too many requests",
        "status 429",
        "status: 429",
        "status=429",
        "http 429",
        "quota exceeded",
        "overloaded_error",
        "not logged in",
    ];
    needles.iter().any(|n| low.contains(n))
}

#[derive(Debug)]
enum TurnError {
    RateLimit(String),
    Runtime(String),
    Cancelled,
}

pub fn extract_effort(req: &Json) -> String {
    let mut raw = req.get("reasoning_effort").and_then(|v| v.as_str());
    if raw.is_none() {
        raw = req
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(|v| v.as_str());
    }
    if raw.is_none() {
        if let Some(eb) = req.get("extra_body") {
            raw = eb.get("reasoning_effort").and_then(|v| v.as_str());
            if raw.is_none() {
                raw = eb
                    .get("reasoning")
                    .and_then(|r| r.get("effort"))
                    .and_then(|v| v.as_str());
            }
        }
    }
    let effort = raw.unwrap_or("medium").trim().to_lowercase();
    match effort.as_str() {
        "none" | "minimal" => "low".to_string(),
        "ultra" => "max".to_string(),
        "low" | "medium" | "high" | "xhigh" | "max" => effort,
        _ => "medium".to_string(),
    }
}

fn strip_effort_suffix(model: &str) -> (String, Option<String>) {
    for suffix in ["xhigh", "medium", "high", "low", "max"] {
        let token = format!("-{suffix}");
        if let Some(base) = model.strip_suffix(&token) {
            if AGY_MODELS.contains(&base)
                || CODEX_MODELS.contains(&base)
                || CLAUDE_MODELS.contains(&base)
            {
                return (base.to_string(), Some(suffix.to_string()));
            }
        }
    }
    (model.to_string(), None)
}

pub fn resolve_agy_model(raw_model: &str, reasoning_effort: &str, default_model: &str) -> String {
    let mut model = raw_model.trim();
    if model.is_empty() {
        model = default_model;
    }
    if let Some((_, rest)) = model.split_once('/') {
        model = rest;
    }
    if model.is_empty() {
        model = default_model;
    }
    if model == "gemini-pro-orchestrator-g1-pro" {
        return "gemini-pro-orchestrator".to_string();
    }
    if is_exact_agy_model(model) {
        return model.to_string();
    }
    let (base, suffix_eff) = strip_effort_suffix(model);
    let eff = suffix_eff.as_deref().unwrap_or(reasoning_effort);
    if !AGY_MODELS.contains(&base.as_str())
        || matches!(
            base.as_str(),
            "gemini-pro-orchestrator-neon"
                | "gemini-pro-orchestrator"
                | "gemini-pro-orchestrator-argon"
        )
    {
        return base;
    }
    let tiers = agy_effort_tiers(&base);
    if tiers.contains(&eff) {
        return format!("{base}-{eff}");
    }
    if matches!(eff, "max" | "xhigh") {
        if tiers.contains(&"xhigh") {
            return format!("{base}-xhigh");
        }
        if tiers.contains(&"high") {
            return format!("{base}-high");
        }
    }
    if tiers.contains(&"medium") {
        return format!("{base}-medium");
    }
    format!("{base}-{}", tiers[0])
}

pub fn resolve_codex_model_and_effort(raw_model: &str, reasoning_effort: &str) -> (String, String) {
    let mut model = raw_model.trim();
    if let Some((_, rest)) = model.split_once('/') {
        model = rest;
    }
    let stripped = model.strip_suffix("-900k").unwrap_or(model);
    let (base, suffix_eff) = strip_effort_suffix(stripped);
    let mut eff = suffix_eff.unwrap_or_else(|| reasoning_effort.to_string());
    if !CODEX_EFFORTS.contains(&eff.as_str()) {
        eff = "medium".to_string();
    }
    if eff == "max" && !(base.starts_with("gpt-6") || base.starts_with("gpt-5.6")) {
        eff = "xhigh".to_string();
    }
    (base, eff)
}

pub fn resolve_claude_model_and_effort(
    raw_model: &str,
    reasoning_effort: &str,
) -> (String, String) {
    let mut model = raw_model.trim();
    if let Some((_, rest)) = model.split_once('/') {
        model = rest;
    }
    let (base, suffix_eff) = strip_effort_suffix(model);
    let mut eff = suffix_eff.unwrap_or_else(|| reasoning_effort.to_string());
    if !CLAUDE_EFFORTS.contains(&eff.as_str()) {
        eff = "medium".to_string();
    }
    (base, eff)
}

pub fn detect_primary_provider(raw_model: &str, default_model: &str) -> (String, String) {
    let mut model = raw_model.trim();
    if model.is_empty() {
        model = default_model;
    }
    if let Some((prefix, rest)) = model.split_once('/') {
        let p_low = prefix.trim().to_lowercase();
        if matches!(p_low.as_str(), "agy" | "antigravity" | "google") {
            return ("agy".to_string(), rest.to_string());
        }
        if matches!(p_low.as_str(), "codex" | "openai-codex" | "openai") {
            return ("codex".to_string(), rest.to_string());
        }
        if matches!(p_low.as_str(), "claude" | "claude-code" | "anthropic") {
            return ("claude".to_string(), rest.to_string());
        }
        model = rest;
    }
    let cleaned = model.strip_suffix("-900k").unwrap_or(model);
    let (base, _) = strip_effort_suffix(cleaned);
    let b_str = base.as_str();
    if CODEX_MODELS.contains(&b_str) || b_str.starts_with("gpt-5") || b_str.starts_with("gpt-6") {
        return ("codex".to_string(), cleaned.to_string());
    }
    if AGY_MODELS.contains(&b_str) && !matches!(b_str, "claude-opus-5-5" | "claude-sonnet-5-5") {
        return ("agy".to_string(), cleaned.to_string());
    }
    if is_exact_agy_model(cleaned) && !cleaned.starts_with("claude-") {
        return ("agy".to_string(), cleaned.to_string());
    }
    if CLAUDE_MODELS.contains(&b_str) || b_str.starts_with("claude-") {
        return ("claude".to_string(), cleaned.to_string());
    }
    ("agy".to_string(), cleaned.to_string())
}

pub fn map_fallback_model(
    source_model: &str,
    target_provider: &str,
    default_model: &str,
) -> String {
    let tail = source_model.rsplit('/').next().unwrap_or(source_model);
    let (base, _) = strip_effort_suffix(tail);
    let low = base.to_lowercase();

    let is_flagship = [
        "opus",
        "fable",
        "mythos",
        "terra",
        "neon",
        "hydrogen",
        "orchestrator",
    ]
    .iter()
    .any(|k| low.contains(k));
    let is_fast = ["haiku", "mini", "flash", "spark"]
        .iter()
        .any(|k| low.contains(k));

    match target_provider {
        "agy" => {
            if low == "claude-opus-5-5" || low.contains("claude-opus") {
                "claude-opus-5-5".to_string()
            } else if low == "claude-sonnet-5-5" || low.contains("claude-sonnet") {
                "claude-sonnet-5-5".to_string()
            } else if is_fast {
                "gemini-3.8-flash".to_string()
            } else if is_flagship {
                "gemini-4-neon".to_string()
            } else {
                default_model.to_string()
            }
        }
        "codex" => {
            if is_fast {
                "gpt-5.4-mini".to_string()
            } else if is_flagship {
                "gpt-6.1-terra".to_string()
            } else {
                "gpt-6.1-sol".to_string()
            }
        }
        "claude" => {
            if CLAUDE_MODELS.contains(&low.as_str()) {
                low
            } else if is_fast {
                "claude-haiku-5-5".to_string()
            } else if is_flagship {
                "claude-opus-5-5".to_string()
            } else {
                "claude-sonnet-5-5".to_string()
            }
        }
        _ => default_model.to_string(),
    }
}

fn extract_text_content(content: Option<&Json>) -> String {
    match content {
        None | Some(Json::Null) => String::new(),
        Some(Json::Str(s)) => s.clone(),
        Some(Json::Arr(items)) => {
            let mut parts = Vec::new();
            for item in items {
                if let Some(t) = item.get("type").and_then(|v| v.as_str()) {
                    if t == "text" {
                        if let Some(txt) = item.get("text").and_then(|v| v.as_str()) {
                            parts.push(txt.to_string());
                            continue;
                        }
                    }
                }
                parts.push(item.dump());
            }
            parts.join("\n")
        }
        Some(other) => other.dump(),
    }
}

fn clip_utf8_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn clip_utf8_suffix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn clip_head_tail(text: &str, max_bytes: usize, marker: &str) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    if max_bytes <= marker.len() + 64 {
        return clip_utf8_prefix(text, max_bytes).to_string();
    }
    let budget = max_bytes - marker.len();
    let head_b = budget / 2;
    let tail_b = budget - head_b;
    format!(
        "{}{}{}",
        clip_utf8_prefix(text, head_b),
        marker,
        clip_utf8_suffix(text, tail_b)
    )
}

fn compact_schema(schema: &Json) -> Json {
    if let Some(entries) = schema.as_obj() {
        let mut out = Vec::new();
        for (k, v) in entries {
            if k == "description" {
                if let Some(s) = v.as_str() {
                    if s.len() > 120 {
                        out.push((
                            k.clone(),
                            Json::Str(format!("{}...", clip_utf8_prefix(s, 117))),
                        ));
                        continue;
                    }
                }
            } else if k == "properties" {
                if let Some(props) = v.as_obj() {
                    let mapped = props
                        .iter()
                        .map(|(pk, pv)| (pk.clone(), compact_schema(pv)))
                        .collect();
                    out.push((k.clone(), Json::Obj(mapped)));
                    continue;
                }
            } else if k == "items" {
                out.push((k.clone(), compact_schema(v)));
                continue;
            }
            out.push((k.clone(), v.clone()));
        }
        Json::Obj(out)
    } else {
        schema.clone()
    }
}

fn compact_tools_block(tools: Option<&[Json]>, max_bytes: usize) -> String {
    let Some(tools_slice) = tools else {
        return String::new();
    };
    if tools_slice.is_empty() {
        return String::new();
    }
    let mut tool_defs = Vec::new();
    for t in tools_slice {
        let fn_obj = t.get("function").unwrap_or(t);
        let Some(name) = fn_obj.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let mut desc = fn_obj
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if desc.len() > 160 {
            desc = format!("{}...", clip_utf8_prefix(&desc, 157));
        }
        let params = fn_obj
            .get("parameters")
            .map(compact_schema)
            .unwrap_or_else(|| Json::Obj(Vec::new()));
        tool_defs.push(Json::obj(vec![
            ("name", Json::Str(name.to_string())),
            ("description", Json::Str(desc)),
            ("parameters", params),
        ]));
    }
    if tool_defs.is_empty() {
        return String::new();
    }
    let header = "If you need the caller to execute one or more tools below, output ONLY XML blocks in this exact format:\n<tool_call>{\"name\": \"<tool_name>\", \"arguments\": {<json_args>}}</tool_call>\nOtherwise, respond directly in plain text/markdown.\n\nCaller tools:\n";
    let block = format!("{header}{}", Json::Arr(tool_defs).dump());
    clip_head_tail(
        &block,
        max_bytes,
        "\n... [middle truncated to fit context] ...\n",
    )
}

fn format_message(msg: &Json) -> String {
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
    let text = extract_text_content(msg.get("content"));
    match role {
        "system" => format!("[SYSTEM]\n{text}"),
        "user" => format!("[USER]\n{text}"),
        "assistant" => {
            let mut parts = Vec::new();
            if let Some(tc) = msg.get("tool_calls") {
                if !matches!(tc, Json::Null) {
                    parts.push(format!("[ASSISTANT TOOL_CALLS]\n{}", tc.dump()));
                }
            }
            if !text.is_empty() {
                parts.push(format!("[ASSISTANT]\n{text}"));
            }
            if parts.is_empty() {
                "[ASSISTANT]".to_string()
            } else {
                parts.join("\n")
            }
        }
        "tool" => {
            let id = msg
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("");
            format!("[TOOL RESULT id={id} name={name}]\n{text}")
        }
        other => format!("[{}]\n{text}", other.to_uppercase()),
    }
}

fn msg_fingerprint(msg: &Json) -> String {
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let content = extract_text_content(msg.get("content"));
    let tc = msg.get("tool_calls").map(|v| v.dump()).unwrap_or_default();
    let tcid = msg
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("");
    fnv1a_hex(&format!("{role}|{content}|{tc}|{tcid}|{name}"))
}

fn compute_header_key(model: &str, system_msgs: &[&Json], tools: Option<&[Json]>) -> String {
    let sys_fps: Vec<String> = system_msgs.iter().map(|m| msg_fingerprint(m)).collect();
    let tools_dump = tools
        .map(|t| Json::Arr(t.to_vec()).dump())
        .unwrap_or_default();
    fnv1a_hex(&format!("{model}|{}|{tools_dump}", sys_fps.join(",")))
}

fn build_prompt(messages: &[Json], tools: Option<&[Json]>, max_bytes: usize) -> String {
    let preamble = "IMPORTANT: Answer directly as the assistant. Do NOT call any built-in CLI tools (such as view_file, run_command, grep_search, Bash, etc.); rely strictly on the conversation messages below.";
    let system_msgs: Vec<&Json> = messages
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"))
        .collect();
    let chat_msgs: Vec<&Json> = messages
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) != Some("system"))
        .collect();

    let tools_budget = (max_bytes / 4).max(2000);
    let system_budget = (max_bytes / 5).max(1500);

    let tools_block = compact_tools_block(tools, tools_budget);
    let system_raw: Vec<String> = system_msgs.iter().map(|m| format_message(m)).collect();
    let system_joined = system_raw.join("\n\n");
    let system_block = if system_joined.is_empty() {
        String::new()
    } else {
        clip_head_tail(
            &system_joined,
            system_budget,
            "\n... [middle truncated to fit context] ...\n",
        )
    };

    let mut header_parts = vec![preamble.to_string()];
    if !tools_block.is_empty() {
        header_parts.push(tools_block);
    }
    if !system_block.is_empty() {
        header_parts.push(system_block);
    }
    let header_text = header_parts.join("\n\n");
    let remaining_bytes = max_bytes.saturating_sub(header_text.len() + 256).max(4000);

    let mut selected_rev = Vec::new();
    let mut omitted_count = 0usize;
    let mut used_bytes = 0usize;

    for (idx, msg) in chat_msgs.iter().rev().enumerate() {
        let mut formatted = format_message(msg);
        if idx >= 2
            && msg.get("role").and_then(|v| v.as_str()) == Some("tool")
            && formatted.len() > 2000
        {
            formatted = clip_head_tail(&formatted, 2000, "\n... [older tool output trimmed] ...\n");
        }
        if idx == 0 && formatted.len() > remaining_bytes {
            formatted = clip_head_tail(
                &formatted,
                remaining_bytes,
                "\n... [middle truncated to fit context] ...\n",
            );
            selected_rev.push(formatted);
            omitted_count = chat_msgs.len().saturating_sub(1);
            break;
        }
        if used_bytes + formatted.len() + 2 > remaining_bytes {
            omitted_count = chat_msgs.len().saturating_sub(selected_rev.len());
            break;
        }
        used_bytes += formatted.len() + 2;
        selected_rev.push(formatted);
    }

    selected_rev.reverse();
    if omitted_count > 0 {
        selected_rev.insert(
            0,
            format!(
                "[... {omitted_count} earlier message(s) omitted to fit {max_bytes}-byte bridge limit ...]"
            ),
        );
    }
    header_parts.extend(selected_rev);
    header_parts.join("\n\n").trim().to_string()
}

fn build_delta_prompt(delta_msgs: &[&Json], max_bytes: usize) -> String {
    let mut slice = delta_msgs;
    while slice.len() > 1 && slice[0].get("role").and_then(|v| v.as_str()) == Some("assistant") {
        slice = &slice[1..];
    }
    let mut selected_rev = Vec::new();
    let mut used_bytes = 0usize;
    for (idx, msg) in slice.iter().rev().enumerate() {
        let mut formatted = format_message(msg);
        if idx == 0 && formatted.len() > max_bytes {
            formatted = clip_head_tail(
                &formatted,
                max_bytes,
                "\n... [middle truncated to fit context] ...\n",
            );
            selected_rev.push(formatted);
            break;
        }
        if used_bytes + formatted.len() + 2 > max_bytes {
            break;
        }
        used_bytes += formatted.len() + 2;
        selected_rev.push(formatted);
    }
    selected_rev.reverse();
    selected_rev.join("\n\n").trim().to_string()
}

pub fn parse_tool_calls(output: &str) -> (String, Vec<Json>) {
    let mut tool_calls = Vec::new();
    let mut cleaned = String::new();
    let mut cursor = 0usize;
    let open_tag = "<tool_call>";
    let close_tag = "</tool_call>";

    while let Some(rel_start) = output[cursor..].find(open_tag) {
        let start = cursor + rel_start;
        let content_start = start + open_tag.len();
        let Some(rel_end) = output[content_start..].find(close_tag) else {
            break;
        };
        let content_end = content_start + rel_end;
        cleaned.push_str(&output[cursor..start]);
        let raw_json = output[content_start..content_end].trim();
        if let Ok(parsed) = parse_json(raw_json) {
            if let Some(name) = parsed.get("name").and_then(|v| v.as_str()) {
                if !name.is_empty() {
                    let args_val = parsed
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| Json::Obj(Vec::new()));
                    let args_str = match args_val {
                        Json::Str(s) => s,
                        other => other.dump(),
                    };
                    tool_calls.push(Json::obj(vec![
                        ("id", Json::Str(gen_hex_id("call_"))),
                        ("type", Json::Str("function".to_string())),
                        (
                            "function",
                            Json::obj(vec![
                                ("name", Json::Str(name.to_string())),
                                ("arguments", Json::Str(args_str)),
                            ]),
                        ),
                    ]));
                }
            }
        }
        cursor = content_end + close_tag.len();
    }
    if tool_calls.is_empty() {
        return (output.to_string(), tool_calls);
    }
    cleaned.push_str(&output[cursor..]);
    (cleaned.trim().to_string(), tool_calls)
}

fn default_usage_json(prompt_tok: i64, comp_tok: i64, cached_tok: i64) -> Json {
    Json::obj(vec![
        ("prompt_tokens", Json::Num(prompt_tok as f64)),
        ("completion_tokens", Json::Num(comp_tok as f64)),
        ("total_tokens", Json::Num((prompt_tok + comp_tok) as f64)),
        (
            "prompt_tokens_details",
            Json::obj(vec![("cached_tokens", Json::Num(cached_tok as f64))]),
        ),
    ])
}

struct AgyWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl AgyWorker {
    fn spawn_initialized(cfg: &Config, model: &str) -> Result<Self, TurnError> {
        let _ = fs::create_dir_all(&cfg.workdir);
        let mut child = Command::new(&cfg.agy_bin)
            .args([
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "-p=",
                "--model",
                model,
                "--disable-slash-commands",
            ])
            .current_dir(&cfg.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| TurnError::Runtime(format!("failed to spawn agy: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TurnError::Runtime("missing agy stdin".to_string()))?;
        let stdout_raw = child
            .stdout
            .take()
            .ok_or_else(|| TurnError::Runtime("missing agy stdout".to_string()))?;
        let mut stdout = BufReader::new(stdout_raw);
        let mut line = String::new();

        loop {
            line.clear();
            let n = stdout
                .read_line(&mut line)
                .map_err(|e| TurnError::Runtime(e.to_string()))?;
            if n == 0 {
                if is_cancelled() {
                    return Err(TurnError::Cancelled);
                }
                let mut err_str = String::new();
                if let Some(mut se) = child.stderr.take() {
                    let _ = se.read_to_string(&mut err_str);
                }
                let _ = child.kill();
                let _ = child.wait();
                if is_rate_limit_message(&err_str) {
                    return Err(TurnError::RateLimit(err_str.trim().to_string()));
                }
                return Err(TurnError::Runtime(format!(
                    "agy exited before init: {}",
                    err_str.trim()
                )));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(evt) = parse_json(trimmed) {
                if evt.get("event").and_then(|v| v.as_str()) == Some("init") {
                    return Ok(Self {
                        child,
                        stdin,
                        stdout,
                    });
                }
            }
        }
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn close(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn send_turn(
        &mut self,
        prompt: &str,
        on_delta: &mut Option<&mut dyn FnMut(&str)>,
    ) -> Result<(String, Json, i64), TurnError> {
        if is_cancelled() {
            return Err(TurnError::Cancelled);
        }
        let _pid_guard = ActivePidGuard::set(self.child.id());
        let payload = Json::obj(vec![
            ("event", Json::Str("user".to_string())),
            (
                "message",
                Json::obj(vec![("content", Json::Str(prompt.to_string()))]),
            ),
        ])
        .dump();
        self.stdin
            .write_all(format!("{payload}\n").as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| TurnError::Runtime(format!("agy stdin write failed: {e}")))?;

        let mut collected = String::new();
        let mut prompt_tok = 0i64;
        let mut comp_tok = 0i64;
        let mut cached_tok = 0i64;
        let mut line = String::new();

        loop {
            line.clear();
            let n = self
                .stdout
                .read_line(&mut line)
                .map_err(|e| TurnError::Runtime(e.to_string()))?;
            if n == 0 {
                if is_cancelled() {
                    return Err(TurnError::Cancelled);
                }
                let mut err_str = String::new();
                if let Some(mut se) = self.child.stderr.take() {
                    let _ = se.read_to_string(&mut err_str);
                }
                if is_rate_limit_message(&err_str) {
                    return Err(TurnError::RateLimit(err_str.trim().to_string()));
                }
                return Err(TurnError::Runtime(format!(
                    "agy stdout closed unexpectedly: {}",
                    err_str.trim()
                )));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(evt) = parse_json(trimmed) else {
                continue;
            };
            match evt.get("event").and_then(|v| v.as_str()) {
                Some("step_update") => {
                    if let Some(su) = evt.get("step_update") {
                        if su.get("step_type").and_then(|v| v.as_str()) == Some("agent_response") {
                            if let Some(delta) = su.get("text_delta").and_then(|v| v.as_str()) {
                                if !delta.is_empty() {
                                    collected.push_str(delta);
                                    if let Some(cb) = on_delta.as_mut() {
                                        cb(delta);
                                    }
                                }
                            }
                            if let Some(u) = su.get("usage") {
                                prompt_tok =
                                    u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                comp_tok =
                                    u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                cached_tok = u
                                    .get("cache_read_tokens")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0);
                            }
                        }
                    }
                }
                Some("result") => {
                    if let Some(res) = evt.get("result") {
                        if let Some(u) = res.get("usage") {
                            prompt_tok =
                                u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                            comp_tok = u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                            cached_tok = u
                                .get("cache_read_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                        }
                        if res.get("status").and_then(|v| v.as_str()) == Some("ERROR") {
                            let err_msg = res
                                .get("error")
                                .and_then(|v| v.as_str())
                                .unwrap_or("agy stream-json turn error")
                                .to_string();
                            if is_rate_limit_message(&err_msg) {
                                return Err(TurnError::RateLimit(err_msg));
                            }
                            return Err(TurnError::Runtime(err_msg));
                        }
                        if collected.is_empty() {
                            if let Some(resp_txt) = res.get("response").and_then(|v| v.as_str()) {
                                collected = resp_txt.to_string();
                            }
                        }
                        return Ok((
                            collected.trim().to_string(),
                            default_usage_json(prompt_tok, comp_tok, cached_tok),
                            cached_tok,
                        ));
                    }
                }
                _ => {}
            }
        }
    }
}

impl Drop for AgyWorker {
    fn drop(&mut self) {
        self.close();
    }
}

struct StandbyPoolInner {
    standby: HashMap<String, AgyWorker>,
    warming: HashSet<String>,
}

struct StandbyModelPool {
    cfg: Config,
    inner: Arc<Mutex<StandbyPoolInner>>,
}

impl StandbyModelPool {
    fn new(cfg: Config) -> Self {
        Self {
            cfg,
            inner: Arc::new(Mutex::new(StandbyPoolInner {
                standby: HashMap::new(),
                warming: HashSet::new(),
            })),
        }
    }

    fn prewarm_async(&self, model: &str) {
        {
            let mut guard = self.inner.lock().unwrap();
            if let Some(w) = guard.standby.get_mut(model) {
                if w.is_alive() {
                    return;
                }
            }
            if guard.warming.contains(model) {
                return;
            }
            guard.warming.insert(model.to_string());
        }
        let cfg = self.cfg.clone();
        let inner = Arc::clone(&self.inner);
        let model_owned = model.to_string();
        thread::spawn(move || {
            let spawned = AgyWorker::spawn_initialized(&cfg, &model_owned);
            let mut guard = inner.lock().unwrap();
            guard.warming.remove(&model_owned);
            if let Ok(worker) = spawned {
                let replace = match guard.standby.get_mut(&model_owned) {
                    Some(existing) => !existing.is_alive(),
                    None => true,
                };
                if replace {
                    guard.standby.insert(model_owned, worker);
                }
            }
        });
    }

    fn acquire(&self, model: &str) -> Result<AgyWorker, TurnError> {
        let mut existing = None;
        {
            let mut guard = self.inner.lock().unwrap();
            if let Some(mut cand) = guard.standby.remove(model) {
                if cand.is_alive() {
                    existing = Some(cand);
                }
            }
        }
        self.prewarm_async(model);
        if let Some(w) = existing {
            return Ok(w);
        }
        AgyWorker::spawn_initialized(&self.cfg, model)
    }

    fn status(&self) -> Json {
        let mut guard = self.inner.lock().unwrap();
        let mut ready = Vec::new();
        for (m, w) in guard.standby.iter_mut() {
            if w.is_alive() {
                ready.push(Json::Str(m.clone()));
            }
        }
        let warming: Vec<Json> = guard.warming.iter().cloned().map(Json::Str).collect();
        Json::obj(vec![
            ("standby_ready", Json::Arr(ready)),
            ("warming", Json::Arr(warming)),
        ])
    }
}

struct StickySession {
    id: String,
    model: String,
    header_key: String,
    worker: AgyWorker,
    msg_hashes: Vec<String>,
    last_used: f64,
    turns: u64,
}

struct StickyPoolState {
    sessions: Vec<Arc<Mutex<StickySession>>>,
    cache_hits: u64,
    cache_misses: u64,
    total_cached_tokens: i64,
}

struct StickySessionManager {
    cfg: Config,
    standby: Arc<StandbyModelPool>,
    state: Mutex<StickyPoolState>,
}

impl StickySessionManager {
    fn new(cfg: Config, standby: Arc<StandbyModelPool>) -> Self {
        Self {
            cfg,
            standby,
            state: Mutex::new(StickyPoolState {
                sessions: Vec::new(),
                cache_hits: 0,
                cache_misses: 0,
                total_cached_tokens: 0,
            }),
        }
    }

    fn evict_expired_locked(&self, st: &mut StickyPoolState) {
        let now = now_f64();
        let ttl = self.cfg.sticky_ttl_sec;
        st.sessions.retain(|arc_s| {
            if let Ok(mut s) = arc_s.try_lock() {
                s.worker.is_alive() && (now - s.last_used <= ttl)
            } else {
                true
            }
        });
        while st.sessions.len() > self.cfg.max_sticky_sessions {
            st.sessions.remove(0);
        }
    }

    fn run_cached_turn(
        &self,
        model: &str,
        messages: &[Json],
        tools: Option<&[Json]>,
        on_delta: &mut Option<&mut dyn FnMut(&str)>,
    ) -> Result<(String, Json), TurnError> {
        let system_msgs: Vec<&Json> = messages
            .iter()
            .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"))
            .collect();
        let chat_msgs: Vec<&Json> = messages
            .iter()
            .filter(|m| m.get("role").and_then(|v| v.as_str()) != Some("system"))
            .collect();
        let header_key = compute_header_key(model, &system_msgs, tools);
        let chat_hashes: Vec<String> = chat_msgs.iter().map(|m| msg_fingerprint(m)).collect();

        let mut matched: Option<(Arc<Mutex<StickySession>>, usize)> = None;
        {
            let mut st = self.state.lock().unwrap();
            self.evict_expired_locked(&mut st);
            let mut best_len = 0usize;
            for arc_s in &st.sessions {
                if let Ok(mut s) = arc_s.try_lock() {
                    if s.header_key != header_key || !s.worker.is_alive() {
                        continue;
                    }
                    let n = s.msg_hashes.len();
                    if n > 0
                        && chat_hashes.len() > n
                        && chat_hashes[..n] == s.msg_hashes[..]
                        && n > best_len
                    {
                        best_len = n;
                        matched = Some((Arc::clone(arc_s), n));
                    }
                }
            }
            if matched.is_some() {
                st.cache_hits += 1;
            } else {
                st.cache_misses += 1;
            }
        }

        let (session_arc, is_hit, prefix_len) = if let Some((arc_s, plen)) = matched {
            (arc_s, true, plen)
        } else {
            let worker = self.standby.acquire(model)?;
            let new_sess = Arc::new(Mutex::new(StickySession {
                id: gen_hex_id("")[..12].to_string(),
                model: model.to_string(),
                header_key,
                worker,
                msg_hashes: Vec::new(),
                last_used: now_f64(),
                turns: 0,
            }));
            let mut st = self.state.lock().unwrap();
            st.sessions.push(Arc::clone(&new_sess));
            self.evict_expired_locked(&mut st);
            (new_sess, false, 0)
        };

        let prompt = if is_hit {
            build_delta_prompt(&chat_msgs[prefix_len..], self.cfg.max_prompt_bytes)
        } else {
            build_prompt(messages, tools, self.cfg.max_prompt_bytes)
        };

        let mut sess = session_arc.lock().unwrap();
        match sess.worker.send_turn(&prompt, on_delta) {
            Ok((out, usage, cached_tok)) => {
                sess.msg_hashes = chat_hashes;
                sess.turns += 1;
                sess.last_used = now_f64();
                if cached_tok > 0 {
                    let mut st = self.state.lock().unwrap();
                    st.total_cached_tokens += cached_tok;
                }
                Ok((out, usage))
            }
            Err(err) => {
                sess.worker.close();
                drop(sess);
                let mut st = self.state.lock().unwrap();
                st.sessions.retain(|s| !Arc::ptr_eq(s, &session_arc));
                Err(err)
            }
        }
    }

    fn stats(&self) -> Json {
        let mut st = self.state.lock().unwrap();
        self.evict_expired_locked(&mut st);
        let now = now_f64();
        let mut sess_list = Vec::new();
        for arc_s in &st.sessions {
            if let Ok(s) = arc_s.try_lock() {
                let idle = ((now - s.last_used) * 10.0).round() / 10.0;
                sess_list.push(Json::obj(vec![
                    ("id", Json::Str(s.id.clone())),
                    ("model", Json::Str(s.model.clone())),
                    ("turns", Json::Num(s.turns as f64)),
                    ("cached_messages", Json::Num(s.msg_hashes.len() as f64)),
                    ("idle_sec", Json::Num(idle)),
                ]));
            }
        }
        Json::obj(vec![
            ("active_sessions", Json::Num(sess_list.len() as f64)),
            ("cache_hits", Json::Num(st.cache_hits as f64)),
            ("cache_misses", Json::Num(st.cache_misses as f64)),
            (
                "total_cached_tokens",
                Json::Num(st.total_cached_tokens as f64),
            ),
            ("sessions", Json::Arr(sess_list)),
        ])
    }
}

fn execute_claude_turn(
    cfg: &Config,
    messages: &[Json],
    tools: Option<&[Json]>,
    raw_model: &str,
    effort: &str,
    on_delta: &mut Option<&mut dyn FnMut(&str)>,
) -> Result<(String, Json, String), TurnError> {
    let (model, resolved_effort) = resolve_claude_model_and_effort(raw_model, effort);
    let prompt = build_prompt(messages, tools, cfg.max_prompt_bytes);
    let _ = fs::create_dir_all(&cfg.workdir);

    let mut child = Command::new(&cfg.claude_bin)
        .args([
            "-p",
            "--setting-sources",
            "",
            "--tools",
            "",
            "--no-session-persistence",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--model",
            &model,
            "--effort",
            &resolved_effort,
        ])
        .current_dir(&cfg.workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| TurnError::Runtime(format!("failed to spawn claude: {e}")))?;

    let _pid_guard = ActivePidGuard::set(child.id());
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    let stdout_raw = child
        .stdout
        .take()
        .ok_or_else(|| TurnError::Runtime("missing claude stdout".to_string()))?;
    let reader = BufReader::new(stdout_raw);

    let mut collected = String::new();
    let mut final_result = String::new();
    let mut prompt_tok = 0i64;
    let mut comp_tok = 0i64;
    let mut cached_tok = 0i64;

    for line_res in reader.lines() {
        let Ok(line) = line_res else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(evt) = parse_json(trimmed) else {
            continue;
        };
        match evt.get("type").and_then(|v| v.as_str()) {
            Some("stream_event") => {
                if let Some(se) = evt.get("event") {
                    if se.get("type").and_then(|v| v.as_str()) == Some("content_block_delta") {
                        if let Some(d) = se.get("delta") {
                            if d.get("type").and_then(|v| v.as_str()) == Some("text_delta") {
                                if let Some(chunk) = d.get("text").and_then(|v| v.as_str()) {
                                    if !chunk.is_empty() {
                                        collected.push_str(chunk);
                                        if let Some(cb) = on_delta.as_mut() {
                                            cb(chunk);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Some("result") => {
                if let Some(u) = evt.get("usage") {
                    prompt_tok = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    comp_tok = u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    cached_tok = u
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                }
                let res_text = evt
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if evt.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                    let status_code = evt
                        .get("api_error_status")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let err_type = evt
                        .get("api_error_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let err_code = evt.get("api_error").and_then(|v| v.as_str()).unwrap_or("");
                    let combined = format!("{status_code} {err_type} {err_code} {res_text}");
                    let _ = child.kill();
                    let _ = child.wait();
                    if status_code == 429 || is_rate_limit_message(&combined) {
                        return Err(TurnError::RateLimit(if res_text.is_empty() {
                            combined
                        } else {
                            res_text
                        }));
                    }
                    return Err(TurnError::Runtime(format!("Claude CLI error: {combined}")));
                }
                final_result = res_text;
            }
            _ => {}
        }
    }
    let _ = child.wait();
    if is_cancelled() {
        return Err(TurnError::Cancelled);
    }
    let out = if !collected.trim().is_empty() {
        collected.trim().to_string()
    } else {
        final_result.trim().to_string()
    };
    Ok((
        out,
        default_usage_json(prompt_tok, comp_tok, cached_tok),
        format!("{model} (effort={resolved_effort})"),
    ))
}

fn execute_codex_turn(
    cfg: &Config,
    messages: &[Json],
    tools: Option<&[Json]>,
    raw_model: &str,
    effort: &str,
    on_delta: &mut Option<&mut dyn FnMut(&str)>,
) -> Result<(String, Json, String), TurnError> {
    let (model, resolved_effort) = resolve_codex_model_and_effort(raw_model, effort);
    let prompt = build_prompt(messages, tools, cfg.max_prompt_bytes);
    let _ = fs::create_dir_all(&cfg.workdir);

    let effort_cfg = format!("model_reasoning_effort=\"{resolved_effort}\"");
    let mut child = Command::new(&cfg.codex_bin)
        .args([
            "exec",
            "--ephemeral",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "-s",
            "read-only",
            "--json",
            "-m",
            &model,
            "-c",
            &effort_cfg,
            "-",
        ])
        .current_dir(&cfg.workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| TurnError::Runtime(format!("failed to spawn codex: {e}")))?;

    let _pid_guard = ActivePidGuard::set(child.id());
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    let stdout_raw = child
        .stdout
        .take()
        .ok_or_else(|| TurnError::Runtime("missing codex stdout".to_string()))?;
    let reader = BufReader::new(stdout_raw);

    let mut messages_out = Vec::new();
    let mut errors_out = Vec::new();
    let mut prompt_tok = 0i64;
    let mut comp_tok = 0i64;
    let mut cached_tok = 0i64;

    for line_res in reader.lines() {
        let Ok(line) = line_res else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(evt) = parse_json(trimmed) else {
            continue;
        };
        match evt.get("type").and_then(|v| v.as_str()) {
            Some("item.completed") => {
                if let Some(item) = evt.get("item") {
                    let itype = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if matches!(itype, "agent_message" | "message") {
                        let mut txt = item
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if txt.is_empty() {
                            if let Some(arr) = item.get("content").and_then(|v| v.as_arr()) {
                                for c in arr {
                                    if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                                        txt.push_str(t);
                                    }
                                }
                            }
                        }
                        if !txt.is_empty() {
                            if let Some(cb) = on_delta.as_mut() {
                                cb(&txt);
                            }
                            messages_out.push(txt);
                        }
                    }
                }
            }
            Some("turn.completed") => {
                if let Some(u) = evt.get("usage") {
                    prompt_tok = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    comp_tok = u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    cached_tok = u
                        .get("cached_input_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                }
            }
            Some("error") => {
                if let Some(m) = evt.get("message").and_then(|v| v.as_str()) {
                    if !m.is_empty() {
                        errors_out.push(m.to_string());
                    }
                }
            }
            Some("turn.failed") => {
                if let Some(m) = evt
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                {
                    if !m.is_empty() && !errors_out.iter().any(|x| x == m) {
                        errors_out.push(m.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    let _ = child.wait();
    if is_cancelled() {
        return Err(TurnError::Cancelled);
    }
    if !errors_out.is_empty() && messages_out.is_empty() {
        let joined = errors_out.join("; ");
        if is_rate_limit_message(&joined) {
            return Err(TurnError::RateLimit(joined));
        }
        return Err(TurnError::Runtime(format!("Codex CLI error: {joined}")));
    }
    Ok((
        messages_out.join("\n").trim().to_string(),
        default_usage_json(prompt_tok, comp_tok, cached_tok),
        format!("{model} (effort={resolved_effort})"),
    ))
}

fn load_agent_routes(path: &str) -> Json {
    if let Ok(raw) = fs::read_to_string(path) {
        if let Ok(parsed) = parse_json(&raw) {
            if parsed.as_obj().is_some() {
                return parsed;
            }
        }
    }
    Json::obj(vec![("agents", Json::Obj(Vec::new()))])
}

fn save_agent_routes(path: &str, data: &Json) -> Result<(), String> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    let tmp = format!("{path}.tmp");
    fs::write(&tmp, format!("{}\n", data.dump_pretty())).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

struct RouterState {
    fallback_order: Vec<String>,
    cooldown_until: HashMap<String, f64>,
    last_error: HashMap<String, String>,
    provider_calls: HashMap<String, u64>,
    fallback_events: Vec<Json>,
}

pub struct ProviderRouter {
    cfg: Config,
    sticky: Arc<StickySessionManager>,
    state: Mutex<RouterState>,
}

impl ProviderRouter {
    fn new(cfg: Config, sticky: Arc<StickySessionManager>) -> Self {
        let mut cooldown_until = HashMap::new();
        let mut last_error = HashMap::new();
        let mut provider_calls = HashMap::new();
        for p in ["claude", "codex", "agy"] {
            cooldown_until.insert(p.to_string(), 0.0);
            last_error.insert(p.to_string(), String::new());
            provider_calls.insert(p.to_string(), 0);
        }
        Self {
            cfg,
            sticky,
            state: Mutex::new(RouterState {
                fallback_order: vec!["claude".to_string(), "codex".to_string(), "agy".to_string()],
                cooldown_until,
                last_error,
                provider_calls,
                fallback_events: Vec::new(),
            }),
        }
    }

    pub fn mark_limited(&self, provider: &str, reason: &str, cooldown_sec: f64) {
        let mut st = self.state.lock().unwrap();
        st.cooldown_until
            .insert(provider.to_string(), now_f64() + cooldown_sec);
        st.last_error
            .insert(provider.to_string(), reason.to_string());
        println!("[cligate] Provider '{provider}' marked limited for {cooldown_sec}s: {reason}");
    }

    pub fn reset_cooldown(&self, provider: Option<&str>) -> Json {
        {
            let mut st = self.state.lock().unwrap();
            if let Some(p) = provider {
                if st.cooldown_until.contains_key(p) {
                    st.cooldown_until.insert(p.to_string(), 0.0);
                    st.last_error.insert(p.to_string(), String::new());
                }
            } else {
                for p in ["claude", "codex", "agy"] {
                    st.cooldown_until.insert(p.to_string(), 0.0);
                    st.last_error.insert(p.to_string(), String::new());
                }
            }
        }
        self.status()
    }

    fn get_agent_config(&self, agent_name: Option<&str>) -> Option<(String, Vec<String>)> {
        let name = agent_name?.trim().to_lowercase();
        if name.is_empty() {
            return None;
        }
        let routes = load_agent_routes(&self.cfg.routes_path);
        let ag = routes.get("agents")?.get(&name)?;
        let effort = ag
            .get("effort")
            .and_then(|v| v.as_str())
            .unwrap_or("medium")
            .to_string();
        let chain: Vec<String> = ag
            .get("chain")
            .and_then(|v| v.as_arr())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Some((effort, chain))
    }

    pub fn set_agent_chain(
        &self,
        agent_name: &str,
        chain: &[String],
        effort: Option<&str>,
    ) -> Result<Json, String> {
        let name = agent_name.trim().to_lowercase();
        let clean: Vec<String> = chain
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if clean.is_empty() {
            return Err("chain must contain at least 1 model".to_string());
        }
        let _guard = self.state.lock().unwrap();
        let mut routes = load_agent_routes(&self.cfg.routes_path);
        let prev_eff = routes
            .get("agents")
            .and_then(|a| a.get(&name))
            .and_then(|c| c.get("effort"))
            .and_then(|v| v.as_str())
            .unwrap_or("medium")
            .to_string();
        let eff = effort.unwrap_or(&prev_eff).trim().to_lowercase();
        let entry = Json::obj(vec![
            ("effort", Json::Str(eff)),
            (
                "chain",
                Json::Arr(clean.into_iter().map(Json::Str).collect()),
            ),
        ]);
        let agents_obj = routes.get_mut_or_insert("agents", Json::Obj(Vec::new()));
        agents_obj.set(&name, entry.clone());
        save_agent_routes(&self.cfg.routes_path, &routes)?;
        Ok(entry)
    }

    pub fn build_target_candidates(
        &self,
        requested_model: &str,
        agent_name: Option<&str>,
    ) -> Vec<(String, String)> {
        let (primary_provider, bare_req) =
            detect_primary_provider(requested_model, &self.cfg.default_model);
        let mut raw_targets: Vec<(String, String)> = Vec::new();

        if let Some((_, chain)) = self.get_agent_config(agent_name) {
            if !chain.is_empty() {
                let (_, first_bare) = detect_primary_provider(&chain[0], &self.cfg.default_model);
                if !matches!(requested_model, "" | "auto" | "default") && bare_req != first_bare {
                    raw_targets.push((primary_provider.clone(), bare_req.clone()));
                }
                for item in &chain {
                    let (p, b) = detect_primary_provider(item, &self.cfg.default_model);
                    if !raw_targets.contains(&(p.clone(), b.clone())) {
                        raw_targets.push((p, b));
                    }
                }
            }
        }

        if raw_targets.is_empty() {
            raw_targets.push((primary_provider.clone(), bare_req.clone()));
            let st = self.state.lock().unwrap();
            for p in &st.fallback_order {
                if p != &primary_provider {
                    let mapped = map_fallback_model(&bare_req, p, &self.cfg.default_model);
                    raw_targets.push((p.clone(), mapped));
                }
            }
        }

        if !raw_targets.iter().any(|(p, _)| p == "agy") {
            raw_targets.push((
                "agy".to_string(),
                map_fallback_model(&bare_req, "agy", &self.cfg.default_model),
            ));
        }

        let st = self.state.lock().unwrap();
        let now = now_f64();
        let mut healthy = Vec::new();
        let mut cooling = Vec::new();
        for (p, m) in raw_targets {
            let until = st.cooldown_until.get(&p).copied().unwrap_or(0.0);
            if now >= until {
                healthy.push((p, m));
            } else {
                cooling.push((p, m));
            }
        }
        healthy.extend(cooling);
        healthy
    }

    fn execute_turn(
        &self,
        messages: &[Json],
        tools: Option<&[Json]>,
        requested_model: &str,
        mut effort: String,
        agent_name: Option<&str>,
        on_delta: &mut Option<&mut dyn FnMut(&str)>,
    ) -> Result<(String, Json, String, String), String> {
        if (effort.is_empty() || effort == "medium") && agent_name.is_some() {
            if let Some((ag_eff, _)) = self.get_agent_config(agent_name) {
                effort = ag_eff;
            }
        }
        let candidates = self.build_target_candidates(requested_model, agent_name);
        let primary_provider = candidates
            .first()
            .map(|(p, _)| p.clone())
            .unwrap_or_else(|| "agy".to_string());

        let mut last_err = String::from("no candidates available");
        let mut prev_limited: Option<String> = None;
        for (idx, (provider, target_model)) in candidates.iter().enumerate() {
            if is_cancelled() {
                return Err("Interrupted by user".to_string());
            }
            let progress = match &prev_limited {
                Some(p) => format!("{p} limited, falling back to {provider}/{target_model}"),
                None => format!("Routing to {provider}/{target_model}"),
            };
            set_progress(&progress);
            let res = match provider.as_str() {
                "agy" => {
                    let resolved =
                        resolve_agy_model(target_model, &effort, &self.cfg.default_model);
                    self.sticky
                        .run_cached_turn(&resolved, messages, tools, on_delta)
                        .map(|(out, usage)| (out, usage, resolved))
                }
                "codex" => {
                    execute_codex_turn(&self.cfg, messages, tools, target_model, &effort, on_delta)
                }
                "claude" => {
                    execute_claude_turn(&self.cfg, messages, tools, target_model, &effort, on_delta)
                }
                _ => continue,
            };

            match res {
                Ok((out, usage, resolved)) => {
                    let mut st = self.state.lock().unwrap();
                    *st.provider_calls.entry(provider.clone()).or_insert(0) += 1;
                    if idx > 0 || provider != &primary_provider {
                        st.fallback_events.push(Json::obj(vec![
                            ("ts", Json::Num(now_sec() as f64)),
                            ("agent", Json::Str(agent_name.unwrap_or("").to_string())),
                            ("requested_model", Json::Str(requested_model.to_string())),
                            ("from_provider", Json::Str(primary_provider)),
                            ("to_provider", Json::Str(provider.clone())),
                            ("resolved_model", Json::Str(resolved.clone())),
                            ("effort", Json::Str(effort)),
                        ]));
                        if st.fallback_events.len() > 30 {
                            let drain = st.fallback_events.len() - 30;
                            st.fallback_events.drain(0..drain);
                        }
                    }
                    return Ok((out, usage, resolved, provider.clone()));
                }
                Err(TurnError::Cancelled) => {
                    return Err("Interrupted by user".to_string());
                }
                Err(TurnError::RateLimit(msg)) => {
                    self.mark_limited(provider, &msg, self.cfg.cooldown_sec);
                    prev_limited = Some(provider.clone());
                    last_err = msg;
                }
                Err(TurnError::Runtime(msg)) => {
                    if is_rate_limit_message(&msg) {
                        self.mark_limited(provider, &msg, self.cfg.cooldown_sec);
                        prev_limited = Some(provider.clone());
                    }
                    last_err = msg;
                    if idx + 1 >= candidates.len() {
                        break;
                    }
                }
            }
        }
        Err(format!(
            "All CLI OAuth providers in chain failed (last error: {last_err})"
        ))
    }

    fn status(&self) -> Json {
        let now = now_f64();
        let routes = load_agent_routes(&self.cfg.routes_path);
        let st = self.state.lock().unwrap();
        let mut provs = Vec::new();
        for p in ["claude", "codex", "agy"] {
            let until = st.cooldown_until.get(p).copied().unwrap_or(0.0);
            let rem = ((until - now).max(0.0) * 10.0).round() / 10.0;
            let calls = st.provider_calls.get(p).copied().unwrap_or(0);
            let last_err = st.last_error.get(p).cloned().unwrap_or_default();
            provs.push((
                p.to_string(),
                Json::obj(vec![
                    ("available", Json::Bool(rem == 0.0)),
                    ("cooldown_remaining_sec", Json::Num(rem)),
                    ("calls", Json::Num(calls as f64)),
                    ("last_limit_error", Json::Str(last_err)),
                ]),
            ));
        }
        let recent_start = st.fallback_events.len().saturating_sub(10);
        let recent = st.fallback_events[recent_start..].to_vec();
        Json::obj(vec![
            (
                "fallback_order",
                Json::Arr(st.fallback_order.iter().cloned().map(Json::Str).collect()),
            ),
            ("providers", Json::Obj(provs)),
            (
                "agents",
                routes
                    .get("agents")
                    .cloned()
                    .unwrap_or_else(|| Json::Obj(Vec::new())),
            ),
            ("recent_fallbacks", Json::Arr(recent)),
        ])
    }
}

struct HttpRequest {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: String,
}

fn read_http_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut reader = BufReader::new(stream);
    let mut req_line = String::new();
    if reader.read_line(&mut req_line).ok()? == 0 {
        return None;
    }
    let parts: Vec<&str> = req_line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let method = parts[0].to_uppercase();
    let full_path = parts[1];
    let (path, query) = match full_path.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (full_path.to_string(), String::new()),
    };

    let mut headers = HashMap::new();
    let mut content_len = 0usize;
    loop {
        let mut hline = String::new();
        if reader.read_line(&mut hline).ok()? == 0 {
            break;
        }
        let trimmed = hline.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_len = val.parse().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }

    let mut body_bytes = vec![0u8; content_len];
    if content_len > 0 {
        reader.read_exact(&mut body_bytes).ok()?;
    }
    let body = String::from_utf8_lossy(&body_bytes).into_owned();

    Some(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn send_json_response(stream: &mut TcpStream, status: u16, status_text: &str, payload: &Json) {
    let body = payload.dump();
    let resp = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn extract_agent_and_subpath(
    raw_path: &str,
    headers: &HashMap<String, String>,
) -> (Option<String>, String) {
    let trimmed = raw_path.strip_prefix("/v1").unwrap_or(raw_path);
    if let Some(rest) = trimmed.strip_prefix("/agents/") {
        if let Some((ag, sub)) = rest.split_once('/') {
            let sub_path = format!("/{sub}");
            let normalized = if matches!(sub_path.as_str(), "/models" | "/chat/completions") {
                format!("/v1{sub_path}")
            } else {
                sub_path
            };
            return (Some(ag.to_string()), normalized);
        }
    }
    if let Some(h) = headers.get("x-agent-name") {
        if !h.trim().is_empty() {
            return (Some(h.trim().to_string()), raw_path.to_string());
        }
    }
    if let Some(auth) = headers.get("authorization") {
        let low = auth.trim().to_lowercase();
        if let Some(ag) = low.strip_prefix("bearer agent:") {
            if !ag.trim().is_empty() {
                return (Some(ag.trim().to_string()), raw_path.to_string());
            }
        }
    }
    (None, raw_path.to_string())
}

fn handle_client(
    mut stream: TcpStream,
    cfg: &Config,
    router: &ProviderRouter,
    standby: &StandbyModelPool,
    sticky: &StickySessionManager,
) {
    let Some(req) = read_http_request(&mut stream) else {
        return;
    };
    let (mut agent_name, path) = extract_agent_and_subpath(&req.path, &req.headers);

    if req.method == "GET" {
        if matches!(path.as_str(), "/health" | "/v1/router/status") {
            let payload = Json::obj(vec![
                ("status", Json::Str("ok".to_string())),
                ("service", Json::Str("cligate".to_string())),
                ("default_model", Json::Str(cfg.default_model.clone())),
                ("workdir", Json::Str(cfg.workdir.clone())),
                ("max_prompt_bytes", Json::Num(cfg.max_prompt_bytes as f64)),
                (
                    "active_engine",
                    Json::Str("cligate-rust-multi-cli-oauth".to_string()),
                ),
                ("router", router.status()),
                ("pool", standby.status()),
                ("prompt_cache", sticky.stats()),
            ]);
            send_json_response(&mut stream, 200, "OK", &payload);
            return;
        }

        if path == "/v1/router/agents" {
            let routes = load_agent_routes(&cfg.routes_path);
            send_json_response(&mut stream, 200, "OK", &routes);
            return;
        }

        if path.starts_with("/v1/models") || path.starts_with("/models") {
            let mut filter_prov = String::new();
            for pair in req.query.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    if k == "provider" {
                        filter_prov = v.trim().to_lowercase();
                    }
                }
            }
            let now = now_sec();
            let mut data = Vec::new();
            for (m, prov, efforts) in all_known_models() {
                if !filter_prov.is_empty() && prov != filter_prov {
                    continue;
                }
                let owner = match prov {
                    "agy" => "antigravity",
                    "codex" => "openai-codex",
                    "claude" => "anthropic-claude-code",
                    other => other,
                };
                data.push(Json::obj(vec![
                    ("id", Json::Str(m.to_string())),
                    ("object", Json::Str("model".to_string())),
                    ("created", Json::Num(now as f64)),
                    ("owned_by", Json::Str(owner.to_string())),
                    ("provider", Json::Str(prov.to_string())),
                    (
                        "supported_reasoning_efforts",
                        Json::Arr(
                            efforts
                                .iter()
                                .map(|e| Json::Str((*e).to_string()))
                                .collect(),
                        ),
                    ),
                ]));
            }
            let agent_json = agent_name.map(Json::Str).unwrap_or(Json::Null);
            let payload = Json::obj(vec![
                ("object", Json::Str("list".to_string())),
                ("data", Json::Arr(data)),
                ("agent", agent_json),
            ]);
            send_json_response(&mut stream, 200, "OK", &payload);
            return;
        }

        send_json_response(
            &mut stream,
            404,
            "Not Found",
            &Json::obj(vec![(
                "error",
                Json::obj(vec![
                    ("message", Json::Str(format!("Not found: {}", req.path))),
                    ("type", Json::Str("invalid_request_error".to_string())),
                ]),
            )]),
        );
        return;
    }

    if req.method == "POST" {
        if path == "/v1/router/reset" {
            let parsed = parse_json(&req.body).unwrap_or_else(|_| Json::Obj(Vec::new()));
            let prov = parsed.get("provider").and_then(|v| v.as_str());
            let status = router.reset_cooldown(prov);
            send_json_response(&mut stream, 200, "OK", &status);
            return;
        }

        if path == "/v1/router/agents" {
            let Ok(parsed) = parse_json(&req.body) else {
                send_json_response(
                    &mut stream,
                    400,
                    "Bad Request",
                    &Json::obj(vec![(
                        "error",
                        Json::obj(vec![("message", Json::Str("Invalid JSON".to_string()))]),
                    )]),
                );
                return;
            };
            let ag = parsed
                .get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let chain: Vec<String> = parsed
                .get("chain")
                .and_then(|v| v.as_arr())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let eff = parsed.get("effort").and_then(|v| v.as_str());
            match router.set_agent_chain(&ag, &chain, eff) {
                Ok(updated) => {
                    let resp = Json::obj(vec![
                        ("status", Json::Str("ok".to_string())),
                        ("agent", Json::Str(ag)),
                        ("config", updated),
                    ]);
                    send_json_response(&mut stream, 200, "OK", &resp);
                }
                Err(e) => {
                    send_json_response(
                        &mut stream,
                        400,
                        "Bad Request",
                        &Json::obj(vec![("error", Json::obj(vec![("message", Json::Str(e))]))]),
                    );
                }
            }
            return;
        }

        if !(path.starts_with("/v1/chat/completions") || path.starts_with("/chat/completions")) {
            send_json_response(
                &mut stream,
                404,
                "Not Found",
                &Json::obj(vec![(
                    "error",
                    Json::obj(vec![
                        ("message", Json::Str(format!("Not found: {}", req.path))),
                        ("type", Json::Str("invalid_request_error".to_string())),
                    ]),
                )]),
            );
            return;
        }

        let Ok(body_json) = parse_json(&req.body) else {
            send_json_response(
                &mut stream,
                400,
                "Bad Request",
                &Json::obj(vec![(
                    "error",
                    Json::obj(vec![
                        ("message", Json::Str("Invalid JSON body".to_string())),
                        ("type", Json::Str("invalid_request_error".to_string())),
                    ]),
                )]),
            );
            return;
        };

        if agent_name.is_none() {
            agent_name = body_json
                .get("agent")
                .or_else(|| body_json.get("user"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
        }

        let effort = extract_effort(&body_json);
        let default_for_turn = router
            .get_agent_config(agent_name.as_deref())
            .and_then(|(_, chain)| chain.into_iter().next())
            .unwrap_or_else(|| cfg.default_model.clone());
        let requested_model = body_json
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&default_for_turn)
            .to_string();

        let messages = body_json
            .get("messages")
            .and_then(|v| v.as_arr())
            .unwrap_or(&[]);
        let tools = body_json.get("tools").and_then(|v| v.as_arr());
        let stream_mode = body_json
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let completion_id = gen_hex_id("chatcmpl-");
        let created = now_sec();

        if !stream_mode || tools.map(|t| !t.is_empty()).unwrap_or(false) {
            let mut no_cb: Option<&mut dyn FnMut(&str)> = None;
            match router.execute_turn(
                messages,
                tools,
                &requested_model,
                effort,
                agent_name.as_deref(),
                &mut no_cb,
            ) {
                Ok((raw_output, usage, resolved_model, used_provider)) => {
                    let (text_out, tool_calls) = parse_tool_calls(&raw_output);
                    let finish_reason = if tool_calls.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    };

                    if !stream_mode {
                        let content_val = if !tool_calls.is_empty() && text_out.is_empty() {
                            Json::Null
                        } else {
                            Json::Str(text_out)
                        };
                        let mut msg_entries = vec![
                            ("role", Json::Str("assistant".to_string())),
                            ("content", content_val),
                        ];
                        if !tool_calls.is_empty() {
                            msg_entries.push(("tool_calls", Json::Arr(tool_calls)));
                        }
                        let agent_json = agent_name.map(Json::Str).unwrap_or(Json::Null);
                        let resp_payload = Json::obj(vec![
                            ("id", Json::Str(completion_id)),
                            ("object", Json::Str("chat.completion".to_string())),
                            ("created", Json::Num(created as f64)),
                            ("model", Json::Str(resolved_model)),
                            ("provider", Json::Str(used_provider)),
                            ("agent", agent_json),
                            (
                                "choices",
                                Json::Arr(vec![Json::obj(vec![
                                    ("index", Json::Num(0.0)),
                                    ("message", Json::obj(msg_entries)),
                                    ("finish_reason", Json::Str(finish_reason.to_string())),
                                ])]),
                            ),
                            ("usage", usage),
                        ]);
                        send_json_response(&mut stream, 200, "OK", &resp_payload);
                        return;
                    }

                    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(headers.as_bytes());
                    let mut delta_entries = vec![("role", Json::Str("assistant".to_string()))];
                    if !text_out.is_empty() {
                        delta_entries.push(("content", Json::Str(text_out)));
                    }
                    if !tool_calls.is_empty() {
                        let indexed: Vec<Json> = tool_calls
                            .into_iter()
                            .enumerate()
                            .map(|(idx, tc)| {
                                Json::obj(vec![
                                    ("index", Json::Num(idx as f64)),
                                    ("id", tc.get("id").cloned().unwrap_or(Json::Null)),
                                    ("type", Json::Str("function".to_string())),
                                    (
                                        "function",
                                        tc.get("function").cloned().unwrap_or(Json::Null),
                                    ),
                                ])
                            })
                            .collect();
                        delta_entries.push(("tool_calls", Json::Arr(indexed)));
                    }
                    let chunk1 = Json::obj(vec![
                        ("id", Json::Str(completion_id.clone())),
                        ("object", Json::Str("chat.completion.chunk".to_string())),
                        ("created", Json::Num(created as f64)),
                        ("model", Json::Str(resolved_model.clone())),
                        ("provider", Json::Str(used_provider.clone())),
                        (
                            "choices",
                            Json::Arr(vec![Json::obj(vec![
                                ("index", Json::Num(0.0)),
                                ("delta", Json::obj(delta_entries)),
                                ("finish_reason", Json::Null),
                            ])]),
                        ),
                    ]);
                    let chunk2 = Json::obj(vec![
                        ("id", Json::Str(completion_id)),
                        ("object", Json::Str("chat.completion.chunk".to_string())),
                        ("created", Json::Num(created as f64)),
                        ("model", Json::Str(resolved_model)),
                        ("provider", Json::Str(used_provider)),
                        (
                            "choices",
                            Json::Arr(vec![Json::obj(vec![
                                ("index", Json::Num(0.0)),
                                ("delta", Json::Obj(Vec::new())),
                                ("finish_reason", Json::Str(finish_reason.to_string())),
                            ])]),
                        ),
                    ]);
                    let _ = stream.write_all(format!("data: {}\n\n", chunk1.dump()).as_bytes());
                    let _ = stream.write_all(format!("data: {}\n\n", chunk2.dump()).as_bytes());
                    let _ = stream.write_all(b"data: [DONE]\n\n");
                    let _ = stream.flush();
                }
                Err(e) => {
                    send_json_response(
                        &mut stream,
                        500,
                        "Internal Server Error",
                        &Json::obj(vec![(
                            "error",
                            Json::obj(vec![
                                ("message", Json::Str(e)),
                                ("type", Json::Str("server_error".to_string())),
                            ]),
                        )]),
                    );
                }
            }
            return;
        }

        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(headers.as_bytes());

        let role_chunk = Json::obj(vec![
            ("id", Json::Str(completion_id.clone())),
            ("object", Json::Str("chat.completion.chunk".to_string())),
            ("created", Json::Num(created as f64)),
            ("model", Json::Str(requested_model.clone())),
            (
                "choices",
                Json::Arr(vec![Json::obj(vec![
                    ("index", Json::Num(0.0)),
                    (
                        "delta",
                        Json::obj(vec![("role", Json::Str("assistant".to_string()))]),
                    ),
                    ("finish_reason", Json::Null),
                ])]),
            ),
        ]);
        let _ = stream.write_all(format!("data: {}\n\n", role_chunk.dump()).as_bytes());
        let _ = stream.flush();

        let cid_ref = completion_id.clone();
        let mod_ref = requested_model.clone();
        let mut send_delta = |delta_text: &str| {
            let chunk = Json::obj(vec![
                ("id", Json::Str(cid_ref.clone())),
                ("object", Json::Str("chat.completion.chunk".to_string())),
                ("created", Json::Num(created as f64)),
                ("model", Json::Str(mod_ref.clone())),
                (
                    "choices",
                    Json::Arr(vec![Json::obj(vec![
                        ("index", Json::Num(0.0)),
                        (
                            "delta",
                            Json::obj(vec![("content", Json::Str(delta_text.to_string()))]),
                        ),
                        ("finish_reason", Json::Null),
                    ])]),
                ),
            ]);
            let _ = stream.write_all(format!("data: {}\n\n", chunk.dump()).as_bytes());
            let _ = stream.flush();
        };
        let mut err_to_send = None;
        {
            let mut cb_opt: Option<&mut dyn FnMut(&str)> = Some(&mut send_delta);
            if let Err(e) = router.execute_turn(
                messages,
                tools,
                &requested_model,
                effort,
                agent_name.as_deref(),
                &mut cb_opt,
            ) {
                err_to_send = Some(e);
            }
        }
        if let Some(e) = err_to_send {
            send_delta(&format!("\n[Error: {e}]"));
        }

        let stop_chunk = Json::obj(vec![
            ("id", Json::Str(completion_id)),
            ("object", Json::Str("chat.completion.chunk".to_string())),
            ("created", Json::Num(created as f64)),
            ("model", Json::Str(requested_model)),
            (
                "choices",
                Json::Arr(vec![Json::obj(vec![
                    ("index", Json::Num(0.0)),
                    ("delta", Json::Obj(Vec::new())),
                    ("finish_reason", Json::Str("stop".to_string())),
                ])]),
            ),
        ]);
        let _ = stream.write_all(format!("data: {}\n\n", stop_chunk.dump()).as_bytes());
        let _ = stream.write_all(b"data: [DONE]\n\n");
        let _ = stream.flush();
    }
}

fn run_server(mut cfg: Config, host_override: Option<String>, port_override: Option<u16>) {
    if let Some(h) = host_override {
        cfg.host = h;
    }
    if let Some(p) = port_override {
        cfg.port = p;
    }
    let _ = fs::create_dir_all(&cfg.workdir);
    let standby = Arc::new(StandbyModelPool::new(cfg.clone()));
    standby.prewarm_async(&resolve_agy_model(
        &cfg.default_model,
        "medium",
        &cfg.default_model,
    ));
    let sticky = Arc::new(StickySessionManager::new(cfg.clone(), Arc::clone(&standby)));
    let router = Arc::new(ProviderRouter::new(cfg.clone(), Arc::clone(&sticky)));

    let bind_addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = TcpListener::bind(&bind_addr).unwrap_or_else(|e| {
        eprintln!("Failed to bind {bind_addr}: {e}");
        std::process::exit(1);
    });
    println!(
        "[cligate] Listening on http://{bind_addr}/v1 (default_model={}, max_prompt_bytes={})",
        cfg.default_model, cfg.max_prompt_bytes
    );

    for stream in listener.incoming().flatten() {
        let cfg_c = cfg.clone();
        let router_c = Arc::clone(&router);
        let standby_c = Arc::clone(&standby);
        let sticky_c = Arc::clone(&sticky);
        thread::spawn(move || {
            handle_client(stream, &cfg_c, &router_c, &standby_c, &sticky_c);
        });
    }
}

fn infer_provider_and_model(raw_model: &str) -> (String, String) {
    let item = raw_model.trim();
    if let Some((p, m)) = item.split_once('/') {
        let p_low = p.trim().to_lowercase();
        let mapped = match p_low.as_str() {
            "claude" | "anthropic" => "anthropic",
            "codex" | "openai-codex" => "openai-codex",
            "agy" | "antigravity" => "agy",
            other => other,
        };
        return (mapped.to_string(), m.trim().to_string());
    }
    let low = item.to_lowercase();
    if low.starts_with("gpt-5") || low.starts_with("gpt-6") {
        ("openai-codex".to_string(), item.to_string())
    } else if low.starts_with("gemini-") || low.starts_with("gpt-oss-") {
        ("agy".to_string(), item.to_string())
    } else if low.starts_with("claude-") {
        ("anthropic".to_string(), item.to_string())
    } else {
        ("agy".to_string(), item.to_string())
    }
}

fn http_call(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Json>,
) -> Result<Json, String> {
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).map_err(|e| e.to_string())?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let body_str = body.map(|b| b.dump()).unwrap_or_default();
    let req = if body.is_some() {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_str}",
            body_str.len()
        )
    } else {
        format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
    };
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut raw_resp = String::new();
    stream
        .read_to_string(&mut raw_resp)
        .map_err(|e| e.to_string())?;
    let (_, json_body) = raw_resp
        .split_once("\r\n\r\n")
        .ok_or_else(|| "invalid HTTP response".to_string())?;
    parse_json(json_body.trim())
}

fn cmd_agent_status(cfg: &Config) {
    let routes = load_agent_routes(&cfg.routes_path);
    let Some(agents) = routes.get("agents").and_then(|v| v.as_obj()) else {
        println!("No agent chains configured in {}", cfg.routes_path);
        return;
    };
    if agents.is_empty() {
        println!("No agent chains configured in {}", cfg.routes_path);
        return;
    }
    println!(
        "{:<15} | {:<7} | PRIMARY -> FALLBACK CHAIN",
        "AGENT", "EFFORT"
    );
    println!("{}", "-".repeat(78));
    for (name, info) in agents {
        let eff = info
            .get("effort")
            .and_then(|v| v.as_str())
            .unwrap_or("medium");
        let chain = info.get("chain").and_then(|v| v.as_arr()).unwrap_or(&[]);
        let mut formatted = Vec::new();
        for (idx, m) in chain.iter().enumerate() {
            if let Some(m_str) = m.as_str() {
                let (prov, bare) = infer_provider_and_model(m_str);
                let tag = if idx == 0 {
                    "default".to_string()
                } else {
                    format!("fb#{idx}")
                };
                formatted.push(format!("[{tag}] {prov}/{bare}"));
            }
        }
        println!("{name:<15} | {eff:<7} | {}", formatted.join("  ->  "));
    }
}

fn cmd_agent_set(cfg: &Config, agent_name: &str, models: &[String], effort: Option<&str>) {
    let name = agent_name.trim().to_lowercase();
    let mut routes = load_agent_routes(&cfg.routes_path);
    let prev_eff = routes
        .get("agents")
        .and_then(|a| a.get(&name))
        .and_then(|c| c.get("effort"))
        .and_then(|v| v.as_str())
        .unwrap_or("medium")
        .to_string();
    let eff = effort.unwrap_or(&prev_eff).trim().to_lowercase();
    let chain_json: Vec<Json> = models.iter().cloned().map(Json::Str).collect();
    let entry = Json::obj(vec![
        ("effort", Json::Str(eff.clone())),
        ("chain", Json::Arr(chain_json.clone())),
    ]);
    let agents_obj = routes.get_mut_or_insert("agents", Json::Obj(Vec::new()));
    agents_obj.set(&name, entry);
    if let Err(e) = save_agent_routes(&cfg.routes_path, &routes) {
        eprintln!("Failed to save {}: {e}", cfg.routes_path);
        std::process::exit(1);
    }

    let body = Json::obj(vec![
        ("agent", Json::Str(name.clone())),
        ("chain", Json::Arr(chain_json)),
        ("effort", Json::Str(eff.clone())),
    ]);
    let _ = http_call(
        &cfg.host,
        cfg.port,
        "POST",
        "/v1/router/agents",
        Some(&body),
    );

    let chain_str: Vec<String> = models
        .iter()
        .map(|m| {
            let (p, b) = infer_provider_and_model(m);
            format!("{p}/{b}")
        })
        .collect();
    println!("Updated agent '{name}' (effort={eff}):");
    println!("  Chain: {}", chain_str.join(" -> "));
    println!("  Saved: {}", cfg.routes_path);
}

fn cmd_models(filter_provider: &str) {
    let p_filter = filter_provider.trim().to_lowercase();
    println!("{:<32} | {:<8} | EFFORT LEVELS", "MODEL ID", "PROVIDER");
    println!("{}", "-".repeat(72));
    for (m, prov, efforts) in all_known_models() {
        if !p_filter.is_empty() && prov != p_filter {
            continue;
        }
        println!("{m:<32} | {prov:<8} | {}", efforts.join(", "));
    }
}

fn build_local_router(cfg: &Config) -> Arc<ProviderRouter> {
    let _ = fs::create_dir_all(&cfg.workdir);
    let standby = Arc::new(StandbyModelPool::new(cfg.clone()));
    let sticky = Arc::new(StickySessionManager::new(cfg.clone(), standby));
    Arc::new(ProviderRouter::new(cfg.clone(), sticky))
}

struct CliRunFlags {
    agent: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    resume: Option<String>,
    prompt: String,
}

fn parse_cli_run_flags(args: &[String]) -> CliRunFlags {
    let mut agent = None;
    let mut model = None;
    let mut effort = None;
    let mut resume = None;
    let mut prompt_parts = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-a" | "--agent" if i + 1 < args.len() => {
                agent = Some(args[i + 1].clone());
                i += 2;
            }
            "-m" | "--model" if i + 1 < args.len() => {
                model = Some(args[i + 1].clone());
                i += 2;
            }
            "-e" | "--effort" | "--reasoning" if i + 1 < args.len() => {
                effort = Some(args[i + 1].clone());
                i += 2;
            }
            "-c" | "--continue" => {
                resume = Some("latest".to_string());
                i += 1;
            }
            "-r" | "--resume" => {
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    resume = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    resume = Some("picker".to_string());
                    i += 1;
                }
            }
            other => {
                prompt_parts.push(other.to_string());
                i += 1;
            }
        }
    }
    CliRunFlags {
        agent,
        model,
        effort,
        resume,
        prompt: prompt_parts.join(" "),
    }
}

fn print_help() {
    println!(
        "cligate — OpenAI-Compatible CLI-OAuth Gateway, TUI & Per-Agent Fallback Router\n\n\
         Commands:\n  \
         cligate [-c|--continue] [-r|--resume [id]] [-a <agent>] [-m <model>] [-e <level>]\n  \
         cligate tui [-c] [-r [id]] [-a <agent>] [-m <model>] [-e <level>]\n  \
         cligate ask [-a <agent>] [-m <model>] [-e <level>] <prompt>\n  \
         cligate serve [--host 127.0.0.1] [--port 8787]       Start OpenAI HTTP gateway server\n  \
         cligate status                                       Show live router health, cooldowns & cache\n  \
         cligate models [claude|codex|agy]                    List native models & supported effort levels\n  \
         cligate reset [claude|codex|agy]                     Reset provider rate-limit cooldowns\n  \
         cligate agent status                                 Show per-agent primary & fallback model chains\n  \
         cligate agent set <agent> <m1> <m2> [m3 ...] [--effort <low|medium|high|xhigh|max>]"
    );
}

fn main() {
    let cfg = Config::from_env();
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        let router = build_local_router(&cfg);
        tui::run_tui(cfg, router, None, None, None, None);
        return;
    }
    if matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return;
    }
    if args[0].starts_with('-') {
        let flags = parse_cli_run_flags(&args);
        let router = build_local_router(&cfg);
        tui::run_tui(
            cfg,
            router,
            flags.agent,
            flags.model,
            flags.effort,
            flags.resume,
        );
        return;
    }

    match args[0].as_str() {
        "tui" | "chat" => {
            let flags = parse_cli_run_flags(&args[1..]);
            let router = build_local_router(&cfg);
            tui::run_tui(
                cfg,
                router,
                flags.agent,
                flags.model,
                flags.effort,
                flags.resume,
            );
        }
        "ask" | "run" => {
            let flags = parse_cli_run_flags(&args[1..]);
            let (agent, model, effort, prompt) =
                (flags.agent, flags.model, flags.effort, flags.prompt);
            if prompt.trim().is_empty() {
                eprintln!("Usage: cligate ask [-a <agent>] [-m <model>] [-e <effort>] <prompt>");
                std::process::exit(1);
            }
            let router = build_local_router(&cfg);
            let req_model = model.unwrap_or_else(|| cfg.default_model.clone());
            let req_effort = effort.unwrap_or_else(|| "medium".to_string());
            let messages = vec![Json::obj(vec![
                ("role", Json::Str("user".to_string())),
                ("content", Json::Str(prompt)),
            ])];
            let mut printed_delta = false;
            let res = {
                let mut on_delta = |chunk: &str| {
                    printed_delta = true;
                    print!("{chunk}");
                    let _ = std::io::stdout().flush();
                };
                let mut cb_opt: Option<&mut dyn FnMut(&str)> = Some(&mut on_delta);
                router.execute_turn(
                    &messages,
                    None,
                    &req_model,
                    req_effort,
                    agent.as_deref(),
                    &mut cb_opt,
                )
            };
            match res {
                Ok((final_text, _, _, _)) => {
                    if !printed_delta {
                        println!("{final_text}");
                    } else {
                        println!();
                    }
                }
                Err(e) => {
                    eprintln!("\nError: {e}");
                    std::process::exit(1);
                }
            }
        }
        "serve" => {
            let mut host_ov = None;
            let mut port_ov = None;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--host" && i + 1 < args.len() {
                    host_ov = Some(args[i + 1].clone());
                    i += 2;
                } else if args[i] == "--port" && i + 1 < args.len() {
                    port_ov = args[i + 1].parse().ok();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            run_server(cfg, host_ov, port_ov);
        }
        "status" => match http_call(&cfg.host, cfg.port, "GET", "/v1/router/status", None) {
            Ok(resp) => println!("{}", resp.dump_pretty()),
            Err(e) => {
                eprintln!(
                    "Error connecting to cligate at http://{}:{}: {e}",
                    cfg.host, cfg.port
                );
                std::process::exit(1);
            }
        },
        "models" => {
            let filter = args.get(1).map(|s| s.as_str()).unwrap_or("");
            cmd_models(filter);
        }
        "reset" => {
            let prov_json = args
                .get(1)
                .map(|s| Json::Str(s.clone()))
                .unwrap_or(Json::Null);
            let body = Json::obj(vec![("provider", prov_json)]);
            match http_call(&cfg.host, cfg.port, "POST", "/v1/router/reset", Some(&body)) {
                Ok(resp) => println!("{}", resp.dump_pretty()),
                Err(e) => {
                    eprintln!("Error connecting to cligate: {e}");
                    std::process::exit(1);
                }
            }
        }
        "agent" | "set" | "list" => {
            let sub_args = if args[0] == "agent" {
                &args[1..]
            } else {
                &args[..]
            };
            let sub = sub_args.first().map(|s| s.as_str()).unwrap_or("status");
            if matches!(sub, "status" | "list") {
                cmd_agent_status(&cfg);
                return;
            }
            if sub == "set" {
                if sub_args.len() < 3 {
                    eprintln!("Usage: cligate agent set <agent> <primary_model> [fallback_1 fallback_2 ...] [--effort <level>]");
                    std::process::exit(1);
                }
                let agent_name = &sub_args[1];
                let rest = &sub_args[2..];
                let mut effort: Option<String> = None;
                let mut models = Vec::new();
                let mut i = 0;
                while i < rest.len() {
                    if matches!(rest[i].as_str(), "--effort" | "--reasoning" | "-e")
                        && i + 1 < rest.len()
                    {
                        effort = Some(rest[i + 1].clone());
                        i += 2;
                    } else if let Some(val) = rest[i].strip_prefix("--effort=") {
                        effort = Some(val.to_string());
                        i += 1;
                    } else {
                        models.push(rest[i].clone());
                        i += 1;
                    }
                }
                if models.is_empty() {
                    eprintln!("Error: provide at least 1 model for the chain.");
                    std::process::exit(1);
                }
                cmd_agent_set(&cfg, agent_name, &models, effort.as_deref());
                return;
            }
            eprintln!("Unknown agent subcommand '{sub}'.");
            std::process::exit(1);
        }
        other => {
            eprintln!("Unknown command '{other}'. Run 'cligate --help' for usage.");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(routes_path: &str) -> Config {
        let mut cfg = Config::from_env();
        cfg.routes_path = routes_path.to_string();
        cfg
    }

    #[test]
    fn test_agy_effort_resolution() {
        assert_eq!(
            resolve_agy_model("gemini-4-neon", "xhigh", "gemini-4-argon"),
            "gemini-4-neon-xhigh"
        );
        assert_eq!(
            resolve_agy_model("gemini-4-argon", "high", "gemini-4-argon"),
            "gemini-4-argon-high"
        );
        assert_eq!(
            resolve_agy_model("gemini-4-argon", "xhigh", "gemini-4-argon"),
            "gemini-4-argon-high"
        );
        assert_eq!(
            resolve_agy_model("gemini-4-carbon", "low", "gemini-4-argon"),
            "gemini-4-carbon-low"
        );
    }

    #[test]
    fn test_codex_and_claude_effort_resolution() {
        assert_eq!(
            resolve_codex_model_and_effort("gpt-6.1-sol-900k", "max"),
            ("gpt-6.1-sol".to_string(), "max".to_string())
        );
        assert_eq!(
            resolve_codex_model_and_effort("gpt-5.4", "max"),
            ("gpt-5.4".to_string(), "xhigh".to_string())
        );
        assert_eq!(
            resolve_claude_model_and_effort("claude-opus-5-5", "max"),
            ("claude-opus-5-5".to_string(), "max".to_string())
        );
    }

    #[test]
    fn test_per_agent_fallback_chain_and_cooldown() {
        let tmp_path = env::temp_dir()
            .join("cligate_rust_test_routes.json")
            .to_string_lossy()
            .into_owned();
        let _ = fs::remove_file(&tmp_path);
        let cfg = test_config(&tmp_path);
        let standby = Arc::new(StandbyModelPool::new(cfg.clone()));
        let sticky = Arc::new(StickySessionManager::new(cfg.clone(), standby));
        let router = ProviderRouter::new(cfg, sticky);

        router
            .set_agent_chain(
                "coder",
                &[
                    "claude-opus-5-5".to_string(),
                    "gpt-6.1-sol".to_string(),
                    "gemini-4-argon".to_string(),
                ],
                Some("high"),
            )
            .unwrap();

        let cands = router.build_target_candidates("claude-opus-5-5", Some("coder"));
        assert_eq!(
            cands,
            vec![
                ("claude".to_string(), "claude-opus-5-5".to_string()),
                ("codex".to_string(), "gpt-6.1-sol".to_string()),
                ("agy".to_string(), "gemini-4-argon".to_string()),
            ]
        );

        router.mark_limited("claude", "429 usage_limit_reached", 600.0);
        let cands_after = router.build_target_candidates("claude-opus-5-5", Some("coder"));
        assert_eq!(
            cands_after,
            vec![
                ("codex".to_string(), "gpt-6.1-sol".to_string()),
                ("agy".to_string(), "gemini-4-argon".to_string()),
                ("claude".to_string(), "claude-opus-5-5".to_string()),
            ]
        );
        let _ = fs::remove_file(&tmp_path);
    }

    #[test]
    fn test_tool_call_xml_bridge() {
        let raw = "Thinking...\n<tool_call>{\"name\": \"run_command\", \"arguments\": {\"cmd\": \"ls\"}}</tool_call>";
        let (cleaned, calls) = parse_tool_calls(raw);
        assert_eq!(cleaned, "Thinking...");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str()),
            Some("run_command")
        );
    }

    #[test]
    fn test_tui_text_wrapping() {
        let lines = tui::wrap_text_lines("hello world\nsecond line", 10);
        assert_eq!(lines, vec!["hello", "world", "second", "line"]);
    }
}
