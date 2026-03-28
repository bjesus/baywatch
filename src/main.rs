mod config;
mod proxy;
mod service;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use clap::{Parser, Subcommand};
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tracing::{debug, error, info, warn};

use config::Config;
use service::ServiceManager;

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Parser, Debug)]
#[command(name = "baywatch", about = "Lazy-loading reverse proxy daemon")]
struct Cli {
    /// Path to config file (default: $XDG_CONFIG_HOME/baywatch/config.yaml)
    #[arg(short, long, global = true)]
    config: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Stream stdout/stderr of a running service
    Tail {
        /// Name of the service to tail
        service: String,
    },
    /// Restart a service (stops it; next request starts it fresh)
    Restart {
        /// Name of the service to restart
        service: String,
    },
}

/// Resolve the path to the Unix control socket.
/// Uses $XDG_RUNTIME_DIR/baywatch/baywatch.sock, falling back to /tmp/baywatch-<uid>/.
fn socket_path() -> PathBuf {
    let dir = if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("baywatch")
    } else {
        PathBuf::from(format!("/tmp/baywatch-{}", unsafe { libc::getuid() }))
    };
    dir.join("baywatch.sock")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Tail { service }) => run_tail(&service).await,
        Some(Commands::Restart { service }) => run_restart(&service).await,
        None => run_daemon(cli.config.as_deref()).await,
    }
}

/// Run the `baywatch tail <service>` client.
async fn run_tail(service: &str) -> anyhow::Result<()> {
    let path = socket_path();

    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "Could not connect to baywatch daemon ({}): {}",
                path.display(),
                e
            );
            std::process::exit(1);
        }
    };

    let (reader, mut writer) = stream.into_split();

    // Send the command
    writer
        .write_all(format!("tail {}\n", service).as_bytes())
        .await?;

    // Read the first line — either "OK" or "ERR: ..."
    let mut lines = BufReader::new(reader).lines();
    match lines.next_line().await? {
        Some(line) if line == "OK" => {}
        Some(line) => {
            eprintln!("{}", line);
            std::process::exit(1);
        }
        None => {
            eprintln!("Connection closed by daemon");
            std::process::exit(1);
        }
    }

    // Stream log lines to stdout until the connection closes or Ctrl+C
    while let Ok(Some(line)) = lines.next_line().await {
        println!("{}", line);
    }

    Ok(())
}

