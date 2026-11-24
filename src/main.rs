mod catalog;
mod config;
mod json;
mod prompt;
mod providers;
mod router;
mod server;
mod tools;
mod tui;

use catalog::{all_known_models, infer_provider_and_model};
use config::{parse_bool_flag, Config};
use json::Json;
use providers::{StandbyModelPool, StickySessionManager};
use router::{load_agent_routes, save_agent_routes, ProviderRouter};
use server::{http_call, run_server};
use std::env;
use std::fs;
use std::io::Write;
use std::sync::Arc;

fn cmd_agent_status(cfg: &Config) {
    let routes = load_agent_routes(&cfg.routes_path);
    let Some(agents) = routes.get("agents").and_then(|v| v.as_obj()) else {
        println!("No agent chains configured in {}", cfg.routes_path);
        return;
    };
    if agents.is_empty() {
        println!("No agent chains configured in {}", cfg.routes_path);
        return;
    }
    println!(
        "{:<15} | {:<7} | PRIMARY -> FALLBACK CHAIN",
        "AGENT", "EFFORT"
    );
    println!("{}", "-".repeat(78));
    for (name, info) in agents {
        let eff = info
            .get("effort")
            .and_then(|v| v.as_str())
            .unwrap_or("medium");
        let chain = info.get("chain").and_then(|v| v.as_arr()).unwrap_or(&[]);
        let mut formatted = Vec::new();
        for (idx, m) in chain.iter().enumerate() {
            if let Some(m_str) = m.as_str() {
                let (prov, bare) = infer_provider_and_model(m_str);
                let tag = if idx == 0 {
                    "default".to_string()
                } else {
                    format!("fb#{idx}")
                };
                formatted.push(format!("[{tag}] {prov}/{bare}"));
            }
        }
        println!("{name:<15} | {eff:<7} | {}", formatted.join("  ->  "));
    }
}

fn cmd_agent_set(cfg: &Config, agent_name: &str, models: &[String], effort: Option<&str>) {
    let name = agent_name.trim().to_lowercase();
    let mut routes = load_agent_routes(&cfg.routes_path);
    let prev_eff = routes
        .get("agents")
        .and_then(|a| a.get(&name))
        .and_then(|c| c.get("effort"))
        .and_then(|v| v.as_str())
        .unwrap_or("medium")
        .to_string();
    let eff = effort.unwrap_or(&prev_eff).trim().to_lowercase();
    let chain_json: Vec<Json> = models.iter().cloned().map(Json::Str).collect();
    let entry = Json::obj(vec![
        ("effort", Json::Str(eff.clone())),
        ("chain", Json::Arr(chain_json.clone())),
    ]);
    let agents_obj = routes.get_mut_or_insert("agents", Json::Obj(Vec::new()));
    agents_obj.set(&name, entry);
    if let Err(e) = save_agent_routes(&cfg.routes_path, &routes) {
        eprintln!("Failed to save {}: {e}", cfg.routes_path);
        std::process::exit(1);
    }

    let body = Json::obj(vec![
        ("agent", Json::Str(name.clone())),
        ("chain", Json::Arr(chain_json)),
        ("effort", Json::Str(eff.clone())),
    ]);
    let _ = http_call(
        &cfg.host,
        cfg.port,
        "POST",
        "/v1/router/agents",
        Some(&body),
    );

    let chain_str: Vec<String> = models
        .iter()
        .map(|m| {
            let (p, b) = infer_provider_and_model(m);
            format!("{p}/{b}")
        })
        .collect();
    println!("Updated agent '{name}' (effort={eff}):");
    println!("  Chain: {}", chain_str.join(" -> "));
    println!("  Saved: {}", cfg.routes_path);
}

fn cmd_models(filter_provider: &str) {
    let p_filter = filter_provider.trim().to_lowercase();
    println!("{:<32} | {:<8} | EFFORT LEVELS", "MODEL ID", "PROVIDER");
    println!("{}", "-".repeat(72));
    for (m, prov, efforts) in all_known_models() {
        if !p_filter.is_empty() && prov != p_filter {
            continue;
        }
        println!("{m:<32} | {prov:<8} | {}", efforts.join(", "));
    }
}

