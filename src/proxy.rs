use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message, WebSocketStream};
use tracing::{debug, error, info};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Proxy an HTTP request to the given upstream port on 127.0.0.1.
pub async fn proxy_request(
    req: Request<Incoming>,
    upstream_port: u16,
) -> Result<Response<BoxBody<Bytes, BoxError>>, BoxError> {
    // Check for WebSocket upgrade
    if is_websocket_upgrade(&req) {
        return proxy_websocket(req, upstream_port).await;
    }

    let upstream_uri = {
        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");
        format!("http://127.0.0.1:{}{}", upstream_port, path_and_query).parse::<hyper::Uri>()?
    };

    debug!(uri = %upstream_uri, "Proxying HTTP request");

    // Decompose the incoming request and collect the body into bytes,
    // then build a clean outgoing request. Forwarding the raw Incoming
    // body type causes SendRequest errors with hyper's client.
    let (parts, body) = req.into_parts();
    let body_bytes = body.collect().await?.to_bytes();

    let mut builder = Request::builder()
        .method(parts.method)
        .uri(upstream_uri);

    // Copy headers, skipping hop-by-hop headers.
    // The original Host header passes through untouched.
    for (key, value) in &parts.headers {
        match key {
            &hyper::header::CONNECTION | &hyper::header::TRANSFER_ENCODING => continue,
            _ => {
                builder = builder.header(key, value);
            }
        }
    }

    let outgoing = builder.body(Full::new(body_bytes))?;

    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();

    match client.request(outgoing).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            let boxed = body.map_err(|e| -> BoxError { Box::new(e) }).boxed();
            Ok(Response::from_parts(parts, boxed))
        }
        Err(e) => {
            error!(error = %e, "Failed to proxy request to upstream");
            Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("Failed to connect to upstream: {}", e),
            ))
        }
    }
}

/// Proxy a WebSocket connection by connecting to upstream and piping frames
/// bidirectionally via hyper's HTTP upgrade mechanism.
async fn proxy_websocket(
    req: Request<Incoming>,
    upstream_port: u16,
) -> Result<Response<BoxBody<Bytes, BoxError>>, BoxError> {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let upstream_url = format!("ws://127.0.0.1:{}{}", upstream_port, path_and_query);

    info!(url = %upstream_url, "Proxying WebSocket connection");

    // Connect to upstream WebSocket first to ensure it works
    let (upstream_ws, _) = match connect_async(&upstream_url).await {
        Ok(ws) => ws,
        Err(e) => {
            error!(error = %e, "Failed to connect WebSocket to upstream");
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("WebSocket upstream connection failed: {}", e),
            ));
        }
    };

    // Build the 101 Switching Protocols response
    // Extract the key from the client request
    let ws_key = req
        .headers()
        .get("Sec-WebSocket-Key")
        .cloned()
        .unwrap_or_else(|| hyper::header::HeaderValue::from_static(""));

    let ws_accept = tungstenite_accept_key(ws_key.to_str().unwrap_or(""));

    // Get upgrade future from the incoming request
    let upgrade_fut = hyper::upgrade::on(req);

    let response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Accept", ws_accept)
        .body(body_empty())
        .unwrap();

    // Spawn the bidirectional pipe task
    tokio::spawn(async move {
        match upgrade_fut.await {
            Ok(upgraded) => {
                let io = hyper_util::rt::TokioIo::new(upgraded);
                let client_ws = WebSocketStream::from_raw_socket(
                    io,
                    tokio_tungstenite::tungstenite::protocol::Role::Server,
                    None,
                )
                .await;

                pipe_websockets(client_ws, upstream_ws).await;
            }
            Err(e) => {
                error!(error = %e, "WebSocket upgrade failed");
            }
        }
    });

    Ok(response)
}

/// Compute Sec-WebSocket-Accept from a key using SHA-1 + base64.
fn tungstenite_accept_key(key: &str) -> String {
    // Use tungstenite's built-in derive function
    tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes())
}

/// Bidirectionally pipe WebSocket frames between client and upstream.
async fn pipe_websockets<S1, S2>(client_ws: WebSocketStream<S1>, upstream_ws: WebSocketStream<S2>)
where
    S1: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    S2: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut client_sink, mut client_stream) = client_ws.split();
    let (mut upstream_sink, mut upstream_stream) = upstream_ws.split();

    let client_to_upstream = async {
        while let Some(msg) = client_stream.next().await {
            match msg {
                Ok(msg) => {
                    if msg.is_close() {
                        let _ = upstream_sink.send(Message::Close(None)).await;
                        break;
                    }
                    if upstream_sink.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    debug!(error = %e, "Client WebSocket read error");
                    break;
                }
            }
        }
    };

    let upstream_to_client = async {
        while let Some(msg) = upstream_stream.next().await {
            match msg {
                Ok(msg) => {
                    if msg.is_close() {
                        let _ = client_sink.send(Message::Close(None)).await;
                        break;
                    }
                    if client_sink.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    debug!(error = %e, "Upstream WebSocket read error");
                    break;
                }
            }
        }
    };

    tokio::select! {
        _ = client_to_upstream => {},
        _ = upstream_to_client => {},
    }
}

/// Check if a request is a WebSocket upgrade.
fn is_websocket_upgrade(req: &Request<Incoming>) -> bool {
    req.headers()
        .get(hyper::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Create an error response with a text body.
pub fn error_response(status: StatusCode, message: String) -> Response<BoxBody<Bytes, BoxError>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain")
        .body(
            Full::new(Bytes::from(message))
                .map_err(|e| -> BoxError { Box::new(e) })
                .boxed(),
        )
        .unwrap()
}

fn body_empty() -> BoxBody<Bytes, BoxError> {
    http_body_util::Empty::<Bytes>::new()
        .map_err(|e| -> BoxError { match e {} })
        .boxed()
}
