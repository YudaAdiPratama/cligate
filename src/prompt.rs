use crate::config::{fnv1a_hex, gen_hex_id};
use crate::json::{parse_json, Json};

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

pub(crate) fn msg_fingerprint(msg: &Json) -> String {
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

pub(crate) fn compute_header_key(
    model: &str,
    system_msgs: &[&Json],
    tools: Option<&[Json]>,
) -> String {
    let sys_fps: Vec<String> = system_msgs.iter().map(|m| msg_fingerprint(m)).collect();
    let tools_dump = tools
        .map(|t| Json::Arr(t.to_vec()).dump())
        .unwrap_or_default();
    fnv1a_hex(&format!("{model}|{}|{tools_dump}", sys_fps.join(",")))
}

pub(crate) fn build_prompt(messages: &[Json], tools: Option<&[Json]>, max_bytes: usize) -> String {
    let has_tools = tools.map(|t| !t.is_empty()).unwrap_or(false);
    let preamble = if has_tools {
        "Answer as the assistant. Do NOT invoke your own host CLI built-in sandbox tools directly; instead, whenever you need to inspect files, search code, edit files, or run commands, emit <tool_call> XML blocks using the caller tools defined below."
    } else {
        "Answer directly as the assistant. Do NOT invoke any internal CLI tools; rely strictly on the conversation messages below."
    };
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
        if idx >= 6
            && msg.get("role").and_then(|v| v.as_str()) == Some("tool")
            && formatted.len() > 12_000
        {
            formatted = clip_head_tail(
                &formatted,
                12_000,
                "\n... [older tool output trimmed] ...\n",
            );
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

pub(crate) fn build_delta_prompt(delta_msgs: &[&Json], max_bytes: usize) -> String {
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

pub(crate) fn default_usage_json(prompt_tok: i64, comp_tok: i64, cached_tok: i64) -> Json {
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
