use super::cache::CliSessionCache;
use crate::catalog::{is_rate_limit_message, resolve_codex_model_and_effort, TurnError};
use crate::config::{is_cancelled, ActivePidGuard, Config};
use crate::json::{parse_json, Json};
use crate::prompt::{
    build_delta_prompt, build_prompt, compute_header_key, default_usage_json, msg_fingerprint,
};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn run_codex_once(
    cfg: &Config,
    model: &str,
    resolved_effort: &str,
    prompt: &str,
    resume_id: Option<&str>,
    on_delta: &mut Option<&mut dyn FnMut(&str)>,
) -> Result<(String, Json, String, Option<String>, i64), TurnError> {
    let _ = fs::create_dir_all(&cfg.workdir);
    let effort_cfg = format!("model_reasoning_effort=\"{resolved_effort}\"");
    let mut args = vec!["exec".to_string()];
    if let Some(tid) = resume_id {
        args.extend([
            "resume".to_string(),
            "--skip-git-repo-check".to_string(),
            "--ignore-user-config".to_string(),
            "--json".to_string(),
            "-m".to_string(),
            model.to_string(),
            "-c".to_string(),
            effort_cfg,
            "-c".to_string(),
            "sandbox_mode=\"read-only\"".to_string(),
            tid.to_string(),
            "-".to_string(),
        ]);
    } else {
        args.extend([
            "--skip-git-repo-check".to_string(),
            "--ignore-user-config".to_string(),
            "-s".to_string(),
            "read-only".to_string(),
            "--json".to_string(),
            "-m".to_string(),
            model.to_string(),
            "-c".to_string(),
            effort_cfg,
            "-".to_string(),
        ]);
    }
    let mut child = Command::new(&cfg.codex_bin)
        .args(&args)
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
    let mut captured_tid: Option<String> = resume_id.map(|s| s.to_string());
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
            Some("thread.started") => {
                if let Some(tid) = evt.get("thread_id").and_then(|v| v.as_str()) {
                    if !tid.is_empty() {
                        captured_tid = Some(tid.to_string());
                    }
                }
            }
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
        captured_tid,
        cached_tok,
    ))
}

pub(crate) fn execute_codex_turn(
    cfg: &Config,
    cli_cache: &CliSessionCache,
    messages: &[Json],
    tools: Option<&[Json]>,
    raw_model: &str,
    effort: &str,
    on_delta: &mut Option<&mut dyn FnMut(&str)>,
) -> Result<(String, Json, String), TurnError> {
    let (model, resolved_effort) = resolve_codex_model_and_effort(raw_model, effort);
    let system_msgs: Vec<&Json> = messages
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"))
        .collect();
    let chat_msgs: Vec<&Json> = messages
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) != Some("system"))
        .collect();
    let header_key = compute_header_key(&format!("{model}:{resolved_effort}"), &system_msgs, tools);
    let chat_hashes: Vec<String> = chat_msgs.iter().map(|m| msg_fingerprint(m)).collect();

    if let Some((tid, prefix_len)) = cli_cache.find_match("codex", &header_key, &chat_hashes) {
        let delta_prompt = build_delta_prompt(&chat_msgs[prefix_len..], cfg.max_prompt_bytes);
        match run_codex_once(
            cfg,
            &model,
            &resolved_effort,
            &delta_prompt,
            Some(&tid),
            on_delta,
        ) {
            Ok((out, usage, resolved, new_tid, cached_tok)) => {
                cli_cache.record(
                    "codex",
                    &model,
                    &header_key,
                    new_tid.as_deref().unwrap_or(&tid),
                    chat_hashes,
                    true,
                    cached_tok,
                );
                return Ok((out, usage, resolved));
            }
            Err(TurnError::Runtime(_)) => {
                cli_cache.invalidate("codex", &tid);
            }
            Err(other) => return Err(other),
        }
    }

    let prompt = build_prompt(messages, tools, cfg.max_prompt_bytes);
    let (out, usage, resolved, new_tid, cached_tok) =
        run_codex_once(cfg, &model, &resolved_effort, &prompt, None, on_delta)?;
    cli_cache.record(
        "codex",
        &model,
        &header_key,
        new_tid.as_deref().unwrap_or(""),
        chat_hashes,
        false,
        cached_tok,
    );
    Ok((out, usage, resolved))
}
