use super::storage::{
    estimate_history_tokens, format_tokens_short, list_saved_sessions, MAX_WINDOW_PRESETS,
};
use super::{Entry, Kind, Modal, TuiState};
use crate::catalog::infer_provider_and_model;
use crate::config::PROGRESS;
use crate::tools::{list_skills, PermissionMode};
use std::io::{self, Write};
use std::time::Duration;

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

const SPLASH_MS: u64 = 2200;
const REVEAL_MS: usize = 900;
const LOGO_COLORS: [u8; 6] = [166, 172, 173, 209, 215, 216];
const LOGO_GLYPHS: [[&str; 5]; 7] = [
    [" ███", "█   ", "█   ", "█   ", " ███"],
    ["█   ", "█   ", "█   ", "█   ", "████"],
    ["███", " █ ", " █ ", " █ ", "███"],
    [" ███", "█   ", "█ ██", "█  █", " ███"],
    [" ██ ", "█  █", "████", "█  █", "█  █"],
    ["███", " █ ", " █ ", " █ ", " █ "],
    ["████", "█   ", "███ ", "█   ", "████"],
];

fn logo_rows() -> Vec<Vec<char>> {
    (0..5)
        .map(|r| {
            LOGO_GLYPHS
                .iter()
                .map(|g| g[r])
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .collect()
        })
        .collect()
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

pub(super) fn clip(s: &str, width: usize) -> String {
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

pub(super) fn display_cwd() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && cwd.starts_with(&h) => format!("~{}", &cwd[h.len()..]),
        _ => cwd,
    }
}

pub(super) fn progress_bar(pct: usize, width: usize) -> String {
    let filled = (pct.min(100) * width) / 100;
    format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(width.saturating_sub(filled))
    )
}

