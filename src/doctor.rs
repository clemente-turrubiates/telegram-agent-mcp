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
            Probe::Hub {
                version,
                clients,
                idle_shutdown_secs,
                idle_for_secs,
            } => {
                if version != env!("CARGO_PKG_VERSION") {
                    println!(
                        "  ! running at {sock}, but on {version} rather than {}. It serves the \
                         older code until restarted — close every agent, then start one again.",
                        env!("CARGO_PKG_VERSION")
                    );
                } else {
                    println!("  ✓ running at {sock} — agents started now will join it");
                }
                if let Some(n) = clients {
                    println!("  clients:  {n} bridge process(es) currently connected");
                }
                match (idle_shutdown_secs, idle_for_secs) {
                    (None, _) => println!(
                        "  idle shutdown: off (started with a hand-run --hub, or an older build)"
                    ),
                    // `idle_for_secs` being unset means "not currently
                    // counting down", which is either because a client is
                    // connected or because the watcher hasn't ticked since it
                    // started — `clients` disambiguates which.
                    (Some(after), None) if clients.unwrap_or(0) > 0 => println!(
                        "  idle shutdown: armed at {after}s of no clients — has clients right \
                         now, so not counting down"
                    ),
                    (Some(after), None) => println!(
                        "  idle shutdown: armed at {after}s of no clients — not counting down \
                         yet (just started)"
                    ),
                    (Some(after), Some(idle)) => println!(
                        "  idle shutdown: armed at {after}s — idle for {idle}s; will exit if \
                         that reaches {after}s with nothing connected"
                    ),
                }
            }
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
