mod config;
mod proxy;
mod service;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use clap::Parser;
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use config::Config;
use service::ServiceManager;

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Parser, Debug)]
#[command(name = "baywatch", about = "Lazy-loading reverse proxy daemon")]
struct Cli {
    /// Path to config file (default: $XDG_CONFIG_HOME/baywatch/config.yaml)
    #[arg(short, long)]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Resolve and load config
    let config_path = config::resolve_config_path(cli.config.as_deref());
    info!(path = %config_path.display(), "Loading config");

    let config = Config::load(&config_path)?;

    info!(
        bind = %config.bind,
        port = config.port,
        domain = %config.domain,
        services = config.services.len(),
        "Configuration loaded"
    );

    for (name, svc) in &config.services {
        info!(
            service = %name,
            port = svc.port,
            command = %svc.command,
            idle_timeout = svc.idle_timeout,
            pwd = %svc.pwd.display(),
            "Registered service"
        );
    }

    let addr: SocketAddr = format!("{}:{}", config.bind, config.port).parse()?;
    let domain = config.domain.clone();

    // Create the shared service manager
    let manager = Arc::new(ServiceManager::new(&config));

    // Spawn the idle reaper task
    let reaper_manager = manager.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            reaper_manager.reap_idle_services().await;
        }
    });

    // Spawn signal handler for graceful shutdown
    let shutdown_manager = manager.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!("Received SIGINT, shutting down all services...");
                shutdown_manager.shutdown_all().await;
                std::process::exit(0);
            }
            Err(e) => {
                error!(error = %e, "Failed to listen for ctrl-c");
            }
        }
    });

    // Bind the TCP listener
    let listener = TcpListener::bind(addr).await?;
    info!(addr = %addr, "Baywatch is listening");

    loop {
        let (stream, remote_addr) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let manager = manager.clone();
        let domain = domain.clone();

        tokio::spawn(async move {
            let manager = manager.clone();
            let domain = domain.clone();

            let service = service_fn(move |req: Request<Incoming>| {
                let manager = manager.clone();
                let domain = domain.clone();
                async move {
                    let resp = match handle_request(req, &manager, &domain, remote_addr).await {
                        Ok(resp) => resp,
                        Err(e) => {
                            error!(error = %e, "Unhandled error in request handler");
                            proxy::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("Internal error: {}", e),
                            )
                        }
                    };
                    Ok::<_, hyper::Error>(resp)
                }
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                if !e.is_incomplete_message() {
                    warn!(
                        remote = %remote_addr,
                        error = %e,
                        "Error serving connection"
                    );
                }
            }
        });
    }
}

/// Handle an incoming HTTP request by routing it to the appropriate service.
async fn handle_request(
    req: Request<Incoming>,
    manager: &ServiceManager,
    domain: &str,
    remote_addr: SocketAddr,
) -> Result<Response<BoxBody<Bytes, BoxError>>, BoxError> {
    // Extract the host header
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Strip port from host if present (e.g., "freetar.localhost:80" -> "freetar.localhost")
    let host_no_port = host.split(':').next().unwrap_or(host);

    // Extract the service name by stripping the domain suffix
    let service_name = extract_service_name(host_no_port, domain);

    let service_name = match service_name {
        Some(name) if manager.has_service(name) => name,
        _ => {
            // No matching service, return a helpful 404
            let services: Vec<String> = manager
                .service_names()
                .iter()
                .map(|s| format!("  {}.{}", s, domain))
                .collect();
            let body = format!(
                "Baywatch: No service matched host '{}'\n\nAvailable services:\n{}",
                host,
                services.join("\n")
            );
            return Ok(proxy::error_response(StatusCode::NOT_FOUND, body));
        }
    };

    info!(
        service = %service_name,
        method = %req.method(),
        uri = %req.uri(),
        remote = %remote_addr,
        "Incoming request"
    );

    // Ensure the service is running
    let port = match manager.ensure_running(&service_name).await {
        Ok(port) => port,
        Err(e) => {
            error!(service = %service_name, error = %e, "Failed to start service");
            return Ok(proxy::error_response(
                StatusCode::BAD_GATEWAY,
                format!("Failed to start service '{}': {}", service_name, e),
            ));
        }
    };

    // Record activity for idle tracking
    manager.record_activity(&service_name).await;

    // Proxy the request
    proxy::proxy_request(req, port).await
}

/// Extract the service name from a hostname.
/// e.g., "freetar.localhost" with domain "localhost" -> Some("freetar")
/// e.g., "sub.freetar.localhost" with domain "localhost" -> Some("sub.freetar")
/// e.g., "localhost" with domain "localhost" -> None
/// e.g., "random.com" with domain "localhost" -> None
fn extract_service_name<'a>(host: &'a str, domain: &str) -> Option<&'a str> {
    let suffix = format!(".{}", domain);
    if host.ends_with(&suffix) {
        let name = &host[..host.len() - suffix.len()];
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_service_name() {
        assert_eq!(
            extract_service_name("freetar.localhost", "localhost"),
            Some("freetar")
        );
        assert_eq!(
            extract_service_name("foo.localhost", "localhost"),
            Some("foo")
        );
        assert_eq!(extract_service_name("localhost", "localhost"), None);
        assert_eq!(
            extract_service_name("app.example.com", "example.com"),
            Some("app")
        );
        assert_eq!(extract_service_name("random.com", "localhost"), None);
    }
}