fn build_local_router(cfg: &Config) -> Arc<ProviderRouter> {
    let _ = fs::create_dir_all(&cfg.workdir);
    let standby = Arc::new(StandbyModelPool::new(cfg.clone()));
    let sticky = Arc::new(StickySessionManager::new(cfg.clone(), standby));
    Arc::new(ProviderRouter::new(cfg.clone(), sticky))
}

struct CliRunFlags {
    agent: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    resume: Option<String>,
    always_allow: bool,
    prompt: String,
}

fn parse_cli_run_flags(args: &[String]) -> CliRunFlags {
    let mut agent = None;
    let mut model = None;
    let mut effort = None;
    let mut resume = None;
    let mut always_allow = false;
    let mut prompt_parts = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-a" | "--agent" if i + 1 < args.len() => {
                agent = Some(args[i + 1].clone());
                i += 2;
            }
            "-m" | "--model" if i + 1 < args.len() => {
                model = Some(args[i + 1].clone());
                i += 2;
            }
            "-e" | "--effort" | "--reasoning" if i + 1 < args.len() => {
                effort = Some(args[i + 1].clone());
                i += 2;
            }
            "--always-allow" | "--dangerously-skip-permissions" => {
                always_allow = true;
                i += 1;
            }
            "-c" | "--continue" => {
                resume = Some("latest".to_string());
                i += 1;
            }
            "-r" | "--resume" => {
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    resume = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    resume = Some("picker".to_string());
                    i += 1;
                }
            }
            other => {
                prompt_parts.push(other.to_string());
                i += 1;
            }
        }
    }
    CliRunFlags {
        agent,
        model,
        effort,
        resume,
        always_allow,
        prompt: prompt_parts.join(" "),
    }
}

fn print_help() {
    println!(
        "cligate — OpenAI-Compatible CLI-OAuth Gateway, TUI & Per-Agent Fallback Router\n\n\
         Commands:\n  \
         cligate [-c|--continue] [-r|--resume [id]] [-a <agent>] [-m <model>] [-e <level>] [--always-allow]\n  \
         cligate tui [-c] [-r [id]] [-a <agent>] [-m <model>] [-e <level>] [--always-allow]\n  \
         cligate ask [-a <agent>] [-m <model>] [-e <level>] <prompt>\n  \
         cligate serve [--host 127.0.0.1] [--port 8787] [--no-fallback]\n  \
         cligate fallback [on|off|status]                     Toggle or inspect API fallback routing\n  \
         cligate status                                       Show live router health, cooldowns & cache\n  \
         cligate models [claude|codex|agy]                    List native models & supported effort levels\n  \
         cligate reset [claude|codex|agy]                     Reset provider rate-limit cooldowns\n  \
         cligate agent status                                 Show per-agent primary & fallback model chains\n  \
         cligate agent set <agent> <m1> <m2> [m3 ...] [--effort <low|medium|high|xhigh|max>]"
    );
}

