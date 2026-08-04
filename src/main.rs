mod autostart;
mod config;
mod doctor;
mod flexible_id;
mod http;
mod registry;
mod server;
mod telegram;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use rmcp::transport::io::stdio;

use config::Mode;
use registry::AgentRegistry;
use server::TelegramMcpServer;
use telegram::Hub;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // reqwest is built with `rustls-no-provider`, so the process must choose a
    // crypto provider before any TLS connection is made — including before
    // building the shared `reqwest::Client`.
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install the rustls `ring` crypto provider"))?;

    let cli = config::parse_args(std::env::args().skip(1))?;

    match cli.mode.clone() {
        Mode::Stdio => run_stdio(&cli).await,
        Mode::Hub {
            addr,
            idle_shutdown_secs,
        } => run_hub(&cli, &addr, idle_shutdown_secs).await,
        Mode::Doctor => doctor::run(&cli).await,
        Mode::AddAgent {
            name,
            token,
            model,
            description,
        } => {
            let path = config::add_agent(
                cli.config_path.clone(),
                &name,
                &token,
                model.as_deref(),
                description.as_deref(),
            )?;
            println!("added agent {name:?} to {}\n", path.display());
            println!("Next:");
            println!(
                "  1. In @BotFather, turn off privacy mode for this bot (Bot Settings -> Group \
                 Privacy -> Turn off)."
            );
            println!(
                "  2. Add the bot to your Telegram group (or DM it) and say something, so it \
                 learns the chat exists."
            );
            println!("  3. Point one MCP client at:\n");
            println!("         telegram-agent-mcp --agent {name}\n");
            Ok(())
        }
        Mode::Bridge {
            url,
            agent,
            autostart,
        } => {
            // The autostarted hub's address comes from the config file, which
            // may have moved it off the default port; `url` was only a guess.
            let url = if autostart {
                autostart::ensure_hub(&cli, agent.as_deref()).await?
            } else {
                url
            };
            run_bridge(&url, agent.as_deref()).await
        }
    }
}

/// The original mode: one bot, one poller, MCP over stdin/stdout.
async fn run_stdio(cli: &config::Cli) -> Result<()> {
    let config = config::load(cli.config_path.clone())?;

    // Binding stdio to one of several agents would silently pick a bot on the
    // caller's behalf, so require the choice to be unambiguous.
    if config.agents.len() > 1 && config.primary.is_none() {
        anyhow::bail!(
            "{} agents are configured ({}), so this command has to be told which one to run as. \
             Add `--agent NAME`; that also puts them on a shared hub, which is what lets them \
             see each other's messages.",
            config.agents.len(),
            config.agent_names()
        );
    }
    let agent = config
        .default_agent()
        .context("no agent to run")?
        .id
        .clone();

    let hub = Arc::new(Hub::new());
    if let Some(dir) = config::config_dir() {
        let _ = std::fs::create_dir_all(&dir);
        hub.use_chat_cache(dir.join("chats.json"));
    }
    let registry = AgentRegistry::build(&config, hub, reqwest::Client::new())?;
    let session = registry
        .get(&agent.to_string())
        .context("configured agent missing from registry")?;

    // Only the agent being served gets a poller. Polling the others would run
    // bots the caller did not ask for, and — since getUpdates allows one
    // consumer per token — would fight a hub already polling them, with each
    // side kicking the other off with a 409.
    let others: Vec<String> = config
        .agents
        .iter()
        .map(|a| a.id.to_string())
        .filter(|name| name != &agent.to_string())
        .collect();
    if !others.is_empty() {
        tracing::info!(
            "serving {agent} over stdio; the other configured agents ({}) are not started. Run \
             with --hub to have them all share one process and see each other.",
            others.join(", ")
        );
    }
    registry::spawn_poller(Arc::clone(&session));

    let service = TelegramMcpServer::bound(session)
        .serve(stdio())
        .await
        .inspect_err(|err| tracing::error!("failed to start MCP server: {err:#}"))?;
    service.waiting().await?;
    Ok(())
}

