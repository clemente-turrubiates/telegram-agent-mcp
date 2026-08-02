//! `--doctor`: answer "why isn't this working" without reading any code.
//!
//! Almost every setup failure is one of four things — no config file, a
//! config file somewhere other than where the user thinks, a bad token, or a
//! bot that was never added to the group. Each of those is invisible from
//! inside an MCP client, which reports only that the tools are missing. This
//! checks all four and prints them in one pass.

use anyhow::Result;
use std::sync::Arc;

use crate::autostart::Probe;
use crate::config::{self, Cli};
use crate::registry::AgentRegistry;
use crate::telegram::Hub;

pub async fn run(cli: &Cli) -> Result<()> {
    println!("{} {}\n", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

    println!("configuration");
    println!("  source:  {}", config::describe_source(&cli.config_path));
    if config::resolve_config_path(cli.config_path.clone()).is_none() {
        println!("  looked in:");
        for path in config::candidate_config_paths() {
            println!("    {}", path.display());
        }
    }

    let config = match config::load(cli.config_path.clone()) {
        Ok(c) => c,
        Err(err) => {
            println!("\n✗ {err:#}");
            return Ok(());
        }
    };

    let addr = config
        .http_addr
        .clone()
        .unwrap_or_else(|| config::DEFAULT_HTTP_ADDR.to_string());
    println!("  hub addr: {addr}");
    println!("  agents:   {}", config.agent_names());

    // Each token is checked with getMe: it is the one call that proves the
    // token is real without needing a chat, and it returns the @username the
    // agent must be added to the group under.
    println!("\nbots");
    let hub = Arc::new(Hub::new());
    let registry = AgentRegistry::build(&config, hub, reqwest::Client::new())?;
    for session in registry.sessions() {
        match session.get_me().await {
            Ok(me) => println!(
                "  ✓ {:<12} @{}",
                session.id().to_string(),
                me.username.as_deref().unwrap_or("(no username)")
            ),
            Err(err) => println!("  ✗ {:<12} {err:#}", session.id().to_string()),
        }
    }

    println!("\nhub");
    match config::parse_addr(&addr) {
        Ok(sock) => match crate::autostart::probe(&format!("http://{sock}/mcp")).await {
            Probe::Hub => println!("  ✓ running at {sock} — agents started now will join it"),
            Probe::Closed => {
                println!("  · not running; the first agent to start will bring one up")
            }
            // Worth calling out: an unrelated server on the port turns into
            // an unexplained 404 during a client's MCP handshake.
            Probe::Stranger => println!(
                "  ✗ {sock} is held by something that is not a hub. Free the port, or set a \
                 different one under [server] http_addr."
            ),
        },
        Err(err) => println!("  ✗ {err:#}"),
    }
    println!("  log: {}", crate::autostart::log_path().display());

    println!("\nMCP client configuration");
    println!("  Give each client its own agent, for example:\n");
    for name in registry.names() {
        println!("    telegram-agent-mcp --agent {name}");
    }
    println!(
        "\n  Then add every bot above to one Telegram group, and turn privacy\n  \
         mode off for each in @BotFather so they can read the conversation."
    );
    Ok(())
}
