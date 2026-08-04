//! Serving MCP over HTTP so several agents can share one process.
//!
//! Each connected client acts as a specific agent. There are two ways to say
//! which:
//!
//! - `POST /mcp/<agent>` — the route decides, so nothing has to be resolved
//!   per request. This is the reliable path.
//! - `POST /mcp?agent=<name>` — resolved per request from the query string.
//!
//! Both exist because identity resolution at `initialize` alone is not
//! sufficient: a client negotiating protocol `2026-07-28` is served
//! statelessly, which builds a fresh handler for every request and never calls
//! `initialize` at all. Anything that depends on handshake state would then
//! quietly fail for that client only.
//!
//! Note that `?agent=` is *selection*, not authentication — anything that can
//! reach the port can act as any agent and send messages as that bot. The
//! listener is bound to loopback for exactly that reason.

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;

use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;

use crate::registry::AgentRegistry;
use crate::server::TelegramMcpServer;
use crate::telegram::Hub;

/// Builds the router. One `StreamableHttpService` per agent for the explicit
/// routes, plus one that resolves `?agent=` per request.
pub fn router(registry: Arc<AgentRegistry>, hub: Arc<Hub>) -> axum::Router {
    let mut router = axum::Router::new();

    for session in registry.sessions() {
        let bound = Arc::clone(&session);
        let service = StreamableHttpService::new(
            move || Ok(TelegramMcpServer::bound(Arc::clone(&bound))),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );
        router = router.nest_service(&format!("/mcp/{}", session.id()), service);
    }

    let shared = Arc::clone(&registry);
    let service = StreamableHttpService::new(
        move || Ok(TelegramMcpServer::unbound(Arc::clone(&shared))),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let for_health = Arc::clone(&registry);
    let for_health_hub = Arc::clone(&hub);
    let for_heartbeat = Arc::clone(&hub);
    let for_disconnect = Arc::clone(&hub);
    router
        .route(
            "/health",
            axum::routing::get(move || {
                let registry = Arc::clone(&for_health);
                let hub = Arc::clone(&for_health_hub);
                async move {
                    // `clients=`/`idle_after=`/`idle_for=` are extra tokens
                    // appended after the line `--doctor` and autostart's own
                    // probe have always parsed (version, then `agents=[...]`)
                    // — both only ever read the first token off this line, so
                    // adding more here can't break them. `idle_after=-` means
                    // idle shutdown isn't armed on this hub at all;
                    // `idle_for=-` means it is armed but not currently idle
                    // (a client is connected right now).
                    let idle = match hub.idle_shutdown_status() {
                        None => "idle_after=- idle_for=-".to_string(),
                        Some(status) => format!(
                            "idle_after={} idle_for={}",
                            status.after.as_secs(),
                            status
                                .idle_since
                                .map(|t| t.elapsed().as_secs().to_string())
                                .unwrap_or_else(|| "-".to_string())
                        ),
                    };
                    format!(
                        "{HEALTH_MARKER} {} agents=[{}] clients={} {idle}\n",
                        env!("CARGO_PKG_VERSION"),
                        registry.names().join(", "),
                        hub.client_count(),
                    )
                }
            }),
        )
        // A bridge's "I'm still here" and "I'm leaving" — the only signal an
        // autostarted hub has for deciding it is idle enough to shut down.
        // Keyed by the bridge's own process id, not an MCP session: several
        // MCP sessions can share one bridge process, but idle shutdown cares
        // about processes still holding the port open, not protocol state.
        .route(
            "/clients/{id}/heartbeat",
            axum::routing::post(move |axum::extract::Path(id): axum::extract::Path<u32>| {
                let hub = Arc::clone(&for_heartbeat);
                async move {
                    hub.client_heartbeat(id);
                }
            }),
        )
        .route(
            "/clients/{id}",
            axum::routing::delete(move |axum::extract::Path(id): axum::extract::Path<u32>| {
                let hub = Arc::clone(&for_disconnect);
                async move {
                    hub.client_disconnected(id);
                }
            }),
        )
        .nest_service("/mcp", service)
}

/// Identifies this process as a hub, in `/health`.
///
/// An open TCP port is not proof that *our* hub is behind it — an unrelated
/// service on the same port would otherwise be mistaken for one, and the
/// resulting 404s from a client's MCP handshake say nothing about the cause.
pub const HEALTH_MARKER: &str = "telegram-agent-mcp";

/// Claims the port. Separate from [`serve`] so the caller can take the port
/// before starting anything with side effects outside this process.
pub async fn bind(addr: std::net::SocketAddr) -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    registry: Arc<AgentRegistry>,
    hub: Arc<Hub>,
) -> Result<()> {
    let addr = listener
        .local_addr()
        .context("reading the listener's address")?;

    let names = registry.names().join(", ");
    tracing::info!("MCP hub listening on http://{addr}/mcp — agents: [{names}]");
    for name in registry.names() {
        tracing::info!("  http://{addr}/mcp/{name}   (or http://{addr}/mcp?agent={name})");
    }

    axum::serve(listener, router(registry, hub))
        .await
        .context("serving MCP over HTTP")
}

