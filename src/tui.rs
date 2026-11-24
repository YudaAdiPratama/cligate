use crate::json::{parse_json, Json};
use crate::tools::{
    execute_local_tool, is_read_only_tool, summarize_tool_call, tui_tool_definitions,
    PermissionMode,
};
use crate::{
    all_known_models, gen_hex_id, home_dir, http_call, infer_provider_and_model, is_cancelled,
    load_agent_routes, now_sec, parse_tool_calls, request_cancel, reset_cancel, Config,
    ProviderRouter, PROGRESS,
};
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const ACCENT: &str = "\x1b[38;5;173m";
const GRAY: &str = "\x1b[38;5;244m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
const CODE: &str = "\x1b[38;5;150m";
const RED: &str = "\x1b[38;5;203m";
const GREEN: &str = "\x1b[38;5;114m";
const USER_BG: &str = "\x1b[48;5;236m\x1b[38;5;253m";
const SPINNER: &[&str] = &["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];
const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const COMMANDS: &[(&str, &str)] = &[
    ("/model", "Switch model and reasoning level"),
    ("/effort", "Set reasoning effort level"),
    ("/setfallback", "Configure TUI-only fallback model chain"),
    (
        "/permissions",
        "Configure tool permissions and always-allow",
    ),
    ("/agent", "Switch agent fallback chain"),
    ("/compact", "Summarize and compact conversation history"),
    (
        "/autocompact",
        "Set auto-compact context window limit (or 'off')",
    ),
    ("/resume", "Resume a previous session"),
    ("/new", "Start a new session"),
    ("/status", "Show provider health and cooldowns"),
    ("/reset", "Clear rate-limit cooldowns"),
    ("/clear", "Clear the conversation"),
    ("/help", "Show shortcuts"),
    ("/quit", "Exit cligate"),
];
const HELP_TEXT: &str = "Shortcuts\n  /model       F2 / ctrl+n   switch model and effort level\n  /effort      F3 / ctrl+t   reasoning effort\n  /setfallback [m1 m2 ...]   configure TUI-only fallback chain (or 'reset')\n  /permissions [mode]        tool permissions (ask, always-allow, read-only, reset)\n  /agent       F4 / ctrl+g   agent fallback chain\n  /compact     [focus]       compact conversation into a summary\n  /autocompact [32k|64k|off] set auto-compact token limit\n  /resume                    resume a saved session\n  /new                       start a fresh session\n  /status      F5            provider health\n  /reset       ctrl+r        clear rate-limit cooldowns\n  /clear       ctrl+l        clear conversation\n  scroll: mouse wheel, shift+up/down, PgUp/PgDn | stop: esc or ctrl+c | quit: ctrl+c";

struct RawTermGuard {
    saved_state: Option<String>,
}

impl RawTermGuard {
    fn enter() -> Self {
        let saved = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            });
        let _ = Command::new("stty")
            .args([
                "-icanon", "-echo", "-isig", "-ixon", "min", "1", "time", "0",
            ])
            .stdin(Stdio::inherit())
            .status();
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?1007h\x1b[2J\x1b[H");
        let _ = out.flush();
        Self { saved_state: saved }
    }
}

impl Drop for RawTermGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?1007l\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[0m\x1b[?1049l");
        let _ = out.flush();
        let restore = match self.saved_state {
            Some(ref st) => Command::new("stty")
                .arg(st)
                .stdin(Stdio::inherit())
                .status(),
            None => Command::new("stty")
                .arg("sane")
                .stdin(Stdio::inherit())
                .status(),
        };
        let _ = restore;
    }
}

fn parse_stty_size(out: &std::process::Output) -> Option<(usize, usize)> {
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut parts = s.split_whitespace();
    let r = parts.next()?.parse::<usize>().ok()?;
    let c = parts.next()?.parse::<usize>().ok()?;
    if r >= 10 && c >= 30 {
        Some((r, c))
    } else {
        None
    }
}

fn detect_term_size() -> (usize, usize) {
    let probes: &[&[&str]] = &[
        &["-F", "/dev/tty", "size"],
        &["-f", "/dev/tty", "size"],
        &["size"],
    ];
    for args in probes {
        if let Ok(out) = Command::new("stty")
            .args(*args)
            .stdin(Stdio::inherit())
            .stderr(Stdio::null())
            .output()
        {
            if let Some(sz) = parse_stty_size(&out) {
                return sz;
            }
        }
    }
    if let (Ok(r), Ok(c)) = (std::env::var("LINES"), std::env::var("COLUMNS")) {
        if let (Ok(rows), Ok(cols)) = (r.parse::<usize>(), c.parse::<usize>()) {
            if rows >= 10 && cols >= 30 {
                return (rows, cols);
            }
        }
    }
    (28, 100)
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    ScrollUp,
    ScrollDown,
    F2,
    F3,
    F4,
    F5,
    CtrlC,
    CtrlG,
    CtrlL,
    CtrlN,
    CtrlP,
    CtrlR,
    CtrlT,
    CtrlU,
    Unknown,
}

fn spawn_input_thread() -> Receiver<u8> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut b = [0u8; 1];
        while let Ok(1) = stdin.read(&mut b) {
            if tx.send(b[0]).is_err() {
                break;
            }
        }
    });
    rx
}

fn parse_escape(rx: &Receiver<u8>) -> Key {
    let short = Duration::from_millis(40);
    let Ok(b1) = rx.recv_timeout(short) else {
        return Key::Esc;
    };
    match b1 {
        b'[' => {
            let mut params = String::new();
            loop {
                let Ok(b) = rx.recv_timeout(short) else {
                    return Key::Unknown;
                };
                if (0x40..=0x7e).contains(&b) {
                    if b == b'M' && params.is_empty() {
                        let cb = rx.recv_timeout(short).ok().unwrap_or(0);
                        let _ = rx.recv_timeout(short);
                        let _ = rx.recv_timeout(short);
                        return match cb.saturating_sub(32) & 0x43 {
                            64 => Key::ScrollUp,
                            65 => Key::ScrollDown,
                            _ => Key::Unknown,
                        };
                    }
                    if matches!(b, b'M' | b'm') && params.starts_with('<') {
                        if let Some(btn) = params[1..]
                            .split(';')
                            .next()
                            .and_then(|s| s.parse::<u32>().ok())
                        {
                            return match btn & 0x43 {
                                64 => Key::ScrollUp,
                                65 => Key::ScrollDown,
                                _ => Key::Unknown,
                            };
                        }
                        return Key::Unknown;
                    }
                    return match (b as char, params.as_str()) {
                        ('A', p) if p.contains(';') => Key::ScrollUp,
                        ('B', p) if p.contains(';') => Key::ScrollDown,
                        ('A', _) => Key::Up,
                        ('B', _) => Key::Down,
                        ('C', _) => Key::Right,
                        ('D', _) => Key::Left,
                        ('H', _) | ('~', "1") | ('~', "7") => Key::Home,
                        ('F', _) | ('~', "4") | ('~', "8") => Key::End,
                        ('~', "3") => Key::Delete,
                        ('~', "5") => Key::PageUp,
                        ('~', "6") => Key::PageDown,
                        ('~', "12") => Key::F2,
                        ('~', "13") => Key::F3,
                        ('~', "14") => Key::F4,
                        ('~', "15") => Key::F5,
                        _ => Key::Unknown,
                    };
                }
                params.push(b as char);
            }
        }
        b'O' => match rx.recv_timeout(short) {
            Ok(b'Q') => Key::F2,
            Ok(b'R') => Key::F3,
            Ok(b'S') => Key::F4,
            Ok(b'H') => Key::Home,
            Ok(b'F') => Key::End,
            _ => Key::Unknown,
        },
        _ => Key::Esc,
    }
}

fn decode_key(first: u8, rx: &Receiver<u8>) -> Key {
    match first {
        3 | 4 => Key::CtrlC,
        1 => Key::Home,
        5 => Key::End,
        7 => Key::CtrlG,
        9 => Key::Tab,
        10 | 13 => Key::Enter,
        12 => Key::CtrlL,
        14 => Key::CtrlN,
        16 => Key::CtrlP,
        18 => Key::CtrlR,
        20 => Key::CtrlT,
        21 => Key::CtrlU,
        27 => parse_escape(rx),
        127 | 8 => Key::Backspace,
        c if (32..127).contains(&c) => Key::Char(c as char),
        lead if lead >= 0xC0 => {
            let extra = if lead < 0xE0 {
                1
            } else if lead < 0xF0 {
                2
            } else {
                3
            };
            let mut buf = vec![lead];
            for _ in 0..extra {
                match rx.recv_timeout(Duration::from_millis(40)) {
                    Ok(b) => buf.push(b),
                    Err(_) => break,
                }
            }
            std::str::from_utf8(&buf)
                .ok()
                .and_then(|s| s.chars().next())
                .map(Key::Char)
                .unwrap_or(Key::Unknown)
        }
        _ => Key::Unknown,
    }
}

fn next_key(rx: &Receiver<u8>, wait: Duration) -> Option<Key> {
    match rx.recv_timeout(wait) {
        Ok(b) => Some(decode_key(b, rx)),
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) => Some(Key::CtrlC),
    }
}

