use std::{
    collections::HashMap,
    fs,
    net::SocketAddr,
    sync::{atomic::AtomicBool, mpsc::channel, Arc, RwLock},
    time::Duration,
};

use clap::Parser;
#[allow(unused_imports)]
use colored::Colorize;
use log::{debug, info, LevelFilter};
use screenpipe_audio::{
    default_input_device, default_output_device, list_audio_devices, parse_audio_device,
    AudioDevice, DeviceControl,
};

use screenpipe_server::{start_continuous_recording, DatabaseManager, ResourceMonitor, Server};

// keep in mind this is the most important feature ever // TODO: add a pipe and a ⭐️ e.g screen | ⭐️ somehow in ascii ♥️🤓
const DISPLAY: &str = r"
                                            _          
   __________________  ___  ____     ____  (_____  ___ 
  / ___/ ___/ ___/ _ \/ _ \/ __ \   / __ \/ / __ \/ _ \
 (__  / /__/ /  /  __/  __/ / / /  / /_/ / / /_/ /  __/
/____/\___/_/   \___/\___/_/ /_/  / .___/_/ .___/\___/ 
                              /_/     /_/           

";

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// FPS for continuous recording
    #[arg(short, long, default_value_t = 5.0)]
    fps: f64,

    /// Audio chunk duration in seconds
    #[arg(short, long, default_value_t = 30)]
    audio_chunk_duration: u64,

    /// Port to run the server on
    #[arg(short, long, default_value_t = 3030)]
    port: u16,

    /// Disable audio recording
    #[arg(long, default_value_t = false)]
    disable_audio: bool,

    /// Memory usage threshold for restart (in percentage)
    #[arg(long, default_value_t = 80.0)]
    memory_threshold: f64,

    /// Runtime threshold for restart (in minutes)
    #[arg(long, default_value_t = 60)]
    runtime_threshold: u64,

    /// Audio devices to use (can be specified multiple times)
    #[arg(long)]
    audio_device: Vec<String>,

    /// List available audio devices
    #[arg(long)]
    list_audio_devices: bool,

    /// Data directory
    #[arg(long, default_value_t = String::from("./data"))]
    data_dir: String,

    /// Enable debug logging for screenpipe modules
    #[arg(long)]
    debug: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    let cli = Cli::parse();

    let mut builder = env_logger::Builder::new();
    builder
        .filter(None, LevelFilter::Info)
        .filter_module("tokenizers", LevelFilter::Error)
        .filter_module("rusty_tesseract", LevelFilter::Error);

    if cli.debug {
        builder.filter_module("screenpipe", LevelFilter::Debug);
    }

    builder.init();
    // Add warning for Linux and Windows users
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        warn!("Screenpipe hasn't been extensively tested on this OS. We'd love your feedback!");
        println!(
            "{}",
            "Would love your feedback on the UX, let's a 15 min call soon:".bright_yellow()
        );
        println!(
            "{}",
            "https://cal.com/louis030195/screenpipe"
                .bright_blue()
                .underline()
        );
    }
    let all_audio_devices = list_audio_devices()?;

    if cli.list_audio_devices {
        println!("Available audio devices:");
        for (i, device) in all_audio_devices.iter().enumerate() {
            println!("  {}. {}", i + 1, device);
        }
        return Ok(());
    }

    let mut audio_devices = Vec::new();
    let audio_devices_control: Arc<RwLock<HashMap<AudioDevice, Arc<DeviceControl>>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Add all available audio devices to the controls
    for device in &all_audio_devices {
        let device_control = DeviceControl {
            is_running: Arc::new(AtomicBool::new(false)),
            is_paused: Arc::new(AtomicBool::new(false)),
        };
        audio_devices_control
            .write()
            .unwrap()
            .insert(device.clone(), Arc::new(device_control));
    }

    if !cli.disable_audio {
        if cli.audio_device.is_empty() {
            debug!("Using default devices");
            // Use default devices
            if let Ok(input_device) = default_input_device() {
                audio_devices.push(Arc::new(input_device.clone()));
                if let Some(control) = audio_devices_control
                    .write()
                    .unwrap()
                    .get_mut(&input_device)
                {
                    control
                        .is_running
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            if let Ok(output_device) = default_output_device() {
                audio_devices.push(Arc::new(output_device.clone()));
                if let Some(control) = audio_devices_control
                    .write()
                    .unwrap()
                    .get_mut(&output_device)
                {
                    control
                        .is_running
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        } else {
            // Use specified devices
            for d in &cli.audio_device {
                let device = parse_audio_device(d).expect("Failed to parse audio device");
                audio_devices.push(Arc::new(device.clone()));
                if let Some(control) = audio_devices_control.write().unwrap().get_mut(&device) {
                    control
                        .is_running
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    debug!("running audio device: {}", device.to_string());
                }
            }
        }

        if audio_devices.is_empty() {
            eprintln!("No audio devices available. Audio recording will be disabled.");
        } else {
            info!("Using audio devices:");
            for device in &audio_devices {
                info!("  {}", device);
            }
        }
    }

    ResourceMonitor::new(cli.memory_threshold, cli.runtime_threshold)
        .start_monitoring(Duration::from_secs(10)); // Log every 10 seconds

    let local_data_dir = cli.data_dir; // TODO: Use $HOME/.screenpipe/data
    fs::create_dir_all(&local_data_dir)?;
    let local_data_dir = Arc::new(local_data_dir);
    let local_data_dir_record = local_data_dir.clone();
    let db = Arc::new(
        DatabaseManager::new(&format!("{}/db.sqlite", local_data_dir))
            .await
            .unwrap(),
    );
    let db_record = db.clone();
    let db_server = db.clone();

    // Channel for controlling the recorder ! TODO RENAME SHIT
    let (_control_tx, control_rx) = channel();
    let vision_control = Arc::new(AtomicBool::new(true));

    let vision_control_server_clone = vision_control.clone();
    let audio_devices_control_server_clone = audio_devices_control.clone();

    // Start continuous recording in a separate task
    let _recording_task = tokio::spawn({
        async move {
            let audio_chunk_duration = Duration::from_secs(cli.audio_chunk_duration);

            start_continuous_recording(
                db_record,
                local_data_dir_record,
                cli.fps,
                audio_chunk_duration,
                control_rx,
                !cli.disable_audio,
                vision_control,
                audio_devices_control,
            )
            .await
        }
    });

    tokio::spawn(async move {
        let server = Server::new(
            db_server,
            SocketAddr::from(([0, 0, 0, 0], cli.port)),
            vision_control_server_clone,
            audio_devices_control_server_clone,
        );
        server.start().await.unwrap();
    });

    // Wait for the server to start
    info!("Server started on http://localhost:{}", cli.port);

    // print screenpipe in gradient
    println!(
        "\n\n{}",
        DISPLAY
            .truecolor(147, 112, 219)
            .on_truecolor(255, 255, 255)
            .bold()
    );
    println!(
        "\n{}",
        "Extend your human memory with LLM".bright_yellow().italic()
    );
    println!(
        "{}\n\n",
        "Open source | Runs locally | Developer friendly".bright_green()
    );

    // Keep the main thread running
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
