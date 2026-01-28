use crate::commands::*;
use crate::constants::{OK_CODES, QCODES, TCP_PORT};
use crate::error::{DVRIPError, Result};
use crate::protocol::{receive_data, receive_json, receive_packet_header, send_packet};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::Duration;

pub struct DVRIPCam {
    pub(crate) ip: String,
    pub(crate) port: u16,
    pub(crate) timeout: Duration,

    pub(crate) username: Option<String>,

    // Atomic state
    pub(crate) connected: Arc<AtomicBool>,
    pub(crate) authenticated: Arc<AtomicBool>,
    pub(crate) monitoring: Arc<AtomicBool>,
    pub(crate) alarm_monitoring: Arc<AtomicBool>,

    // Atomic counters
    pub(crate) session: Arc<AtomicU32>,
    pub(crate) packet_count: Arc<AtomicU32>,

    // Connection
    pub(crate) stream: Arc<Mutex<Option<TcpStream>>>,

    // Callbacks
    pub(crate) alarm_callback: Arc<Mutex<Option<AlarmCallback>>>,

    // Background tasks
    pub(crate) keep_alive_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    pub(crate) alarm_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,

    // Configuration
    pub(crate) alive_time: Arc<AtomicU64>,
}

impl DVRIPCam {
    pub fn new(ip: impl Into<String>) -> Self {
        let ip = ip.into();
        Self {
            ip,
            username: None,
            port: TCP_PORT,
            timeout: Duration::from_secs(10),
            connected: Arc::new(AtomicBool::new(false)),
            authenticated: Arc::new(AtomicBool::new(false)),
            monitoring: Arc::new(AtomicBool::new(false)),
            alarm_monitoring: Arc::new(AtomicBool::new(false)),
            session: Arc::new(AtomicU32::new(0)),
            packet_count: Arc::new(AtomicU32::new(1)),
            stream: Arc::new(Mutex::new(None)),
            alarm_callback: Arc::new(Mutex::new(None)),
            keep_alive_handle: Arc::new(Mutex::new(None)),
            alarm_handle: Arc::new(Mutex::new(None)),
            alive_time: Arc::new(AtomicU64::new(20)),
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub(crate) async fn send_command_recv_bin(
        &self,
        msg_id: u16,
        data: Value,
        wait_response: bool,
    ) -> Result<Option<Vec<u8>>> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(DVRIPError::ConnectionError("Not connected".to_string()));
        }

        let mut stream_guard = self.stream.lock().await;
        let stream = stream_guard
            .as_mut()
            .ok_or_else(|| DVRIPError::ConnectionError("Stream not available".to_string()))?;

        // Use split to read and write simultaneously
        // Note: split() consumes the stream, but returns reader and writer that can be used
        let (mut reader, mut writer) = tokio::io::split(stream);

        let session = self.session.load(Ordering::Acquire);
        let packet_count = self.packet_count.fetch_add(1, Ordering::SeqCst);

        let data_bytes = serde_json::to_string(&data)
            .map_err(|e| DVRIPError::SerializationError(e.to_string()))?
            .into_bytes();

        send_packet(&mut writer, session, packet_count, msg_id, &data_bytes, 0).await?;
        writer.flush().await?; // Ensure data was sent

        if !wait_response {
            return Ok(None);
        }

        // Small delay to ensure the server processed the request
        // Similar to sleep(0.1) in Python code
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let header = match receive_packet_header(&mut reader).await {
            Ok(h) => h,
            Err(e) => {
                // If reading header fails, connection may have been closed
                self.connected.store(false, Ordering::Release);
                return Err(e);
            }
        };
        self.session.store(header.session, Ordering::Release);

        let timeout = self.timeout;
        let reply = match receive_data(&mut reader, header.data_len as usize, timeout).await {
            Ok(r) => r,
            Err(e) => {
                // If reading data fails, connection may have been closed
                self.connected.store(false, Ordering::Release);
                return Err(e);
            }
        };

        Ok(Some(reply))
    }

    pub(crate) async fn send_command(
        &self,
        msg_id: u16,
        data: Value,
        wait_response: bool,
    ) -> Result<Option<Value>> {
        let Some(data) = self
            .send_command_recv_bin(msg_id, data, wait_response)
            .await?
            .map(|x| serde_json::from_slice(&x[..x.len() - 2]))
        else {
            return Ok(None);
        };
        data.map_err(|_| DVRIPError::SerializationError("Failed to parse JSON Header".to_owned()))
    }

    pub(crate) async fn get_command(&self, command: &str, code: Option<u32>) -> Result<Value> {
        let msg_id =
            code.unwrap_or_else(|| QCODES.get(command).copied().unwrap_or(0).into()) as u16;

        let session = self.session.load(Ordering::Acquire);
        let data = json!({
            "Name": command,
            "SessionID": format!("0x{:08X}", session)
        });

        let reply = self
            .send_command(msg_id, data, true)
            .await?
            .ok_or_else(|| DVRIPError::ProtocolError("Empty response".to_string()))?;

        if let Some(ret) = reply.get("Ret")
            && let Some(ret_code) = ret.as_u64()
            && OK_CODES.contains(&(ret_code as u32))
            && let Some(cmd_data) = reply.get(command)
        {
            return Ok(cmd_data.clone());
        }

        Ok(reply)
    }

