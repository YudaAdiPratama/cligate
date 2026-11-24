use crate::json::Json;
use std::collections::HashSet;

pub(crate) const AGY_MODELS: &[&str] = &[
    "gemini-4-neon",
    "gemini-4-carbon",
    "gemini-4-argon",
    "gemini-4-hydrogen",
    "gemini-pro-orchestrator-neon",
    "gemini-pro-orchestrator",
    "gemini-pro-orchestrator-argon",
    "gemini-3.8-flash",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "gpt-oss-120b",
];

pub(crate) const CODEX_MODELS: &[&str] = &[
    "gpt-6.1-sol",
    "gpt-6.1-terra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex-spark",
];

pub(crate) const CLAUDE_MODELS: &[&str] = &[
    "claude-fable-5-2",
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-opus-4-5-20251101",
    "claude-sonnet-5-5",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
    "claude-haiku-5-5",
    "claude-haiku-4-5-20251001",
    "claude-mythos-5-2",
    "claude-mythos-5-1",
];

pub(crate) const CODEX_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
pub(crate) const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

fn agy_effort_tiers(model: &str) -> &'static [&'static str] {
    match model {
        "gemini-4-neon" | "gemini-4-hydrogen" => &["low", "medium", "high", "xhigh"],
        "gemini-4-carbon" | "gemini-4-argon" | "gemini-3.8-flash" | "claude-opus-5-5"
        | "claude-sonnet-5-5" => &["low", "medium", "high"],
        "gpt-oss-120b" => &["medium"],
        _ => &["medium"],
    }
}

fn is_exact_agy_model(model: &str) -> bool {
    if matches!(
        model,
        "gemini-pro-orchestrator-neon"
            | "gemini-pro-orchestrator"
            | "gemini-pro-orchestrator-argon"
            | "gemini-pro-orchestrator-g1-pro"
    ) {
        return true;
    }
    for base in AGY_MODELS {
        for tier in agy_effort_tiers(base) {
            if model == format!("{base}-{tier}") {
                return true;
            }
        }
    }
    false
}

pub(crate) fn all_known_models() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for &m in AGY_MODELS {
        if CLAUDE_MODELS.contains(&m) {
            continue;
        }
        seen.insert(m);
        out.push((m, "agy", agy_effort_tiers(m)));
    }
    for &m in CODEX_MODELS {
        if seen.insert(m) {
            out.push((m, "codex", CODEX_EFFORTS));
        }
    }
    for &m in CLAUDE_MODELS {
        if seen.insert(m) {
            out.push((m, "claude", CLAUDE_EFFORTS));
        }
    }
    out
}

pub(crate) fn is_rate_limit_message(text: &str) -> bool {
    let low = text.to_lowercase();
    let needles = [
        "usage_limit_reached",
        "rate_limit_error",
        "hit your weekly limit",
        "hit your usage limit",
        "out of usage credits",
        "insufficient_quota",
        "too many requests",
        "status 429",
        "status: 429",
        "status=429",
        "http 429",
        "quota exceeded",
        "overloaded_error",
        "not logged in",
    ];
    needles.iter().any(|n| low.contains(n))
}

#[derive(Debug)]
pub(crate) enum TurnError {
    RateLimit(String),
    Runtime(String),
    Cancelled,
}

pub fn extract_effort(req: &Json) -> String {
    let mut raw = req.get("reasoning_effort").and_then(|v| v.as_str());
    if raw.is_none() {
        raw = req
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(|v| v.as_str());
    }
    if raw.is_none() {
        if let Some(eb) = req.get("extra_body") {
            raw = eb.get("reasoning_effort").and_then(|v| v.as_str());
            if raw.is_none() {
                raw = eb
                    .get("reasoning")
                    .and_then(|r| r.get("effort"))
                    .and_then(|v| v.as_str());
            }
        }
    }
    let effort = raw.unwrap_or("medium").trim().to_lowercase();
    match effort.as_str() {
        "none" | "minimal" => "low".to_string(),
        "ultra" => "max".to_string(),
        "low" | "medium" | "high" | "xhigh" | "max" => effort,
        _ => "medium".to_string(),
    }
}

