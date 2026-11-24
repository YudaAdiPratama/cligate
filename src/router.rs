use crate::catalog::{
    detect_primary_provider, is_rate_limit_message, map_fallback_model, resolve_agy_model,
    TurnError,
};
use crate::config::{is_cancelled, now_f64, now_sec, set_progress, Config};
use crate::json::{parse_json, Json};
use crate::providers::{
    execute_claude_turn, execute_codex_turn, CliSessionCache, StickySessionManager,
};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub(crate) fn load_agent_routes(path: &str) -> Json {
    if let Ok(raw) = fs::read_to_string(path) {
        if let Ok(parsed) = parse_json(&raw) {
            if parsed.as_obj().is_some() {
                return parsed;
            }
        }
    }
    Json::obj(vec![("agents", Json::Obj(Vec::new()))])
}

pub(crate) fn save_agent_routes(path: &str, data: &Json) -> Result<(), String> {
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
    fallback_enabled: bool,
    fallback_order: Vec<String>,
    cooldown_until: HashMap<String, f64>,
    last_error: HashMap<String, String>,
    provider_calls: HashMap<String, u64>,
    provider_input_tokens: HashMap<String, i64>,
    provider_output_tokens: HashMap<String, i64>,
    provider_cached_tokens: HashMap<String, i64>,
    fallback_events: Vec<Json>,
}

pub struct ProviderRouter {
    cfg: Config,
    sticky: Arc<StickySessionManager>,
    cli_cache: Arc<CliSessionCache>,
    state: Mutex<RouterState>,
}

