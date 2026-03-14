//! TCP port forwarding from host to VM.
//!
//! When a service publishes ports (e.g., "8080:80"), we bind a TCP listener
//! on the host and proxy connections to the VM's IP.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// A running port forwarder. Proxies TCP connections from a host port to a VM port.
pub struct PortForwarder {
    stop: Arc<Notify>,
    handle: tokio::task::JoinHandle<()>,
    /// The host address being listened on.
    pub bind_addr: SocketAddr,
    /// The host port being listened on.
    pub host_port: u16,
    /// The target address (VM IP + port).
    pub target: SocketAddr,
}

impl PortForwarder {
    /// Start forwarding `host_port` on loopback to `target_ip:target_port`.
    pub async fn start(
        host_port: u16,
        target_ip: &str,
        target_port: u16,
    ) -> Result<Self, PortForwardError> {
        Self::start_on("127.0.0.1", host_port, target_ip, target_port).await
    }

    /// Start forwarding `host_port` on a specific host bind address.
    pub async fn start_on(
        bind_ip: &str,
        host_port: u16,
        target_ip: &str,
        target_port: u16,
    ) -> Result<Self, PortForwardError> {
        let bind_addr: SocketAddr = format!("{}:{}", bind_ip, host_port)
            .parse()
            .map_err(|e| PortForwardError::InvalidBindAddress(format!("{}", e)))?;
        let target: SocketAddr = format!("{}:{}", target_ip, target_port)
            .parse()
            .map_err(|e| PortForwardError::InvalidTarget(format!("{}", e)))?;

        let listener =
            TcpListener::bind(bind_addr)
                .await
                .map_err(|e| PortForwardError::BindFailed {
                    address: bind_addr,
                    detail: format!("{}", e),
                })?;

        tracing::info!(bind = %bind_addr, target = %target, "port forwarder started");

        let stop = Arc::new(Notify::new());
        let stop_clone = Arc::clone(&stop);

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((client, _)) => {
                                tokio::spawn(async move {
                                    proxy(client, target).await;
                                });
                            }
                            Err(e) => {
                                tracing::error!("port forwarder accept error: {}", e);
                                break;
                            }
                        }
                    }
                    _ = stop_clone.notified() => break,
                }
            }
        });

        Ok(PortForwarder {
            stop,
            handle,
            bind_addr,
            host_port,
            target,
        })
    }

    /// Stop forwarding and clean up.
    pub fn stop(self) {
        self.stop.notify_one();
        self.handle.abort();
    }
}

/// Proxy TCP traffic bidirectionally between client and server.
async fn proxy(mut client: TcpStream, target: SocketAddr) {
    let mut server = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        TcpStream::connect(target),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!("port forward connect failed to {}: {}", target, e);
            return;
        }
        Err(_) => {
            tracing::warn!("port forward connect timeout to {}", target);
            return;
        }
    };

    if let Err(e) = tokio::io::copy_bidirectional(&mut client, &mut server).await {
        tracing::warn!("port forward proxy error: {}", e);
    }
}

/// Port forwarding errors.
#[derive(Debug, thiserror::Error)]
pub enum PortForwardError {
    #[error("invalid bind address: {0}")]
    InvalidBindAddress(String),

    #[error("invalid target address: {0}")]
    InvalidTarget(String),

    #[error("cannot bind {address}: {detail}")]
    BindFailed { address: SocketAddr, detail: String },
}
