use crate::json::Json;
use crate::{
    all_known_models, http_call, infer_provider_and_model, load_agent_routes, Config,
    ProviderRouter,
};
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct ChatEntry {
    role: String,
    header: String,
    text: String,
}

#[derive(Clone, PartialEq)]
enum Modal {
    None,
    ModelPicker { selected: usize, filter: String },
    EffortPicker { selected: usize },
    AgentPicker { selected: usize },
}

const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

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
            .args(["-icanon", "-echo", "min", "1", "time", "0"])
            .stdin(Stdio::inherit())
            .status();

        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?1049h\x1b[2J\x1b[H");
        let _ = out.flush();

        Self { saved_state: saved }
    }
}

impl Drop for RawTermGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?25h\x1b[0m\x1b[?1049l");
        let _ = out.flush();
        if let Some(ref st) = self.saved_state {
            let _ = Command::new("stty")
                .arg(st)
                .stdin(Stdio::inherit())
                .status();
        } else {
            let _ = Command::new("stty")
                .arg("sane")
                .stdin(Stdio::inherit())
                .status();
        }
    }
}

fn detect_term_size() -> (usize, usize) {
    if let Ok(out) = Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            let mut parts = s.split_whitespace();
            if let (Some(r), Some(c)) = (parts.next(), parts.next()) {
                if let (Ok(rows), Ok(cols)) = (r.parse::<usize>(), c.parse::<usize>()) {
                    if rows >= 12 && cols >= 40 {
                        return (rows, cols);
                    }
                }
            }
        }
    }
    (28, 100)
}

#[derive(Debug, PartialEq)]
enum Key {
    Char(char),
    Enter,
    Backspace,
    Esc,
    Up,
    Down,
    PageUp,
    PageDown,
    F2,
    F3,
    F4,
    F5,
    CtrlC,
    CtrlM,
    CtrlE,
    CtrlA,
    CtrlR,
    CtrlL,
    Unknown,
}

fn read_key(stdin: &mut io::Stdin) -> io::Result<Key> {
    let mut b = [0u8; 1];
    if stdin.read(&mut b)? == 0 {
        return Ok(Key::CtrlC);
    }
    match b[0] {
        3 | 17 => Ok(Key::CtrlC),
        1 => Ok(Key::CtrlA),
        5 => Ok(Key::CtrlE),
        12 => Ok(Key::CtrlL),
        18 => Ok(Key::CtrlR),
        10 | 13 => Ok(Key::Enter),
        127 | 8 => Ok(Key::Backspace),
        14 => Ok(Key::CtrlM),
        27 => {
            let mut seq = [0u8; 5];
            let n = stdin.read(&mut seq).unwrap_or(0);
            if n == 0 {
                return Ok(Key::Esc);
            }
            if seq[0] == b'[' {
                if n >= 2 {
                    match seq[1] {
                        b'A' => return Ok(Key::Up),
                        b'B' => return Ok(Key::Down),
                        b'5' if n >= 3 && seq[2] == b'~' => return Ok(Key::PageUp),
                        b'6' if n >= 3 && seq[2] == b'~' => return Ok(Key::PageDown),
                        b'1' if n >= 4 && seq[3] == b'~' => match seq[2] {
                            b'2' => return Ok(Key::F2),
                            b'3' => return Ok(Key::F3),
                            b'4' => return Ok(Key::F4),
                            b'5' => return Ok(Key::F5),
                            _ => {}
                        },
                        _ => {}
                    }
                }
            } else if seq[0] == b'O' && n >= 2 {
                match seq[1] {
                    b'Q' => return Ok(Key::F2),
                    b'R' => return Ok(Key::F3),
                    b'S' => return Ok(Key::F4),
                    _ => {}
                }
            }
            Ok(Key::Esc)
        }
        c if (32..127).contains(&c) => Ok(Key::Char(c as char)),
        first if first >= 0xC0 => {
            let width = if first < 0xE0 {
                2
            } else if first < 0xF0 {
                3
            } else {
                4
            };
            let mut buf = vec![first];
            let mut rest = vec![0u8; width - 1];
            if stdin.read_exact(&mut rest).is_ok() {
                buf.extend_from_slice(&rest);
                if let Ok(s) = std::str::from_utf8(&buf) {
                    if let Some(ch) = s.chars().next() {
                        return Ok(Key::Char(ch));
                    }
                }
            }
            Ok(Key::Unknown)
        }
        _ => Ok(Key::Unknown),
    }
}