    pub(crate) async fn set_command(
        &self,
        command: &str,
        data: Value,
        code: Option<u32>,
    ) -> Result<Value> {
        let msg_id =
            code.unwrap_or_else(|| QCODES.get(command).copied().unwrap_or(0) as u32) as u16;

        let session = self.session.load(Ordering::Acquire);
        let mut cmd_data = json!({
            "Name": command,
            "SessionID": format!("0x{:08X}", session),
        });
        cmd_data[command] = data;

        let reply = self
            .send_command(msg_id, cmd_data, true)
            .await?
            .ok_or_else(|| DVRIPError::ProtocolError("Empty response".to_string()))?;

        Ok(reply)
    }

    pub(crate) async fn start_keep_alive(&self) {
        let session = self.session.clone();
        let alive_time = self.alive_time.clone();
        let stream = self.stream.clone();
        let connected = self.connected.clone();
        let _ = self.timeout;
        let keep_alive_code = QCODES.get("KeepAlive").copied().unwrap_or(1006);

        let handle = tokio::spawn(async move {
            loop {
                if !connected.load(Ordering::Acquire) {
                    break;
                }

                let interval = Duration::from_secs(alive_time.load(Ordering::Acquire));
                tokio::time::sleep(interval).await;

                let mut stream_guard = stream.lock().await;
                if let Some(s) = stream_guard.as_mut() {
                    let (_, mut writer) = s.split();
                    let session_id = session.load(Ordering::Acquire);
                    let packet_count = 0u32; // Keep alive can use fixed counter

                    let data = json!({
                        "Name": "KeepAlive",
                        "SessionID": format!("0x{:08X}", session_id)
                    });

                    if let Ok(data_bytes) = serde_json::to_string(&data) {
                        // We don't wait for keep-alive response, just send
                        if send_packet(
                            &mut writer,
                            session_id,
                            packet_count,
                            keep_alive_code,
                            data_bytes.as_bytes(),
                            0,
                        )
                        .await
                        .is_err()
                        {
                            connected.store(false, Ordering::Release);
                            break;
                        }
                        // Flush to ensure data was sent
                        if writer.flush().await.is_err() {
                            connected.store(false, Ordering::Release);
                            break;
                        }
                    }
                } else {
                    connected.store(false, Ordering::Release);
                    break;
                }
            }
        });

        *self.keep_alive_handle.lock().await = Some(handle);
    }

    pub(crate) async fn start_alarm_worker(&self) {
        let stream = self.stream.clone();
        let session = self.session.clone();
        let packet_count = self.packet_count.clone();
        let alarm_callback = self.alarm_callback.clone();
        let alarm_monitoring = self.alarm_monitoring.clone();
        let connected = self.connected.clone();
        let timeout = self.timeout;
        let alarm_info_code = QCODES.get("AlarmInfo").copied().unwrap_or(1504);

        let handle = tokio::spawn(async move {
            while alarm_monitoring.load(Ordering::Acquire) && connected.load(Ordering::Acquire) {
                let mut stream_guard = stream.lock().await;
                if let Some(s) = stream_guard.as_mut() {
                    let (mut reader, _) = s.split();

                    match receive_packet_header(&mut reader).await {
                        Ok(header) => {
                            if header.msg_id == alarm_info_code
                                && header.session == session.load(Ordering::Acquire)
                            {
                                match receive_json(&mut reader, header.data_len as usize, timeout)
                                    .await
                                {
                                    Ok(reply) => {
                                        packet_count.fetch_add(1, Ordering::SeqCst);
                                        let callback_guard = alarm_callback.lock().await;
                                        if let Some(ref callback) = *callback_guard
                                            && let Some(name) =
                                                reply.get("Name").and_then(|n| n.as_str())
                                            && let Some(alarm_data) = reply.get(name)
                                        {
                                            callback(alarm_data.clone(), header.packet_count);
                                        }
                                    }
                                    Err(e) => {
                                        // If there's an error reading JSON, connection may have been closed
                                        match &e {
                                            DVRIPError::ConnectionError(_)
                                            | DVRIPError::IoError(_) => {
                                                connected.store(false, Ordering::Release);
                                                break;
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            // If there's an error reading header, connection may have been closed
                            match &e {
                                DVRIPError::ConnectionError(_) | DVRIPError::IoError(_) => {
                                    connected.store(false, Ordering::Release);
                                    break;
                                }
                                _ => {
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                }
                            }
                        }
                    }
                } else {
                    break;
                }
            }
        });

        *self.alarm_handle.lock().await = Some(handle);
    }
}
