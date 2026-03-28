use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Mutex, Notify, RwLock};
use tracing::{debug, error, info, warn};

use crate::config::{Config, ServiceConfig};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
    Failed,
}

struct ServiceInner {
    config: ServiceConfig,
    state: ServiceState,
    process: Option<Child>,
    last_activity: Instant,
    ready_notify: Arc<Notify>,
}

pub struct ServiceManager {
    services: RwLock<HashMap<String, Arc<Mutex<ServiceInner>>>>,
    /// Broadcast channel per service for streaming stdout/stderr to `tail` clients.
    log_senders: RwLock<HashMap<String, broadcast::Sender<String>>>,
    shutdown_grace: AtomicU64,
    startup_timeout: AtomicU64,
}

impl ServiceManager {
    pub fn new(config: &Config) -> Self {
        let mut services = HashMap::new();
        let mut log_senders = HashMap::new();

        for (name, svc_config) in &config.services {
            services.insert(
                name.clone(),
                Arc::new(Mutex::new(ServiceInner {
                    config: svc_config.clone(),
                    state: ServiceState::Stopped,
                    process: None,
                    last_activity: Instant::now(),
                    ready_notify: Arc::new(Notify::new()),
                })),
            );
            let (tx, _) = broadcast::channel(256);
            log_senders.insert(name.clone(), tx);
        }

        ServiceManager {
            services: RwLock::new(services),
            log_senders: RwLock::new(log_senders),
            shutdown_grace: AtomicU64::new(config.shutdown_grace),
            startup_timeout: AtomicU64::new(config.startup_timeout),
        }
    }

    /// Check if a service exists in the config.
    pub async fn has_service(&self, name: &str) -> bool {
        self.services.read().await.contains_key(name)
    }

    /// Get all service names.
    pub async fn service_names(&self) -> Vec<String> {
        self.services.read().await.keys().cloned().collect()
    }

    /// Subscribe to a service's log stream (stdout + stderr).
    /// Returns None if the service doesn't exist.
    pub async fn subscribe_logs(&self, name: &str) -> Option<broadcast::Receiver<String>> {
        self.log_senders.read().await.get(name).map(|tx| tx.subscribe())
    }

    /// Get the current config for a service (for diffing during reload).
    pub async fn get_service_config(&self, name: &str) -> Option<ServiceConfig> {
        let services = self.services.read().await;
        let svc = services.get(name)?;
        let inner = svc.lock().await;
        Some(inner.config.clone())
    }

    /// Add a new service to the manager.
    pub async fn add_service(&self, name: String, config: ServiceConfig) {
        let inner = Arc::new(Mutex::new(ServiceInner {
            config,
            state: ServiceState::Stopped,
            process: None,
            last_activity: Instant::now(),
            ready_notify: Arc::new(Notify::new()),
        }));
        self.services.write().await.insert(name.clone(), inner);
        let (tx, _) = broadcast::channel(256);
        self.log_senders.write().await.insert(name, tx);
    }

    /// Remove a service from the manager, stopping it first if running.
    pub async fn remove_service(&self, name: &str) {
        self.stop_service(name).await;
        self.services.write().await.remove(name);
        self.log_senders.write().await.remove(name);
    }

    /// Update a service's config, stopping it first if running.
    /// Next request will start it with the new config.
    pub async fn update_service_config(&self, name: &str, config: ServiceConfig) {
        self.stop_service(name).await;
        let services = self.services.read().await;
        if let Some(svc) = services.get(name) {
            let mut inner = svc.lock().await;
            inner.config = config;
        }
    }

    /// Update global shutdown_grace.
    pub fn set_shutdown_grace(&self, value: u64) {
        self.shutdown_grace.store(value, Ordering::Relaxed);
    }

    /// Update global startup_timeout.
    pub fn set_startup_timeout(&self, value: u64) {
        self.startup_timeout.store(value, Ordering::Relaxed);
    }

    /// Ensure a service is running and ready to accept connections.
    /// Returns the port to proxy to, or an error.
    pub async fn ensure_running(&self, name: &str) -> Result<u16, String> {
        let svc = {
            let services = self.services.read().await;
            services
                .get(name)
                .cloned()
                .ok_or_else(|| format!("Unknown service: {}", name))?
        };

        let startup_timeout = self.startup_timeout.load(Ordering::Relaxed);

        let (state, port, notify) = {
            let inner = svc.lock().await;
            (inner.state, inner.config.port, inner.ready_notify.clone())
        };

        match state {
            ServiceState::Running => {
                // Already running, just update activity
                let mut inner = svc.lock().await;
                inner.last_activity = Instant::now();
                Ok(port)
            }
            ServiceState::Starting => {
                // Someone else is starting it, wait for the notification
                info!(service = name, "Waiting for service to become ready...");
                let timeout = tokio::time::timeout(
                    Duration::from_secs(startup_timeout),
                    notify.notified(),
                );

                match timeout.await {
                    Ok(()) => {
                        let inner = svc.lock().await;
                        if inner.state == ServiceState::Running {
                            Ok(port)
                        } else {
                            Err(format!("Service {} failed to start", name))
                        }
                    }
                    Err(_) => Err(format!(
                        "Timed out waiting for service {} to start",
                        name
                    )),
                }
            }
            ServiceState::Stopped | ServiceState::Failed => {
                // We need to start it
                self.start_service(name, &svc).await
            }
        }
    }

