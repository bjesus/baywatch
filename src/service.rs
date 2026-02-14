use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};
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
    services: HashMap<String, Arc<Mutex<ServiceInner>>>,
    shutdown_grace: u64,
    startup_timeout: u64,
}

impl ServiceManager {
    pub fn new(config: &Config) -> Self {
        let mut services = HashMap::new();

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
        }

        ServiceManager {
            services,
            shutdown_grace: config.shutdown_grace,
            startup_timeout: config.startup_timeout,
        }
    }

    /// Check if a service exists in the config.
    pub fn has_service(&self, name: &str) -> bool {
        self.services.contains_key(name)
    }

    /// Get all service names.
    pub fn service_names(&self) -> Vec<String> {
        self.services.keys().cloned().collect()
    }

    /// Ensure a service is running and ready to accept connections.
    /// Returns the port to proxy to, or an error.
    pub async fn ensure_running(&self, name: &str) -> Result<u16, String> {
        let svc = self
            .services
            .get(name)
            .ok_or_else(|| format!("Unknown service: {}", name))?;

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
                    Duration::from_secs(self.startup_timeout),
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
                self.start_service(name, svc).await
            }
        }
    }

    /// Record activity for a service (called on each proxied request).
    pub async fn record_activity(&self, name: &str) {
        if let Some(svc) = self.services.get(name) {
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
                    Duration::from_secs(self.startup_timeout),
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
        // Inherit stdout so service output is visible in baywatch's log stream.
        // Pipe stderr so we can capture error messages if the process fails.
        let mut cmd = Command::new("sh");
        cmd.arg("-lc")
            .arg(&command_str)
            .current_dir(&pwd)
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::piped())
            .process_group(0); // Create new process group so we can kill the tree

        for (key, value) in &env_vars {
            cmd.env(key, value);
        }

        let child = cmd.spawn();

        let child = match child {
            Ok(c) => c,
            Err(e) => {
                error!(service = name, error = %e, "Failed to spawn service");
                let mut inner = svc.lock().await;
                inner.state = ServiceState::Failed;
                notify.notify_waiters();
                return Err(format!("Failed to spawn {}: {}", name, e));
            }
        };

        {
            let mut inner = svc.lock().await;
            inner.process = Some(child);
        }

        // Poll the port until ready
        let start = Instant::now();
        let timeout_dur = Duration::from_secs(self.startup_timeout);
        let mut interval = tokio::time::interval(Duration::from_millis(200));

        loop {
            interval.tick().await;

            // Check if process has exited
            {
                let mut inner = svc.lock().await;
                if let Some(ref mut process) = inner.process {
                    match process.try_wait() {
                        Ok(Some(status)) => {
                            // Capture stderr for the error message
                            let stderr_msg = if let Some(stderr) = process.stderr.take() {
                                use tokio::io::AsyncReadExt;
                                let mut buf = Vec::new();
                                let mut reader = stderr;
                                let _ = reader.read_to_end(&mut buf).await;
                                let s = String::from_utf8_lossy(&buf);
                                let trimmed = s.trim();
                                if trimmed.is_empty() {
                                    String::new()
                                } else {
                                    // Log full stderr and return last line for the error
                                    error!(service = name, stderr = %trimmed, "Service stderr output");
                                    let last_line = trimmed.lines().last().unwrap_or("").to_string();
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
                            name, self.startup_timeout
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
        for (name, svc) in &self.services {
            let should_stop = {
                let inner = svc.lock().await;
                if inner.state != ServiceState::Running {
                    false
                } else {
                    inner.last_activity.elapsed()
                        > Duration::from_secs(inner.config.sleep_after)
                }
            };

            if should_stop {
                info!(service = name, "Shutting down idle service");
                self.stop_service(name).await;
            }
        }
    }

    /// Stop a specific service.
    pub async fn stop_service(&self, name: &str) {
        let svc = match self.services.get(name) {
            Some(s) => s,
            None => return,
        };

        let mut inner = svc.lock().await;
        if inner.state == ServiceState::Stopped {
            return;
        }

        if let Some(ref process) = inner.process {
            if let Some(pid) = process.id() {
                let pgid = -(pid as i32);
                info!(service = name, pid = pid, "Sending SIGTERM to process group");

                // Send SIGTERM to the process group
                unsafe {
                    libc::kill(pgid, libc::SIGTERM);
                }

                // Wait for grace period, then SIGKILL
                let grace = self.shutdown_grace;
                let svc_clone = svc.clone();
                let name_owned = name.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(grace)).await;
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
        let names: Vec<String> = self.services.keys().cloned().collect();
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

    Ok(()
    )
}