pub(super) fn format_dur(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

pub(super) fn draw_box(
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

impl TuiState {
    pub(super) fn splash_animating(&self) -> bool {
        !self.standalone_resume
            && self.entries.is_empty()
            && self.opened_at.elapsed() < Duration::from_millis(SPLASH_MS)
    }

    fn logo_lines(&self) -> Vec<String> {
        let rows = logo_rows();
        let width = rows.first().map(|r| r.len()).unwrap_or(0);
        let elapsed = self.opened_at.elapsed().as_millis() as usize;
        let animating = self.entries.is_empty() && elapsed < SPLASH_MS as usize;
        let revealed = if animating {
            (width * elapsed.min(REVEAL_MS) / REVEAL_MS).min(width)
        } else {
            width
        };
        let n = LOGO_COLORS.len();
        rows.iter()
            .map(|row| {
                let mut s = String::new();
                for (c, &ch) in row.iter().enumerate() {
                    if ch == ' ' {
                        s.push(' ');
                    } else if c >= revealed {
                        s.push_str("\x1b[38;5;238m░");
                    } else {
                        let idx = if animating {
                            (c / 2 + self.tick) % n
                        } else {
                            (c * n / width.max(1)).min(n - 1)
                        };
                        s.push_str(&format!("\x1b[38;5;{}m█", LOGO_COLORS[idx]));
                    }
                }
                s.push_str(RESET);
                s
            })
            .collect()
    }

    fn transcript_lines(&self, cols: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let box_w = cols.saturating_sub(2).clamp(30, 64);
        let inner = box_w - 2;
        let boxed = |plain_len: usize, styled: String| {
            format!(
                "{ACCENT}│{RESET}{styled}{}{ACCENT}│{RESET}",
                " ".repeat(inner.saturating_sub(plain_len))
            )
        };
        let ver = env!("CARGO_PKG_VERSION");
        lines.push(format!("{ACCENT}╭{}╮{RESET}", "─".repeat(inner)));
        let title_plain = format!(" ✻ Welcome to cligate v{ver}");
        lines.push(boxed(
            title_plain.chars().count(),
            format!(" {ACCENT}✻{RESET} {BOLD}Welcome to cligate{RESET} {DIM}v{ver}{RESET}"),
        ));
        lines.push(boxed(0, String::new()));
        let logo = self.logo_lines();
        let logo_w = logo_rows().first().map(|r| r.len()).unwrap_or(0);
        if inner >= logo_w + 4 {
            let pad_left = (inner - logo_w) / 2;
            for l in logo {
                lines.push(boxed(
                    pad_left + logo_w,
                    format!("{}{l}", " ".repeat(pad_left)),
                ));
            }
            lines.push(boxed(0, String::new()));
        }
        let (prov, bare) = infer_provider_and_model(&self.model);
        let info = [
            ("model", format!("{prov}/{bare} · {}", self.effort)),
            ("perms", self.perm_mode.as_str().to_string()),
            ("cwd", self.cwd.clone()),
        ];
        for (label, value) in info {
            let v = clip(&value, inner.saturating_sub(10));
            lines.push(boxed(
                9 + v.chars().count(),
                format!("  {GRAY}{label:<6}{RESET} {v}"),
            ));
        }
        lines.push(boxed(0, String::new()));
        let hint = clip(
            "  /help shortcuts · /model switch · /resume sessions",
            inner,
        );
        lines.push(boxed(hint.chars().count(), format!("{DIM}{hint}{RESET}")));
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
        if self.last_ctrl_c.is_some() {
            let msg = "Press Ctrl+C again to exit";
            let msg_len = msg.chars().count();
            if msg_len + 3 >= max_w {
                return format!(
                    "{ACCENT}{BOLD} {}{RESET}",
                    clip(msg, max_w.saturating_sub(1))
                );
            }
            let right_room = max_w.saturating_sub(msg_len + 3);
            let right_part = clip(&right_plain, right_room);
            let gap = max_w.saturating_sub(msg_len + 1 + right_part.chars().count());
            return format!(
                "{ACCENT}{BOLD} {msg}{RESET}{}{GRAY}{right_part}{RESET}",
                " ".repeat(gap)
            );
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
            "/help for shortcuts, ctrl+c twice to quit".to_string()
        };
        let left_room = max_w.saturating_sub(right_len + 2);
        let left = clip(&left_text, left_room);
        let gap = max_w.saturating_sub(left.chars().count() + right_len + 1);
        let left_style = if self.last_ctrl_c.is_some() {
            format!("{ACCENT}{BOLD} {left}{RESET}")
        } else {
            format!("{DIM} {left}{RESET}")
        };
        format!("{left_style}{}{right_styled}", " ".repeat(gap))
    }

    pub(super) fn render(&mut self) {
        let (rows, cols) = self.size;
        if self.standalone_resume {
            let mut buf = String::with_capacity(rows * cols);
            buf.push_str("\x1b[?2026h\x1b[?25l\x1b[2J");
            let modal = self.modal.clone();
            self.draw_modal(&mut buf, &modal, rows, cols);
            buf.push_str("\x1b[?2026l");
            let mut out = io::stdout();
            let _ = out.write_all(buf.as_bytes());
            let _ = out.flush();
            return;
        }
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
        let win = sel
            .saturating_sub(sugg_rows.saturating_sub(1))
            .min(suggestions.len().saturating_sub(sugg_rows));
        for (slot, (cmd, desc)) in suggestions.iter().skip(win).take(sugg_rows).enumerate() {
            let idx = win + slot;
            let plain = pad_or_clip(&format!("{cmd:<13} {desc}"), cols.saturating_sub(5));
            let row = chat_h + 5 + slot;
            if idx == sel {
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
            Modal::Skills { selected } => {
                let skills = list_skills();
                let width = cols.saturating_sub(8).clamp(50, 82);
                let height = rows.saturating_sub(6).clamp(8, 18);
                let list_h = height - 2;
                let sel = (*selected).min(skills.len().saturating_sub(1));
                let win = sel
                    .saturating_sub(list_h / 2)
                    .min(skills.len().saturating_sub(list_h));
                let mut items = Vec::new();
                if skills.is_empty() {
                    items.push((
                        "no skills found in ~/.local/share/cligate/skills or ~/.agents/skills"
                            .to_string(),
                        false,
                    ));
                }
                for (i, s) in skills.iter().enumerate().skip(win).take(list_h) {
                    let active = if self.active_skills.iter().any(|a| a == &s.name) {
                        "[on] "
                    } else {
                        "     "
                    };
                    let line = format!("{active}{:<18} {}", clip(&s.name, 18), s.description);
                    items.push((line, i == sel));
                }
                draw_box(
                    buf,
                    rows,
                    cols,
                    width,
                    height,
                    "Local Skills (Enter toggle · ←/Esc close)",
                    &items,
                );
            }
            Modal::Usage { rows: usage_rows } => {
                let width = cols.saturating_sub(6).clamp(58, 84);
                let mut items: Vec<(String, bool)> = Vec::new();
                let ctx_used = estimate_history_tokens(&self.history);
                let ctx_max = if self.auto_compact_tokens > 0 {
                    self.auto_compact_tokens
                } else {
                    200_000
                };
                let ctx_pct = ((ctx_used.saturating_mul(100)) / ctx_max.max(1)).min(100);
                items.push(("Context Window".to_string(), false));
                items.push((
                    format!(
                        "{} {:>3}%  ~{} / {} tokens",
                        progress_bar(ctx_pct, 20),
                        ctx_pct,
                        format_tokens_short(ctx_used),
                        format_tokens_short(ctx_max),
                    ),
                    false,
                ));
                items.push((String::new(), false));
                for r in usage_rows {
                    let avail_pct = if r.available {
                        100
                    } else {
                        let tot = r.cooldown_total.max(1);
                        let done = tot.saturating_sub(r.cooldown_rem);
                        ((done * 100) / tot).min(99) as usize
                    };
                    let state_str = if r.available {
                        "ready".to_string()
                    } else {
                        format!("limited · retry in {}", format_dur(r.cooldown_rem))
                    };
                    let tot_tok = r.input_tokens + r.output_tokens;
                    let cache_part = if r.cached_tokens > 0 {
                        let denom = (r.input_tokens + r.cached_tokens).max(1);
                        let c_pct = ((r.cached_tokens * 100) / denom).min(100);
                        format!(" · cache {c_pct}%")
                    } else {
                        String::new()
                    };
                    items.push((format!("{:<8} ({state_str})", r.name), false));
                    items.push((
                        format!(
                            "{} {:>3}%  {} calls · {} tok{cache_part}",
                            progress_bar(avail_pct, 20),
                            avail_pct,
                            r.calls,
                            format_tokens_short(tot_tok as usize),
                        ),
                        false,
                    ));
                    if !r.available && !r.last_error.is_empty() {
                        items.push((
                            format!("  limit: {}", clip(&r.last_error, width.saturating_sub(14))),
                            false,
                        ));
                    }
                    items.push((String::new(), false));
                }
                items.push((
                    "Server-side account quota % is not exposed by CLI tools;".to_string(),
                    false,
                ));
                items.push((
                    "bars reflect live rate-limit availability & cooldown recovery.".to_string(),
                    false,
                ));
                items.push(("r refresh · ← / Esc close".to_string(), false));
                let height = (items.len() + 2).min(rows.saturating_sub(2));
                draw_box(
                    buf,
                    rows,
                    cols,
                    width,
                    height,
                    "Provider Quota & Limits (/usage)",
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
                    let short_id = clip(&s.id, 14);
                    let line = format!("{short_id:<14} {:<18} {}", clip(&s.model, 18), s.title);
                    items.push((line, i == sel));
                }
                draw_box(buf, rows, cols, width, height, "Resume session", &items);
            }
        }
    }
}