impl ProviderRouter {
    pub(crate) fn new(cfg: Config, sticky: Arc<StickySessionManager>) -> Self {
        let mut cooldown_until = HashMap::new();
        let mut last_error = HashMap::new();
        let mut provider_calls = HashMap::new();
        let mut provider_input_tokens = HashMap::new();
        let mut provider_output_tokens = HashMap::new();
        let mut provider_cached_tokens = HashMap::new();
        for p in ["claude", "codex", "agy"] {
            cooldown_until.insert(p.to_string(), 0.0);
            last_error.insert(p.to_string(), String::new());
            provider_calls.insert(p.to_string(), 0);
            provider_input_tokens.insert(p.to_string(), 0);
            provider_output_tokens.insert(p.to_string(), 0);
            provider_cached_tokens.insert(p.to_string(), 0);
        }
        let routes = load_agent_routes(&cfg.routes_path);
        let fallback_enabled = cfg.fallback_enabled.unwrap_or_else(|| {
            routes
                .get("fallback_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
        });
        let cli_cache = Arc::new(CliSessionCache::new(&cfg));
        Self {
            cfg,
            sticky,
            cli_cache,
            state: Mutex::new(RouterState {
                fallback_enabled,
                fallback_order: vec!["claude".to_string(), "codex".to_string(), "agy".to_string()],
                cooldown_until,
                last_error,
                provider_calls,
                provider_input_tokens,
                provider_output_tokens,
                provider_cached_tokens,
                fallback_events: Vec::new(),
            }),
        }
    }

    pub fn is_fallback_enabled(&self) -> bool {
        self.state.lock().unwrap().fallback_enabled
    }

    pub fn set_fallback_enabled(&self, enabled: bool) -> Result<(), String> {
        let mut st = self.state.lock().unwrap();
        st.fallback_enabled = enabled;
        let mut routes = load_agent_routes(&self.cfg.routes_path);
        routes.set("fallback_enabled", Json::Bool(enabled));
        save_agent_routes(&self.cfg.routes_path, &routes)
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

    pub(crate) fn get_agent_config(
        &self,
        agent_name: Option<&str>,
    ) -> Option<(String, Vec<String>)> {
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

    #[cfg(test)]
    pub fn build_target_candidates(
        &self,
        requested_model: &str,
        agent_name: Option<&str>,
    ) -> Vec<(String, String)> {
        self.build_target_candidates_with_chain(requested_model, agent_name, None)
    }

    pub fn build_target_candidates_with_chain(
        &self,
        requested_model: &str,
        agent_name: Option<&str>,
        explicit_chain: Option<&[String]>,
    ) -> Vec<(String, String)> {
        let (primary_provider, bare_req) =
            detect_primary_provider(requested_model, &self.cfg.default_model);
        let fallback_on = self.is_fallback_enabled();
        if explicit_chain.is_none() && !fallback_on {
            return vec![(primary_provider, bare_req)];
        }
        let mut raw_targets: Vec<(String, String)> = Vec::new();

        if let Some(chain) = explicit_chain {
            if !chain.is_empty() {
                let (_, first_bare) = detect_primary_provider(&chain[0], &self.cfg.default_model);
                if !matches!(requested_model, "" | "auto" | "default") && bare_req != first_bare {
                    raw_targets.push((primary_provider.clone(), bare_req.clone()));
                }
                for item in chain {
                    let (p, b) = detect_primary_provider(item, &self.cfg.default_model);
                    if !raw_targets.contains(&(p.clone(), b.clone())) {
                        raw_targets.push((p, b));
                    }
                }
            }
        } else if let Some((_, chain)) = self.get_agent_config(agent_name) {
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

    pub(crate) fn execute_turn(
        &self,
        messages: &[Json],
        tools: Option<&[Json]>,
        requested_model: &str,
        effort: String,
        agent_name: Option<&str>,
        on_delta: &mut Option<&mut dyn FnMut(&str)>,
    ) -> Result<(String, Json, String, String), String> {
        self.execute_turn_with_chain(
            messages,
            tools,
            requested_model,
            effort,
            agent_name,
            None,
            on_delta,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_turn_with_chain(
        &self,
        messages: &[Json],
        tools: Option<&[Json]>,
        requested_model: &str,
        mut effort: String,
        agent_name: Option<&str>,
        explicit_chain: Option<&[String]>,
        on_delta: &mut Option<&mut dyn FnMut(&str)>,
    ) -> Result<(String, Json, String, String), String> {
        if (effort.is_empty() || effort == "medium") && agent_name.is_some() {
            if let Some((ag_eff, _)) = self.get_agent_config(agent_name) {
                effort = ag_eff;
            }
        }
        let candidates =
            self.build_target_candidates_with_chain(requested_model, agent_name, explicit_chain);
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
                "codex" => execute_codex_turn(
                    &self.cfg,
                    &self.cli_cache,
                    messages,
                    tools,
                    target_model,
                    &effort,
                    on_delta,
                ),
                "claude" => execute_claude_turn(
                    &self.cfg,
                    &self.cli_cache,
                    messages,
                    tools,
                    target_model,
                    &effort,
                    on_delta,
                ),
                _ => continue,
            };

            match res {
                Ok((out, usage, resolved)) => {
                    let mut st = self.state.lock().unwrap();
                    *st.provider_calls.entry(provider.clone()).or_insert(0) += 1;
                    let pt = usage
                        .get("prompt_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let ct = usage
                        .get("completion_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let ca = usage
                        .get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    *st.provider_input_tokens
                        .entry(provider.clone())
                        .or_insert(0) += pt;
                    *st.provider_output_tokens
                        .entry(provider.clone())
                        .or_insert(0) += ct;
                    *st.provider_cached_tokens
                        .entry(provider.clone())
                        .or_insert(0) += ca;
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

    pub fn status(&self) -> Json {
        let now = now_f64();
        let routes = load_agent_routes(&self.cfg.routes_path);
        let st = self.state.lock().unwrap();
        let mut provs = Vec::new();
        for p in ["claude", "codex", "agy"] {
            let until = st.cooldown_until.get(p).copied().unwrap_or(0.0);
            let rem = ((until - now).max(0.0) * 10.0).round() / 10.0;
            let calls = st.provider_calls.get(p).copied().unwrap_or(0);
            let in_tok = st.provider_input_tokens.get(p).copied().unwrap_or(0);
            let out_tok = st.provider_output_tokens.get(p).copied().unwrap_or(0);
            let cached_tok = st.provider_cached_tokens.get(p).copied().unwrap_or(0);
            let last_err = st.last_error.get(p).cloned().unwrap_or_default();
            provs.push((
                p.to_string(),
                Json::obj(vec![
                    ("available", Json::Bool(rem == 0.0)),
                    ("cooldown_remaining_sec", Json::Num(rem)),
                    ("cooldown_sec", Json::Num(self.cfg.cooldown_sec)),
                    ("calls", Json::Num(calls as f64)),
                    ("input_tokens", Json::Num(in_tok as f64)),
                    ("output_tokens", Json::Num(out_tok as f64)),
                    ("cached_tokens", Json::Num(cached_tok as f64)),
                    ("last_limit_error", Json::Str(last_err)),
                ]),
            ));
        }
        let recent_start = st.fallback_events.len().saturating_sub(10);
        let recent = st.fallback_events[recent_start..].to_vec();
        Json::obj(vec![
            ("fallback_enabled", Json::Bool(st.fallback_enabled)),
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
            ("cli_session_cache", self.cli_cache.stats()),
        ])
    }
}
