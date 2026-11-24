use crate::catalog::{is_rate_limit_message, TurnError};
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