fn strip_effort_suffix(model: &str) -> (String, Option<String>) {
    for suffix in ["xhigh", "medium", "high", "low", "max"] {
        let token = format!("-{suffix}");
        if let Some(base) = model.strip_suffix(&token) {
            if AGY_MODELS.contains(&base)
                || CODEX_MODELS.contains(&base)
                || CLAUDE_MODELS.contains(&base)
            {
                return (base.to_string(), Some(suffix.to_string()));
            }
        }
    }
    (model.to_string(), None)
}

pub fn resolve_agy_model(raw_model: &str, reasoning_effort: &str, default_model: &str) -> String {
    let mut model = raw_model.trim();
    if model.is_empty() || matches!(model.to_lowercase().as_str(), "default" | "auto") {
        model = default_model;
    }
    if let Some((_, rest)) = model.split_once('/') {
        model = rest;
    }
    if model.is_empty() {
        model = default_model;
    }
    if model == "gemini-pro-orchestrator-g1-pro" {
        return "gemini-pro-orchestrator".to_string();
    }
    if is_exact_agy_model(model) {
        return model.to_string();
    }
    let (base, suffix_eff) = strip_effort_suffix(model);
    let eff = suffix_eff.as_deref().unwrap_or(reasoning_effort);
    if !AGY_MODELS.contains(&base.as_str())
        || matches!(
            base.as_str(),
            "gemini-pro-orchestrator-neon"
                | "gemini-pro-orchestrator"
                | "gemini-pro-orchestrator-argon"
        )
    {
        return base;
    }
    let tiers = agy_effort_tiers(&base);
    if tiers.contains(&eff) {
        return format!("{base}-{eff}");
    }
    if matches!(eff, "max" | "xhigh") {
        if tiers.contains(&"xhigh") {
            return format!("{base}-xhigh");
        }
        if tiers.contains(&"high") {
            return format!("{base}-high");
        }
    }
    if tiers.contains(&"medium") {
        return format!("{base}-medium");
    }
    format!("{base}-{}", tiers[0])
}

pub fn resolve_codex_model_and_effort(raw_model: &str, reasoning_effort: &str) -> (String, String) {
    let mut model = raw_model.trim();
    if let Some((_, rest)) = model.split_once('/') {
        model = rest;
    }
    let stripped = model.strip_suffix("-900k").unwrap_or(model);
    let (base, suffix_eff) = strip_effort_suffix(stripped);
    let mut eff = suffix_eff.unwrap_or_else(|| reasoning_effort.to_string());
    if !CODEX_EFFORTS.contains(&eff.as_str()) {
        eff = "medium".to_string();
    }
    if eff == "max" && !(base.starts_with("gpt-6") || base.starts_with("gpt-5.6")) {
        eff = "xhigh".to_string();
    }
    (base, eff)
}

pub fn resolve_claude_model_and_effort(
    raw_model: &str,
    reasoning_effort: &str,
) -> (String, String) {
    let mut model = raw_model.trim();
    if let Some((_, rest)) = model.split_once('/') {
        model = rest;
    }
    let (base, suffix_eff) = strip_effort_suffix(model);
    let mut eff = suffix_eff.unwrap_or_else(|| reasoning_effort.to_string());
    if !CLAUDE_EFFORTS.contains(&eff.as_str()) {
        eff = "medium".to_string();
    }
    (base, eff)
}