pub fn wrap_text_lines(text: &str, max_width: usize) -> Vec<String> {
    let width = max_width.max(10);
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        for ch in raw_line.chars() {
            if current.chars().count() >= width {
                out.push(current);
                current = String::new();
            }
            current.push(ch);
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn pad_or_clip(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count > width {
        s.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
    } else {
        format!("{s}{}", " ".repeat(width - count))
    }
}

pub struct TuiState {
    pub model: String,
    pub effort: String,
    pub agent: Option<String>,
    history: Vec<Json>,
    transcript: Vec<ChatEntry>,
    input: String,
    scroll_offset: usize,
    modal: Modal,
    status_note: String,
    provider_badges: String,
}

impl TuiState {
    pub fn new(cfg: &Config) -> Self {
        Self {
            model: cfg.default_model.clone(),
            effort: "medium".to_string(),
            agent: None,
            history: Vec::new(),
            transcript: vec![ChatEntry {
                role: "system".to_string(),
                header: "cligate TUI ready".to_string(),
                text: "Commands: /model (or F2) switch model | /effort (or F3) switch reasoning level | /agent (or F4) switch agent fallback chain | /status (or F5) router health | /reset clear 429 cooldowns | /clear reset chat | /quit exit.".to_string(),
            }],
            input: String::new(),
            scroll_offset: 0,
            modal: Modal::None,
            status_note: "Ready".to_string(),
            provider_badges: "claude:OK codex:OK agy:OK".to_string(),
        }
    }

    fn filtered_models(
        &self,
        filter: &str,
    ) -> Vec<(&'static str, &'static str, &'static [&'static str])> {
        let low = filter.trim().to_lowercase();
        all_known_models()
            .into_iter()
            .filter(|(m, p, _)| {
                low.is_empty() || m.to_lowercase().contains(&low) || p.to_lowercase().contains(&low)
            })
            .collect()
    }

    fn agent_entries(&self, cfg: &Config) -> Vec<(String, String, Vec<String>)> {
        let routes = load_agent_routes(&cfg.routes_path);
        let mut out = vec![(
            "none".to_string(),
            "medium".to_string(),
            vec!["(direct model routing without agent override)".to_string()],
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

    fn refresh_provider_badges(&mut self, cfg: &Config, local_router: &ProviderRouter) {
        let st = http_call(&cfg.host, cfg.port, "GET", "/v1/router/status", None)
            .ok()
            .and_then(|r| r.get("router").cloned())
            .unwrap_or_else(|| local_router.status());

        if let Some(provs) = st.get("providers") {
            let mut parts = Vec::new();
            for p in ["claude", "codex", "agy"] {
                if let Some(info) = provs.get(p) {
                    let avail = info
                        .get("available")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    if avail {
                        parts.push(format!("{p}:OK"));
                    } else {
                        let rem = info
                            .get("cooldown_remaining_sec")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        parts.push(format!("{p}:429({rem}s)"));
                    }
                }
            }
            if !parts.is_empty() {
                self.provider_badges = parts.join(" ");
            }
        }
    }

    fn render(&self, cfg: &Config) {
        let (rows, cols) = detect_term_size();
        let mut buf = String::with_capacity(rows * cols * 2);
        buf.push_str("\x1b[?25l\x1b[H");

        let (prov, bare_mod) = infer_provider_and_model(&self.model);
        let ag_label = self.agent.as_deref().unwrap_or("none");
        let top_bar = format!(
            " cligate TUI | Agent: {ag_label} | Model: {prov}/{bare_mod} | Effort: {} | Providers: {} ",
            self.effort, self.provider_badges
        );
        buf.push_str("\x1b[1;37;44m");
        buf.push_str(&pad_or_clip(&top_bar, cols));
        buf.push_str("\x1b[0m\r\n");

        let chat_height = rows.saturating_sub(5);
        let inner_w = cols.saturating_sub(4);
        let mut rendered_lines: Vec<(String, &'static str)> = Vec::new();

        for entry in &self.transcript {
            let (badge_color, prefix) = match entry.role.as_str() {
                "user" => ("\x1b[1;32m", "You"),
                "assistant" => ("\x1b[1;36m", "Assistant"),
                _ => ("\x1b[1;33m", "System"),
            };
            let hdr = if entry.header.is_empty() {
                format!("┌─ {prefix}")
            } else {
                format!("┌─ {prefix} ({})", entry.header)
            };
            rendered_lines.push((pad_or_clip(&hdr, cols.saturating_sub(2)), badge_color));
            for wline in wrap_text_lines(&entry.text, inner_w) {
                rendered_lines.push((format!("│ {}", pad_or_clip(&wline, inner_w)), "\x1b[0m"));
            }
            rendered_lines.push((String::new(), "\x1b[0m"));
        }

        let total_lines = rendered_lines.len();
        let max_scroll = total_lines.saturating_sub(chat_height);
        let effective_scroll = self.scroll_offset.min(max_scroll);
        let start_idx = total_lines
            .saturating_sub(chat_height)
            .saturating_sub(effective_scroll);
        let end_idx = (start_idx + chat_height).min(total_lines);

        for row_idx in 0..chat_height {
            let line_idx = start_idx + row_idx;
            if line_idx < end_idx {
                let (ref txt, color) = rendered_lines[line_idx];
                buf.push_str(color);
                buf.push(' ');
                buf.push_str(&pad_or_clip(txt, cols.saturating_sub(1)));
                buf.push_str("\x1b[0m\r\n");
            } else {
                buf.push_str(&" ".repeat(cols));
                buf.push_str("\r\n");
            }
        }

        let status_line = format!(
            " Status: {} | Scroll: +{} lines ",
            self.status_note, effective_scroll
        );
        buf.push_str("\x1b[30;47m");
        buf.push_str(&pad_or_clip(&status_line, cols));
        buf.push_str("\x1b[0m\r\n");

        let prompt_prefix = " > ";
        let avail_input = cols.saturating_sub(prompt_prefix.len() + 1);
        let input_tail: String = if self.input.chars().count() > avail_input {
            let skip = self.input.chars().count() - avail_input;
            self.input.chars().skip(skip).collect()
        } else {
            self.input.clone()
        };
        buf.push_str("\x1b[1;32m");
        buf.push_str(prompt_prefix);
        buf.push_str("\x1b[0m");
        buf.push_str(&pad_or_clip(&input_tail, avail_input));
        buf.push_str("\r\n");

        let help_bar = " [Enter] Send  [F2|/model] Model  [F3|/effort] Level  [F4|/agent] Agent  [F5|/status] Health  [/reset] Clear 429  [Ctrl+C] Quit ";
        buf.push_str("\x1b[1;37;100m");
        buf.push_str(&pad_or_clip(help_bar, cols));
        buf.push_str("\x1b[0m");

        match &self.modal {
            Modal::None => {}
            Modal::ModelPicker { selected, filter } => {
                let models = self.filtered_models(filter);
                let box_w = (cols.saturating_sub(10)).clamp(36, 74);
                let box_h = (rows.saturating_sub(6)).clamp(8, 18);
                let start_r = (rows.saturating_sub(box_h)) / 2;
                let start_c = (cols.saturating_sub(box_w)) / 2;
                let title = format!(
                    " Select Native Model (Filter: '{}' | ↑/↓/Enter/Esc) ",
                    filter
                );
                draw_modal_box(&mut buf, start_r, start_c, box_w, box_h, &title, |list_h| {
                    let mut rows_out = Vec::new();
                    if models.is_empty() {
                        rows_out.push(("  (No matching models)".to_string(), false));
                        return rows_out;
                    }
                    let sel = (*selected).min(models.len() - 1);
                    let win_start = sel.saturating_sub(list_h / 2);
                    for (idx, (m, prov, efforts)) in
                        models.iter().enumerate().skip(win_start).take(list_h)
                    {
                        let line =
                            format!(" {:<28} [{:<6}] levels: {}", m, prov, efforts.join(","));
                        rows_out.push((line, idx == sel));
                    }
                    rows_out
                });
            }
            Modal::EffortPicker { selected } => {
                let box_w = 52.min(cols.saturating_sub(6));
                let box_h = 9;
                let start_r = (rows.saturating_sub(box_h)) / 2;
                let start_c = (cols.saturating_sub(box_w)) / 2;
                let title = " Select Reasoning Effort Level (↑/↓/Enter/Esc) ";
                draw_modal_box(&mut buf, start_r, start_c, box_w, box_h, title, |_| {
                    EFFORT_LEVELS
                        .iter()
                        .enumerate()
                        .map(|(i, lvl)| {
                            let desc = match *lvl {
                                "low" => "Fastest response, minimal thinking",
                                "medium" => "Balanced speed and reasoning (default)",
                                "high" => "Deep thinking for coding & debugging",
                                "xhigh" => "Extra-high reasoning depth",
                                "max" => "Maximum reasoning budget",
                                _ => "",
                            };
                            (format!(" {:<8} - {desc}", lvl), i == *selected)
                        })
                        .collect()
                });
            }
            Modal::AgentPicker { selected } => {
                let entries = self.agent_entries(cfg);
                let box_w = (cols.saturating_sub(8)).clamp(40, 78);
                let box_h = (rows.saturating_sub(6)).clamp(8, 16);
                let start_r = (rows.saturating_sub(box_h)) / 2;
                let start_c = (cols.saturating_sub(box_w)) / 2;
                let title = " Select Agent Fallback Chain (↑/↓/Enter/Esc) ";
                draw_modal_box(&mut buf, start_r, start_c, box_w, box_h, title, |list_h| {
                    let sel = (*selected).min(entries.len().saturating_sub(1));
                    let win_start = sel.saturating_sub(list_h / 2);
                    entries
                        .iter()
                        .enumerate()
                        .skip(win_start)
                        .take(list_h)
                        .map(|(i, (name, eff, chain))| {
                            let line = format!(
                                " {:<14} (effort={:<6}) -> {}",
                                name,
                                eff,
                                chain.join(" -> ")
                            );
                            (line, i == sel)
                        })
                        .collect()
                });
            }
        }

        let cursor_row = rows.saturating_sub(1);
        let cursor_col = (prompt_prefix.len() + input_tail.chars().count() + 1).min(cols);
        buf.push_str(&format!("\x1b[{cursor_row};{cursor_col}H\x1b[?25h"));

        let mut out = io::stdout();
        let _ = out.write_all(buf.as_bytes());
        let _ = out.flush();
    }
}

fn draw_modal_box<F>(
    buf: &mut String,
    top_r: usize,
    left_c: usize,
    width: usize,
    height: usize,
    title: &str,
    build_rows: F,
) where
    F: FnOnce(usize) -> Vec<(String, bool)>,
{
    let inner_w = width.saturating_sub(2);
    let list_h = height.saturating_sub(2);
    let rows = build_rows(list_h);

    let top_border = format!("┌{}┐", pad_or_clip(title, inner_w).replace(' ', "─"));
    buf.push_str(&format!(
        "\x1b[{};{}H\x1b[1;37;45m{}\x1b[0m",
        top_r, left_c, top_border
    ));

    for i in 0..list_h {
        let r = top_r + 1 + i;
        buf.push_str(&format!("\x1b[{r};{left_c}H"));
        if let Some((ref text, is_sel)) = rows.get(i) {
            if *is_sel {
                buf.push_str("\x1b[1;30;46m│");
                buf.push_str(&pad_or_clip(text, inner_w));
                buf.push_str("│\x1b[0m");
            } else {
                buf.push_str("\x1b[37;40m│");
                buf.push_str(&pad_or_clip(text, inner_w));
                buf.push_str("│\x1b[0m");
            }
        } else {
            buf.push_str("\x1b[37;40m│");
            buf.push_str(&" ".repeat(inner_w));
            buf.push_str("│\x1b[0m");
        }
    }

    let bot_r = top_r + height - 1;
    buf.push_str(&format!(
        "\x1b[{bot_r};{left_c}H\x1b[1;37;45m└{}┘\x1b[0m",
        "─".repeat(inner_w)
    ));
}

fn send_prompt_turn(
    state: &Arc<Mutex<TuiState>>,
    cfg: &Config,
    local_router: &ProviderRouter,
    user_text: &str,
) {
    let (model, effort, agent, messages_snapshot) = {
        let mut st = state.lock().unwrap();
        st.scroll_offset = 0;
        st.transcript.push(ChatEntry {
            role: "user".to_string(),
            header: String::new(),
            text: user_text.to_string(),
        });
        st.history.push(Json::obj(vec![
            ("role", Json::Str("user".to_string())),
            ("content", Json::Str(user_text.to_string())),
        ]));
        let hdr = format!("{} | effort={}", st.model, st.effort);
        st.transcript.push(ChatEntry {
            role: "assistant".to_string(),
            header: hdr,
            text: "…".to_string(),
        });
        st.status_note = format!("Generating via {} (effort={})...", st.model, st.effort);
        st.render(cfg);
        (
            st.model.clone(),
            st.effort.clone(),
            st.agent.clone(),
            st.history.clone(),
        )
    };

    let state_cb = Arc::clone(state);
    let mut accumulated = String::new();
    let res = {
        let mut on_delta = |chunk: &str| {
            accumulated.push_str(chunk);
            if let Ok(mut st) = state_cb.lock() {
                if let Some(last) = st.transcript.last_mut() {
                    last.text = accumulated.clone();
                }
                st.render(cfg);
            }
        };
        let mut cb_opt: Option<&mut dyn FnMut(&str)> = Some(&mut on_delta);

        local_router.execute_turn(
            &messages_snapshot,
            None,
            &model,
            effort.clone(),
            agent.as_deref(),
            &mut cb_opt,
        )
    };

    let mut st = state.lock().unwrap();
    match res {
        Ok((final_text, _usage, resolved_model, used_provider)) => {
            if let Some(last) = st.transcript.last_mut() {
                last.header = format!("{resolved_model} via {used_provider}");
                last.text = if final_text.is_empty() {
                    accumulated
                } else {
                    final_text.clone()
                };
            }
            st.history.push(Json::obj(vec![
                ("role", Json::Str("assistant".to_string())),
                ("content", Json::Str(final_text)),
            ]));
            st.status_note = format!("Completed ({resolved_model} via {used_provider})");
        }
        Err(err) => {
            if let Some(last) = st.transcript.last_mut() {
                last.header = "error".to_string();
                last.text = format!("Error: {err}");
            }
            st.status_note = "Turn failed".to_string();
        }
    }
    st.refresh_provider_badges(cfg, local_router);
    st.render(cfg);
}

fn handle_slash_command(
    cmd_line: &str,
    state: &Arc<Mutex<TuiState>>,
    cfg: &Config,
    local_router: &ProviderRouter,
) -> bool {
    let parts: Vec<&str> = cmd_line.split_whitespace().collect();
    if parts.is_empty() {
        return false;
    }
    let mut st = state.lock().unwrap();
    match parts[0] {
        "/quit" | "/exit" | "/q" => return true,
        "/clear" | "/new" => {
            st.history.clear();
            st.transcript.clear();
            st.scroll_offset = 0;
            st.status_note = "Conversation cleared".to_string();
        }
        "/model" | "/m" => {
            if parts.len() == 1 {
                st.modal = Modal::ModelPicker {
                    selected: 0,
                    filter: String::new(),
                };
            } else {
                st.model = parts[1].to_string();
                let mut i = 2;
                while i < parts.len() {
                    if matches!(parts[i], "--reasoning" | "--effort" | "-e") && i + 1 < parts.len()
                    {
                        st.effort = parts[i + 1].to_lowercase();
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                st.status_note = format!("Switched model to {} (effort={})", st.model, st.effort);
            }
        }
        "/effort" | "/reasoning" | "/level" => {
            if parts.len() == 1 {
                let idx = EFFORT_LEVELS
                    .iter()
                    .position(|e| *e == st.effort)
                    .unwrap_or(1);
                st.modal = Modal::EffortPicker { selected: idx };
            } else {
                let lvl = parts[1].to_lowercase();
                if EFFORT_LEVELS.contains(&lvl.as_str()) {
                    st.effort = lvl;
                    st.status_note = format!("Reasoning effort set to {}", st.effort);
                } else {
                    st.status_note =
                        "Invalid effort level (choose: low, medium, high, xhigh, max)".to_string();
                }
            }
        }
        "/agent" | "/a" => {
            if parts.len() == 1 {
                st.modal = Modal::AgentPicker { selected: 0 };
            } else if parts[1] == "set" && parts.len() >= 4 {
                let ag_name = parts[2];
                let mut models = Vec::new();
                let mut eff_ov: Option<&str> = None;
                let mut i = 3;
                while i < parts.len() {
                    if matches!(parts[i], "--effort" | "--reasoning" | "-e") && i + 1 < parts.len()
                    {
                        eff_ov = Some(parts[i + 1]);
                        i += 2;
                    } else {
                        models.push(parts[i].to_string());
                        i += 1;
                    }
                }
                if local_router
                    .set_agent_chain(ag_name, &models, eff_ov)
                    .is_ok()
                {
                    st.agent = Some(ag_name.to_string());
                    if let Some(first) = models.first() {
                        st.model = first.clone();
                    }
                    if let Some(e) = eff_ov {
                        st.effort = e.to_lowercase();
                    }
                    st.status_note =
                        format!("Updated agent '{}' chain: {}", ag_name, models.join(" -> "));
                }
            } else if matches!(parts[1], "none" | "off" | "reset") {
                st.agent = None;
                st.status_note = "Agent override disabled".to_string();
            } else {
                let target = parts[1].to_lowercase();
                let entries = st.agent_entries(cfg);
                if let Some((_, eff, chain)) = entries.into_iter().find(|(n, _, _)| *n == target) {
                    st.agent = Some(target.clone());
                    st.effort = eff;
                    if let Some(first) = chain.first() {
                        st.model = first.clone();
                    }
                    st.status_note = format!("Active agent switched to '{target}'");
                } else {
                    st.agent = Some(target.clone());
                    st.status_note = format!("Active agent set to '{target}'");
                }
            }
        }
        "/reset" => {
            let prov = parts.get(1).copied();
            local_router.reset_cooldown(prov);
            let body = Json::obj(vec![(
                "provider",
                prov.map(|s| Json::Str(s.to_string())).unwrap_or(Json::Null),
            )]);
            let _ = http_call(&cfg.host, cfg.port, "POST", "/v1/router/reset", Some(&body));
            st.refresh_provider_badges(cfg, local_router);
            st.status_note = "Provider rate-limit cooldowns reset".to_string();
        }
        "/status" => {
            st.refresh_provider_badges(cfg, local_router);
            let status_json = http_call(&cfg.host, cfg.port, "GET", "/v1/router/status", None)
                .unwrap_or_else(|_| local_router.status());
            st.transcript.push(ChatEntry {
                role: "system".to_string(),
                header: "router status".to_string(),
                text: status_json.dump_pretty(),
            });
            st.scroll_offset = 0;
            st.status_note = "Displayed live router status".to_string();
        }
        _ => {
            st.status_note = format!(
                "Unknown command '{}'. Try /model, /effort, /agent, /status, /reset, /clear, /quit",
                parts[0]
            );
        }
    }
    st.render(cfg);
    false
}

pub fn run_tui(
    cfg: Config,
    router: Arc<ProviderRouter>,
    initial_agent: Option<String>,
    initial_model: Option<String>,
    initial_effort: Option<String>,
) {
    let _guard = RawTermGuard::enter();
    let state = Arc::new(Mutex::new(TuiState::new(&cfg)));
    {
        let mut st = state.lock().unwrap();
        if let Some(ag) = initial_agent {
            let entries = st.agent_entries(&cfg);
            if let Some((_, eff, chain)) = entries.into_iter().find(|(n, _, _)| *n == ag) {
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
        st.refresh_provider_badges(&cfg, &router);
        st.render(&cfg);
    }

    let mut stdin = io::stdin();
    while let Ok(key) = read_key(&mut stdin) {
        let mut pending_turn: Option<String> = None;
        let mut pending_cmd: Option<String> = None;

        {
            let mut st = state.lock().unwrap();
            match st.modal.clone() {
                Modal::ModelPicker {
                    mut selected,
                    mut filter,
                } => {
                    let models = st.filtered_models(&filter);
                    match key {
                        Key::Esc | Key::F2 => st.modal = Modal::None,
                        Key::CtrlC => break,
                        Key::Up => {
                            selected = selected.saturating_sub(1);
                            st.modal = Modal::ModelPicker { selected, filter };
                        }
                        Key::Down => {
                            if !models.is_empty() {
                                selected = (selected + 1).min(models.len() - 1);
                            }
                            st.modal = Modal::ModelPicker { selected, filter };
                        }
                        Key::Backspace => {
                            filter.pop();
                            selected = 0;
                            st.modal = Modal::ModelPicker { selected, filter };
                        }
                        Key::Char(ch) => {
                            filter.push(ch);
                            selected = 0;
                            st.modal = Modal::ModelPicker { selected, filter };
                        }
                        Key::Enter => {
                            if let Some((m, prov, _)) = models.get(selected) {
                                st.model = (*m).to_string();
                                st.status_note =
                                    format!("Selected model: {prov}/{m} (effort={})", st.effort);
                            }
                            st.modal = Modal::None;
                        }
                        _ => {}
                    }
                    st.render(&cfg);
                    continue;
                }
                Modal::EffortPicker { mut selected } => {
                    match key {
                        Key::Esc | Key::F3 => st.modal = Modal::None,
                        Key::CtrlC => break,
                        Key::Up => {
                            selected = selected.saturating_sub(1);
                            st.modal = Modal::EffortPicker { selected };
                        }
                        Key::Down => {
                            selected = (selected + 1).min(EFFORT_LEVELS.len() - 1);
                            st.modal = Modal::EffortPicker { selected };
                        }
                        Key::Enter => {
                            st.effort = EFFORT_LEVELS[selected].to_string();
                            st.status_note = format!("Reasoning effort set to '{}'", st.effort);
                            st.modal = Modal::None;
                        }
                        _ => {}
                    }
                    st.render(&cfg);
                    continue;
                }
                Modal::AgentPicker { mut selected } => {
                    let entries = st.agent_entries(&cfg);
                    match key {
                        Key::Esc | Key::F4 => st.modal = Modal::None,
                        Key::CtrlC => break,
                        Key::Up => {
                            selected = selected.saturating_sub(1);
                            st.modal = Modal::AgentPicker { selected };
                        }
                        Key::Down => {
                            if !entries.is_empty() {
                                selected = (selected + 1).min(entries.len() - 1);
                            }
                            st.modal = Modal::AgentPicker { selected };
                        }
                        Key::Enter => {
                            if let Some((name, eff, chain)) = entries.get(selected) {
                                if name == "none" {
                                    st.agent = None;
                                    st.status_note =
                                        "Agent chain disabled (direct model routing)".to_string();
                                } else {
                                    st.agent = Some(name.clone());
                                    st.effort = eff.clone();
                                    if let Some(first) = chain.first() {
                                        st.model = first.clone();
                                    }
                                    st.status_note =
                                        format!("Active agent: {} ({})", name, chain.join(" -> "));
                                }
                            }
                            st.modal = Modal::None;
                        }
                        _ => {}
                    }
                    st.render(&cfg);
                    continue;
                }
                Modal::None => {}
            }

            match key {
                Key::CtrlC => break,
                Key::F2 | Key::CtrlM => {
                    st.modal = Modal::ModelPicker {
                        selected: 0,
                        filter: String::new(),
                    };
                }
                Key::F3 | Key::CtrlE => {
                    let idx = EFFORT_LEVELS
                        .iter()
                        .position(|e| *e == st.effort)
                        .unwrap_or(1);
                    st.modal = Modal::EffortPicker { selected: idx };
                }
                Key::F4 | Key::CtrlA => {
                    st.modal = Modal::AgentPicker { selected: 0 };
                }
                Key::F5 => {
                    pending_cmd = Some("/status".to_string());
                }
                Key::CtrlR => {
                    pending_cmd = Some("/reset".to_string());
                }
                Key::CtrlL => {
                    pending_cmd = Some("/clear".to_string());
                }
                Key::Up => {
                    st.scroll_offset = st.scroll_offset.saturating_add(1);
                }
                Key::Down => {
                    st.scroll_offset = st.scroll_offset.saturating_sub(1);
                }
                Key::PageUp => {
                    st.scroll_offset = st.scroll_offset.saturating_add(8);
                }
                Key::PageDown => {
                    st.scroll_offset = st.scroll_offset.saturating_sub(8);
                }
                Key::Backspace => {
                    st.input.pop();
                }
                Key::Esc => {
                    st.input.clear();
                }
                Key::Char(ch) => {
                    st.input.push(ch);
                }
                Key::Enter => {
                    let trimmed = st.input.trim().to_string();
                    st.input.clear();
                    if !trimmed.is_empty() {
                        if trimmed.starts_with('/') {
                            pending_cmd = Some(trimmed);
                        } else {
                            pending_turn = Some(trimmed);
                        }
                    }
                }
                Key::Unknown => {}
            }
            st.render(&cfg);
        }

        if let Some(cmd) = pending_cmd {
            if handle_slash_command(&cmd, &state, &cfg, &router) {
                break;
            }
        } else if let Some(prompt_text) = pending_turn {
            send_prompt_turn(&state, &cfg, &router, &prompt_text);
        }
    }
}
