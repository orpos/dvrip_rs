use dvrip_rs::{Authentication, Connection, DVRIPCam, Monitoring};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        println!("Usage: {} <IP> <Username> <Password>", args[0]);
        return Ok(());
    }

    let ip = &args[1];
    let user = &args[2];
    let pass = &args[3];

    let mut cam = DVRIPCam::new(ip);

    println!("Connecting and logging in...");
    cam.connect(Duration::from_secs(5)).await?;

    if !cam.login(user, pass).await? {
        println!("Login failed");
        return Ok(());
    }

    println!("Starting real-time stream...");

    // Define the frame callback
    let callback = Box::new(|frame: Vec<u8>, metadata: dvrip_rs::FrameMetadata| {
        println!(
            "Received frame: {} bytes, Type: {:?}, MIME: {:?}, Size: {:?}x{:?}, Device Time: {:?}",
            frame.len(),
            metadata.frame_type.unwrap_or_else(|| "Unknown".to_string()),
            metadata.media_type.unwrap_or_else(|| "Unknown".to_string()),
            metadata.width.unwrap_or(0),
            metadata.height.unwrap_or(0),
            metadata.datetime
        );
    });

    // Start monitoring on channel 0, main stream ("Main")
    cam.start_monitor(callback, "Main", 0).await?;

    println!("Receiving frames for 15 seconds. Press Ctrl+C to stop early.");
    tokio::time::sleep(Duration::from_secs(15)).await;

    println!("Stopping stream...");
    cam.stop_monitor().await?;
    cam.close().await?;

    Ok(())
}