pub fn detect_primary_provider(raw_model: &str, default_model: &str) -> (String, String) {
    let mut model = raw_model.trim();
    if model.is_empty() || matches!(model.to_lowercase().as_str(), "default" | "auto") {
        model = default_model;
    }
    if let Some((prefix, rest)) = model.split_once('/') {
        let p_low = prefix.trim().to_lowercase();
        if matches!(p_low.as_str(), "agy" | "antigravity" | "google") {
            return ("agy".to_string(), rest.to_string());
        }
        if matches!(p_low.as_str(), "codex" | "openai-codex" | "openai") {
            return ("codex".to_string(), rest.to_string());
        }
        if matches!(p_low.as_str(), "claude" | "claude-code" | "anthropic") {
            return ("claude".to_string(), rest.to_string());
        }
        model = rest;
    }
    let cleaned = model.strip_suffix("-900k").unwrap_or(model);
    let (base, _) = strip_effort_suffix(cleaned);
    let b_str = base.as_str();
    if CODEX_MODELS.contains(&b_str) || b_str.starts_with("gpt-5") || b_str.starts_with("gpt-6") {
        return ("codex".to_string(), cleaned.to_string());
    }
    if AGY_MODELS.contains(&b_str) && !matches!(b_str, "claude-opus-5-5" | "claude-sonnet-5-5") {
        return ("agy".to_string(), cleaned.to_string());
    }
    if is_exact_agy_model(cleaned) && !cleaned.starts_with("claude-") {
        return ("agy".to_string(), cleaned.to_string());
    }
    if CLAUDE_MODELS.contains(&b_str) || b_str.starts_with("claude-") {
        return ("claude".to_string(), cleaned.to_string());
    }
    ("agy".to_string(), cleaned.to_string())
}

pub fn map_fallback_model(
    source_model: &str,
    target_provider: &str,
    default_model: &str,
) -> String {
    let tail = source_model.rsplit('/').next().unwrap_or(source_model);
    let (base, _) = strip_effort_suffix(tail);
    let low = base.to_lowercase();

    let is_flagship = [
        "opus",
        "fable",
        "mythos",
        "terra",
        "neon",
        "hydrogen",
        "orchestrator",
    ]
    .iter()
    .any(|k| low.contains(k));
    let is_fast = ["haiku", "mini", "flash", "spark"]
        .iter()
        .any(|k| low.contains(k));

    match target_provider {
        "agy" => {
            if low == "claude-opus-5-5" || low.contains("claude-opus") {
                "claude-opus-5-5".to_string()
            } else if low == "claude-sonnet-5-5" || low.contains("claude-sonnet") {
                "claude-sonnet-5-5".to_string()
            } else if is_fast {
                "gemini-3.8-flash".to_string()
            } else {
                "gemini-4-neon".to_string()
            }
        }
        "codex" => {
            if is_fast {
                "gpt-5.4-mini".to_string()
            } else if is_flagship {
                "gpt-6.1-terra".to_string()
            } else {
                "gpt-6.1-sol".to_string()
            }
        }
        "claude" => {
            if CLAUDE_MODELS.contains(&low.as_str()) {
                low
            } else if is_fast {
                "claude-haiku-5-5".to_string()
            } else if is_flagship {
                "claude-opus-5-5".to_string()
            } else {
                "claude-sonnet-5-5".to_string()
            }
        }
        _ => default_model.to_string(),
    }
}

pub(crate) fn infer_provider_and_model(raw_model: &str) -> (String, String) {
    let item = raw_model.trim();
    if let Some((p, m)) = item.split_once('/') {
        let p_low = p.trim().to_lowercase();
        let mapped = match p_low.as_str() {
            "claude" | "anthropic" => "anthropic",
            "codex" | "openai-codex" => "openai-codex",
            "agy" | "antigravity" => "agy",
            other => other,
        };
        return (mapped.to_string(), m.trim().to_string());
    }
    let low = item.to_lowercase();
    if low.starts_with("gpt-5") || low.starts_with("gpt-6") {
        ("openai-codex".to_string(), item.to_string())
    } else if low.starts_with("gemini-") || low.starts_with("gpt-oss-") {
        ("agy".to_string(), item.to_string())
    } else if low.starts_with("claude-") {
        ("anthropic".to_string(), item.to_string())
    } else {
        ("agy".to_string(), item.to_string())
    }
}