/// Pumps JSON-RPC between stdin/stdout and a hub over HTTP, for MCP clients
/// that can only launch a command.
///
/// This holds no MCP state of its own — it is a wire, not a server. Messages
/// line up exactly: what the stdio side receives from a client is what the
/// HTTP side sends to the hub, and vice versa.
///
/// `select!` is safe here: rmcp's stdio transport keeps partially-read lines
/// buffered across a cancelled `receive()`, so a dropped read resumes rather
/// than losing the request.
pub async fn bridge_stdio_to(url: &str) -> Result<()> {
    use rmcp::RoleServer;
    use rmcp::transport::async_rw::AsyncRwTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use rmcp::transport::{StreamableHttpClientTransport, Transport};

    tracing::info!("bridging stdio to {url}");

    let mut downstream =
        AsyncRwTransport::<RoleServer, _, _>::new(tokio::io::stdin(), tokio::io::stdout());
    // rmcp's `from_uri`/`from_config` build a reqwest client with
    // `pool_max_idle_per_host(0)` (avoiding a Linux Delayed-ACK stall), which
    // means every call opens a fresh TCP connection that lands in TIME_WAIT.
    // This bridge lives for one long-running stdio session and makes many
    // calls against the same hub, so we opt back into keep-alive here — the
    // Linux quirk the upstream default is dodging doesn't apply to us, and
    // without pooling a long session exhausts the local ephemeral port range.
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(4)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building the keep-alive HTTP client for the hub bridge")?;
    let mut upstream = StreamableHttpClientTransport::with_client(
        client.clone(),
        StreamableHttpClientTransportConfig::with_uri(url),
    );

    // Tells the hub this bridge is alive, so an autostarted hub with nothing
    // left connected can shut itself down instead of running forever. Keyed
    // by this process's own id: several MCP sessions can share one bridge
    // process, but this is about the process holding the connection open, not
    // protocol-level session state. A hub not configured for idle shutdown
    // just ignores these — sending them unconditionally is simpler than the
    // bridge having to know whether it's talking to one.
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
    let client_id = std::process::id();
    let origin = reqwest::Url::parse(url)
        .ok()
        .map(|u| u.origin().ascii_serialization());
    let heartbeat_task = origin.clone().map(|origin| {
        let client = client.clone();
        tokio::spawn(async move {
            loop {
                let _ = client
                    .post(format!("{origin}/clients/{client_id}/heartbeat"))
                    .send()
                    .await;
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
            }
        })
    });

    // After stdin closes there may still be replies in flight. Keep draining
    // them until the hub goes quiet, rather than dropping the answer to the
    // client's last request.
    const DRAIN_AFTER_EOF: Duration = Duration::from_millis(500);
    let mut client_open = true;

    loop {
        tokio::select! {
            from_client = downstream.receive(), if client_open => match from_client {
                // stdin closed: stop reading, but keep writing.
                None => client_open = false,
                Some(msg) => upstream
                    .send(msg)
                    .await
                    .with_context(|| format!("forwarding a request to {url}"))?,
            },
            from_hub = upstream.receive() => match from_hub {
                None => break,
                Some(msg) => downstream
                    .send(msg)
                    .await
                    .context("writing a hub response to stdout")?,
            },
            _ = tokio::time::sleep(DRAIN_AFTER_EOF), if !client_open => break,
        }
    }

    let _ = downstream.close().await;
    let _ = upstream.close().await;

    // Best-effort: says goodbye now rather than making the hub wait out the
    // full staleness window. A killed (rather than closed) bridge never
    // reaches this, which is exactly what that staleness window is for.
    if let Some(task) = heartbeat_task {
        task.abort();
    }
    if let Some(origin) = origin {
        let _ = client
            .delete(format!("{origin}/clients/{client_id}"))
            .send()
            .await;
    }
    Ok(())
}

/// Extracts `agent` from a raw query string (`a=1&agent=qwen`).
pub fn agent_from_query(query: &str) -> Option<&str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "agent" && !v.is_empty()).then_some(v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_is_read_from_the_query_string() {
        assert_eq!(agent_from_query("agent=qwen"), Some("qwen"));
        assert_eq!(agent_from_query("x=1&agent=opencode"), Some("opencode"));
        assert_eq!(agent_from_query("agent=a&b=2"), Some("a"));
    }

    #[test]
    fn a_missing_or_empty_agent_is_none() {
        assert_eq!(agent_from_query(""), None);
        assert_eq!(agent_from_query("other=1"), None);
        assert_eq!(agent_from_query("agent="), None);
        // Not a prefix match: `agentic` is a different parameter.
        assert_eq!(agent_from_query("agentic=1"), None);
    }
}
