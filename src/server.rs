use crate::catalog::{all_known_models, extract_effort, resolve_agy_model};
use crate::config::{gen_hex_id, now_sec, Config};
use crate::json::{parse_json, Json};
use crate::prompt::parse_tool_calls;
use crate::providers::{StandbyModelPool, StickySessionManager};
use crate::router::{load_agent_routes, ProviderRouter};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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

        if path == "/v1/router/fallback" {
            let payload = Json::obj(vec![
                ("fallback_enabled", Json::Bool(router.is_fallback_enabled())),
                ("default_model", Json::Str(cfg.default_model.clone())),
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

        if path == "/v1/router/fallback" {
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
            let Some(enabled) = parsed.get("enabled").and_then(|v| v.as_bool()) else {
                send_json_response(
                    &mut stream,
                    400,
                    "Bad Request",
                    &Json::obj(vec![(
                        "error",
                        Json::obj(vec![(
                            "message",
                            Json::Str("Field 'enabled' (boolean) is required".to_string()),
                        )]),
                    )]),
                );
                return;
            };
            match router.set_fallback_enabled(enabled) {
                Ok(()) => {
                    let status = router.status();
                    send_json_response(&mut stream, 200, "OK", &status);
                }
                Err(e) => {
                    send_json_response(
                        &mut stream,
                        500,
                        "Internal Server Error",
                        &Json::obj(vec![("error", Json::obj(vec![("message", Json::Str(e))]))]),
                    );
                }
            }
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
        let default_for_turn = if router.is_fallback_enabled() {
            router
                .get_agent_config(agent_name.as_deref())
                .and_then(|(_, chain)| chain.into_iter().next())
                .unwrap_or_else(|| cfg.default_model.clone())
        } else {
            cfg.default_model.clone()
        };
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

pub(crate) fn run_server(
    mut cfg: Config,
    host_override: Option<String>,
    port_override: Option<u16>,
    fallback_override: Option<bool>,
) {
    if let Some(h) = host_override {
        cfg.host = h;
    }
    if let Some(p) = port_override {
        cfg.port = p;
    }
    if let Some(fb) = fallback_override {
        cfg.fallback_enabled = Some(fb);
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
        "[cligate] Listening on http://{bind_addr}/v1 (default_model={}, fallback={}, max_prompt_bytes={})",
        cfg.default_model,
        if router.is_fallback_enabled() { "on" } else { "off" },
        cfg.max_prompt_bytes
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

pub(crate) fn http_call(
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
