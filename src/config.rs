use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(1);
pub static PROGRESS: Mutex<String> = Mutex::new(String::new());
static CANCELLED: AtomicBool = AtomicBool::new(false);
static ACTIVE_CHILD_PID: AtomicU32 = AtomicU32::new(0);

pub(crate) fn set_progress(msg: &str) {
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

pub(crate) struct ActivePidGuard;

impl ActivePidGuard {
    pub(crate) fn set(pid: u32) -> Self {
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

pub(crate) fn now_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs()
}

pub(crate) fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs_f64()
}

pub(crate) fn gen_hex_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{nanos:016x}{seq:08x}")
}

pub fn fnv1a_hex(data: &str) -> String {
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

pub(crate) fn home_dir() -> PathBuf {
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

pub(crate) fn parse_bool_flag(raw: &str) -> Option<bool> {
    match raw.trim().to_lowercase().as_str() {
        "1" | "true" | "on" | "yes" | "enable" | "enabled" => Some(true),
        "0" | "false" | "off" | "no" | "disable" | "disabled" => Some(false),
        _ => None,
    }
}

#[derive(Clone)]
pub(crate) struct Config {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) agy_bin: String,
    pub(crate) codex_bin: String,
    pub(crate) claude_bin: String,
    pub(crate) default_model: String,
    pub(crate) workdir: String,
    pub(crate) routes_path: String,
    pub(crate) max_prompt_bytes: usize,
    pub(crate) sticky_ttl_sec: f64,
    pub(crate) max_sticky_sessions: usize,
    pub(crate) cooldown_sec: f64,
    pub(crate) fallback_enabled: Option<bool>,
}

impl Config {
    pub(crate) fn from_env() -> Self {
        let host = env::var("CLIGATE_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = env::var("CLIGATE_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8787);
        let default_model =
            env::var("CLIGATE_DEFAULT_MODEL").unwrap_or_else(|_| "gemini-4-neon".to_string());
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
            .unwrap_or(180_000);
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
        let fallback_enabled = env::var("CLIGATE_FALLBACK")
            .ok()
            .and_then(|v| parse_bool_flag(&v));

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
            fallback_enabled,
        }
    }
}
