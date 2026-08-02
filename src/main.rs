mod autostart;
mod config;
mod doctor;
mod flexible_id;
mod http;
mod registry;
mod server;
mod telegram;

use std::sync::Arc;

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
        Mode::Hub { addr } => run_hub(&cli, &addr).await,
        Mode::Doctor => doctor::run(&cli).await,
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
async fn run_hub(cli: &config::Cli, addr: &str) -> Result<()> {
    let config = config::load(cli.config_path.clone())?;
    tracing::info!("loaded {}", config::describe_source(&cli.config_path));
    // An explicit `--hub ADDR` wins over the config file's `http_addr`.
    let addr = if addr == config::DEFAULT_HTTP_ADDR {
        config.http_addr.as_deref().unwrap_or(addr)
    } else {
        addr
    };
    let addr = config::parse_addr(addr)?;

    let hub = Arc::new(Hub::new());
    if let Some(dir) = config::config_dir() {
        let _ = std::fs::create_dir_all(&dir);
        hub.use_chat_cache(dir.join("chats.json"));
    }
    let registry = AgentRegistry::build(&config, hub, reqwest::Client::new())?;

    // Bind before polling. Two clients starting at once both find the port
    // closed and both spawn a hub; the loser must fail on the port, quietly,
    // rather than first opening a getUpdates connection per token and 409ing
    // the winner off every one of its bots on the way out.
    let listener = http::bind(addr).await?;
    registry.spawn_pollers();

    http::serve(listener, registry).await
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
