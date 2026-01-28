use crate::dvrip::DVRIPCam;
use crate::error::Result;
use crate::protocol::{receive_data, receive_packet_header};
use async_trait::async_trait;
use chrono::{DateTime, Local};
use serde_json::{Value, json};
use std::path::Path;
use tokio::{fs::File, io::AsyncWriteExt};

#[async_trait]
pub trait FileManagement: Send + Sync {
    /// List local files on the device
    async fn list_local_files(
        &mut self,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        file_type: &str,
        channel: u8,
    ) -> Result<Vec<Value>>;

    /// Download a file from the device
    async fn download_file(
        &mut self,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        filename: &str,
        target_path: &str,
    ) -> Result<()>;
}

#[async_trait]
impl FileManagement for DVRIPCam {
    async fn list_local_files(
        &mut self,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        file_type: &str,
        channel: u8,
    ) -> Result<Vec<Value>> {
        let start_str = start_time.format("%Y-%m-%d %H:%M:%S").to_string();
        let end_str = end_time.format("%Y-%m-%d %H:%M:%S").to_string();

        let data = json!({
            "Name": "OPFileQuery",
            "OPFileQuery": {
                "BeginTime": start_str,
                "Channel": channel,
                "DriverTypeMask": "0x0000FFFF",
                "EndTime": end_str,
                "Event": "*",
                "StreamType": "0x00000000",
                "Type": file_type,
            },
        });

        let mut reply = self
            .send_command(1440, data, true)
            .await?
            .ok_or_else(|| crate::error::DVRIPError::ProtocolError("Empty response".to_string()))?;

        let mut result = Vec::new();

        if let Some(ret) = reply.get("Ret").and_then(|r| r.as_u64())
            && ret != 100
        {
            return Ok(vec![]);
        }

        if let Some(files) = reply.get_mut("OPFileQuery").and_then(|f| f.as_array_mut()) {
            result.append(files);
        }

        // OPFileQuery only returns the first 64 items
        // We need to keep querying until we get all
        while let Some(files) = reply.get("OPFileQuery").and_then(|f| f.as_array()) {
            if files.len() == 64 {
                if let Some(last_file) = files.last() {
                    if let Some(new_start) = last_file.get("BeginTime").and_then(|t| t.as_str()) {
                        let data = json!({
                            "Name": "OPFileQuery",
                            "OPFileQuery": {
                                "BeginTime": new_start,
                                "Channel": channel,
                                "DriverTypeMask": "0x0000FFFF",
                                "EndTime": end_str,
                                "Event": "*",
                                "StreamType": "0x00000000",
                                "Type": file_type,
                            },
                        });

                        reply = self.send_command(1440, data, true).await?.ok_or_else(|| {
                            crate::error::DVRIPError::ProtocolError("Resposta vazia".to_string())
                        })?;

                        if let Some(new_files) = reply.get("OPFileQuery").and_then(|f| f.as_array())
                        {
                            if new_files.is_empty() {
                                break;
                            }
                            result.extend(new_files.clone());
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(result)
    }

    async fn download_file(
        &mut self,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        filename: &str,
        target_path: &str,
    ) -> Result<()> {
        // Create directory if it doesn't exist
        if let Some(parent) = Path::new(target_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let start_str = start_time.format("%Y-%m-%d %H:%M:%S").to_string();
        let end_str = end_time.format("%Y-%m-%d %H:%M:%S").to_string();

        // Claim
        let claim_data = json!({
            "Name": "OPPlayBack",
            "OPPlayBack": {
                "Action": "Claim",
                "Parameter": {
                    "PlayMode": "ByName",
                    "FileName": filename,
                    "StreamType": 0,
                    "Value": 0,
                    "TransMode": "TCP",
                },
                "StartTime": start_str,
                "EndTime": end_str,
            },
        });

        self.send_command(1424, claim_data, true).await?;

        // DownloadStart
        let download_start_data = json!({
            "Name": "OPPlayBack",
            "OPPlayBack": {
                "Action": "DownloadStart",
                "Parameter": {
                    "PlayMode": "ByName",
                    "FileName": filename,
                    "StreamType": 0,
                    "Value": 0,
                    "TransMode": "TCP",
                },
                "StartTime": start_str,
                "EndTime": end_str,
            },
        });

        self.send_command(1420, download_start_data, false).await?;

        // Receber dados do arquivo
        {
            let mut stream_guard = self.stream.lock().await;
            if let Some(s) = stream_guard.as_mut() {
                let (mut reader, _) = s.split();

                // Ler header
                let header = receive_packet_header(&mut reader).await?;

                // Ler primeiro chunk
                let mut file_data =
                    receive_data(&mut reader, header.data_len as usize, self.timeout).await?;

                // Continue reading chunks until receiving one with data_len == 0
                loop {
                    let next_header = receive_packet_header(&mut reader).await?;
                    if next_header.data_len == 0 {
                        break;
                    }
                    let chunk =
                        receive_data(&mut reader, next_header.data_len as usize, self.timeout)
                            .await?;
                    file_data.extend_from_slice(&chunk);
                }
                // Escrever arquivo
                let mut file = File::create(target_path).await?;
                file.write_all(&file_data).await?;
                file.sync_all().await?;
            }
        }

        // DownloadStop
        let download_stop_data = json!({
            "Name": "OPPlayBack",
            "OPPlayBack": {
                "Action": "DownloadStop",
                "Parameter": {
                    "FileName": filename,
                    "PlayMode": "ByName",
                    "StreamType": 0,
                    "TransMode": "TCP",
                    "Channel": 0,
                    "Value": 0,
                },
                "StartTime": start_str,
                "EndTime": end_str,
            },
        });

        self.send_command(1420, download_stop_data, false).await?;

        Ok(())
    }
}
