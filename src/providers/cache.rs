use crate::config::{now_f64, Config};
use crate::json::Json;
use std::sync::Mutex;

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