fn main() {
    let cfg = Config::from_env();
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        let router = build_local_router(&cfg);
        tui::run_tui(cfg, router, None, None, None, None, false);
        return;
    }
    if matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return;
    }
    if args[0].starts_with('-') {
        let flags = parse_cli_run_flags(&args);
        let router = build_local_router(&cfg);
        tui::run_tui(
            cfg,
            router,
            flags.agent,
            flags.model,
            flags.effort,
            flags.resume,
            flags.always_allow,
        );
        return;
    }

    match args[0].as_str() {
        "tui" | "chat" => {
            let flags = parse_cli_run_flags(&args[1..]);
            let router = build_local_router(&cfg);
            tui::run_tui(
                cfg,
                router,
                flags.agent,
                flags.model,
                flags.effort,
                flags.resume,
                flags.always_allow,
            );
        }
        "ask" | "run" => {
            let flags = parse_cli_run_flags(&args[1..]);
            let (agent, model, effort, prompt) =
                (flags.agent, flags.model, flags.effort, flags.prompt);
            if prompt.trim().is_empty() {
                eprintln!("Usage: cligate ask [-a <agent>] [-m <model>] [-e <effort>] <prompt>");
                std::process::exit(1);
            }
            let router = build_local_router(&cfg);
            let req_model = model.unwrap_or_else(|| cfg.default_model.clone());
            let req_effort = effort.unwrap_or_else(|| "medium".to_string());
            let messages = vec![Json::obj(vec![
                ("role", Json::Str("user".to_string())),
                ("content", Json::Str(prompt)),
            ])];
            let mut printed_delta = false;
            let res = {
                let mut on_delta = |chunk: &str| {
                    printed_delta = true;
                    print!("{chunk}");
                    let _ = std::io::stdout().flush();
                };
                let mut cb_opt: Option<&mut dyn FnMut(&str)> = Some(&mut on_delta);
                router.execute_turn(
                    &messages,
                    None,
                    &req_model,
                    req_effort,
                    agent.as_deref(),
                    &mut cb_opt,
                )
            };
            match res {
                Ok((final_text, _, _, _)) => {
                    if !printed_delta {
                        println!("{final_text}");
                    } else {
                        println!();
                    }
                }
                Err(e) => {
                    eprintln!("\nError: {e}");
                    std::process::exit(1);
                }
            }
        }
        "serve" => {
            let mut host_ov = None;
            let mut port_ov = None;
            let mut fb_ov = None;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--host" && i + 1 < args.len() {
                    host_ov = Some(args[i + 1].clone());
                    i += 2;
                } else if args[i] == "--port" && i + 1 < args.len() {
                    port_ov = args[i + 1].parse().ok();
                    i += 2;
                } else if args[i] == "--no-fallback" {
                    fb_ov = Some(false);
                    i += 1;
                } else if args[i] == "--fallback" && i + 1 < args.len() {
                    fb_ov = parse_bool_flag(&args[i + 1]);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            run_server(cfg, host_ov, port_ov, fb_ov);
        }
        "fallback" => {
            let arg = args.get(1).map(|s| s.as_str()).unwrap_or("status");
            if arg == "status" {
                if let Ok(resp) = http_call(&cfg.host, cfg.port, "GET", "/v1/router/status", None) {
                    let on = resp
                        .get("fallback_enabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    println!("API fallback: {} (live)", if on { "on" } else { "off" });
                } else {
                    let routes = load_agent_routes(&cfg.routes_path);
                    let on = cfg.fallback_enabled.unwrap_or_else(|| {
                        routes
                            .get("fallback_enabled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true)
                    });
                    println!("API fallback: {} (saved)", if on { "on" } else { "off" });
                }
            } else if let Some(enabled) = parse_bool_flag(arg) {
                let mut routes = load_agent_routes(&cfg.routes_path);
                routes.set("fallback_enabled", Json::Bool(enabled));
                if let Err(e) = save_agent_routes(&cfg.routes_path, &routes) {
                    eprintln!("Error saving {}: {e}", cfg.routes_path);
                    std::process::exit(1);
                }
                let body = Json::obj(vec![("enabled", Json::Bool(enabled))]);
                let live = http_call(
                    &cfg.host,
                    cfg.port,
                    "POST",
                    "/v1/router/fallback",
                    Some(&body),
                )
                .is_ok();
                println!(
                    "API fallback: {} (saved: {}, live synced: {})",
                    if enabled { "on" } else { "off" },
                    cfg.routes_path,
                    if live { "yes" } else { "no" }
                );
            } else {
                eprintln!("Usage: cligate fallback [on|off|status]");
                std::process::exit(1);
            }
        }
        "status" => match http_call(&cfg.host, cfg.port, "GET", "/v1/router/status", None) {
            Ok(resp) => println!("{}", resp.dump_pretty()),
            Err(e) => {
                eprintln!(
                    "Error connecting to cligate at http://{}:{}: {e}",
                    cfg.host, cfg.port
                );
                std::process::exit(1);
            }
        },
        "models" => {
            let filter = args.get(1).map(|s| s.as_str()).unwrap_or("");
            cmd_models(filter);
        }
        "reset" => {
            let prov_json = args
                .get(1)
                .map(|s| Json::Str(s.clone()))
                .unwrap_or(Json::Null);
            let body = Json::obj(vec![("provider", prov_json)]);
            match http_call(&cfg.host, cfg.port, "POST", "/v1/router/reset", Some(&body)) {
                Ok(resp) => println!("{}", resp.dump_pretty()),
                Err(e) => {
                    eprintln!("Error connecting to cligate: {e}");
                    std::process::exit(1);
                }
            }
        }
        "agent" | "set" | "list" => {
            let sub_args = if args[0] == "agent" {
                &args[1..]
            } else {
                &args[..]
            };
            let sub = sub_args.first().map(|s| s.as_str()).unwrap_or("status");
            if matches!(sub, "status" | "list") {
                cmd_agent_status(&cfg);
                return;
            }
            if sub == "set" {
                if sub_args.len() < 3 {
                    eprintln!("Usage: cligate agent set <agent> <primary_model> [fallback_1 fallback_2 ...] [--effort <level>]");
                    std::process::exit(1);
                }
                let agent_name = &sub_args[1];
                let rest = &sub_args[2..];
                let mut effort: Option<String> = None;
                let mut models = Vec::new();
                let mut i = 0;
                while i < rest.len() {
                    if matches!(rest[i].as_str(), "--effort" | "--reasoning" | "-e")
                        && i + 1 < rest.len()
                    {
                        effort = Some(rest[i + 1].clone());
                        i += 2;
                    } else if let Some(val) = rest[i].strip_prefix("--effort=") {
                        effort = Some(val.to_string());
                        i += 1;
                    } else {
                        models.push(rest[i].clone());
                        i += 1;
                    }
                }
                if models.is_empty() {
                    eprintln!("Error: provide at least 1 model for the chain.");
                    std::process::exit(1);
                }
                cmd_agent_set(&cfg, agent_name, &models, effort.as_deref());
                return;
            }
            eprintln!("Unknown agent subcommand '{sub}'.");
            std::process::exit(1);
        }
        other => {
            eprintln!("Unknown command '{other}'. Run 'cligate --help' for usage.");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        resolve_agy_model, resolve_claude_model_and_effort, resolve_codex_model_and_effort,
    };
    use crate::prompt::parse_tool_calls;
    use crate::providers::CliSessionCache;

    fn test_config(routes_path: &str) -> Config {
        let mut cfg = Config::from_env();
        cfg.routes_path = routes_path.to_string();
        cfg
    }

    #[test]
    fn test_agy_effort_resolution() {
        assert_eq!(
            resolve_agy_model("gemini-4-neon", "xhigh", "gemini-4-argon"),
            "gemini-4-neon-xhigh"
        );
        assert_eq!(
            resolve_agy_model("gemini-4-argon", "high", "gemini-4-argon"),
            "gemini-4-argon-high"
        );
        assert_eq!(
            resolve_agy_model("gemini-4-argon", "xhigh", "gemini-4-argon"),
            "gemini-4-argon-high"
        );
        assert_eq!(
            resolve_agy_model("gemini-4-carbon", "low", "gemini-4-argon"),
            "gemini-4-carbon-low"
        );
    }

    #[test]
    fn test_codex_and_claude_effort_resolution() {
        assert_eq!(
            resolve_codex_model_and_effort("gpt-6.1-sol-900k", "max"),
            ("gpt-6.1-sol".to_string(), "max".to_string())
        );
        assert_eq!(
            resolve_codex_model_and_effort("gpt-5.4", "max"),
            ("gpt-5.4".to_string(), "xhigh".to_string())
        );
        assert_eq!(
            resolve_claude_model_and_effort("claude-opus-5-5", "max"),
            ("claude-opus-5-5".to_string(), "max".to_string())
        );
        let models = all_known_models();
        assert!(models
            .iter()
            .any(|(m, p, _)| *m == "claude-opus-5-5" && *p == "claude"));
        assert!(models
            .iter()
            .any(|(m, p, _)| *m == "claude-sonnet-5-5" && *p == "claude"));
    }

    #[test]
    fn test_per_agent_fallback_chain_and_cooldown() {
        let tmp_path = env::temp_dir()
            .join("cligate_rust_test_routes.json")
            .to_string_lossy()
            .into_owned();
        let _ = fs::remove_file(&tmp_path);
        let cfg = test_config(&tmp_path);
        let standby = Arc::new(StandbyModelPool::new(cfg.clone()));
        let sticky = Arc::new(StickySessionManager::new(cfg.clone(), standby));
        let router = ProviderRouter::new(cfg, sticky);

        router
            .set_agent_chain(
                "coder",
                &[
                    "claude-opus-5-5".to_string(),
                    "gpt-6.1-sol".to_string(),
                    "gemini-4-argon".to_string(),
                ],
                Some("high"),
            )
            .unwrap();

        let cands = router.build_target_candidates("claude-opus-5-5", Some("coder"));
        assert_eq!(
            cands,
            vec![
                ("claude".to_string(), "claude-opus-5-5".to_string()),
                ("codex".to_string(), "gpt-6.1-sol".to_string()),
                ("agy".to_string(), "gemini-4-argon".to_string()),
            ]
        );

        router.mark_limited("claude", "429 usage_limit_reached", 600.0);
        let cands_after = router.build_target_candidates("claude-opus-5-5", Some("coder"));
        assert_eq!(
            cands_after,
            vec![
                ("codex".to_string(), "gpt-6.1-sol".to_string()),
                ("agy".to_string(), "gemini-4-argon".to_string()),
                ("claude".to_string(), "claude-opus-5-5".to_string()),
            ]
        );
        let _ = fs::remove_file(&tmp_path);
    }

    #[test]
    fn test_tool_call_xml_bridge() {
        let raw = "Thinking...\n<tool_call>{\"name\": \"run_command\", \"arguments\": {\"cmd\": \"ls\"}}</tool_call>";
        let (cleaned, calls) = parse_tool_calls(raw);
        assert_eq!(cleaned, "Thinking...");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str()),
            Some("run_command")
        );
    }

    #[test]
    fn test_api_fallback_off_uses_default_provider_only() {
        let tmp_path = env::temp_dir()
            .join("cligate_rust_test_fallback_off.json")
            .to_string_lossy()
            .into_owned();
        let _ = fs::remove_file(&tmp_path);
        let mut cfg = test_config(&tmp_path);
        cfg.default_model = "gemini-4-neon".to_string();
        let standby = Arc::new(StandbyModelPool::new(cfg.clone()));
        let sticky = Arc::new(StickySessionManager::new(cfg.clone(), standby));
        let router = ProviderRouter::new(cfg, sticky);

        router
            .set_agent_chain(
                "coder",
                &[
                    "claude-opus-5-5".to_string(),
                    "gpt-6.1-sol".to_string(),
                    "gemini-4-neon".to_string(),
                ],
                Some("high"),
            )
            .unwrap();
        router.set_fallback_enabled(false).unwrap();
        router.mark_limited("claude", "429 usage_limit_reached", 600.0);

        let cands_default = router.build_target_candidates("default", Some("coder"));
        assert_eq!(
            cands_default,
            vec![("agy".to_string(), "gemini-4-neon".to_string())]
        );

        let cands_explicit_model = router.build_target_candidates("claude-opus-5-5", Some("coder"));
        assert_eq!(
            cands_explicit_model,
            vec![("claude".to_string(), "claude-opus-5-5".to_string())]
        );

        let tui_chain = vec!["gpt-6.1-sol".to_string(), "gemini-4-neon".to_string()];
        let cands_tui =
            router.build_target_candidates_with_chain("gpt-6.1-sol", None, Some(&tui_chain));
        assert_eq!(
            cands_tui,
            vec![
                ("codex".to_string(), "gpt-6.1-sol".to_string()),
                ("agy".to_string(), "gemini-4-neon".to_string()),
            ]
        );
        let _ = fs::remove_file(&tmp_path);
    }

    #[test]
    fn test_local_tools_and_permissions() {
        assert_eq!(
            tools::PermissionMode::parse("always-allow"),
            Some(tools::PermissionMode::AlwaysAllow)
        );
        assert_eq!(
            tools::PermissionMode::parse("read-only"),
            Some(tools::PermissionMode::ReadOnly)
        );
        assert!(tools::is_read_only_tool("read_file"));
        assert!(!tools::is_read_only_tool("run_command"));

        let dir = env::temp_dir().join("cligate_tools_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cwd = dir.to_string_lossy().into_owned();

        let (ok, _) = tools::execute_local_tool(
            &cwd,
            "write_file",
            &Json::obj(vec![
                ("path", Json::Str("hello.txt".to_string())),
                ("content", Json::Str("hello world\n".to_string())),
            ]),
        );
        assert!(ok);

        let (ok, _) = tools::execute_local_tool(
            &cwd,
            "edit_file",
            &Json::obj(vec![
                ("path", Json::Str("hello.txt".to_string())),
                ("target", Json::Str("world".to_string())),
                ("replacement", Json::Str("cligate".to_string())),
            ]),
        );
        assert!(ok);

        let (ok, out) = tools::execute_local_tool(
            &cwd,
            "read_file",
            &Json::obj(vec![("path", Json::Str("hello.txt".to_string()))]),
        );
        assert!(ok);
        assert!(out.contains("1: hello cligate"));

        let (ok, out) = tools::execute_local_tool(
            &cwd,
            "run_command",
            &Json::obj(vec![("command", Json::Str("cat hello.txt".to_string()))]),
        );
        assert!(ok);
        assert!(out.contains("hello cligate"));

        let (ok, out) = tools::execute_local_tool(
            &cwd,
            "grep_search",
            &Json::obj(vec![("query", Json::Str("cligate".to_string()))]),
        );
        assert!(ok);
        assert!(out.contains("hello.txt:1:"));

        let (ok, out) = tools::execute_local_tool(
            &cwd,
            "find_files",
            &Json::obj(vec![("pattern", Json::Str("hello".to_string()))]),
        );
        assert!(ok);
        assert!(out.contains("hello.txt"));

        fs::write(dir.join("AGENTS.md"), "Always keep changes minimal.\n").unwrap();
        let proj = tools::load_project_context(&cwd);
        assert!(proj.is_some());
        assert_eq!(proj.unwrap().0, "AGENTS.md");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cli_session_cache_prefix_matching() {
        let cfg = Config::from_env();
        let cache = CliSessionCache::new(&cfg);
        let h1 = vec!["a".to_string(), "b".to_string()];
        assert!(cache.find_match("claude", "hk1", &h1).is_none());

        cache.record("claude", "claude-opus-5-5", "hk1", "sid-1", h1, false, 0);
        let h2 = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(
            cache.find_match("claude", "hk1", &h2),
            Some(("sid-1".to_string(), 2))
        );
        assert!(cache.find_match("codex", "hk1", &h2).is_none());
        assert!(cache.find_match("claude", "hk2", &h2).is_none());

        cache.record("claude", "claude-opus-5-5", "hk1", "sid-1", h2, true, 1200);
        let stats = cache.stats();
        assert_eq!(stats.get("cache_hits").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            stats.get("total_cached_tokens").and_then(|v| v.as_i64()),
            Some(1200)
        );
        cache.invalidate("claude", "sid-1");
        let h3 = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        assert!(cache.find_match("claude", "hk1", &h3).is_none());
    }

    #[test]
    fn test_tui_text_wrapping() {
        let lines = tui::wrap_text_lines("hello world\nsecond line", 10);
        assert_eq!(lines, vec!["hello", "world", "second", "line"]);
        assert_eq!(tui::parse_token_limit("off"), Some(0));
        assert_eq!(tui::parse_token_limit("0"), Some(0));
        assert_eq!(tui::parse_token_limit("64k"), Some(64_000));
        assert_eq!(tui::parse_token_limit("128K"), Some(128_000));
        assert_eq!(tui::parse_token_limit("1m"), Some(1_000_000));
        assert_eq!(tui::parse_token_limit("80000"), Some(80_000));
        assert_eq!(tui::parse_token_limit("abc"), None);
        let s1 = tui::new_session_id();
        let s2 = tui::new_session_id();
        assert_eq!(s1.len(), 36);
        assert_eq!(s1.chars().filter(|&c| c == '-').count(), 4);
        assert_ne!(s1, s2);
    }
}