    /// Record activity for a service (called on each proxied request).
    pub async fn record_activity(&self, name: &str) {
        let services = self.services.read().await;
        if let Some(svc) = services.get(name) {
            let mut inner = svc.lock().await;
            inner.last_activity = Instant::now();
        }
    }

    /// Start a service process and wait until it's ready.
    async fn start_service(
        &self,
        name: &str,
        svc: &Arc<Mutex<ServiceInner>>,
    ) -> Result<u16, String> {
        let startup_timeout = self.startup_timeout.load(Ordering::Relaxed);

        let (port, command_str, pwd, env_vars, notify) = {
            let mut inner = svc.lock().await;
            // Double-check state under lock
            if inner.state == ServiceState::Running {
                inner.last_activity = Instant::now();
                return Ok(inner.config.port);
            }
            if inner.state == ServiceState::Starting {
                // Another task is starting it; drop lock and wait
                let notify = inner.ready_notify.clone();
                let port = inner.config.port;
                drop(inner);
                let timeout = tokio::time::timeout(
                    Duration::from_secs(startup_timeout),
                    notify.notified(),
                );
                return match timeout.await {
                    Ok(()) => Ok(port),
                    Err(_) => Err(format!("Timed out waiting for service {}", name)),
                };
            }
            inner.state = ServiceState::Starting;
            inner.ready_notify = Arc::new(Notify::new()); // fresh notify for this start cycle
            (
                inner.config.port,
                inner.config.command.clone(),
                inner.config.pwd.clone(),
                inner.config.env.clone(),
                inner.ready_notify.clone(),
            )
        };

        info!(service = name, command = %command_str, "Starting service...");

        // Use login shell so user PATH (e.g. ~/.local/bin) is available.
        // Pipe both stdout and stderr so we can broadcast them to `tail` clients.
        let mut cmd = Command::new("sh");
        cmd.arg("-lc")
            .arg(&command_str)
            .current_dir(&pwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0); // Create new process group so we can kill the tree

        for (key, value) in &env_vars {
            cmd.env(key, value);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                error!(service = name, error = %e, "Failed to spawn service");
                let mut inner = svc.lock().await;
                inner.state = ServiceState::Failed;
                notify.notify_waiters();
                return Err(format!("Failed to spawn {}: {}", name, e));
            }
        };

