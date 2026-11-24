use super::cache::CliSessionCache;
use crate::catalog::{is_rate_limit_message, resolve_claude_model_and_effort, TurnError};
use crate::config::{is_cancelled, ActivePidGuard, Config};
use crate::json::{parse_json, Json};
use crate::prompt::{
    build_delta_prompt, build_prompt, compute_header_key, default_usage_json, msg_fingerprint,
};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn run_claude_once(
    cfg: &Config,
    model: &str,
    resolved_effort: &str,
    prompt: &str,
    resume_id: Option<&str>,
    on_delta: &mut Option<&mut dyn FnMut(&str)>,
) -> Result<(String, Json, String, Option<String>, i64), TurnError> {
    let _ = fs::create_dir_all(&cfg.workdir);
    let mut args = vec![
        "-p".to_string(),
        "--setting-sources".to_string(),
        String::new(),
        "--tools".to_string(),
        String::new(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--include-partial-messages".to_string(),
        "--model".to_string(),
        model.to_string(),
        "--effort".to_string(),
        resolved_effort.to_string(),
    ];
    if let Some(sid) = resume_id {
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }
    let mut child = Command::new(&cfg.claude_bin)
        .args(&args)
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
    let mut captured_sid: Option<String> = resume_id.map(|s| s.to_string());
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
        if let Some(sid) = evt.get("session_id").and_then(|v| v.as_str()) {
            if !sid.is_empty() {
                captured_sid = Some(sid.to_string());
            }
        }
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
        captured_sid,
        cached_tok,
    ))
}

pub(crate) fn execute_claude_turn(
    cfg: &Config,
    cli_cache: &CliSessionCache,
    messages: &[Json],
    tools: Option<&[Json]>,
    raw_model: &str,
    effort: &str,
    on_delta: &mut Option<&mut dyn FnMut(&str)>,
) -> Result<(String, Json, String), TurnError> {
    let (model, resolved_effort) = resolve_claude_model_and_effort(raw_model, effort);
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

    if let Some((sid, prefix_len)) = cli_cache.find_match("claude", &header_key, &chat_hashes) {
        let delta_prompt = build_delta_prompt(&chat_msgs[prefix_len..], cfg.max_prompt_bytes);
        match run_claude_once(
            cfg,
            &model,
            &resolved_effort,
            &delta_prompt,
            Some(&sid),
            on_delta,
        ) {
            Ok((out, usage, resolved, new_sid, cached_tok)) => {
                cli_cache.record(
                    "claude",
                    &model,
                    &header_key,
                    new_sid.as_deref().unwrap_or(&sid),
                    chat_hashes,
                    true,
                    cached_tok,
                );
                return Ok((out, usage, resolved));
            }
            Err(TurnError::Runtime(_)) => {
                cli_cache.invalidate("claude", &sid);
            }
            Err(other) => return Err(other),
        }
    }

    let prompt = build_prompt(messages, tools, cfg.max_prompt_bytes);
    let (out, usage, resolved, new_sid, cached_tok) =
        run_claude_once(cfg, &model, &resolved_effort, &prompt, None, on_delta)?;
    cli_cache.record(
        "claude",
        &model,
        &header_key,
        new_sid.as_deref().unwrap_or(""),
        chat_hashes,
        false,
        cached_tok,
    );
    Ok((out, usage, resolved))
}