/// Several agents in one process, served over HTTP. They see each other's
/// messages because they share one hub.
async fn run_hub(cli: &config::Cli, addr: &str, idle_shutdown_secs: Option<u64>) -> Result<()> {
    let config = config::load(cli.config_path.clone())?;
    tracing::info!("loaded {}", config::describe_source(&cli.config_path));
    // An explicit `--hub ADDR` wins over the config file's `http_addr`.
    let addr = if addr == config::DEFAULT_HTTP_ADDR {
        config.http_addr.as_deref().unwrap_or(addr)
    } else {
        addr
    };
    let addr = config::parse_addr(addr)?;
    // Same precedence as the address: an explicit `--idle-shutdown` wins,
    // the config file is the fallback, "off" (run forever) if neither said
    // anything — which is always true for a hand-run `--hub` unless the file
    // opts it in.
    let idle_shutdown_secs = idle_shutdown_secs.or(config.idle_shutdown_secs);

    let hub = Arc::new(Hub::new());
    if let Some(dir) = config::config_dir() {
        let _ = std::fs::create_dir_all(&dir);
        hub.use_chat_cache(dir.join("chats.json"));
    }
    let registry = AgentRegistry::build(&config, Arc::clone(&hub), reqwest::Client::new())?;

    // Bind before polling. Two clients starting at once both find the port
    // closed and both spawn a hub; the loser must fail on the port, quietly,
    // rather than first opening a getUpdates connection per token and 409ing
    // the winner off every one of its bots on the way out.
    let listener = http::bind(addr).await?;
    registry.spawn_pollers();

    // Only meaningful when running off an actual file: an agent added via
    // `--add-agent` while this hub is already up becomes reachable at
    // `/mcp?agent=<name>` within one check interval, no restart needed.
    if let Some(path) = config::resolve_config_path(cli.config_path.clone()) {
        tokio::spawn(config_reload_watch(path, Arc::clone(&registry)));
    }

    if let Some(secs) = idle_shutdown_secs {
        tokio::spawn(idle_shutdown_watch(
            Arc::clone(&hub),
            Duration::from_secs(secs),
        ));
    }

    http::serve(listener, registry, hub).await
}

/// How often the config-reload watcher checks the file's mtime.
const CONFIG_RELOAD_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Watches the config file for agents that weren't there at startup and adds
/// them to the running registry — the other half of making `--add-agent`
/// actually plug-and-play: without this, an agent added while a hub is
/// already up would sit in the file unused until something restarted it.
/// Only ever adds; see [`AgentRegistry::reload`] for why an existing agent is
/// left alone even if its token changed on disk.
async fn config_reload_watch(path: std::path::PathBuf, registry: Arc<AgentRegistry>) {
    let mut last_modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    loop {
        tokio::time::sleep(CONFIG_RELOAD_CHECK_INTERVAL).await;

        let modified = match std::fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(m) => m,
            // Momentarily missing mid-write, or genuinely gone — either way
            // there is nothing new to read yet; try again next tick rather
            // than treating a transient stat failure as a config problem.
            Err(_) => continue,
        };
        if last_modified == Some(modified) {
            continue;
        }
        last_modified = Some(modified);

        let config = match config::load(Some(path.clone())) {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!(
                    "{} changed but no longer parses ({err:#}); leaving the running agents as \
                     they are until it does",
                    path.display()
                );
                continue;
            }
        };
        for session in registry.reload(&config) {
            tracing::info!(
                "picked up new agent {} from {} — reachable now at /mcp?agent={}",
                session.id(),
                path.display(),
                session.id()
            );
            registry::spawn_poller(session);
        }
    }
}

/// How often the idle-shutdown watcher rechecks. Independent of the
/// heartbeat interval — this only needs to be frequent enough that the
/// eventual shutdown doesn't overshoot its deadline noticeably.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Exits the process once no bridge client has been seen for `idle_after`.
/// Only spawned for a hub this process autostarted (see [`autostart`]) — a
/// hand-run `--hub` has an operator who can stop it, and would be surprised
/// to have it vanish out from under them.
async fn idle_shutdown_watch(hub: Arc<Hub>, idle_after: Duration) {
    hub.arm_idle_shutdown(idle_after);
    let mut idle_since = std::time::Instant::now();
    loop {
        tokio::time::sleep(IDLE_CHECK_INTERVAL).await;
        hub.prune_stale_clients();
        if hub.has_clients() {
            idle_since = std::time::Instant::now();
            hub.note_idle_since(None);
            continue;
        }
        hub.note_idle_since(Some(idle_since));
        if idle_since.elapsed() >= idle_after {
            tracing::info!(
                "no MCP client seen for {idle_after:?}; shutting down the idle autostarted hub"
            );
            std::process::exit(0);
        }
    }
}

/// stdio on one side, a hub over HTTP on the other, for MCP clients that can
/// only launch a command. This holds no state and speaks no MCP itself; it
/// just moves JSON-RPC between the two ends.
async fn run_bridge(url: &str, agent: Option<&str>) -> Result<()> {
    let target = match agent {
        Some(name) if !url.contains("agent=") => {
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}agent={name}")
        }
        _ => url.to_string(),
    };
    http::bridge_stdio_to(&target).await
}