        // Take stdout and stderr pipes, spawn tasks to read lines and
        // broadcast them to any connected `tail` clients.
        {
            let log_senders = self.log_senders.read().await;
            if let Some(log_tx) = log_senders.get(name) {
                if let Some(stdout) = child.stdout.take() {
                    let tx = log_tx.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncBufReadExt, BufReader};
                        let mut lines = BufReader::new(stdout).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            // Ignore send errors — just means nobody is listening
                            let _ = tx.send(line);
                        }
                    });
                }
                if let Some(stderr) = child.stderr.take() {
                    let tx = log_tx.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncBufReadExt, BufReader};
                        let mut lines = BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let _ = tx.send(line);
                        }
                    });
                }
            }
        }

        {
            let mut inner = svc.lock().await;
            inner.process = Some(child);
        }

        // Subscribe to the log broadcast so we can capture output if the process
        // fails during startup (stderr pipe was already taken above).
        let mut startup_log_rx = {
            let log_senders = self.log_senders.read().await;
            log_senders.get(name).map(|tx| tx.subscribe())
        };

        // Poll the port until ready
        let start = Instant::now();
        let timeout_dur = Duration::from_secs(startup_timeout);
        let mut interval = tokio::time::interval(Duration::from_millis(200));

        loop {
            interval.tick().await;

            // Check if process has exited
            {
                let mut inner = svc.lock().await;
                if let Some(ref mut process) = inner.process {
                    match process.try_wait() {
                        Ok(Some(status)) => {
                            // Drain any log lines captured during startup
                            let stderr_msg = if let Some(ref mut rx) = startup_log_rx {
                                let mut lines = Vec::new();
                                while let Ok(line) = rx.try_recv() {
                                    lines.push(line);
                                }
                                if lines.is_empty() {
                                    String::new()
                                } else {
                                    let full = lines.join("\n");
                                    error!(service = name, output = %full, "Service output before exit");
                                    let last_line = lines.last().unwrap().clone();
                                    format!(" ({})", last_line)
                                }
                            } else {
                                String::new()
                            };
                            error!(
                                service = name,
                                status = %status,
                                "Service process exited before becoming ready"
                            );
                            inner.state = ServiceState::Failed;
                            inner.process = None;
                            notify.notify_waiters();
                            return Err(format!("Service {} exited with {}{}", name, status, stderr_msg));
                        }
                        Ok(None) => {} // still running
                        Err(e) => {
                            warn!(service = name, error = %e, "Error checking process status");
                        }
                    }
                }
            }

            // Try an HTTP request to verify the service is actually ready.
            // A bare TCP connect is not enough — some tools (e.g. uvx) open
            // the port during their own startup before the app is serving.
            match http_health_check(port).await {
                Ok(()) => {
                    info!(
                        service = name,
                        elapsed_ms = start.elapsed().as_millis(),
                        "Service is ready"
                    );
                    let mut inner = svc.lock().await;
                    inner.state = ServiceState::Running;
                    inner.last_activity = Instant::now();
                    notify.notify_waiters();
                    return Ok(port);
                }
                Err(_) => {
                    if start.elapsed() > timeout_dur {
                        error!(service = name, "Service startup timed out");
                        // Kill the process
                        let mut inner = svc.lock().await;
                        inner.state = ServiceState::Failed;
                        if let Some(ref process) = inner.process {
                            if let Some(pid) = process.id() {
                                unsafe {
                                    libc::kill(-(pid as i32), libc::SIGTERM);
                                }
                            }
                        }
                        inner.process = None;
                        notify.notify_waiters();
                        return Err(format!(
                            "Service {} did not become ready within {}s",
                            name, startup_timeout
                        ));
                    }
                    debug!(
                        service = name,
                        elapsed_ms = start.elapsed().as_millis(),
                        "Waiting for service to be ready..."
                    );
                }
            }
        }
    }

    /// Check all services for inactivity and shut down idle ones.
    pub async fn reap_idle_services(&self) {
        // Collect names to stop while holding the read lock, then release it
        // before calling stop_service (which also takes the read lock).
        let to_stop: Vec<String> = {
            let services = self.services.read().await;
            let mut names = Vec::new();
            for (name, svc) in services.iter() {
                let inner = svc.lock().await;
                if inner.state == ServiceState::Running
                    && inner.last_activity.elapsed()
                        > Duration::from_secs(inner.config.idle_timeout)
                {
                    names.push(name.clone());
                }
            }
            names
        };

        for name in to_stop {
            info!(service = %name, "Shutting down idle service");
            self.stop_service(&name).await;
        }
    }

    /// Stop a specific service.
    pub async fn stop_service(&self, name: &str) {
        let svc = {
            let services = self.services.read().await;
            match services.get(name) {
                Some(s) => s.clone(),
                None => return,
            }
        };

        let mut inner = svc.lock().await;
        if inner.state == ServiceState::Stopped {
            return;
        }

        let shutdown_grace = self.shutdown_grace.load(Ordering::Relaxed);

        if let Some(ref process) = inner.process {
            if let Some(pid) = process.id() {
                let pgid = -(pid as i32);
                info!(service = name, pid = pid, "Sending SIGTERM to process group");

                // Send SIGTERM to the process group
                unsafe {
                    libc::kill(pgid, libc::SIGTERM);
                }

                // Wait for grace period, then SIGKILL
                let svc_clone = svc.clone();
                let name_owned = name.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(shutdown_grace)).await;
                    let mut inner = svc_clone.lock().await;
                    if let Some(ref mut process) = inner.process {
                        match process.try_wait() {
                            Ok(Some(_)) => {
                                // Already exited
                                debug!(service = %name_owned, "Process already exited");
                            }
                            _ => {
                                warn!(
                                    service = %name_owned,
                                    "Process did not exit after grace period, sending SIGKILL"
                                );
                                if let Some(pid) = process.id() {
                                    unsafe {
                                        libc::kill(-(pid as i32), libc::SIGKILL);
                                    }
                                }
                                let _ = process.wait().await;
                            }
                        }
                    }
                    inner.process = None;
                    inner.state = ServiceState::Stopped;
                });
            }
        }

        inner.state = ServiceState::Stopped;
        inner.process = None;
    }

    /// Shut down all running services (for graceful daemon shutdown).
    pub async fn shutdown_all(&self) {
        let names: Vec<String> = self.services.read().await.keys().cloned().collect();
        for name in names {
            self.stop_service(&name).await;
        }
        // Give processes a moment to exit
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Check if a service is actually responding to HTTP on the given port.
/// We send a minimal HEAD request and accept any valid HTTP response
/// (even 4xx/5xx — that means the server is up).
async fn http_health_check(port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stream = TcpStream::connect(format!("127.0.0.1:{}", port)).await?;
    let io = hyper_util::rt::TokioIo::new(stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

    // Drive the connection in the background
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = hyper::Request::builder()
        .method(hyper::Method::HEAD)
        .uri("/")
        .header("Host", format!("127.0.0.1:{}", port))
        .body(http_body_util::Empty::<bytes::Bytes>::new())?;

    // We don't care about the response status — any HTTP response means the server is up
    let _resp = sender.send_request(req).await?;

    Ok(())
}
