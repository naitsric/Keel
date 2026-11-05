//! Serving the tools over MCP's Streamable HTTP transport, on loopback only.
//!
//! What a request goes through, outermost first:
//! - axum, listening on `127.0.0.1:<port>` and nothing else;
//! - [`authenticate`]: every request carries `Authorization: Bearer <token>`, looked up in the store
//!   each time, so a revoked token stops working at once;
//! - rmcp's `StreamableHttpService`, which refuses a `Host` that is not loopback and any `Origin`
//!   (DNS rebinding, web pages) with 403, and a body over 1 MiB with 413, then speaks the protocol.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::serve::ListenerExt;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use rmcp::model::ErrorCode;
use rmcp::transport::streamable_http_server::session::SessionManager;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::McpServer;
use crate::handler::Handler;

/// Where the server answers MCP: `http://127.0.0.1:<port>/mcp`.
pub const MCP_PATH: &str = "/mcp";

/// Largest request body accepted. Tool arguments are small.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The `Host` values accepted, with any port or none.
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

/// How long [`McpServer::stop`] waits for open connections to finish.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// A legacy session closes after this long without traffic. A person can leave an assistant open
/// for a day and come back to it.
const SESSION_IDLE: Duration = Duration::from_secs(24 * 3600);

/// JSON-RPC code of a request without a valid token, from the range left to implementations.
const UNAUTHORIZED: i32 = -32001;

const MISSING_TOKEN: &str = "Keel's MCP server needs a client token, sent as `Authorization: Bearer keel_…`. \
                             The person creates clients in Keel's settings.";
const UNKNOWN_TOKEN: &str = "Keel does not know this client token, or it was revoked. The person can connect \
                             this client again in Keel's settings.";

/// Whether the server listens, and where. The UI words the error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub running: bool,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub error: Option<StartError>,
}

impl ServerStatus {
    fn listening(addr: SocketAddr) -> Self {
        Self {
            running: true,
            port: Some(addr.port()),
            url: Some(format!("http://{addr}{MCP_PATH}")),
            error: None,
        }
    }

    fn failed(error: &StartError) -> Self {
        Self {
            error: Some(error.clone()),
            ..Self::default()
        }
    }
}

/// Why the server could not start listening.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(
    tag = "code",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum StartError {
    #[error("port {port} is in use")]
    PortInUse { port: u16 },
    #[error("could not listen on port {port}: {message}")]
    Listen { port: u16, message: String },
}

impl StartError {
    fn new(port: u16, error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::AddrInUse => Self::PortInUse { port },
            _ => Self::Listen {
                port,
                message: error.to_string(),
            },
        }
    }
}

/// The listener's state, in the server's core.
#[derive(Default)]
pub(crate) struct Listener {
    /// Held while starting or stopping, so they never interleave.
    running: tokio::sync::Mutex<Option<Running>>,
    status: std::sync::Mutex<ServerStatus>,
}

struct Running {
    /// Stops accepting connections and ends rmcp's streams.
    cancel: CancellationToken,
    sessions: Arc<LocalSessionManager>,
    /// axum's server, until every connection closed.
    task: JoinHandle<io::Result<()>>,
}

impl McpServer {
    /// Starts listening on `127.0.0.1` at `port` (0 picks a free one), stopping first if it listens
    /// already. Returns the address it listens on.
    pub async fn start(&self, port: u16) -> Result<SocketAddr, StartError> {
        let mut running = self.0.listener.running.lock().await;
        if let Some(previous) = running.take() {
            shutdown(previous).await;
        }
        let (listener, addr) = match bind(port).await {
            Ok(bound) => bound,
            Err(error) => {
                let error = StartError::new(port, error);
                self.set_status(ServerStatus::failed(&error));
                return Err(error);
            }
        };
        let cancel = CancellationToken::new();
        let (router, sessions) = self.router(cancel.clone());
        let serve =
            axum::serve(listener, router).with_graceful_shutdown(cancel.clone().cancelled_owned());
        let task = tokio::spawn(serve.into_future());
        *running = Some(Running {
            cancel,
            sessions,
            task,
        });
        self.set_status(ServerStatus::listening(addr));
        Ok(addr)
    }

