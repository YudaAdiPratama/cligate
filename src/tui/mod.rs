mod render;
mod storage;
mod term;

#[cfg(test)]
pub use render::wrap_text_lines;
pub use storage::{new_session_id, parse_token_limit};

use crate::catalog::all_known_models;
use crate::config::{is_cancelled, now_sec, request_cancel, reset_cancel, Config, PROGRESS};
use crate::json::{parse_json, Json};
use crate::prompt::parse_tool_calls;
use crate::router::{load_agent_routes, ProviderRouter};
use crate::server::http_call;
use crate::tools::{
    append_memory, build_tui_system_prompt, clear_memory, execute_local_tool, is_read_only_tool,
    list_skills, read_memory_file, summarize_tool_call, tui_tool_definitions, PermissionMode,
};
use render::{clip, display_cwd};
use std::collections::VecDeque;
use std::fs;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use storage::{
    estimate_history_tokens, format_tokens_short, list_saved_sessions, load_prefs, sessions_dir,
    write_prefs, MAX_WINDOW_PRESETS,
};
use term::{detect_term_size, next_key, spawn_input_thread, Key, RawTermGuard};

const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const COMMANDS: &[(&str, &str)] = &[
    ("/model", "Switch model and reasoning level"),
    ("/effort", "Set reasoning effort level"),
    ("/setfallback", "Configure TUI-only fallback model chain"),
    (
        "/permissions",
        "Configure tool permissions and always-allow",
    ),
    ("/memory", "View or edit persistent TUI memory"),
    ("/skills", "Browse and activate local skills"),
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
const HELP_TEXT: &str = "Shortcuts\n  /model       F2 / ctrl+n   switch model and effort level\n  /effort      F3 / ctrl+t   reasoning effort\n  /setfallback [m1 m2 ...]   configure TUI-only fallback chain (or 'reset')\n  /permissions [mode]        tool permissions (ask, always-allow, read-only, reset)\n  /memory      [add|user|clear] view or update persistent TUI memory\n  /skills      [name]        browse or toggle local SKILL.md skills\n  /agent       F4 / ctrl+g   agent fallback chain\n  /compact     [focus]       compact conversation into a summary\n  /autocompact [32k|64k|off] set auto-compact token limit\n  /resume                    resume a saved session\n  /new                       start a fresh session\n  /status      F5            provider health\n  /reset       ctrl+r        clear rate-limit cooldowns\n  /clear       ctrl+l        clear conversation\n  scroll: mouse wheel, shift+up/down, PgUp/PgDn | stop: esc or ctrl+c | quit: ctrl+c x2";

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Kind {
    User,
    Assistant,
    Note,
    Error,
}

pub(super) struct Entry {
    pub(super) kind: Kind,
    pub(super) text: String,
    pub(super) meta: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PermDecision {
    AllowOnce,
    AlwaysAllowTool,
    AlwaysAllowAll,
    Deny,
}

#[derive(Clone, PartialEq)]
pub(super) enum Modal {
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
    Skills {
        selected: usize,
    },
    Session {
        selected: usize,
    },
}

#[derive(Clone)]

enum Action {
    None,
    Submit(String),
    Command(String),
    SaveFallback(Vec<String>),
    Quit,
}

type ModelRow = (&'static str, &'static str, &'static [&'static str]);

pub(super) struct TuiState {
    pub(super) session_id: String,
    pub(super) model: String,
    pub(super) effort: String,
    pub(super) agent: Option<String>,
    pub(super) tui_fallback: Vec<String>,
    pub(super) perm_mode: PermissionMode,
    pub(super) always_allow_tools: Vec<String>,
    pub(super) active_skills: Vec<String>,
    pub(super) auto_compact_tokens: usize,
    pub(super) pending_perm: Option<mpsc::Sender<PermDecision>>,
    pub(super) routes_path: String,
    pub(super) work_dir: String,
    pub(super) cwd: String,
    pub(super) history: Vec<Json>,
    pub(super) entries: Vec<Entry>,
    pub(super) input: Vec<char>,
    pub(super) cursor: usize,
    pub(super) prompts: Vec<String>,
    pub(super) prompt_idx: Option<usize>,
    pub(super) queued_prompts: VecDeque<String>,
    pub(super) last_ctrl_c: Option<Instant>,
    pub(super) scroll: usize,
    pub(super) modal: Modal,
    pub(super) sugg_sel: usize,
    pub(super) busy: bool,
    pub(super) busy_label: String,
    pub(super) busy_since: Instant,
    pub(super) stream_chars: usize,
    pub(super) tick: usize,
    pub(super) opened_at: Instant,
    pub(super) badges: Vec<(String, Option<i64>)>,
    pub(super) notice: String,
    pub(super) dirty: bool,
    pub(super) size: (usize, usize),
    pub(super) size_at: Instant,
    pub(super) force_clear: bool,
    pub(super) standalone_resume: bool,
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
            effort: prefs.effort.unwrap_or_else(|| "high".to_string()),
            agent: None,
            tui_fallback: prefs.fallback_chain,
            perm_mode: prefs.perm_mode,
            always_allow_tools: prefs.always_allow_tools,
            active_skills: Vec::new(),
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
            queued_prompts: VecDeque::new(),
            last_ctrl_c: None,
            scroll: 0,
            modal: Modal::None,
            sugg_sel: 0,
            busy: false,
            busy_label: "Thinking…".to_string(),
            busy_since: Instant::now(),
            stream_chars: 0,
            tick: 0,
            opened_at: Instant::now(),
            badges: Vec::new(),
            notice: String::new(),
            dirty: false,
            size: detect_term_size(),
            size_at: Instant::now(),
            force_clear: true,
            standalone_resume: false,
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
            (
                "prompts",
                Json::Arr(self.prompts.iter().cloned().map(Json::Str).collect()),
            ),
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
        self.prompts = doc
            .get("prompts")
            .and_then(|v| v.as_arr())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if self.prompts.is_empty() {
            self.prompts = self
                .entries
                .iter()
                .filter(|e| e.kind == Kind::User)
                .map(|e| e.text.clone())
                .collect();
        }
        self.prompt_idx = None;
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

    pub(super) fn suggestions(&self) -> Vec<(&'static str, &'static str)> {
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

    pub(super) fn filtered_models(&self, filter: &str) -> Vec<ModelRow> {
        let low = filter.trim().to_lowercase();
        all_known_models()
            .into_iter()
            .filter(|(m, p, _)| {
                low.is_empty() || m.to_lowercase().contains(&low) || p.to_lowercase().contains(&low)
            })
            .collect()
    }

    pub(super) fn agent_entries(&self) -> Vec<(String, String, Vec<String>)> {
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
}

fn wrap_prev(sel: usize, last: usize) -> usize {
    if sel == 0 || sel > last {
        last
    } else {
        sel - 1
    }
}

fn wrap_next(sel: usize, last: usize) -> usize {
    if sel >= last {
        0
    } else {
        sel + 1
    }
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
                Key::Esc | Key::F2 | Key::Left => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Model {
                        selected: wrap_prev(selected, last),
                        filter,
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Model {
                        selected: wrap_next(selected, last),
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
                Key::Left => {
                    if pending_model.is_some() {
                        st.modal = Modal::Model {
                            selected: 0,
                            filter: String::new(),
                        };
                    } else {
                        st.modal = Modal::None;
                    }
                }
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Effort {
                        selected: wrap_prev(selected, last),
                        levels,
                        pending_model,
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Effort {
                        selected: wrap_next(selected, last),
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
                Key::Esc | Key::F4 | Key::Left => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Agent {
                        selected: wrap_prev(selected, last),
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Agent {
                        selected: wrap_next(selected, last),
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
                Key::Left => {
                    if chain.pop().is_some() {
                        st.modal = Modal::Fallback {
                            selected: 0,
                            filter: String::new(),
                            chain,
                        };
                    } else {
                        st.modal = Modal::None;
                    }
                }
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Fallback {
                        selected: wrap_prev(selected, last),
                        filter,
                        chain,
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Fallback {
                        selected: wrap_next(selected, last),
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
                Key::Esc | Key::Left => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Permissions {
                        selected: wrap_prev(selected, max_idx),
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Permissions {
                        selected: wrap_next(selected, max_idx),
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
                Key::Esc | Key::Left | Key::Char('n') | Key::Char('N') | Key::Char('4') => {
                    decision = Some(PermDecision::Deny);
                }
                Key::Char('y') | Key::Char('Y') | Key::Char('1') => {
                    decision = Some(PermDecision::AllowOnce);
                }
                Key::Char('2') => decision = Some(PermDecision::AlwaysAllowTool),
                Key::Char('3') => decision = Some(PermDecision::AlwaysAllowAll),
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::PermissionPrompt {
                        selected: wrap_prev(selected, 3),
                        tool: tool.clone(),
                        summary,
                    };
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::PermissionPrompt {
                        selected: wrap_next(selected, 3),
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
                Key::Esc | Key::Left => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::MaxWindow {
                        selected: wrap_prev(selected, custom_idx),
                        custom,
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::MaxWindow {
                        selected: wrap_next(selected, custom_idx),
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
        Modal::Skills { selected } => {
            let skills = list_skills();
            let last = skills.len().saturating_sub(1);
            match key {
                Key::Esc | Key::Left => st.modal = Modal::None,
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Skills {
                        selected: wrap_prev(selected, last),
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Skills {
                        selected: wrap_next(selected, last),
                    }
                }
                Key::Enter => {
                    if let Some(s) = skills.get(selected.min(last)) {
                        if let Some(pos) = st.active_skills.iter().position(|a| a == &s.name) {
                            st.active_skills.remove(pos);
                            st.note(format!("Deactivated skill '{}'", s.name));
                        } else {
                            st.active_skills.push(s.name.clone());
                            st.note(format!("Activated skill '{}'", s.name));
                        }
                    }
                    st.modal = Modal::None;
                }
                _ => {}
            }
        }
        Modal::Session { selected } => {
            let sessions = list_saved_sessions();
            let last = sessions.len().saturating_sub(1);
            match key {
                Key::Esc | Key::Left => {
                    if st.standalone_resume {
                        return Action::Quit;
                    }
                    st.modal = Modal::None;
                }
                Key::Up | Key::ScrollUp => {
                    st.modal = Modal::Session {
                        selected: wrap_prev(selected, last),
                    }
                }
                Key::Down | Key::ScrollDown => {
                    st.modal = Modal::Session {
                        selected: wrap_next(selected, last),
                    }
                }
                Key::Enter => {
                    if let Some(s) = sessions.get(selected.min(last)) {
                        match st.load_session(&s.id) {
                            Ok(id) => {
                                st.standalone_resume = false;
                                st.force_clear = true;
                                st.note(format!("Resumed session {id}"));
                            }
                            Err(e) => st.notice = e,
                        }
                        st.modal = Modal::None;
                    } else if st.standalone_resume {
                        return Action::Quit;
                    } else {
                        st.modal = Modal::None;
                    }
                }
                _ => {}
            }
        }
    }
    Action::None
}

fn handle_key(st: &mut TuiState, key: Key) -> Action {
    st.notice.clear();
    if key != Key::CtrlC {
        st.last_ctrl_c = None;
    }
    let sugg_len = st.suggestions().len();
    match key {
        Key::CtrlC => {
            if st.busy {
                request_cancel();
                st.last_ctrl_c = None;
                let had_q = !st.queued_prompts.is_empty();
                st.queued_prompts.clear();
                st.notice = if had_q {
                    "Stopping (queue cleared)...".to_string()
                } else {
                    "Stopping...".to_string()
                };
                return Action::None;
            }
            if !st.input.is_empty() {
                st.input.clear();
                st.cursor = 0;
                st.sugg_sel = 0;
                st.prompt_idx = None;
                st.last_ctrl_c = None;
                st.notice = "Input cleared".to_string();
                return Action::None;
            }
            if let Some(prev) = st.last_ctrl_c {
                if prev.elapsed() <= Duration::from_secs(1) {
                    return Action::Quit;
                }
            }
            st.last_ctrl_c = Some(Instant::now());
            st.notice = "Press Ctrl+C again to exit".to_string();
            return Action::None;
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
                let had_q = !st.queued_prompts.is_empty();
                st.queued_prompts.clear();
                st.notice = if had_q {
                    "Stopping (queue cleared)...".to_string()
                } else {
                    "Stopping...".to_string()
                };
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
            st.prompts.push(text.clone());
            if st.busy {
                st.queued_prompts.push_back(text.clone());
                let n = st.queued_prompts.len();
                st.note(format!("Queued (#{n}): {}", clip(&text, 72)));
                return Action::None;
            }
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
    let (model, effort, agent, tui_fallback, work_dir, active_skills) = {
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
            st.active_skills.clone(),
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
                Json::Str(build_tui_system_prompt(&work_dir, &active_skills)),
            ),
        ]);

        for _round in 0..24 {
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
        "/memory" | "/mem" => {
            if parts.len() == 1 || parts[1] == "show" {
                let u = read_memory_file("user");
                let m = read_memory_file("memory");
                let mut out = String::new();
                if !u.trim().is_empty() {
                    out.push_str(&format!("USER.md:\n{}\n\n", u.trim()));
                }
                if !m.trim().is_empty() {
                    out.push_str(&format!("MEMORY.md:\n{}", m.trim()));
                }
                if out.trim().is_empty() {
                    st.note(
                        "Memory is empty. Use /memory add <note> or /memory user <pref>"
                            .to_string(),
                    );
                } else {
                    st.note(out.trim_end().to_string());
                }
            } else if parts[1] == "add" && parts.len() >= 3 {
                let text = parts[2..].join(" ");
                match append_memory("memory", &text) {
                    Ok(_) => st.note(format!("Saved to MEMORY.md: {text}")),
                    Err(e) => st.notice = e,
                }
            } else if parts[1] == "user" && parts.len() >= 3 {
                let text = parts[2..].join(" ");
                match append_memory("user", &text) {
                    Ok(_) => st.note(format!("Saved to USER.md: {text}")),
                    Err(e) => st.notice = e,
                }
            } else if parts[1] == "clear" {
                let _ = clear_memory("memory");
                st.note("Cleared MEMORY.md".to_string());
            } else if parts[1] == "clear-user" {
                let _ = clear_memory("user");
                st.note("Cleared USER.md".to_string());
            } else {
                st.notice = "Usage: /memory [show | add <note> | user <pref> | clear | clear-user]"
                    .to_string();
            }
        }
        "/skills" | "/skill" => {
            if parts.len() == 1 {
                st.modal = Modal::Skills { selected: 0 };
            } else if matches!(parts[1].to_lowercase().as_str(), "clear" | "reset" | "off") {
                st.active_skills.clear();
                st.note("Deactivated all active skills".to_string());
            } else {
                let target = parts[1].to_lowercase();
                let found = list_skills()
                    .into_iter()
                    .find(|s| s.name.to_lowercase() == target);
                match found {
                    Some(s) => {
                        if let Some(pos) = st.active_skills.iter().position(|a| a == &s.name) {
                            st.active_skills.remove(pos);
                            st.note(format!("Deactivated skill '{}'", s.name));
                        } else {
                            st.active_skills.push(s.name.clone());
                            st.note(format!("Activated skill '{}'", s.name));
                        }
                    }
                    None => st.notice = format!("Skill '{}' not found", parts[1]),
                }
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
                st.standalone_resume = true;
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
                    if st
                        .last_ctrl_c
                        .map(|t| t.elapsed() > Duration::from_secs(1))
                        .unwrap_or(false)
                    {
                        st.last_ctrl_c = None;
                        st.notice.clear();
                        st.dirty = true;
                    }
                    if !st.busy && st.modal == Modal::None {
                        if let Some(next_prompt) = st.queued_prompts.pop_front() {
                            Action::Submit(next_prompt)
                        } else {
                            let resized = st.poll_size();
                            if st.busy || st.dirty || resized || st.splash_animating() {
                                st.dirty = false;
                                st.render();
                            }
                            continue;
                        }
                    } else {
                        let resized = st.poll_size();
                        if st.busy || st.dirty || resized || st.splash_animating() {
                            st.dirty = false;
                            st.render();
                        }
                        continue;
                    }
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
        if st.standalone_resume {
            None
        } else {
            st.save_session();
            Some((st.session_id.clone(), !st.history.is_empty()))
        }
    };
    drop(guard);

    if let Some((session_id, has_messages)) = summary {
        if has_messages {
            println!("Session: {session_id}");
            println!("Resume with: cligate --resume {session_id}");
        } else {
            println!("Session: {session_id} (empty, not saved)");
        }
    }
}