pub fn wrap_text_lines(text: &str, max_width: usize) -> Vec<String> {
    let width = max_width.max(10);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let indent: String = raw.chars().take_while(|c| *c == ' ').collect();
        let body = raw.trim_start_matches(' ');
        if body.is_empty() {
            out.push(String::new());
            continue;
        }
        let avail = width.saturating_sub(indent.chars().count()).max(4);
        let mut cur = String::new();
        let mut cur_len = 0usize;
        for word in body.split(' ') {
            let mut w = word;
            let mut wl = w.chars().count();
            while wl > avail {
                if cur_len > 0 {
                    out.push(format!("{indent}{cur}"));
                    cur.clear();
                    cur_len = 0;
                }
                let head: String = w.chars().take(avail).collect();
                w = &w[head.len()..];
                out.push(format!("{indent}{head}"));
                wl = w.chars().count();
            }
            if cur_len == 0 {
                cur.push_str(w);
                cur_len = wl;
            } else if cur_len + 1 + wl <= avail {
                cur.push(' ');
                cur.push_str(w);
                cur_len += 1 + wl;
            } else {
                out.push(format!("{indent}{cur}"));
                cur = w.to_string();
                cur_len = wl;
            }
        }
        if cur_len > 0 {
            out.push(format!("{indent}{cur}"));
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        s.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn pad_or_clip(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n > width {
        clip(s, width)
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

fn style_inline(s: &str, code: &mut bool, bold: &mut bool) -> String {
    let mut out = String::new();
    if *code {
        out.push_str(CODE);
    }
    if *bold {
        out.push_str(BOLD);
    }
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            *code = !*code;
            if *code {
                out.push_str(CODE);
            } else {
                out.push_str(RESET);
                if *bold {
                    out.push_str(BOLD);
                }
            }
            i += 1;
            continue;
        }
        if !*code && chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            *bold = !*bold;
            if *bold {
                out.push_str(BOLD);
            } else {
                out.push_str(RESET);
            }
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    if *code || *bold {
        out.push_str(RESET);
    }
    out
}

fn display_cwd() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && cwd.starts_with(&h) => format!("~{}", &cwd[h.len()..]),
        _ => cwd,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    User,
    Assistant,
    Note,
    Error,
}

struct Entry {
    kind: Kind,
    text: String,
    meta: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PermDecision {
    AllowOnce,
    AlwaysAllowTool,
    AlwaysAllowAll,
    Deny,
}

#[derive(Clone, PartialEq)]
enum Modal {
    None,
    Model {
        selected: usize,
        filter: String,
    },
    Effort {
        selected: usize,
        levels: Vec<String>,
        pending_model: Option<(String, String)>,
    },
    Agent {
        selected: usize,
    },
    Fallback {
        selected: usize,
        filter: String,
        chain: Vec<String>,
    },
    Permissions {
        selected: usize,
    },
    PermissionPrompt {
        selected: usize,
        tool: String,
        summary: String,
    },
    MaxWindow {
        selected: usize,
        custom: String,
    },
    Session {
        selected: usize,
    },
}

#[derive(Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub model: String,
    pub updated_at: u64,
}

fn sessions_dir() -> PathBuf {
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

const MAX_WINDOW_PRESETS: &[(usize, &str)] = &[
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

fn format_tokens_short(n: usize) -> String {
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

fn estimate_history_tokens(history: &[Json]) -> usize {
    history.iter().map(|m| m.dump().len() / 4).sum()
}

struct TuiPrefs {
    model: Option<String>,
    effort: Option<String>,
    fallback_chain: Vec<String>,
    perm_mode: PermissionMode,
    always_allow_tools: Vec<String>,
    auto_compact_tokens: usize,
}

fn load_prefs() -> TuiPrefs {
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

fn write_prefs(st: &TuiState) {
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

fn new_session_id() -> String {
    let raw = gen_hex_id("");
    let short: String = raw.chars().rev().take(8).collect();
    format!("ses_{short}")
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

enum Action {
    None,
    Submit(String),
    Command(String),
    SaveFallback(Vec<String>),
    Quit,
}

type ModelRow = (&'static str, &'static str, &'static [&'static str]);

struct TuiState {
    session_id: String,
    model: String,
    effort: String,
    agent: Option<String>,
    tui_fallback: Vec<String>,
    perm_mode: PermissionMode,
    always_allow_tools: Vec<String>,
    auto_compact_tokens: usize,
    pending_perm: Option<mpsc::Sender<PermDecision>>,
    routes_path: String,
    work_dir: String,
    cwd: String,
    history: Vec<Json>,
    entries: Vec<Entry>,
    input: Vec<char>,
    cursor: usize,
    prompts: Vec<String>,
    prompt_idx: Option<usize>,
    scroll: usize,
    modal: Modal,
    sugg_sel: usize,
    busy: bool,
    busy_label: String,
    busy_since: Instant,
    stream_chars: usize,
    tick: usize,
    badges: Vec<(String, Option<i64>)>,
    notice: String,
    dirty: bool,
    size: (usize, usize),
    size_at: Instant,
    force_clear: bool,
}

impl TuiState {
    fn new(cfg: &Config) -> Self {
        let prefs = load_prefs();
        let work_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string());
        Self {
            session_id: new_session_id(),
            model: prefs.model.unwrap_or_else(|| cfg.default_model.clone()),
            effort: prefs.effort.unwrap_or_else(|| "medium".to_string()),
            agent: None,
            tui_fallback: prefs.fallback_chain,
            perm_mode: prefs.perm_mode,
            always_allow_tools: prefs.always_allow_tools,
            auto_compact_tokens: prefs.auto_compact_tokens,
            pending_perm: None,
            routes_path: cfg.routes_path.clone(),
            work_dir,
            cwd: display_cwd(),
            history: Vec::new(),
            entries: Vec::new(),
            input: Vec::new(),
            cursor: 0,
            prompts: Vec::new(),
            prompt_idx: None,
            scroll: 0,
            modal: Modal::None,
            sugg_sel: 0,
            busy: false,
            busy_label: "Thinking…".to_string(),
            busy_since: Instant::now(),
            stream_chars: 0,
            tick: 0,
            badges: Vec::new(),
            notice: String::new(),
            dirty: false,
            size: detect_term_size(),
            size_at: Instant::now(),
            force_clear: true,
        }
    }

    fn note(&mut self, text: String) {
        self.entries.push(Entry {
            kind: Kind::Note,
            text,
            meta: String::new(),
        });
        self.scroll = 0;
    }

    fn save_session(&self) {
        if self.history.is_empty() {
            return;
        }
        let dir = sessions_dir();
        let _ = fs::create_dir_all(&dir);
        let title = self
            .entries
            .iter()
            .find(|e| e.kind == Kind::User)
            .map(|e| clip(&e.text.replace('\n', " "), 60))
            .unwrap_or_else(|| "session".to_string());
        let entries_json: Vec<Json> = self
            .entries
            .iter()
            .map(|e| {
                let kind_str = match e.kind {
                    Kind::User => "user",
                    Kind::Assistant => "assistant",
                    Kind::Note => "note",
                    Kind::Error => "error",
                };
                Json::obj(vec![
                    ("kind", Json::Str(kind_str.to_string())),
                    ("text", Json::Str(e.text.clone())),
                    ("meta", Json::Str(e.meta.clone())),
                ])
            })
            .collect();
        let doc = Json::obj(vec![
            ("id", Json::Str(self.session_id.clone())),
            ("title", Json::Str(title)),
            ("cwd", Json::Str(self.cwd.clone())),
            ("model", Json::Str(self.model.clone())),
            ("effort", Json::Str(self.effort.clone())),
            (
                "agent",
                self.agent
                    .as_ref()
                    .map(|a| Json::Str(a.clone()))
                    .unwrap_or(Json::Null),
            ),
            ("updated_at", Json::Num(now_sec() as f64)),
            ("history", Json::Arr(self.history.clone())),
            ("entries", Json::Arr(entries_json)),
        ]);
        let target = dir.join(format!("{}.json", self.session_id));
        let tmp = dir.join(format!("{}.json.tmp", self.session_id));
        if fs::write(&tmp, format!("{}\n", doc.dump_pretty())).is_ok() {
            let _ = fs::rename(&tmp, &target);
        }
    }

    fn load_session(&mut self, query: &str) -> Result<String, String> {
        let sessions = list_saved_sessions();
        let target_id = if query == "latest" {
            sessions
                .first()
                .map(|s| s.id.clone())
                .ok_or_else(|| "No saved sessions found".to_string())?
        } else {
            sessions
                .iter()
                .find(|s| s.id == query || s.id.starts_with(query))
                .map(|s| s.id.clone())
                .ok_or_else(|| format!("Session '{query}' not found"))?
        };
        let path = sessions_dir().join(format!("{target_id}.json"));
        let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let doc = parse_json(&raw)?;
        self.session_id = target_id.clone();
        if let Some(m) = doc.get("model").and_then(|v| v.as_str()) {
            if !m.is_empty() {
                self.model = m.to_string();
            }
        }
        if let Some(e) = doc.get("effort").and_then(|v| v.as_str()) {
            if !e.is_empty() {
                self.effort = e.to_string();
            }
        }
        self.agent = doc
            .get("agent")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        self.history = doc
            .get("history")
            .and_then(|v| v.as_arr())
            .map(|a| a.to_vec())
            .unwrap_or_default();
        self.entries = doc
            .get("entries")
            .and_then(|v| v.as_arr())
            .map(|arr| {
                arr.iter()
                    .map(|item| {
                        let kind = match item.get("kind").and_then(|v| v.as_str()) {
                            Some("user") => Kind::User,
                            Some("assistant") => Kind::Assistant,
                            Some("error") => Kind::Error,
                            _ => Kind::Note,
                        };
                        Entry {
                            kind,
                            text: item
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            meta: item
                                .get("meta")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.scroll = 0;
        self.force_clear = true;
        Ok(target_id)
    }

    fn poll_size(&mut self) -> bool {
        if self.size_at.elapsed() < Duration::from_millis(700) {
            return false;
        }
        self.size_at = Instant::now();
        let fresh = detect_term_size();
        if fresh != self.size {
            self.size = fresh;
            self.force_clear = true;
            return true;
        }
        false
    }

    fn suggestions(&self) -> Vec<(&'static str, &'static str)> {
        let text: String = self.input.iter().collect();
        if !text.starts_with('/') || text.contains(' ') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .filter(|(c, _)| c.starts_with(text.as_str()))
            .copied()
            .collect()
    }

    fn filtered_models(&self, filter: &str) -> Vec<ModelRow> {
        let low = filter.trim().to_lowercase();
        all_known_models()
            .into_iter()
            .filter(|(m, p, _)| {
                low.is_empty() || m.to_lowercase().contains(&low) || p.to_lowercase().contains(&low)
            })
            .collect()
    }

    fn agent_entries(&self) -> Vec<(String, String, Vec<String>)> {
        let routes = load_agent_routes(&self.routes_path);
        let mut out = vec![(
            "none".to_string(),
            "medium".to_string(),
            vec!["direct model routing".to_string()],
        )];
        if let Some(agents) = routes.get("agents").and_then(|v| v.as_obj()) {
            for (name, info) in agents {
                let eff = info
                    .get("effort")
                    .and_then(|v| v.as_str())
                    .unwrap_or("medium")
                    .to_string();
                let chain: Vec<String> = info
                    .get("chain")
                    .and_then(|v| v.as_arr())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                out.push((name.clone(), eff, chain));
            }
        }
        out
    }

    fn refresh_badges(&mut self, router: &ProviderRouter) {
        let status = router.status();
        let mut badges = Vec::new();
        if let Some(provs) = status.get("providers") {
            for p in ["claude", "codex", "agy"] {
                if let Some(info) = provs.get(p) {
                    let avail = info
                        .get("available")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    let rem = info
                        .get("cooldown_remaining_sec")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    badges.push((p.to_string(), if avail { None } else { Some(rem) }));
                }
            }
        }
        self.badges = badges;
    }

    fn supported_efforts_for(&self, model_id: &str) -> Vec<String> {
        let bare = model_id.rsplit('/').next().unwrap_or(model_id);
        for (m, _, efforts) in all_known_models() {
            if m == bare {
                return efforts.iter().map(|s| (*s).to_string()).collect();
            }
        }
        EFFORT_LEVELS.iter().map(|s| (*s).to_string()).collect()
    }

    fn open_effort(&mut self) {
        let levels = self.supported_efforts_for(&self.model);
        let idx = levels
            .iter()
            .position(|e| e == &self.effort)
            .unwrap_or_else(|| levels.iter().position(|e| e == "medium").unwrap_or(0));
        self.modal = Modal::Effort {
            selected: idx,
            levels,
            pending_model: None,
        };
    }

    fn transcript_lines(&self, cols: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let box_w = cols.saturating_sub(2).clamp(30, 64);
        let inner = box_w - 2;
        let side = |content: String| format!("{ACCENT}│{RESET}{content}{ACCENT}│{RESET}");
        lines.push(format!("{ACCENT}╭{}╮{RESET}", "─".repeat(inner)));
        lines.push(side(format!(
            " {ACCENT}✻{RESET} {BOLD}Welcome to cligate{RESET}{}",
            " ".repeat(inner.saturating_sub(21))
        )));
        lines.push(side(" ".repeat(inner)));
        lines.push(side(format!(
            "{DIM}{}{RESET}",
            pad_or_clip("  /help for shortcuts, /model to switch models", inner)
        )));
        lines.push(side(format!(
            "{DIM}{}{RESET}",
            pad_or_clip(&format!("  cwd: {}", self.cwd), inner)
        )));
        lines.push(format!("{ACCENT}╰{}╯{RESET}", "─".repeat(inner)));
        lines.push(String::new());

        let body_w = cols.saturating_sub(4).max(10);
        for e in &self.entries {
            match e.kind {
                Kind::User => {
                    for (i, l) in wrap_text_lines(&e.text, body_w).iter().enumerate() {
                        let prefix = if i == 0 { "> " } else { "  " };
                        let plain = format!(" {prefix}{l}");
                        lines.push(format!(
                            "{USER_BG}{}{RESET}",
                            pad_or_clip(&plain, cols.saturating_sub(1))
                        ));
                    }
                }
                Kind::Assistant => self.assistant_lines(e, body_w, &mut lines),
                Kind::Note => {
                    for (i, l) in wrap_text_lines(&e.text, body_w).iter().enumerate() {
                        let prefix = if i == 0 { "⎿  " } else { "   " };
                        lines.push(format!("{DIM}  {prefix}{l}{RESET}"));
                    }
                }
                Kind::Error => {
                    for (i, l) in wrap_text_lines(&e.text, body_w).iter().enumerate() {
                        let prefix = if i == 0 { "✗ " } else { "  " };
                        lines.push(format!("{RED}{prefix}{l}{RESET}"));
                    }
                }
            }
            lines.push(String::new());
        }
        lines
    }

    fn assistant_lines(&self, e: &Entry, body_w: usize, lines: &mut Vec<String>) {
        let mut first = true;
        let mut lead = || {
            if first {
                first = false;
                format!("{ACCENT}●{RESET} ")
            } else {
                "  ".to_string()
            }
        };
        let mut in_code = false;
        for raw in e.text.split('\n') {
            let trimmed = raw.trim_start();
            if trimmed.starts_with("```") {
                in_code = !in_code;
                if in_code {
                    let lang = trimmed.trim_start_matches('`').trim();
                    if !lang.is_empty() {
                        lines.push(format!("{}{DIM}{lang}{RESET}", lead()));
                    }
                }
                continue;
            }
            if in_code {
                for part in wrap_text_lines(raw, body_w) {
                    lines.push(format!("{}{CODE}{part}{RESET}", lead()));
                }
                continue;
            }
            if trimmed.starts_with('#') {
                let heading = trimmed.trim_start_matches('#').trim();
                for part in wrap_text_lines(heading, body_w) {
                    lines.push(format!("{}{BOLD}{part}{RESET}", lead()));
                }
                continue;
            }
            let text = if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                let indent = raw.len() - trimmed.len();
                format!("{}• {}", " ".repeat(indent), &trimmed[2..])
            } else {
                raw.to_string()
            };
            let mut code = false;
            let mut bold = false;
            for part in wrap_text_lines(&text, body_w) {
                lines.push(format!(
                    "{}{}",
                    lead(),
                    style_inline(&part, &mut code, &mut bold)
                ));
            }
        }
        if !e.meta.is_empty() {
            lines.push(format!(
                "{DIM}  ⎿  {}{RESET}",
                clip(&e.meta, body_w.saturating_sub(3))
            ));
        }
    }

    fn spinner_line(&self, cols: usize) -> String {
        if !self.busy {
            return String::new();
        }
        let secs = self.busy_since.elapsed().as_secs();
        let frame = SPINNER[self.tick % SPINNER.len()];
        let phase = PROGRESS.lock().map(|p| p.clone()).unwrap_or_default();
        let label = if self.stream_chars == 0 {
            self.busy_label.as_str()
        } else {
            "Writing…"
        };
        let mut info = format!("{secs}s");
        let tokens = self.stream_chars / 4;
        if tokens > 0 {
            info.push_str(&format!(" · ↓ {tokens} tokens"));
        }
        if self.stream_chars == 0 && !phase.is_empty() {
            info.push_str(&format!(" · {phase}"));
        }
        info.push_str(" · esc/ctrl+c to stop");
        let budget = cols.saturating_sub(label.chars().count() + 8);
        format!(
            "{ACCENT}{frame} {BOLD}{label}{RESET}{DIM} ({}){RESET}",
            clip(&info, budget)
        )
    }

    fn status_line(&self, cols: usize) -> String {
        let max_w = cols.saturating_sub(1);
        let (prov, bare) = infer_provider_and_model(&self.model);
        let route_label = if let Some(a) = self.agent.as_deref() {
            a.to_string()
        } else if self.tui_fallback.len() > 1 {
            format!("fb:{}", self.tui_fallback.len())
        } else {
            "no agent".to_string()
        };
        let perm_tag = self.perm_mode.as_str();
        let ctx_tag = if self.auto_compact_tokens > 0 {
            let used = estimate_history_tokens(&self.history);
            format!(
                " · ctx {}/{}",
                format_tokens_short(used),
                format_tokens_short(self.auto_compact_tokens)
            )
        } else {
            String::new()
        };
        let mut right_plain = format!(
            "{prov}/{bare} · {} · {route_label} · {perm_tag}{ctx_tag}  ",
            self.effort
        );
        let perm_color = match self.perm_mode {
            PermissionMode::AlwaysAllow => GREEN,
            PermissionMode::ReadOnly => ACCENT,
            PermissionMode::Ask => GRAY,
        };
        let mut right_styled = format!(
            "{GRAY}{prov}/{bare} · {} · {route_label} · {RESET}{perm_color}{perm_tag}{RESET}{GRAY}{ctx_tag}{RESET}  ",
            self.effort
        );
        for (name, state) in &self.badges {
            let (mark, color) = match state {
                None => ("●", GREEN),
                Some(_) => ("✗", RED),
            };
            right_plain.push_str(&format!("{name} {mark} "));
            right_styled.push_str(&format!("{GRAY}{name}{RESET} {color}{mark}{RESET} "));
        }
        let right_len = right_plain.chars().count();
        if right_len + 2 >= max_w {
            return format!("{GRAY}{}{RESET}", clip(&right_plain, max_w));
        }
        let left_text = if !self.notice.is_empty() {
            self.notice.clone()
        } else if self.scroll > 0 {
            format!("scrolled up {} lines, PgDn to return", self.scroll)
        } else {
            "/help for shortcuts, ctrl+c to quit".to_string()
        };
        let left_room = max_w.saturating_sub(right_len + 2);
        let left = clip(&left_text, left_room);
        let gap = max_w.saturating_sub(left.chars().count() + right_len + 1);
        format!("{DIM} {left}{RESET}{}{right_styled}", " ".repeat(gap))
    }

    fn render(&mut self) {
        let (rows, cols) = self.size;
        let suggestions = self.suggestions();
        let sugg_rows = suggestions.len().min(8);
        let bottom = 1 + 3 + sugg_rows + 1;
        let chat_h = rows.saturating_sub(bottom);
        let lines = self.transcript_lines(cols);
        let max_scroll = lines.len().saturating_sub(chat_h);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
        let end = lines.len() - self.scroll;
        let start = end.saturating_sub(chat_h);
        let visible = &lines[start..end];

        let mut buf = String::with_capacity(rows * cols * 2);
        buf.push_str("\x1b[?2026h\x1b[?25l");
        if self.force_clear {
            buf.push_str("\x1b[2J");
            self.force_clear = false;
        }
        for r in 0..chat_h {
            let content = visible.get(r).map(String::as_str).unwrap_or("");
            buf.push_str(&format!("\x1b[{};1H{content}\x1b[K", r + 1));
        }
        buf.push_str(&format!(
            "\x1b[{};1H{}\x1b[K",
            chat_h + 1,
            self.spinner_line(cols)
        ));

        let border = if self.busy { DIM } else { GRAY };
        let input_row = chat_h + 3;
        let box_w = cols.saturating_sub(1).max(10);
        let avail = box_w.saturating_sub(6).max(1);
        let offset = if self.cursor >= avail {
            self.cursor + 1 - avail
        } else {
            0
        };
        let shown: String = self.input.iter().skip(offset).take(avail).collect();
        let field = if self.input.is_empty() {
            let hint = clip("Try \"/model\" to switch models", avail);
            format!("{DIM}{hint}{RESET}")
        } else {
            shown
        };
        buf.push_str(&format!(
            "\x1b[{};1H{border}╭{}╮{RESET}\x1b[K",
            chat_h + 2,
            "─".repeat(box_w.saturating_sub(2))
        ));
        buf.push_str(&format!(
            "\x1b[{input_row};1H{border}│{RESET} {ACCENT}>{RESET} {field}\x1b[K\x1b[{input_row};{box_w}H{border}│{RESET}",
        ));
        buf.push_str(&format!(
            "\x1b[{};1H{border}╰{}╯{RESET}\x1b[K",
            chat_h + 4,
            "─".repeat(box_w.saturating_sub(2))
        ));

        let sel = self.sugg_sel.min(suggestions.len().saturating_sub(1));
        for (i, (cmd, desc)) in suggestions.iter().take(8).enumerate() {
            let plain = pad_or_clip(&format!("{cmd:<10} {desc}"), cols.saturating_sub(5));
            let row = chat_h + 5 + i;
            if i == sel {
                buf.push_str(&format!(
                    "\x1b[{row};1H  {ACCENT}{BOLD}› {plain}{RESET}\x1b[K"
                ));
            } else {
                buf.push_str(&format!("\x1b[{row};1H  {GRAY}  {plain}{RESET}\x1b[K"));
            }
        }
        buf.push_str(&format!("\x1b[{rows};1H{}\x1b[K", self.status_line(cols)));

        let modal = self.modal.clone();
        self.draw_modal(&mut buf, &modal, rows, cols);

        if modal == Modal::None {
            let col = (5 + self.cursor.saturating_sub(offset)).min(box_w.saturating_sub(2));
            buf.push_str(&format!("\x1b[{input_row};{col}H\x1b[?25h"));
        } else if let Modal::MaxWindow { selected, custom } = &modal {
            let custom_idx = MAX_WINDOW_PRESETS.len();
            if *selected == custom_idx {
                let width = cols.saturating_sub(8).clamp(54, 78);
                let height = custom_idx + 4;
                let top = rows.saturating_sub(height) / 2 + 1;
                let left = cols.saturating_sub(width) / 2 + 1;
                let cur_row = top + 1 + custom_idx;
                let cur_col =
                    (left + 11 + custom.chars().count()).min(left + width.saturating_sub(2));
                buf.push_str(&format!("\x1b[{cur_row};{cur_col}H\x1b[?25h"));
            }
        }
        buf.push_str("\x1b[?2026l");
        let mut out = io::stdout();
        let _ = out.write_all(buf.as_bytes());
        let _ = out.flush();
    }

    fn draw_modal(&self, buf: &mut String, modal: &Modal, rows: usize, cols: usize) {
        match modal {
            Modal::None => {}
            Modal::Model { selected, filter } => {
                let models = self.filtered_models(filter);
                let width = cols.saturating_sub(10).clamp(40, 76);
                let height = rows.saturating_sub(6).clamp(8, 18);
                let list_h = height - 2;
                let sel = (*selected).min(models.len().saturating_sub(1));
                let win = sel
                    .saturating_sub(list_h / 2)
                    .min(models.len().saturating_sub(list_h));
                let mut items = Vec::new();
                if models.is_empty() {
                    items.push(("no matching models".to_string(), false));
                }
                for (idx, (m, prov, efforts)) in models.iter().enumerate().skip(win).take(list_h) {
                    items.push((
                        format!("{m:<28} {prov:<6} {}", efforts.join("/")),
                        idx == sel,
                    ));
                }
                let title = format!("Select model  filter: {filter}");
                draw_box(buf, rows, cols, width, height, &title, &items);
            }
            Modal::Effort {
                selected,
                levels,
                pending_model,
            } => {
                let items: Vec<(String, bool)> = levels
                    .iter()
                    .enumerate()
                    .map(|(i, lvl)| {
                        let desc = match lvl.as_str() {
                            "low" => "fastest, minimal thinking",
                            "medium" => "balanced speed and reasoning",
                            "high" => "deep thinking for coding",
                            "xhigh" => "extra-high reasoning depth",
                            _ => "maximum reasoning budget",
                        };
                        (format!("{lvl:<7} {desc}"), i == *selected)
                    })
                    .collect();
                let width = cols.saturating_sub(6).clamp(32, 60);
                let height = (items.len() + 2).clamp(5, 9);
                let title = match pending_model {
                    Some((m, prov)) => format!("Reasoning level for {prov}/{m}"),
                    None => format!("Reasoning level ({})", self.model),
                };
                draw_box(buf, rows, cols, width, height, &title, &items);
            }
            Modal::Agent { selected } => {
                let entries = self.agent_entries();
                let width = cols.saturating_sub(8).clamp(40, 80);
                let height = rows.saturating_sub(6).clamp(8, 16);
                let list_h = height - 2;
                let sel = (*selected).min(entries.len().saturating_sub(1));
                let win = sel
                    .saturating_sub(list_h / 2)
                    .min(entries.len().saturating_sub(list_h));
                let items: Vec<(String, bool)> = entries
                    .iter()
                    .enumerate()
                    .skip(win)
                    .take(list_h)
                    .map(|(i, (name, eff, chain))| {
                        (
                            format!("{name:<14} {eff:<6} {}", chain.join(" -> ")),
                            i == sel,
                        )
                    })
                    .collect();
                draw_box(
                    buf,
                    rows,
                    cols,
                    width,
                    height,
                    "Agent fallback chain",
                    &items,
                );
            }
            Modal::Fallback {
                selected,
                filter,
                chain,
            } => {
                let models = self.filtered_models(filter);
                let width = cols.saturating_sub(8).clamp(44, 80);
                let height = rows.saturating_sub(6).clamp(8, 18);
                let list_h = height - 2;
                let sel = (*selected).min(models.len().saturating_sub(1));
                let win = sel
                    .saturating_sub(list_h / 2)
                    .min(models.len().saturating_sub(list_h));
                let mut items = Vec::new();
                if models.is_empty() {
                    items.push(("no matching models".to_string(), false));
                }
                for (idx, (m, prov, efforts)) in models.iter().enumerate().skip(win).take(list_h) {
                    items.push((
                        format!("{m:<28} {prov:<6} {}", efforts.join("/")),
                        idx == sel,
                    ));
                }
                let step = match chain.len() {
                    0 => "Pick primary (1/3)",
                    1 => "Pick fallback #1 (2/3, Tab=save)",
                    _ => "Pick fallback #2 (3/3, Tab=save)",
                };
                let title = if filter.is_empty() {
                    format!("TUI Fallback {step}")
                } else {
                    format!("TUI Fallback {step}  filter: {filter}")
                };
                draw_box(buf, rows, cols, width, height, &title, &items);
            }
            Modal::Permissions { selected } => {
                let mut rows_list = vec![
                    (
                        format!(
                            "ask          Prompt before edits & shell commands{}",
                            if self.perm_mode == PermissionMode::Ask {
                                "  (active)"
                            } else {
                                ""
                            }
                        ),
                        *selected == 0,
                    ),
                    (
                        format!(
                            "always-allow Auto-approve all tools without asking{}",
                            if self.perm_mode == PermissionMode::AlwaysAllow {
                                "  (active)"
                            } else {
                                ""
                            }
                        ),
                        *selected == 1,
                    ),
                    (
                        format!(
                            "read-only    Allow read/list only, deny writes & shell{}",
                            if self.perm_mode == PermissionMode::ReadOnly {
                                "  (active)"
                            } else {
                                ""
                            }
                        ),
                        *selected == 2,
                    ),
                ];
                if !self.always_allow_tools.is_empty() {
                    rows_list.push((
                        format!(
                            "clear list   Reset always-allow tools ({})",
                            self.always_allow_tools.join(", ")
                        ),
                        *selected == 3,
                    ));
                }
                let width = cols.saturating_sub(8).clamp(50, 78);
                let height = rows_list.len() + 3;
                draw_box(
                    buf,
                    rows,
                    cols,
                    width,
                    height,
                    "Tool Permissions (/permissions)",
                    &rows_list,
                );
            }
            Modal::PermissionPrompt {
                selected,
                tool,
                summary,
            } => {
                let width = cols.saturating_sub(8).clamp(48, 82);
                let inner = width.saturating_sub(6);
                let items = vec![
                    (format!("Tool: {tool}"), false),
                    (format!("  {}", clip(summary, inner)), false),
                    (String::new(), false),
                    ("1. Allow once".to_string(), *selected == 0),
                    (format!("2. Always allow '{tool}'"), *selected == 1),
                    ("3. Always allow all tools".to_string(), *selected == 2),
                    ("4. Deny".to_string(), *selected == 3),
                    (
                        "↑↓ / 1-4 / y / n · Enter confirm · Esc deny".to_string(),
                        false,
                    ),
                ];
                draw_box(
                    buf,
                    rows,
                    cols,
                    width,
                    items.len() + 2,
                    "Permission Required",
                    &items,
                );
            }
            Modal::MaxWindow { selected, custom } => {
                let custom_idx = MAX_WINDOW_PRESETS.len();
                let mut items: Vec<(String, bool)> = MAX_WINDOW_PRESETS
                    .iter()
                    .enumerate()
                    .map(|(i, (val, label))| {
                        let mark = if *val == self.auto_compact_tokens {
                            "  (active)"
                        } else {
                            ""
                        };
                        (format!("{label}{mark}"), i == *selected)
                    })
                    .collect();
                items.push((format!("custom  {custom}"), *selected == custom_idx));
                items.push((
                    "↑↓ choose · type digits/k/m for custom · Enter save · Esc cancel".to_string(),
                    false,
                ));
                let width = cols.saturating_sub(8).clamp(54, 78);
                let height = items.len() + 2;
                draw_box(
                    buf,
                    rows,
                    cols,
                    width,
                    height,
                    "Auto-Compact Limit (/autocompact)",
                    &items,
                );
            }
            Modal::Session { selected } => {
                let sessions = list_saved_sessions();
                let width = cols.saturating_sub(8).clamp(42, 82);
                let height = rows.saturating_sub(6).clamp(8, 16);
                let list_h = height - 2;
                let sel = (*selected).min(sessions.len().saturating_sub(1));
                let win = sel
                    .saturating_sub(list_h / 2)
                    .min(sessions.len().saturating_sub(list_h));
                let mut items = Vec::new();
                if sessions.is_empty() {
                    items.push(("no saved sessions yet".to_string(), false));
                }
                for (i, s) in sessions.iter().enumerate().skip(win).take(list_h) {
                    let line = format!("{:<12} {:<18} {}", s.id, clip(&s.model, 18), s.title);
                    items.push((line, i == sel));
                }
                draw_box(buf, rows, cols, width, height, "Resume session", &items);
            }
        }
    }
}

fn draw_box(
    buf: &mut String,
    rows: usize,
    cols: usize,
    width: usize,
    height: usize,
    title: &str,
    items: &[(String, bool)],
) {
    let top = rows.saturating_sub(height) / 2 + 1;
    let left = cols.saturating_sub(width) / 2 + 1;
    let inner = width.saturating_sub(2);
    let head = clip(&format!("─ {title} "), inner);
    let rest = inner.saturating_sub(head.chars().count());
    buf.push_str(&format!(
        "\x1b[{top};{left}H{ACCENT}╭{head}{}╮{RESET}",
        "─".repeat(rest)
    ));
    for i in 0..height.saturating_sub(2) {
        let row = top + 1 + i;
        let line = match items.get(i) {
            Some((text, true)) => format!(
                "{ACCENT}│{BOLD}{}{RESET}{ACCENT}│{RESET}",
                pad_or_clip(&format!("❯ {text}"), inner)
            ),
            Some((text, false)) => format!(
                "{ACCENT}│{RESET}{}{ACCENT}│{RESET}",
                pad_or_clip(&format!("  {text}"), inner)
            ),
            None => format!("{ACCENT}│{RESET}{}{ACCENT}│{RESET}", " ".repeat(inner)),
        };
        buf.push_str(&format!("\x1b[{row};{left}H{line}"));
    }
    let bottom = top + height.saturating_sub(1);
    buf.push_str(&format!(
        "\x1b[{bottom};{left}H{ACCENT}╰{}╯{RESET}",
        "─".repeat(inner)
    ));
}

fn handle_modal_key(st: &mut TuiState, key: Key) -> Action {
    if key == Key::CtrlC {
        if matches!(st.modal, Modal::PermissionPrompt { .. }) {
            if let Some(tx) = st.pending_perm.take() {
                let _ = tx.send(PermDecision::Deny);
            }
            request_cancel();
            st.modal = Modal::None;
            return Action::None;
        }
        return Action::Quit;
    }
    match st.modal.clone() {
        Modal::None => {}
        Modal::Model { selected, filter } => {
            let models = st.filtered_models(&filter);
            let last = models.len().saturating_sub(1);
            match key {
                Key::Esc | Key::F2 => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Model {
                        selected: selected.saturating_sub(1),
                        filter,
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Model {
                        selected: (selected + 1).min(last),
                        filter,
                    }
                }
                Key::Backspace => {
                    let mut f = filter;
                    f.pop();
                    st.modal = Modal::Model {
                        selected: 0,
                        filter: f,
                    };
                }
                Key::Char(c) => {
                    let mut f = filter;
                    f.push(c);
                    st.modal = Modal::Model {
                        selected: 0,
                        filter: f,
                    };
                }
                Key::Enter => {
                    if let Some((m, prov, efforts)) = models.get(selected.min(last)) {
                        let levels: Vec<String> =
                            efforts.iter().map(|s| (*s).to_string()).collect();
                        let idx =
                            levels
                                .iter()
                                .position(|e| e == &st.effort)
                                .unwrap_or_else(|| {
                                    levels.iter().position(|e| e == "medium").unwrap_or(0)
                                });
                        st.modal = Modal::Effort {
                            selected: idx,
                            levels,
                            pending_model: Some(((*m).to_string(), (*prov).to_string())),
                        };
                    } else {
                        st.modal = Modal::None;
                    }
                }
                _ => {}
            }
        }
        Modal::Effort {
            selected,
            levels,
            pending_model,
        } => {
            let last = levels.len().saturating_sub(1);
            match key {
                Key::Esc | Key::F3 => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Effort {
                        selected: selected.saturating_sub(1),
                        levels,
                        pending_model,
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Effort {
                        selected: (selected + 1).min(last),
                        levels,
                        pending_model,
                    }
                }
                Key::Enter => {
                    if let Some(lvl) = levels.get(selected.min(last)) {
                        st.effort = lvl.clone();
                    }
                    let eff = st.effort.clone();
                    if let Some((m, prov)) = pending_model {
                        st.model = m.clone();
                        write_prefs(st);
                        st.note(format!("Model set to {m} ({prov}, effort {eff})"));
                    } else {
                        write_prefs(st);
                        st.note(format!("Reasoning effort set to {eff}"));
                    }
                    st.modal = Modal::None;
                }
                _ => {}
            }
        }
        Modal::Agent { selected } => {
            let entries = st.agent_entries();
            let last = entries.len().saturating_sub(1);
            match key {
                Key::Esc | Key::F4 => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Agent {
                        selected: selected.saturating_sub(1),
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Agent {
                        selected: (selected + 1).min(last),
                    }
                }
                Key::Enter => {
                    if let Some((name, eff, chain)) = entries.get(selected.min(last)) {
                        if name == "none" {
                            st.agent = None;
                            st.note("Agent chain disabled, using direct model routing".to_string());
                        } else {
                            st.agent = Some(name.clone());
                            st.effort = eff.clone();
                            if let Some(first) = chain.first() {
                                st.model = first.clone();
                            }
                            st.note(format!("Agent {name}: {}", chain.join(" -> ")));
                        }
                    }
                    st.modal = Modal::None;
                }
                _ => {}
            }
        }
        Modal::Fallback {
            selected,
            filter,
            mut chain,
        } => {
            let models = st.filtered_models(&filter);
            let last = models.len().saturating_sub(1);
            match key {
                Key::Esc => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Fallback {
                        selected: selected.saturating_sub(1),
                        filter,
                        chain,
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Fallback {
                        selected: (selected + 1).min(last),
                        filter,
                        chain,
                    }
                }
                Key::Backspace => {
                    let mut f = filter;
                    f.pop();
                    st.modal = Modal::Fallback {
                        selected: 0,
                        filter: f,
                        chain,
                    };
                }
                Key::Char(c) => {
                    let mut f = filter;
                    f.push(c);
                    st.modal = Modal::Fallback {
                        selected: 0,
                        filter: f,
                        chain,
                    };
                }
                Key::Tab => {
                    st.modal = Modal::None;
                    if !chain.is_empty() {
                        return Action::SaveFallback(chain);
                    }
                }
                Key::Enter => {
                    if let Some((m, _, _)) = models.get(selected.min(last)) {
                        let picked = (*m).to_string();
                        if !chain.contains(&picked) {
                            chain.push(picked);
                        }
                    }
                    if chain.len() >= 3 {
                        st.modal = Modal::None;
                        return Action::SaveFallback(chain);
                    }
                    st.modal = Modal::Fallback {
                        selected: 0,
                        filter: String::new(),
                        chain,
                    };
                }
                _ => {}
            }
        }
        Modal::Permissions { selected } => {
            let max_idx = if st.always_allow_tools.is_empty() {
                2
            } else {
                3
            };
            match key {
                Key::Esc => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Permissions {
                        selected: selected.saturating_sub(1),
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Permissions {
                        selected: (selected + 1).min(max_idx),
                    }
                }
                Key::Enter => {
                    match selected.min(max_idx) {
                        0 => {
                            st.perm_mode = PermissionMode::Ask;
                            write_prefs(st);
                            st.note(
                                "Permissions set to ask (prompts before edits & shell)".to_string(),
                            );
                        }
                        1 => {
                            st.perm_mode = PermissionMode::AlwaysAllow;
                            write_prefs(st);
                            st.note(
                                "Permissions set to always-allow (all tools auto-approved)"
                                    .to_string(),
                            );
                        }
                        2 => {
                            st.perm_mode = PermissionMode::ReadOnly;
                            write_prefs(st);
                            st.note(
                                "Permissions set to read-only (writes & shell denied)".to_string(),
                            );
                        }
                        _ => {
                            st.always_allow_tools.clear();
                            write_prefs(st);
                            st.note("Cleared per-tool always-allow list".to_string());
                        }
                    }
                    st.modal = Modal::None;
                }
                _ => {}
            }
        }
        Modal::PermissionPrompt {
            selected,
            tool,
            summary,
        } => {
            let mut decision: Option<PermDecision> = None;
            match key {
                Key::Esc | Key::Char('n') | Key::Char('N') | Key::Char('4') => {
                    decision = Some(PermDecision::Deny);
                }
                Key::Char('y') | Key::Char('Y') | Key::Char('1') => {
                    decision = Some(PermDecision::AllowOnce);
                }
                Key::Char('2') => decision = Some(PermDecision::AlwaysAllowTool),
                Key::Char('3') => decision = Some(PermDecision::AlwaysAllowAll),
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::PermissionPrompt {
                        selected: selected.saturating_sub(1),
                        tool: tool.clone(),
                        summary,
                    };
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::PermissionPrompt {
                        selected: (selected + 1).min(3),
                        tool: tool.clone(),
                        summary,
                    };
                }
                Key::Enter => {
                    decision = Some(match selected {
                        0 => PermDecision::AllowOnce,
                        1 => PermDecision::AlwaysAllowTool,
                        2 => PermDecision::AlwaysAllowAll,
                        _ => PermDecision::Deny,
                    });
                }
                _ => {}
            }
            if let Some(dec) = decision {
                match dec {
                    PermDecision::AlwaysAllowTool => {
                        if !st.always_allow_tools.iter().any(|t| t == &tool) {
                            st.always_allow_tools.push(tool.clone());
                            write_prefs(st);
                        }
                    }
                    PermDecision::AlwaysAllowAll => {
                        st.perm_mode = PermissionMode::AlwaysAllow;
                        write_prefs(st);
                    }
                    _ => {}
                }
                if let Some(tx) = st.pending_perm.take() {
                    let _ = tx.send(dec);
                }
                st.modal = Modal::None;
            }
        }
        Modal::MaxWindow { selected, custom } => {
            let custom_idx = MAX_WINDOW_PRESETS.len();
            match key {
                Key::Esc => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::MaxWindow {
                        selected: selected.saturating_sub(1),
                        custom,
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::MaxWindow {
                        selected: (selected + 1).min(custom_idx),
                        custom,
                    }
                }
                Key::Backspace => {
                    let mut c = custom;
                    c.pop();
                    st.modal = Modal::MaxWindow {
                        selected: custom_idx,
                        custom: c,
                    };
                }
                Key::Char(ch) if ch.is_ascii_alphanumeric() => {
                    let mut c = custom;
                    if c.len() < 12 {
                        c.push(ch);
                    }
                    st.modal = Modal::MaxWindow {
                        selected: custom_idx,
                        custom: c,
                    };
                }
                Key::Enter => {
                    if selected == custom_idx {
                        if let Some(limit) = parse_token_limit(&custom) {
                            st.auto_compact_tokens = limit;
                            write_prefs(st);
                            if limit == 0 {
                                st.note("Auto-compact disabled".to_string());
                            } else {
                                st.note(format!(
                                    "Auto-compact limit set to {} tokens",
                                    format_tokens_short(limit)
                                ));
                            }
                            st.modal = Modal::None;
                        } else {
                            st.notice =
                                "Enter a valid number (e.g. 50k, 80000, or off)".to_string();
                        }
                    } else if let Some((val, _)) = MAX_WINDOW_PRESETS.get(selected) {
                        st.auto_compact_tokens = *val;
                        write_prefs(st);
                        if *val == 0 {
                            st.note("Auto-compact disabled".to_string());
                        } else {
                            st.note(format!(
                                "Auto-compact limit set to {} tokens",
                                format_tokens_short(*val)
                            ));
                        }
                        st.modal = Modal::None;
                    }
                }
                _ => {}
            }
        }
        Modal::Session { selected } => {
            let sessions = list_saved_sessions();
            let last = sessions.len().saturating_sub(1);
            match key {
                Key::Esc => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Session {
                        selected: selected.saturating_sub(1),
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Session {
                        selected: (selected + 1).min(last),
                    }
                }
                Key::Enter => {
                    if let Some(s) = sessions.get(selected.min(last)) {
                        match st.load_session(&s.id) {
                            Ok(id) => st.note(format!("Resumed session {id}")),
                            Err(e) => st.notice = e,
                        }
                    }
                    st.modal = Modal::None;
                }
                _ => {}
            }
        }
    }
    Action::None
}

fn handle_key(st: &mut TuiState, key: Key) -> Action {
    st.notice.clear();
    let sugg_len = st.suggestions().len();
    match key {
        Key::CtrlC => {
            if st.busy {
                request_cancel();
                st.notice = "Stopping...".to_string();
                return Action::None;
            }
            return Action::Quit;
        }
        Key::F2 | Key::CtrlN => {
            st.modal = Modal::Model {
                selected: 0,
                filter: String::new(),
            }
        }
        Key::F3 | Key::CtrlT => st.open_effort(),
        Key::F4 | Key::CtrlG => st.modal = Modal::Agent { selected: 0 },
        Key::F5 => return Action::Command("/status".to_string()),
        Key::CtrlR => return Action::Command("/reset".to_string()),
        Key::CtrlL => return Action::Command("/clear".to_string()),
        Key::PageUp => st.scroll = st.scroll.saturating_add(8),
        Key::PageDown => st.scroll = st.scroll.saturating_sub(8),
        Key::ScrollUp => st.scroll = st.scroll.saturating_add(3),
        Key::ScrollDown => st.scroll = st.scroll.saturating_sub(3),
        Key::Left => st.cursor = st.cursor.saturating_sub(1),
        Key::Right => st.cursor = (st.cursor + 1).min(st.input.len()),
        Key::Home => st.cursor = 0,
        Key::End => st.cursor = st.input.len(),
        Key::Backspace => {
            if st.cursor > 0 {
                st.input.remove(st.cursor - 1);
                st.cursor -= 1;
                st.sugg_sel = 0;
            }
        }
        Key::Delete => {
            if st.cursor < st.input.len() {
                st.input.remove(st.cursor);
            }
        }
        Key::CtrlU => {
            st.input.clear();
            st.cursor = 0;
            st.sugg_sel = 0;
        }
        Key::Esc => {
            if st.busy {
                request_cancel();
                st.notice = "Stopping...".to_string();
            } else {
                st.input.clear();
                st.cursor = 0;
                st.sugg_sel = 0;
            }
        }
        Key::Tab => {
            if sugg_len > 0 {
                let cmd = st.suggestions()[st.sugg_sel.min(sugg_len - 1)].0;
                st.input = format!("{cmd} ").chars().collect();
                st.cursor = st.input.len();
                st.sugg_sel = 0;
            }
        }
        Key::CtrlP => {
            if !st.prompts.is_empty() {
                let idx = match st.prompt_idx {
                    None => st.prompts.len() - 1,
                    Some(i) => i.saturating_sub(1),
                };
                st.prompt_idx = Some(idx);
                st.input = st.prompts[idx].chars().collect();
                st.cursor = st.input.len();
            }
        }
        Key::Up => {
            if sugg_len > 0 {
                st.sugg_sel = if st.sugg_sel == 0 {
                    sugg_len - 1
                } else {
                    st.sugg_sel - 1
                };
            } else if st.input.is_empty() && st.prompt_idx.is_none() {
                st.scroll = st.scroll.saturating_add(2);
            } else if !st.prompts.is_empty() {
                let idx = match st.prompt_idx {
                    None => st.prompts.len() - 1,
                    Some(i) => i.saturating_sub(1),
                };
                st.prompt_idx = Some(idx);
                st.input = st.prompts[idx].chars().collect();
                st.cursor = st.input.len();
            }
        }
        Key::Down => {
            if sugg_len > 0 {
                st.sugg_sel = (st.sugg_sel + 1) % sugg_len;
            } else if st.input.is_empty() && st.prompt_idx.is_none() {
                st.scroll = st.scroll.saturating_sub(2);
            } else if let Some(i) = st.prompt_idx {
                if i + 1 < st.prompts.len() {
                    st.prompt_idx = Some(i + 1);
                    st.input = st.prompts[i + 1].chars().collect();
                    st.cursor = st.input.len();
                } else {
                    st.prompt_idx = None;
                    st.input.clear();
                    st.cursor = 0;
                }
            }
        }
        Key::Char(c) => {
            st.input.insert(st.cursor, c);
            st.cursor += 1;
            st.sugg_sel = 0;
            st.prompt_idx = None;
            st.scroll = 0;
        }
        Key::Enter => {
            let sugg = st.suggestions();
            let mut text: String = st.input.iter().collect();
            if !sugg.is_empty() {
                text = sugg[st.sugg_sel.min(sugg.len() - 1)].0.to_string();
            }
            let text = text.trim().to_string();
            st.input.clear();
            st.cursor = 0;
            st.sugg_sel = 0;
            st.prompt_idx = None;
            st.scroll = 0;
            if text.is_empty() {
                return Action::None;
            }
            if text.starts_with('/') {
                return Action::Command(text);
            }
            if st.busy {
                st.notice = "Still generating, wait for the current response".to_string();
                st.input = text.chars().collect();
                st.cursor = st.input.len();
                return Action::None;
            }
            st.prompts.push(text.clone());
            return Action::Submit(text);
        }
        Key::Unknown => {}
    }
    Action::None
}

fn status_text(router: &ProviderRouter) -> String {
    let status = router.status();
    let mut out = String::new();
    if let Some(provs) = status.get("providers").and_then(|v| v.as_obj()) {
        for (name, info) in provs {
            let avail = info
                .get("available")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let calls = info.get("calls").and_then(|v| v.as_i64()).unwrap_or(0);
            if avail {
                out.push_str(&format!("{name:<7} available, {calls} calls\n"));
            } else {
                let rem = info
                    .get("cooldown_remaining_sec")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let err = info
                    .get("last_limit_error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                out.push_str(&format!("{name:<7} limited, retry in {rem}s: {err}\n"));
            }
        }
    }
    out.trim_end().to_string()
}

fn check_tool_permission(state: &Arc<Mutex<TuiState>>, tool: &str, summary: &str) -> bool {
    if is_read_only_tool(tool) {
        return true;
    }
    let rx = {
        let mut st = state.lock().unwrap();
        match st.perm_mode {
            PermissionMode::AlwaysAllow => return true,
            PermissionMode::ReadOnly => return false,
            PermissionMode::Ask => {
                if st.always_allow_tools.iter().any(|t| t == tool) {
                    return true;
                }
                let (tx, rx) = mpsc::channel();
                st.pending_perm = Some(tx);
                st.modal = Modal::PermissionPrompt {
                    selected: 0,
                    tool: tool.to_string(),
                    summary: summary.to_string(),
                };
                st.dirty = true;
                rx
            }
        }
    };
    loop {
        if is_cancelled() {
            let mut st = state.lock().unwrap();
            st.pending_perm = None;
            if matches!(st.modal, Modal::PermissionPrompt { .. }) {
                st.modal = Modal::None;
            }
            return false;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(PermDecision::Deny) => return false,
            Ok(_) => return true,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn start_turn(state: &Arc<Mutex<TuiState>>, router: &Arc<ProviderRouter>, text: String) {
    let (model, effort, agent, tui_fallback, work_dir) = {
        let mut st = state.lock().unwrap();
        st.history.push(Json::obj(vec![
            ("role", Json::Str("user".to_string())),
            ("content", Json::Str(text.clone())),
        ]));
        st.entries.push(Entry {
            kind: Kind::User,
            text,
            meta: String::new(),
        });
        st.busy = true;
        st.busy_label = "Thinking…".to_string();
        st.busy_since = Instant::now();
        st.stream_chars = 0;
        st.scroll = 0;
        reset_cancel();
        if let Ok(mut p) = PROGRESS.lock() {
            p.clear();
        }
        st.render();
        let fb = if st.agent.is_none() && !st.tui_fallback.is_empty() {
            Some(st.tui_fallback.clone())
        } else {
            None
        };
        (
            st.model.clone(),
            st.effort.clone(),
            st.agent.clone(),
            fb,
            st.work_dir.clone(),
        )
    };
    let state_t = Arc::clone(state);
    let router_t = Arc::clone(router);
    thread::spawn(move || {
        let started = Instant::now();
        let tool_defs = tui_tool_definitions();
        let sys_msg = Json::obj(vec![
            ("role", Json::Str("system".to_string())),
            (
                "content",
                Json::Str(format!(
                    "You are a coding assistant in working directory {work_dir}. Use the provided caller tools when you need to inspect files, edit code, or run shell commands."
                )),
            ),
        ]);

        for _round in 0..12 {
            let (snapshot, idx) = {
                let mut st = state_t.lock().unwrap();
                st.entries.push(Entry {
                    kind: Kind::Assistant,
                    text: String::new(),
                    meta: String::new(),
                });
                st.stream_chars = 0;
                st.busy_label = "Thinking…".to_string();
                st.dirty = true;
                let mut msgs = vec![sys_msg.clone()];
                msgs.extend(st.history.clone());
                (msgs, st.entries.len() - 1)
            };

            let mut acc = String::new();
            let res = {
                let state_cb = Arc::clone(&state_t);
                let mut on_delta = |chunk: &str| {
                    acc.push_str(chunk);
                    if let Ok(mut st) = state_cb.lock() {
                        st.stream_chars = acc.chars().count();
                        if !acc.contains("<tool_call>") {
                            if let Some(entry) = st.entries.get_mut(idx) {
                                entry.text = acc.clone();
                            }
                        }
                        st.dirty = true;
                    }
                };
                let mut cb: Option<&mut dyn FnMut(&str)> = Some(&mut on_delta);
                router_t.execute_turn_with_chain(
                    &snapshot,
                    Some(&tool_defs),
                    &model,
                    effort.clone(),
                    agent.as_deref(),
                    tui_fallback.as_deref(),
                    &mut cb,
                )
            };

            let secs = started.elapsed().as_secs_f32();
            match res {
                Ok((final_text, _usage, resolved, provider)) => {
                    let raw = if final_text.is_empty() {
                        acc
                    } else {
                        final_text
                    };
                    let (cleaned, tool_calls) = parse_tool_calls(&raw);
                    if tool_calls.is_empty() {
                        let mut st = state_t.lock().unwrap();
                        if let Some(entry) = st.entries.get_mut(idx) {
                            entry.text = cleaned.clone();
                            entry.meta = format!("{resolved} via {provider} in {secs:.1}s");
                        }
                        st.history.push(Json::obj(vec![
                            ("role", Json::Str("assistant".to_string())),
                            ("content", Json::Str(cleaned)),
                        ]));
                        st.save_session();
                        break;
                    }

                    {
                        let mut st = state_t.lock().unwrap();
                        if cleaned.trim().is_empty() {
                            if st.entries.len() == idx + 1 {
                                st.entries.pop();
                            }
                        } else if let Some(entry) = st.entries.get_mut(idx) {
                            entry.text = cleaned.clone();
                        }
                        st.history.push(Json::obj(vec![
                            ("role", Json::Str("assistant".to_string())),
                            ("content", Json::Str(cleaned)),
                            ("tool_calls", Json::Arr(tool_calls.clone())),
                        ]));
                        st.dirty = true;
                    }

                    for tc in &tool_calls {
                        if is_cancelled() {
                            break;
                        }
                        let call_id = tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("call_0")
                            .to_string();
                        let fn_obj = tc.get("function");
                        let name = fn_obj
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args_raw = fn_obj
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");
                        let args = parse_json(args_raw).unwrap_or_else(|_| Json::Obj(Vec::new()));
                        let summary = summarize_tool_call(&name, &args);

                        let allowed = check_tool_permission(&state_t, &name, &summary);
                        let (ok, output) = if allowed {
                            {
                                let mut st = state_t.lock().unwrap();
                                st.busy_label = format!("Running {name}…");
                                st.dirty = true;
                            }
                            execute_local_tool(&work_dir, &name, &args)
                        } else {
                            let reason = if is_cancelled() {
                                "Cancelled by user".to_string()
                            } else {
                                format!("Denied by user/permission mode for tool '{name}'")
                            };
                            (false, reason)
                        };

                        let mut st = state_t.lock().unwrap();
                        let mark = if ok { "✓" } else { "✗" };
                        st.note(format!("{mark} {name}: {}", clip(&summary, 80)));
                        st.history.push(Json::obj(vec![
                            ("role", Json::Str("tool".to_string())),
                            ("tool_call_id", Json::Str(call_id)),
                            ("name", Json::Str(name)),
                            ("content", Json::Str(output)),
                        ]));
                        st.dirty = true;
                    }

                    if is_cancelled() {
                        let mut st = state_t.lock().unwrap();
                        st.note("Interrupted by user".to_string());
                        break;
                    }
                }
                Err(err) => {
                    let mut st = state_t.lock().unwrap();
                    if let Some(entry) = st.entries.get_mut(idx) {
                        if err == "Interrupted by user" {
                            if !acc.is_empty() {
                                entry.text = acc;
                                entry.meta = format!("interrupted after {secs:.1}s");
                            } else {
                                entry.kind = Kind::Note;
                                entry.text = "Interrupted by user".to_string();
                            }
                        } else {
                            entry.kind = Kind::Error;
                            entry.text = err;
                        }
                    }
                    break;
                }
            }
        }

        let auto_compact_req = {
            let mut st = state_t.lock().unwrap();
            let used = estimate_history_tokens(&st.history);
            if !is_cancelled()
                && st.auto_compact_tokens > 0
                && st.history.len() >= 4
                && used >= st.auto_compact_tokens
            {
                st.busy_label = "Auto-compacting…".to_string();
                st.stream_chars = 0;
                st.dirty = true;
                let mut msgs = st.history.clone();
                msgs.push(Json::obj(vec![
                    ("role", Json::Str("user".to_string())),
                    (
                        "content",
                        Json::Str(
                            "Summarize the conversation above concisely so we can continue seamlessly. Preserve key decisions, file paths, code changes, constraints, and pending next steps.".to_string(),
                        ),
                    ),
                ]));
                Some((msgs, st.history.len(), used, st.auto_compact_tokens))
            } else {
                None
            }
        };

        if let Some((compact_msgs, orig_count, used_tok, limit_tok)) = auto_compact_req {
            let mut no_cb: Option<&mut dyn FnMut(&str)> = None;
            if let Ok((summary, _, _, _)) = router_t.execute_turn_with_chain(
                &compact_msgs,
                None,
                &model,
                effort,
                agent.as_deref(),
                tui_fallback.as_deref(),
                &mut no_cb,
            ) {
                let clean = summary.trim().to_string();
                let mut st = state_t.lock().unwrap();
                st.history = vec![
                    Json::obj(vec![
                        ("role", Json::Str("user".to_string())),
                        (
                            "content",
                            Json::Str("Previous conversation summary:".to_string()),
                        ),
                    ]),
                    Json::obj(vec![
                        ("role", Json::Str("assistant".to_string())),
                        ("content", Json::Str(clean)),
                    ]),
                ];
                let after_tok = estimate_history_tokens(&st.history);
                st.save_session();
                st.note(format!(
                    "Auto-compacted {orig_count} messages (~{} -> ~{}, limit {})",
                    format_tokens_short(used_tok),
                    format_tokens_short(after_tok),
                    format_tokens_short(limit_tok)
                ));
            }
        }

        let mut st = state_t.lock().unwrap();
        st.busy = false;
        st.refresh_badges(&router_t);
        st.render();
    });
}

fn start_compact(state: &Arc<Mutex<TuiState>>, router: &Arc<ProviderRouter>, focus: String) {
    let (model, effort, agent, tui_fallback, snapshot, orig_count) = {
        let mut st = state.lock().unwrap();
        if st.busy {
            st.notice = "Wait for the current response before compacting".to_string();
            st.render();
            return;
        }
        if st.history.is_empty() {
            st.notice = "Nothing to compact yet".to_string();
            st.render();
            return;
        }
        let mut msgs = st.history.clone();
        let instruction = if focus.trim().is_empty() {
            "Summarize the conversation above concisely so we can continue seamlessly. Preserve key decisions, file paths, code changes, constraints, and pending next steps.".to_string()
        } else {
            format!("Summarize the conversation above concisely with focus on: {}. Preserve key decisions, file paths, code changes, constraints, and pending next steps.", focus.trim())
        };
        msgs.push(Json::obj(vec![
            ("role", Json::Str("user".to_string())),
            ("content", Json::Str(instruction)),
        ]));
        st.busy = true;
        st.busy_label = "Compacting…".to_string();
        st.busy_since = Instant::now();
        st.stream_chars = 0;
        st.scroll = 0;
        reset_cancel();
        if let Ok(mut p) = PROGRESS.lock() {
            p.clear();
        }
        st.render();
        let fb = if st.agent.is_none() && !st.tui_fallback.is_empty() {
            Some(st.tui_fallback.clone())
        } else {
            None
        };
        (
            st.model.clone(),
            st.effort.clone(),
            st.agent.clone(),
            fb,
            msgs,
            st.history.len(),
        )
    };
    let state_t = Arc::clone(state);
    let router_t = Arc::clone(router);
    thread::spawn(move || {
        let started = Instant::now();
        let mut no_cb: Option<&mut dyn FnMut(&str)> = None;
        let res = router_t.execute_turn_with_chain(
            &snapshot,
            None,
            &model,
            effort,
            agent.as_deref(),
            tui_fallback.as_deref(),
            &mut no_cb,
        );
        let mut st = state_t.lock().unwrap();
        st.busy = false;
        let secs = started.elapsed().as_secs_f32();
        match res {
            Ok((summary, _usage, resolved, provider)) => {
                let clean = summary.trim().to_string();
                st.history = vec![
                    Json::obj(vec![
                        ("role", Json::Str("user".to_string())),
                        (
                            "content",
                            Json::Str(format!("[Compacted conversation summary]\n{clean}")),
                        ),
                    ]),
                    Json::obj(vec![
                        ("role", Json::Str("assistant".to_string())),
                        (
                            "content",
                            Json::Str(
                                "Understood. I have the compacted context and am ready to continue."
                                    .to_string(),
                            ),
                        ),
                    ]),
                ];
                st.entries = vec![Entry {
                    kind: Kind::Note,
                    text: format!(
                        "Compacted {orig_count} messages via {resolved} ({provider}) in {secs:.1}s\n{clean}"
                    ),
                    meta: String::new(),
                }];
                st.scroll = 0;
                st.force_clear = true;
                st.save_session();
            }
            Err(err) => {
                if err == "Interrupted by user" {
                    st.note("Compaction cancelled".to_string());
                } else {
                    st.entries.push(Entry {
                        kind: Kind::Error,
                        text: format!("Compact failed: {err}"),
                        meta: String::new(),
                    });
                }
            }
        }
        st.refresh_badges(&router_t);
        st.render();
    });
}

fn run_command(
    state: &Arc<Mutex<TuiState>>,
    router: &Arc<ProviderRouter>,
    cfg: &Config,
    line: &str,
) -> bool {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return false;
    }
    let mut st = state.lock().unwrap();
    match parts[0] {
        "/quit" | "/exit" | "/q" => return true,
        "/help" | "/?" => st.note(HELP_TEXT.to_string()),
        "/clear" => {
            if st.busy {
                st.notice = "Wait for the current response before clearing".to_string();
            } else {
                st.history.clear();
                st.entries.clear();
                st.scroll = 0;
                st.force_clear = true;
                st.notice = "Conversation cleared".to_string();
            }
        }
        "/new" => {
            if st.busy {
                st.notice =
                    "Wait for the current response before starting a new session".to_string();
            } else {
                st.save_session();
                st.session_id = new_session_id();
                st.history.clear();
                st.entries.clear();
                st.scroll = 0;
                st.force_clear = true;
                let id = st.session_id.clone();
                st.note(format!("Started new session {id}"));
            }
        }
        "/compact" => {
            let focus = parts[1..].join(" ");
            drop(st);
            start_compact(state, router, focus);
            return false;
        }
        "/resume" | "/sessions" => {
            if parts.len() == 1 {
                st.modal = Modal::Session { selected: 0 };
            } else {
                match st.load_session(parts[1]) {
                    Ok(id) => st.note(format!("Resumed session {id}")),
                    Err(e) => st.notice = e,
                }
            }
        }
        "/model" | "/m" => {
            if parts.len() == 1 {
                st.modal = Modal::Model {
                    selected: 0,
                    filter: String::new(),
                };
            } else {
                st.model = parts[1].to_string();
                let mut i = 2;
                while i < parts.len() {
                    let is_flag = matches!(parts[i], "--reasoning" | "--effort" | "-e");
                    if is_flag && i + 1 < parts.len() {
                        let lvl = parts[i + 1].to_lowercase();
                        if EFFORT_LEVELS.contains(&lvl.as_str()) {
                            st.effort = lvl;
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                let (model, eff) = (st.model.clone(), st.effort.clone());
                write_prefs(&st);
                st.note(format!("Model set to {model} (effort {eff})"));
            }
        }
        "/effort" | "/reasoning" | "/level" => {
            if parts.len() == 1 {
                st.open_effort();
            } else {
                let lvl = parts[1].to_lowercase();
                if EFFORT_LEVELS.contains(&lvl.as_str()) {
                    st.effort = lvl.clone();
                    write_prefs(&st);
                    st.note(format!("Reasoning effort set to {lvl}"));
                } else {
                    st.notice = "Effort must be one of low, medium, high, xhigh, max".to_string();
                }
            }
        }
        "/autocompact" | "/maxwindow" | "/window" => {
            if parts.len() == 1 {
                let sel = MAX_WINDOW_PRESETS
                    .iter()
                    .position(|(v, _)| *v == st.auto_compact_tokens)
                    .unwrap_or(MAX_WINDOW_PRESETS.len());
                let custom = if sel == MAX_WINDOW_PRESETS.len() && st.auto_compact_tokens > 0 {
                    format_tokens_short(st.auto_compact_tokens)
                } else {
                    String::new()
                };
                st.modal = Modal::MaxWindow {
                    selected: sel,
                    custom,
                };
            } else if let Some(limit) = parse_token_limit(parts[1]) {
                st.auto_compact_tokens = limit;
                write_prefs(&st);
                let used = estimate_history_tokens(&st.history);
                if limit == 0 {
                    st.note("Auto-compact disabled".to_string());
                } else {
                    st.note(format!(
                        "Auto-compact max window set to {} (current ~{} tokens)",
                        format_tokens_short(limit),
                        format_tokens_short(used)
                    ));
                }
            } else {
                st.notice = "Usage: /autocompact [off|32k|64k|128k|200k|<tokens>]".to_string();
            }
        }
        "/permissions" | "/perm" | "/allow" => {
            if parts.len() == 1 {
                let sel = match st.perm_mode {
                    PermissionMode::Ask => 0,
                    PermissionMode::AlwaysAllow => 1,
                    PermissionMode::ReadOnly => 2,
                };
                st.modal = Modal::Permissions { selected: sel };
            } else if matches!(parts[1].to_lowercase().as_str(), "reset" | "clear") {
                st.perm_mode = PermissionMode::Ask;
                st.always_allow_tools.clear();
                write_prefs(&st);
                st.note("Permissions reset to ask and allowlist cleared".to_string());
            } else if let Some(mode) = PermissionMode::parse(parts[1]) {
                st.perm_mode = mode;
                write_prefs(&st);
                st.note(format!("Permissions set to {}", mode.as_str()));
            } else {
                st.notice = "Usage: /permissions [ask|always-allow|read-only|reset]".to_string();
            }
        }
        "/setfallback" | "/setfalback" | "/fallback" => {
            if parts.len() == 1 {
                st.modal = Modal::Fallback {
                    selected: 0,
                    filter: String::new(),
                    chain: Vec::new(),
                };
            } else if matches!(parts[1].to_lowercase().as_str(), "reset" | "clear" | "off") {
                st.tui_fallback.clear();
                write_prefs(&st);
                st.note("TUI fallback chain cleared (API routes unchanged)".to_string());
            } else {
                let mut models = Vec::new();
                let mut eff_ov: Option<&str> = None;
                let mut i = 1;
                while i < parts.len() {
                    let is_flag = matches!(parts[i], "--effort" | "--reasoning" | "-e");
                    if is_flag && i + 1 < parts.len() {
                        eff_ov = Some(parts[i + 1]);
                        i += 2;
                    } else {
                        models.push(parts[i].to_string());
                        i += 1;
                    }
                }
                if models.is_empty() {
                    st.notice =
                        "Usage: /setfallback <model1> <model2> [model3] [--effort <level>] | reset"
                            .to_string();
                } else {
                    apply_tui_fallback_chain(&mut st, &models, eff_ov);
                }
            }
        }
        "/agent" | "/a" => agent_command(&mut st, router, &parts),
        "/reset" => {
            let prov = parts.get(1).copied();
            router.reset_cooldown(prov);
            let body = Json::obj(vec![(
                "provider",
                prov.map(|s| Json::Str(s.to_string())).unwrap_or(Json::Null),
            )]);
            let _ = http_call(&cfg.host, cfg.port, "POST", "/v1/router/reset", Some(&body));
            st.refresh_badges(router);
            st.note("Rate-limit cooldowns cleared".to_string());
        }
        "/status" => {
            st.refresh_badges(router);
            st.note(status_text(router));
        }
        other => {
            st.notice = format!("Unknown command {other}, try /help");
        }
    }
    st.render();
    false
}

fn apply_tui_fallback_chain(st: &mut TuiState, models: &[String], eff_ov: Option<&str>) {
    let eff_val = eff_ov.unwrap_or(&st.effort).to_lowercase();
    if !EFFORT_LEVELS.contains(&eff_val.as_str()) {
        st.notice = "Effort must be one of low, medium, high, xhigh, max".to_string();
        return;
    }
    st.agent = None;
    st.tui_fallback = models.to_vec();
    if let Some(first) = models.first() {
        st.model = first.clone();
    }
    st.effort = eff_val.clone();
    write_prefs(st);
    st.note(format!(
        "TUI fallback chain saved (TUI only, effort {eff_val}): {}",
        models.join(" -> ")
    ));
}

fn agent_command(st: &mut TuiState, router: &Arc<ProviderRouter>, parts: &[&str]) {
    if parts.len() == 1 {
        st.modal = Modal::Agent { selected: 0 };
        return;
    }
    if parts[1] == "set" && parts.len() >= 4 {
        let name = parts[2];
        let mut models = Vec::new();
        let mut eff_override: Option<&str> = None;
        let mut i = 3;
        while i < parts.len() {
            let is_flag = matches!(parts[i], "--effort" | "--reasoning" | "-e");
            if is_flag && i + 1 < parts.len() {
                eff_override = Some(parts[i + 1]);
                i += 2;
            } else {
                models.push(parts[i].to_string());
                i += 1;
            }
        }
        match router.set_agent_chain(name, &models, eff_override) {
            Ok(_) => {
                st.agent = Some(name.to_string());
                if let Some(first) = models.first() {
                    st.model = first.clone();
                }
                if let Some(e) = eff_override {
                    st.effort = e.to_lowercase();
                }
                st.note(format!("Agent {name}: {}", models.join(" -> ")));
            }
            Err(e) => st.notice = format!("Could not save agent chain: {e}"),
        }
        return;
    }
    if matches!(parts[1], "none" | "off" | "reset") {
        st.agent = None;
        st.note("Agent chain disabled, using direct model routing".to_string());
        return;
    }
    let target = parts[1].to_lowercase();
    let found = st
        .agent_entries()
        .into_iter()
        .find(|(n, _, _)| *n == target);
    match found {
        Some((_, eff, chain)) => {
            st.agent = Some(target.clone());
            st.effort = eff;
            if let Some(first) = chain.first() {
                st.model = first.clone();
            }
            st.note(format!("Agent {target}: {}", chain.join(" -> ")));
        }
        None => st.notice = format!("No agent named {target} in {}", st.routes_path),
    }
}

pub fn run_tui(
    cfg: Config,
    router: Arc<ProviderRouter>,
    initial_agent: Option<String>,
    initial_model: Option<String>,
    initial_effort: Option<String>,
    resume_target: Option<String>,
    always_allow: bool,
) {
    let guard = RawTermGuard::enter();
    let rx = spawn_input_thread();
    let state = Arc::new(Mutex::new(TuiState::new(&cfg)));
    {
        let mut st = state.lock().unwrap();
        if always_allow {
            st.perm_mode = PermissionMode::AlwaysAllow;
        }
        if let Some(target) = resume_target {
            if target == "picker" {
                st.modal = Modal::Session { selected: 0 };
            } else {
                match st.load_session(&target) {
                    Ok(id) => st.note(format!("Resumed session {id}")),
                    Err(e) => st.notice = e,
                }
            }
        }
        if let Some(ag) = initial_agent {
            let found = st.agent_entries().into_iter().find(|(n, _, _)| *n == ag);
            if let Some((_, eff, chain)) = found {
                st.effort = eff;
                if let Some(first) = chain.first() {
                    st.model = first.clone();
                }
            }
            st.agent = Some(ag);
        }
        if let Some(m) = initial_model {
            st.model = m;
        }
        if let Some(e) = initial_effort {
            st.effort = e;
        }
        st.refresh_badges(&router);
        st.render();
    }

    loop {
        let key = next_key(&rx, Duration::from_millis(90));
        let action = {
            let mut st = state.lock().unwrap();
            match key {
                None => {
                    st.tick = st.tick.wrapping_add(1);
                    let resized = st.poll_size();
                    if st.busy || st.dirty || resized {
                        st.dirty = false;
                        st.render();
                    }
                    continue;
                }
                Some(k) => {
                    let act = if st.modal == Modal::None {
                        handle_key(&mut st, k)
                    } else {
                        handle_modal_key(&mut st, k)
                    };
                    st.render();
                    act
                }
            }
        };
        match action {
            Action::None => {}
            Action::Quit => break,
            Action::Submit(text) => start_turn(&state, &router, text),
            Action::SaveFallback(chain) => {
                let mut st = state.lock().unwrap();
                apply_tui_fallback_chain(&mut st, &chain, None);
                st.render();
            }
            Action::Command(line) => {
                if run_command(&state, &router, &cfg, &line) {
                    break;
                }
            }
        }
    }

    let summary = {
        let st = state.lock().unwrap();
        st.save_session();
        (st.session_id.clone(), !st.history.is_empty())
    };
    drop(guard);

    let (session_id, has_messages) = summary;
    if has_messages {
        println!("Session: {session_id}");
        println!("Resume with: cligate --resume {session_id}");
    } else {
        println!("Session: {session_id} (empty, not saved)");
    }
}
