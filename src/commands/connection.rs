use crate::dvrip::DVRIPCam;
use crate::error::Result;
use async_trait::async_trait;
use std::sync::atomic::Ordering;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::Duration;

#[async_trait]
pub trait Connection: Send + Sync {
    /// Connect to the device
    async fn connect(&mut self, timeout: tokio::time::Duration) -> Result<()>;

    /// Disconnect from the device
    async fn close(&mut self) -> Result<()>;

    /// Check if connected
    fn is_connected(&self) -> bool;

    /// Get the device IP address
    fn ip(&self) -> &str;

    /// Get the device port
    fn port(&self) -> u16;
}

#[async_trait]
impl Connection for DVRIPCam {
    async fn connect(&mut self, timeout: Duration) -> Result<()> {
        self.timeout = timeout;

        let stream: TcpStream =
            tokio::time::timeout(timeout, TcpStream::connect((self.ip.as_str(), self.port)))
                .await
                .map_err(|_| {
                    crate::error::DVRIPError::ConnectionError("Connection timeout".to_string())
                })?
                .map_err(|e| {
                    crate::error::DVRIPError::ConnectionError(format!("Connection error: {}", e))
                })?;

        *self.stream.lock().await = Some(stream);
        self.connected.store(true, Ordering::Release);

        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        self.connected.store(false, Ordering::Release);
        self.authenticated.store(false, Ordering::Release);
        self.monitoring.store(false, Ordering::Release);
        self.alarm_monitoring.store(false, Ordering::Release);

        // Cancel background tasks
        if let Some(handle) = self.keep_alive_handle.lock().await.take() {
            handle.abort();
        }
        if let Some(handle) = self.alarm_handle.lock().await.take() {
            handle.abort();
        }

        if let Some(mut stream) = self.stream.lock().await.take() {
            let _ = stream.shutdown().await;
        }

        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    fn ip(&self) -> &str {
        &self.ip
    }

    fn port(&self) -> u16 {
        self.port
    }
}