    /// Stops listening: new connections are refused, and open ones end once their current request
    /// does, or after 5 seconds. Runs that were started keep going: they belong to Keel.
    pub async fn stop(&self) {
        let mut running = self.0.listener.running.lock().await;
        if let Some(previous) = running.take() {
            shutdown(previous).await;
        }
        self.set_status(ServerStatus::default());
    }

    /// Whether the server listens, and where.
    pub fn status(&self) -> ServerStatus {
        self.0
            .listener
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_status(&self, status: ServerStatus) {
        *self
            .0
            .listener
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = status;
    }

    fn router(&self, cancel: CancellationToken) -> (Router, Arc<LocalSessionManager>) {
        let mut sessions = LocalSessionManager::default();
        sessions.session_config.keep_alive = Some(SESSION_IDLE);
        let sessions = Arc::new(sessions);
        let config = StreamableHttpServerConfig::default()
            .with_allowed_hosts(LOOPBACK_HOSTS)
            .enforce_origin_validation()
            .with_legacy_session_mode(true)
            .with_json_response(true)
            .with_max_request_body_bytes(MAX_BODY_BYTES)
            .with_cancellation_token(cancel);
        let server = self.clone();
        let mcp = StreamableHttpService::new(
            move || Ok(Handler::new(server.clone())),
            sessions.clone(),
            config,
        );
        let router = Router::new()
            .route_service(MCP_PATH, mcp)
            .route_layer(middleware::from_fn_with_state(self.clone(), authenticate));
        (router, sessions)
    }
}

async fn shutdown(running: Running) {
    running.cancel.cancel();
    // Closing a session cancels the calls it runs.
    let ids: Vec<_> = running
        .sessions
        .sessions
        .read()
        .await
        .keys()
        .cloned()
        .collect();
    for id in ids {
        let _ = running.sessions.close_session(&id).await;
    }
    let mut task = running.task;
    if tokio::time::timeout(STOP_GRACE, &mut task).await.is_err() {
        task.abort();
        let _ = task.await;
    }
}

async fn bind(
    port: u16,
) -> io::Result<(impl axum::serve::Listener<Addr = SocketAddr>, SocketAddr)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let addr = listener.local_addr()?;
    // Responses are small writes, which Nagle's algorithm would hold back.
    let listener = listener.tap_io(|connection| {
        let _ = connection.set_nodelay(true);
    });
    Ok((listener, addr))
}

/// Lets a request through only with a valid client token, and gives it its client. The tools read
/// it from the HTTP request parts rmcp hands them.
async fn authenticate(
    State(server): State<McpServer>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer_token(request.headers()) else {
        return unauthorized(MISSING_TOKEN, false);
    };
    match server.authenticate(&token) {
        Ok(Some(client)) => {
            request.extensions_mut().insert(client);
            next.run(request).await
        }
        Ok(None) => unauthorized(UNKNOWN_TOKEN, true),
        Err(e) => {
            let message = format!("Keel could not check the client token: {e}");
            json_rpc_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::INTERNAL_ERROR.0,
                &message,
            )
        }
    }
}

/// The token of an `Authorization: Bearer <token>` header.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_owned())
}

/// 401, with the challenge of RFC 6750: `invalid_token` only when a token was sent.
fn unauthorized(message: &str, token_sent: bool) -> Response {
    let mut response = json_rpc_error(StatusCode::UNAUTHORIZED, UNAUTHORIZED, message);
    let challenge = if token_sent {
        r#"Bearer realm="Keel", error="invalid_token""#
    } else {
        r#"Bearer realm="Keel""#
    };
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(challenge),
    );
    response
}

/// A JSON-RPC error without a request id: the body was not read.
fn json_rpc_error(status: StatusCode, code: i32, message: &str) -> Response {
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": null, "error": { "code": code, "message": message } });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}
