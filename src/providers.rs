use crate::catalog::{
    is_rate_limit_message, resolve_claude_model_and_effort, resolve_codex_model_and_effort,
    TurnError,
};
use crate::config::{gen_hex_id, is_cancelled, now_f64, ActivePidGuard, Config};
use crate::json::{parse_json, Json};
use crate::prompt::{
    build_delta_prompt, build_prompt, compute_header_key, default_usage_json, msg_fingerprint,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

struct AgyWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl AgyWorker {
    fn spawn_initialized(cfg: &Config, model: &str) -> Result<Self, TurnError> {
        let _ = fs::create_dir_all(&cfg.workdir);
        let mut child = Command::new(&cfg.agy_bin)
            .args([
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "-p=",
                "--model",
                model,
                "--disable-slash-commands",
            ])
            .current_dir(&cfg.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| TurnError::Runtime(format!("failed to spawn agy: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TurnError::Runtime("missing agy stdin".to_string()))?;
        let stdout_raw = child
            .stdout
            .take()
            .ok_or_else(|| TurnError::Runtime("missing agy stdout".to_string()))?;
        let mut stdout = BufReader::new(stdout_raw);
        let mut line = String::new();

        loop {
            line.clear();
            let n = stdout
                .read_line(&mut line)
                .map_err(|e| TurnError::Runtime(e.to_string()))?;
            if n == 0 {
                if is_cancelled() {
                    return Err(TurnError::Cancelled);
                }
                let mut err_str = String::new();
                if let Some(mut se) = child.stderr.take() {
                    let _ = se.read_to_string(&mut err_str);
                }
                let _ = child.kill();
                let _ = child.wait();
                if is_rate_limit_message(&err_str) {
                    return Err(TurnError::RateLimit(err_str.trim().to_string()));
                }
                return Err(TurnError::Runtime(format!(
                    "agy exited before init: {}",
                    err_str.trim()
                )));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(evt) = parse_json(trimmed) {
                if evt.get("event").and_then(|v| v.as_str()) == Some("init") {
                    return Ok(Self {
                        child,
                        stdin,
                        stdout,
                    });
                }
            }
        }
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn close(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn send_turn(
        &mut self,
        prompt: &str,
        on_delta: &mut Option<&mut dyn FnMut(&str)>,
    ) -> Result<(String, Json, i64), TurnError> {
        if is_cancelled() {
            return Err(TurnError::Cancelled);
        }
        let _pid_guard = ActivePidGuard::set(self.child.id());
        let payload = Json::obj(vec![
            ("event", Json::Str("user".to_string())),
            (
                "message",
                Json::obj(vec![("content", Json::Str(prompt.to_string()))]),
            ),
        ])
        .dump();
        self.stdin
            .write_all(format!("{payload}\n").as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| TurnError::Runtime(format!("agy stdin write failed: {e}")))?;

        let mut collected = String::new();
        let mut prompt_tok = 0i64;
        let mut comp_tok = 0i64;
        let mut cached_tok = 0i64;
        let mut line = String::new();

        loop {
            line.clear();
            let n = self
                .stdout
                .read_line(&mut line)
                .map_err(|e| TurnError::Runtime(e.to_string()))?;
            if n == 0 {
                if is_cancelled() {
                    return Err(TurnError::Cancelled);
                }
                let mut err_str = String::new();
                if let Some(mut se) = self.child.stderr.take() {
                    let _ = se.read_to_string(&mut err_str);
                }
                if is_rate_limit_message(&err_str) {
                    return Err(TurnError::RateLimit(err_str.trim().to_string()));
                }
                return Err(TurnError::Runtime(format!(
                    "agy stdout closed unexpectedly: {}",
                    err_str.trim()
                )));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(evt) = parse_json(trimmed) else {
                continue;
            };
            match evt.get("event").and_then(|v| v.as_str()) {
                Some("step_update") => {
                    if let Some(su) = evt.get("step_update") {
                        if su.get("step_type").and_then(|v| v.as_str()) == Some("agent_response") {
                            if let Some(delta) = su.get("text_delta").and_then(|v| v.as_str()) {
                                if !delta.is_empty() {
                                    collected.push_str(delta);
                                    if let Some(cb) = on_delta.as_mut() {
                                        cb(delta);
                                    }
                                }
                            }
                            if let Some(u) = su.get("usage") {
                                prompt_tok =
                                    u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                comp_tok =
                                    u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                cached_tok = u
                                    .get("cache_read_tokens")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0);
                            }
                        }
                    }
                }
                Some("result") => {
                    if let Some(res) = evt.get("result") {
                        if let Some(u) = res.get("usage") {
                            prompt_tok =
                                u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                            comp_tok = u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                            cached_tok = u
                                .get("cache_read_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                        }
                        if res.get("status").and_then(|v| v.as_str()) == Some("ERROR") {
                            let err_msg = res
                                .get("error")
                                .and_then(|v| v.as_str())
                                .unwrap_or("agy stream-json turn error")
                                .to_string();
                            if is_rate_limit_message(&err_msg) {
                                return Err(TurnError::RateLimit(err_msg));
                            }
                            return Err(TurnError::Runtime(err_msg));
                        }
                        if collected.is_empty() {
                            if let Some(resp_txt) = res.get("response").and_then(|v| v.as_str()) {
                                collected = resp_txt.to_string();
                            }
                        }
                        return Ok((
                            collected.trim().to_string(),
                            default_usage_json(prompt_tok, comp_tok, cached_tok),
                            cached_tok,
                        ));
                    }
                }
                _ => {}
            }
        }
    }
}

impl Drop for AgyWorker {
    fn drop(&mut self) {
        self.close();
    }
}

struct StandbyPoolInner {
    standby: HashMap<String, AgyWorker>,
    warming: HashSet<String>,
}

pub(crate) struct StandbyModelPool {
    cfg: Config,
    inner: Arc<Mutex<StandbyPoolInner>>,
}

impl StandbyModelPool {
    pub(crate) fn new(cfg: Config) -> Self {
        Self {
            cfg,
            inner: Arc::new(Mutex::new(StandbyPoolInner {
                standby: HashMap::new(),
                warming: HashSet::new(),
            })),
        }
    }

    pub(crate) fn prewarm_async(&self, model: &str) {
        {
            let mut guard = self.inner.lock().unwrap();
            if let Some(w) = guard.standby.get_mut(model) {
                if w.is_alive() {
                    return;
                }
            }
            if guard.warming.contains(model) {
                return;
            }
            guard.warming.insert(model.to_string());
        }
        let cfg = self.cfg.clone();
        let inner = Arc::clone(&self.inner);
        let model_owned = model.to_string();
        thread::spawn(move || {
            let spawned = AgyWorker::spawn_initialized(&cfg, &model_owned);
            let mut guard = inner.lock().unwrap();
            guard.warming.remove(&model_owned);
            if let Ok(worker) = spawned {
                let replace = match guard.standby.get_mut(&model_owned) {
                    Some(existing) => !existing.is_alive(),
                    None => true,
                };
                if replace {
                    guard.standby.insert(model_owned, worker);
                }
            }
        });
    }

    fn acquire(&self, model: &str) -> Result<AgyWorker, TurnError> {
        let mut existing = None;
        {
            let mut guard = self.inner.lock().unwrap();
            if let Some(mut cand) = guard.standby.remove(model) {
                if cand.is_alive() {
                    existing = Some(cand);
                }
            }
        }
        self.prewarm_async(model);
        if let Some(w) = existing {
            return Ok(w);
        }
        AgyWorker::spawn_initialized(&self.cfg, model)
    }

    pub(crate) fn status(&self) -> Json {
        let mut guard = self.inner.lock().unwrap();
        let mut ready = Vec::new();
        for (m, w) in guard.standby.iter_mut() {
            if w.is_alive() {
                ready.push(Json::Str(m.clone()));
            }
        }
        let warming: Vec<Json> = guard.warming.iter().cloned().map(Json::Str).collect();
        Json::obj(vec![
            ("standby_ready", Json::Arr(ready)),
            ("warming", Json::Arr(warming)),
        ])
    }
}

struct StickySession {
    id: String,
    model: String,
    header_key: String,
    worker: AgyWorker,
    msg_hashes: Vec<String>,
    last_used: f64,
    turns: u64,
}

struct StickyPoolState {
    sessions: Vec<Arc<Mutex<StickySession>>>,
    cache_hits: u64,
    cache_misses: u64,
    total_cached_tokens: i64,
}

pub(crate) struct StickySessionManager {
    cfg: Config,
    standby: Arc<StandbyModelPool>,
    state: Mutex<StickyPoolState>,
}

impl StickySessionManager {
    pub(crate) fn new(cfg: Config, standby: Arc<StandbyModelPool>) -> Self {
        Self {
            cfg,
            standby,
            state: Mutex::new(StickyPoolState {
                sessions: Vec::new(),
                cache_hits: 0,
                cache_misses: 0,
                total_cached_tokens: 0,
            }),
        }
    }

    fn evict_expired_locked(&self, st: &mut StickyPoolState) {
        let now = now_f64();
        let ttl = self.cfg.sticky_ttl_sec;
        st.sessions.retain(|arc_s| {
            if let Ok(mut s) = arc_s.try_lock() {
                s.worker.is_alive() && (now - s.last_used <= ttl)
            } else {
                true
            }
        });
        while st.sessions.len() > self.cfg.max_sticky_sessions {
            st.sessions.remove(0);
        }
    }

    pub(crate) fn run_cached_turn(
        &self,
        model: &str,
        messages: &[Json],
        tools: Option<&[Json]>,
        on_delta: &mut Option<&mut dyn FnMut(&str)>,
    ) -> Result<(String, Json), TurnError> {
        let system_msgs: Vec<&Json> = messages
            .iter()
            .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"))
            .collect();
        let chat_msgs: Vec<&Json> = messages
            .iter()
            .filter(|m| m.get("role").and_then(|v| v.as_str()) != Some("system"))
            .collect();
        let header_key = compute_header_key(model, &system_msgs, tools);
        let chat_hashes: Vec<String> = chat_msgs.iter().map(|m| msg_fingerprint(m)).collect();

        let mut matched: Option<(Arc<Mutex<StickySession>>, usize)> = None;
        {
            let mut st = self.state.lock().unwrap();
            self.evict_expired_locked(&mut st);
            let mut best_len = 0usize;
            for arc_s in &st.sessions {
                if let Ok(mut s) = arc_s.try_lock() {
                    if s.header_key != header_key || !s.worker.is_alive() {
                        continue;
                    }
                    let n = s.msg_hashes.len();
                    if n > 0
                        && chat_hashes.len() > n
                        && chat_hashes[..n] == s.msg_hashes[..]
                        && n > best_len
                    {
                        best_len = n;
                        matched = Some((Arc::clone(arc_s), n));
                    }
                }
            }
            if matched.is_some() {
                st.cache_hits += 1;
            } else {
                st.cache_misses += 1;
            }
        }

        let (session_arc, is_hit, prefix_len) = if let Some((arc_s, plen)) = matched {
            (arc_s, true, plen)
        } else {
            let worker = self.standby.acquire(model)?;
            let new_sess = Arc::new(Mutex::new(StickySession {
                id: gen_hex_id("")[..12].to_string(),
                model: model.to_string(),
                header_key,
                worker,
                msg_hashes: Vec::new(),
                last_used: now_f64(),
                turns: 0,
            }));
            let mut st = self.state.lock().unwrap();
            st.sessions.push(Arc::clone(&new_sess));
            self.evict_expired_locked(&mut st);
            (new_sess, false, 0)
        };

        let prompt = if is_hit {
            build_delta_prompt(&chat_msgs[prefix_len..], self.cfg.max_prompt_bytes)
        } else {
            build_prompt(messages, tools, self.cfg.max_prompt_bytes)
        };

        let mut sess = session_arc.lock().unwrap();
        match sess.worker.send_turn(&prompt, on_delta) {
            Ok((out, usage, cached_tok)) => {
                sess.msg_hashes = chat_hashes;
                sess.turns += 1;
                sess.last_used = now_f64();
                if cached_tok > 0 {
                    let mut st = self.state.lock().unwrap();
                    st.total_cached_tokens += cached_tok;
                }
                Ok((out, usage))
            }
            Err(err) => {
                sess.worker.close();
                drop(sess);
                let mut st = self.state.lock().unwrap();
                st.sessions.retain(|s| !Arc::ptr_eq(s, &session_arc));
                Err(err)
            }
        }
    }

    pub(crate) fn stats(&self) -> Json {
        let mut st = self.state.lock().unwrap();
        self.evict_expired_locked(&mut st);
        let now = now_f64();
        let mut sess_list = Vec::new();
        for arc_s in &st.sessions {
            if let Ok(s) = arc_s.try_lock() {
                let idle = ((now - s.last_used) * 10.0).round() / 10.0;
                sess_list.push(Json::obj(vec![
                    ("id", Json::Str(s.id.clone())),
                    ("model", Json::Str(s.model.clone())),
                    ("turns", Json::Num(s.turns as f64)),
                    ("cached_messages", Json::Num(s.msg_hashes.len() as f64)),
                    ("idle_sec", Json::Num(idle)),
                ]));
            }
        }
        Json::obj(vec![
            ("active_sessions", Json::Num(sess_list.len() as f64)),
            ("cache_hits", Json::Num(st.cache_hits as f64)),
            ("cache_misses", Json::Num(st.cache_misses as f64)),
            (
                "total_cached_tokens",
                Json::Num(st.total_cached_tokens as f64),
            ),
            ("sessions", Json::Arr(sess_list)),
        ])
    }
}

struct CliCachedSession {
    provider: String,
    model: String,
    header_key: String,
    session_id: String,
    msg_hashes: Vec<String>,
    last_used: f64,
    turns: u64,
}

struct CliSessionCacheState {
    sessions: Vec<CliCachedSession>,
    cache_hits: u64,
    cache_misses: u64,
    total_cached_tokens: i64,
}

pub(crate) struct CliSessionCache {
    ttl_sec: f64,
    max_sessions: usize,
    state: Mutex<CliSessionCacheState>,
}

impl CliSessionCache {
    pub(crate) fn new(cfg: &Config) -> Self {
        Self {
            ttl_sec: cfg.sticky_ttl_sec,
            max_sessions: (cfg.max_sticky_sessions * 2).max(6),
            state: Mutex::new(CliSessionCacheState {
                sessions: Vec::new(),
                cache_hits: 0,
                cache_misses: 0,
                total_cached_tokens: 0,
            }),
        }
    }

    fn evict_locked(&self, st: &mut CliSessionCacheState) {
        let now = now_f64();
        st.sessions.retain(|s| now - s.last_used <= self.ttl_sec);
        while st.sessions.len() > self.max_sessions {
            st.sessions.remove(0);
        }
    }

    pub(crate) fn find_match(
        &self,
        provider: &str,
        header_key: &str,
        chat_hashes: &[String],
    ) -> Option<(String, usize)> {
        let mut st = self.state.lock().unwrap();
        self.evict_locked(&mut st);
        let mut best_len = 0usize;
        let mut best_sid = None;
        for s in &st.sessions {
            if s.provider != provider || s.header_key != header_key {
                continue;
            }
            let n = s.msg_hashes.len();
            if n > 0
                && chat_hashes.len() > n
                && chat_hashes[..n] == s.msg_hashes[..]
                && n > best_len
            {
                best_len = n;
                best_sid = Some(s.session_id.clone());
            }
        }
        best_sid.map(|sid| (sid, best_len))
    }

    pub(crate) fn invalidate(&self, provider: &str, session_id: &str) {
        let mut st = self.state.lock().unwrap();
        st.sessions
            .retain(|s| !(s.provider == provider && s.session_id == session_id));
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &self,
        provider: &str,
        model: &str,
        header_key: &str,
        session_id: &str,
        msg_hashes: Vec<String>,
        was_hit: bool,
        cached_tok: i64,
    ) {
        let mut st = self.state.lock().unwrap();
        if was_hit {
            st.cache_hits += 1;
        } else {
            st.cache_misses += 1;
        }
        if cached_tok > 0 {
            st.total_cached_tokens += cached_tok;
        }
        if session_id.is_empty() {
            return;
        }
        let now = now_f64();
        if let Some(existing) = st
            .sessions
            .iter_mut()
            .find(|s| s.provider == provider && s.session_id == session_id)
        {
            existing.msg_hashes = msg_hashes;
            existing.last_used = now;
            existing.turns += 1;
        } else {
            st.sessions.push(CliCachedSession {
                provider: provider.to_string(),
                model: model.to_string(),
                header_key: header_key.to_string(),
                session_id: session_id.to_string(),
                msg_hashes,
                last_used: now,
                turns: 1,
            });
        }
        self.evict_locked(&mut st);
    }

    pub(crate) fn stats(&self) -> Json {
        let mut st = self.state.lock().unwrap();
        self.evict_locked(&mut st);
        let now = now_f64();
        let sess_list: Vec<Json> = st
            .sessions
            .iter()
            .map(|s| {
                let idle = ((now - s.last_used) * 10.0).round() / 10.0;
                Json::obj(vec![
                    ("provider", Json::Str(s.provider.clone())),
                    ("model", Json::Str(s.model.clone())),
                    ("turns", Json::Num(s.turns as f64)),
                    ("cached_messages", Json::Num(s.msg_hashes.len() as f64)),
                    ("idle_sec", Json::Num(idle)),
                ])
            })
            .collect();
        Json::obj(vec![
            ("active_sessions", Json::Num(sess_list.len() as f64)),
            ("cache_hits", Json::Num(st.cache_hits as f64)),
            ("cache_misses", Json::Num(st.cache_misses as f64)),
            (
                "total_cached_tokens",
                Json::Num(st.total_cached_tokens as f64),
            ),
            ("sessions", Json::Arr(sess_list)),
        ])
    }
}

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