/// Run the `baywatch restart <service>` client.
async fn run_restart(service: &str) -> anyhow::Result<()> {
    let path = socket_path();

    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "Could not connect to baywatch daemon ({}): {}",
                path.display(),
                e
            );
            std::process::exit(1);
        }
    };

    let (reader, mut writer) = stream.into_split();

    // Send the command
    writer
        .write_all(format!("restart {}\n", service).as_bytes())
        .await?;

    // Read the response
    let mut lines = BufReader::new(reader).lines();
    match lines.next_line().await? {
        Some(line) if line == "OK" => {
            println!("Service '{}' stopped (will restart on next request)", service);
        }
        Some(line) => {
            eprintln!("{}", line);
            std::process::exit(1);
        }
        None => {
            eprintln!("Connection closed by daemon");
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Run the main baywatch daemon.
async fn run_daemon(config_path: Option<&str>) -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Resolve and load config
    let config_path = config::resolve_config_path(config_path);
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

    // Spawn the config file watcher for live reload
    let watch_manager = manager.clone();
    let watch_config_path = config_path.clone();
    let watch_bind = config.bind.clone();
    let watch_port = config.port;
    let watch_domain = config.domain.clone();
    tokio::spawn(async move {
        if let Err(e) = watch_config(watch_config_path, watch_manager, watch_bind, watch_port, watch_domain).await {
            error!(error = %e, "Config watcher exited with error");
        }
    });

    // Start the Unix socket listener for `baywatch tail` clients
    let sock_path = socket_path();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Remove stale socket file from a previous run
    let _ = std::fs::remove_file(&sock_path);

    let unix_listener = UnixListener::bind(&sock_path)?;
    info!(path = %sock_path.display(), "Control socket listening");

    let tail_manager = manager.clone();
    tokio::spawn(async move {
        loop {
            match unix_listener.accept().await {
                Ok((stream, _)) => {
                    let mgr = tail_manager.clone();
                    tokio::spawn(handle_control_client(stream, mgr));
                }
                Err(e) => {
                    warn!(error = %e, "Failed to accept control socket connection");
                }
            }
        }
    });

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
    let shutdown_sock_path = sock_path.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!("Received SIGINT, shutting down all services...");
                shutdown_manager.shutdown_all().await;
                let _ = std::fs::remove_file(&shutdown_sock_path);
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

/// Handle a client connected over the Unix control socket.
/// Protocol: client sends `<command> <service>\n`, daemon responds with
/// "OK\n" (+ streaming for tail) or "ERR: ...\n" and closes.
async fn handle_control_client(stream: tokio::net::UnixStream, manager: Arc<ServiceManager>) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Read the command line
    let line = match lines.next_line().await {
        Ok(Some(line)) => line.trim().to_string(),
        _ => return,
    };

    let (command, service_name) = match line.split_once(' ') {
        Some((cmd, svc)) => (cmd.to_string(), svc.trim().to_string()),
        None => {
            let _ = writer.write_all(b"ERR: expected '<command> <service>'\n").await;
            return;
        }
    };

    if service_name.is_empty() {
        let _ = writer.write_all(b"ERR: missing service name\n").await;
        return;
    }

    match command.as_str() {
        "tail" => handle_tail(service_name, manager, &mut writer).await,
        "restart" => handle_restart(&service_name, manager, &mut writer).await,
        _ => {
            let msg = format!("ERR: unknown command '{}'\n", command);
            let _ = writer.write_all(msg.as_bytes()).await;
        }
    }
}

/// Handle `tail <service>`: subscribe to logs and stream until disconnect.
async fn handle_tail(
    service_name: String,
    manager: Arc<ServiceManager>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) {
    let mut rx = match manager.subscribe_logs(&service_name).await {
        Some(rx) => rx,
        None => {
            let msg = format!("ERR: unknown service '{}'\n", service_name);
            let _ = writer.write_all(msg.as_bytes()).await;
            return;
        }
    };

    debug!(service = %service_name, "Tail client connected");

    if writer.write_all(b"OK\n").await.is_err() {
        return;
    }

    loop {
        match rx.recv().await {
            Ok(line) => {
                let mut buf = line.into_bytes();
                buf.push(b'\n');
                if writer.write_all(&buf).await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                debug!(service = %service_name, skipped = n, "Tail client lagged, continuing");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }

    debug!(service = %service_name, "Tail client disconnected");
}

/// Handle `restart <service>`: stop the service so it restarts on next request.
async fn handle_restart(
    service_name: &str,
    manager: Arc<ServiceManager>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) {
    if !manager.has_service(service_name).await {
        let msg = format!("ERR: unknown service '{}'\n", service_name);
        let _ = writer.write_all(msg.as_bytes()).await;
        return;
    }

    info!(service = %service_name, "Restart requested via control socket");
    manager.stop_service(service_name).await;
    let _ = writer.write_all(b"OK\n").await;
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
        Some(name) if manager.has_service(name).await => name,
        _ => {
            // No matching service, return a helpful 404
            let services: Vec<String> = manager
                .service_names()
                .await
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

/// Watch the config file for changes using inotify (via the `notify` crate).
///
/// We watch the **parent directory** (not the file itself) so that editor
/// rename-and-replace patterns (vim, sed -i) don't break the watch.
///
/// We only react to Create and Rename-To events for our specific filename.
/// This deliberately ignores Access events (Open, Close) which would cause
/// an infinite feedback loop — our own `Config::load()` opens the file for
/// reading, which would re-trigger the watcher.
async fn watch_config(
    config_path: PathBuf,
    manager: Arc<ServiceManager>,
    initial_bind: String,
    initial_port: u16,
    initial_domain: String,
) -> anyhow::Result<()> {
    use notify::{EventKind, RecursiveMode, Watcher};

    let watch_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Config path has no parent directory"))?
        .to_path_buf();

    let file_name = config_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Config path has no file name"))?
        .to_os_string();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(16);

    let mut watcher = notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
        let event = match event {
            Ok(e) => e,
            Err(_) => return,
        };

        // Only react to events that indicate new file content appeared:
        // - Create: a new file was created at this path (echo > file, etc.)
        // - Modify(Name(To)): a file was renamed to this path (vim save)
        // - Modify(Data(_)): file contents were written (in-place edit)
        // Crucially, we do NOT react to Access events (Open, Close) which
        // would cause a feedback loop when we read the config ourselves.
        let dominated = matches!(
            event.kind,
            EventKind::Create(_)
                | EventKind::Modify(notify::event::ModifyKind::Data(_))
                | EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::To))
        );
        if !dominated {
            return;
        }

        // Filter: only care about our specific config file, not swap files etc.
        let dominated = event.paths.iter().any(|p| p.file_name() == Some(&file_name));
        if !dominated {
            return;
        }

        let _ = tx.blocking_send(());
    })?;

    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;
    info!(path = %config_path.display(), "Watching config file for changes");

    while rx.recv().await.is_some() {
        // Manual debounce: wait 500ms for things to settle (vim does multiple
        // filesystem ops per save), then drain any extra signals and reload once.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        while rx.try_recv().is_ok() {}

        info!("Config file changed, reloading...");
        match Config::load(&config_path) {
            Ok(new_config) => {
                apply_config_reload(
                    &manager,
                    &new_config,
                    &initial_bind,
                    initial_port,
                    &initial_domain,
                )
                .await;
            }
            Err(e) => {
                error!(error = %e, "Failed to parse updated config, keeping current config");
            }
        }
    }

    Ok(())
}

/// Apply a config reload by diffing against the current state.
async fn apply_config_reload(
    manager: &ServiceManager,
    new_config: &Config,
    initial_bind: &str,
    initial_port: u16,
    initial_domain: &str,
) {
    // Warn about non-hot-reloadable changes
    if new_config.bind != initial_bind {
        warn!(
            old = initial_bind,
            new = %new_config.bind,
            "bind address changed — restart required to take effect"
        );
    }
    if new_config.port != initial_port {
        warn!(
            old = initial_port,
            new = new_config.port,
            "port changed — restart required to take effect"
        );
    }
    if new_config.domain != initial_domain {
        warn!(
            old = initial_domain,
            new = %new_config.domain,
            "domain changed — restart required to take effect"
        );
    }

    // Update global settings
    manager.set_shutdown_grace(new_config.shutdown_grace);
    manager.set_startup_timeout(new_config.startup_timeout);

    let current_names = manager.service_names().await;

    // Remove services that no longer exist in config
    for name in &current_names {
        if !new_config.services.contains_key(name) {
            info!(service = %name, "Removing service (no longer in config)");
            manager.remove_service(name).await;
        }
    }

    // Add or update services
    for (name, new_svc_config) in &new_config.services {
        if let Some(current_config) = manager.get_service_config(name).await {
            if current_config != *new_svc_config {
                info!(service = %name, "Updating service config (will restart on next request)");
                manager.update_service_config(name, new_svc_config.clone()).await;
            }
        } else {
            info!(service = %name, "Adding new service");
            manager.add_service(name.clone(), new_svc_config.clone()).await;
        }
    }

    info!("Config reload complete");
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
