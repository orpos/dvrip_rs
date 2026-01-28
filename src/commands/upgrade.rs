use crate::Authentication;
use crate::constants::OK_CODES;
use crate::dvrip::DVRIPCam;
use crate::error::Result;
use crate::protocol::{receive_json, receive_packet_header, send_packet};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

pub type UpgradeProgressCallback = Box<dyn Fn(String) + Send + Sync>;

#[async_trait]
pub trait Upgrade: Send + Sync {
    /// Get upgrade information
    async fn get_upgrade_info(&mut self) -> Result<Value>;

    /// Perform system upgrade
    async fn upgrade(
        &mut self,
        filename: &str,
        packet_size: usize,
        progress_callback: Option<UpgradeProgressCallback>,
    ) -> Result<Value>;
}

#[async_trait]
impl Upgrade for DVRIPCam {
    async fn get_upgrade_info(&mut self) -> Result<Value> {
        self.get_command("OPSystemUpgrade", None).await
    }

    async fn upgrade(
        &mut self,
        filename: &str,
        packet_size: usize,
        progress_callback: Option<UpgradeProgressCallback>,
    ) -> Result<Value> {
        // Iniciar upgrade
        let start_data = json!({
            "Action": "Start",
            "Type": "System",
        });

        let reply = self
            .set_command("OPSystemUpgrade", start_data, Some(0x5F0))
            .await?;

        if let Some(ret) = reply.get("Ret").and_then(|r| r.as_u64())
            && !OK_CODES.contains(&(ret as u32))
        {
            return Ok(reply);
        }

        let callback = progress_callback.map(Arc::new);

        // Send file
        let mut file = File::open(filename).await?;
        let mut blocknum = 0u32;
        let file_metadata = file.metadata().await?;
        let file_size = file_metadata.len() as usize;
        let mut sent_bytes = 0usize;

        let mut stream_guard = self.stream.lock().await;
        if let Some(s) = stream_guard.as_mut() {
            let (mut reader, mut writer) = s.split();
            let session = self.session_id();

            loop {
                let mut buffer = vec![0u8; packet_size];
                let bytes_read = file.read(&mut buffer).await?;

                if bytes_read == 0 {
                    break;
                }

                buffer.truncate(bytes_read);

                send_packet(&mut writer, session, blocknum, 0x5F2, &buffer, 0).await?;

                blocknum += 1;
                sent_bytes += bytes_read;

                // Verificar resposta
                let reply_header = receive_packet_header(&mut reader).await?;
                if reply_header.msg_id == 0x5F2 {
                    let reply_data =
                        receive_json(&mut reader, reply_header.data_len as usize, self.timeout)
                            .await?;
                    if let Some(ret) = reply_data.get("Ret").and_then(|r| r.as_u64())
                        && ret != 100
                    {
                        if let Some(cb) = &callback {
                            cb("Upgrade failed".to_string());
                        }
                        return Ok(reply_data);
                    }
                }

                // Progress
                if let Some(cb) = &callback {
                    let progress = (sent_bytes as f64 / file_size as f64) * 100.0;
                    cb(format!("Uploading: {:.1}%", progress));
                }
            }

            let final_packet = vec![0u8; 0];
            send_packet(&mut writer, session, blocknum, 0x5F2, &final_packet, 0).await?;

            // Wait for upgrade start confirmation
            loop {
                let reply_header = receive_packet_header(&mut reader).await?;
                let reply_data =
                    receive_json(&mut reader, reply_header.data_len as usize, self.timeout).await?;

                if let Some(ret) = reply_data.get("Ret").and_then(|r| r.as_u64()) {
                    if ret == 515 {
                        if let Some(cb) = &callback {
                            cb("Upgrade successful".to_string());
                        }
                        return Ok(reply_data);
                    } else if [512, 513, 514].contains(&(ret as u32)) {
                        if let Some(cb) = &callback {
                            cb("Upgrade failed".to_string());
                        }
                        return Ok(reply_data);
                    } else if ret <= 100
                        && let Some(cb) = &callback
                    {
                        cb(format!("Upgrading: {}%", ret));
                    }
                }
            }
        }

        Err(crate::error::DVRIPError::ConnectionError(
            "Stream not available".to_string(),
        ))
    }
}
