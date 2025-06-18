use anyhow::Result;
use audio_visualizer::dynamic::live_input::AudioDevAndCfg;
use audio_visualizer::dynamic::window_top_btm::{open_window_connect_audio, TransformFn};
use buttplug::{
    client::{ButtplugClient, ScalarValueCommand},
    core::connector::new_json_ws_client_connector,
};
use cpal::{
    traits::{DeviceTrait, HostTrait},
    Device,
};
use eframe::egui;
use lowpass_filter::lowpass_filter;
use std::error::Error;
use std::io::{stdin, BufRead};
use std::sync::{Arc, Mutex, OnceLock}; // Import OnceLock
use std::time::Duration;
use tokio::{sync::mpsc, time};

const SAMPLE_LIMIT: usize = 16;

// --- FIX: Create a struct to hold all the state needed by the audio callback ---
struct AudioProcessorState {
    tx: mpsc::Sender<f64>,
    settings: Arc<Mutex<AppSettings>>,
}

// --- FIX: Use a single static OnceLock to hold our state struct ---
// This is necessary because TransformFn::Basic only accepts a `fn` pointer,
// which cannot capture its environment. This static is our workaround.
static AUDIO_STATE: OnceLock<AudioProcessorState> = OnceLock::new();


#[derive(Debug)]
struct AppSettings {
    intensity: f64,
    delay_ms: u64,
    threshold: f64,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            intensity: 10.0,
            delay_ms: 35,
            threshold: 0.005,
        }
    }
}

struct ControlPanelApp {
    settings: Arc<Mutex<AppSettings>>,
}

impl ControlPanelApp {
    fn new(settings: Arc<Mutex<AppSettings>>) -> Self {
        Self { settings }
    }
}

impl eframe::App for ControlPanelApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.style_mut().spacing.slider_width = 250.0;
            ui.heading("Vibration Controls");
            ui.separator();
            let mut settings = self.settings.lock().unwrap();
            ui.add(egui::Slider::new(&mut settings.intensity, 0.0..=1000.0).text("Vibration Intensity"));
            ui.add(egui::Slider::new(&mut settings.delay_ms, 5..=200).text("Instruction Delay (ms)").suffix(" ms"));
            ui.add(egui::Slider::new(&mut settings.threshold, 0.0..=1.0).text("Minimum Threshold"));
            ui.separator();
            ui.label("Close this window and the visualizer to exit.");
        });
    }
}

fn main() -> std::result::Result<(), Box<dyn Error>> {
    let settings = Arc::new(Mutex::new(AppSettings::default()));
    let settings_clone = Arc::clone(&settings);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            if let Err(e) = run_vibration_logic(settings_clone).await {
                eprintln!("Vibration logic failed: {}", e);
            }
        });
    });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([440.0, 140.0]),
        ..Default::default()
    };
    
    eframe::run_native(
        "Subwoofer Control Panel",
        native_options,
        Box::new(|_cc| Ok(Box::new(ControlPanelApp::new(settings)))),
    )?;
    
    Ok(())
}

// --- FIX: Create a standalone function that can be used as a fn pointer ---
// This function does not capture any variables. Instead, it gets its state
// from the global `AUDIO_STATE` static.
// --- UPDATED to affect the visualizer graph ---
fn audio_transform_fn(direct_values: &[f32], sampling_rate: f32) -> Vec<f32> {
    // Get the global state. Panic if it's not initialized.
    let state = AUDIO_STATE.get().expect("AUDIO_STATE not initialized");

    // Get the current settings from the GUI.
    let (intensity, threshold) = {
        let s = state.settings.lock().unwrap();
        (s.intensity, s.threshold)
    };

    // Apply the lowpass filter first.
    let mut raw_values = direct_values.to_vec();
    lowpass_filter(&mut raw_values, sampling_rate, 80.0);

    // --- Vibration Value Calculation (same as before) ---
    // We still calculate a single value to send to the vibrator logic.
    // This part is unchanged.
    let mut vibration_value = *raw_values.last().unwrap_or(&0.0) as f64;
    vibration_value = f64::abs(vibration_value);
    if vibration_value < threshold {
        vibration_value = 0.0;
    }
    vibration_value *= intensity;
    // Use the sender from the global state to send the vibration command.
    let _ = state.tx.try_send(vibration_value);


    // --- NEW: Visualizer Data Modification ---
    // Now, we modify the *entire* dataset that will be returned for plotting.
    // This is what makes the graph change in real-time.
    for sample in raw_values.iter_mut() {
        // Note: `sample` is &mut f32, while settings are f64.
        let sample_abs = sample.abs() as f64;

        if sample_abs < threshold {
            *sample = 0.0; // Apply threshold visually, flattening small waves.
        } else {
            // Apply intensity visually, making waves taller or shorter.
            // We must cast intensity back to f32 for the multiplication.
            *sample *= intensity as f32;
        }
    }

    // Return the modified vector, which will now be plotted by the visualizer.
    raw_values
}

async fn run_vibration_logic(settings: Arc<Mutex<AppSettings>>) -> Result<()> {
    let connector = new_json_ws_client_connector("ws://localhost:12345/buttplug");
    let client = ButtplugClient::new("subwoofer");

    println!("Connecting to Buttplug server...");
    client.connect(connector).await?;
    client.start_scanning().await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    client.stop_scanning().await?;

    let all_devices = client.devices();
    let Some(client_device) = all_devices.first() else {
        panic!("No Buttplug device found! Please ensure a device is connected.");
    };
    println!("Device connected: {}", client_device.name());

    let (tx, mut rx) = mpsc::channel::<f64>(SAMPLE_LIMIT);

    // --- FIX: Initialize the global state before starting the audio thread ---
    let initial_state = AudioProcessorState {
        tx,
        settings: Arc::clone(&settings),
    };
    if AUDIO_STATE.set(initial_state).is_err() {
        panic!("AUDIO_STATE was already initialized");
    }

    let default_out_dev = select_output_dev();
    let default_out_config = default_out_dev.default_output_config().unwrap().config();
    println!("Using audio device: {}", default_out_dev.name()?);

    tokio::spawn(async move {
        open_window_connect_audio(
            "Live Audio Lowpass Filter View",
            None, None, None, None,
            "time (seconds)",
            "Amplitude (with Lowpass filter)",
            AudioDevAndCfg::new(Some(default_out_dev), Some(default_out_config)),
            // --- FIX: Pass the standalone function pointer here ---
            TransformFn::Basic(audio_transform_fn),
        );
    });

    loop {
        let mut collected_values: Vec<f64> = Vec::with_capacity(SAMPLE_LIMIT);
        if rx.recv_many(&mut collected_values, SAMPLE_LIMIT).await == 0 {
            println!("Audio stream closed. Exiting vibration loop.");
            break;
        }

        let collected_length = collected_values.len();
        let mean_value: f64 = if collected_length > 0 {
            collected_values.iter().sum::<f64>() / collected_length as f64
        } else {
            0.0
        };

        let computed_intensity = f64::min(mean_value, 1.0);

        if let Err(e) = client_device.vibrate(&ScalarValueCommand::ScalarValue(computed_intensity)).await {
            eprintln!("Failed to send vibrate command: {}. Disconnecting.", e);
            break;
        }

        let delay = { settings.lock().unwrap().delay_ms };
        time::sleep(Duration::from_millis(delay)).await;
    }

    println!("Disconnecting from Buttplug server.");
    client.disconnect().await?;

    Ok(())
}


// --- Unchanged Helper Functions ---
pub fn list_output_devs() -> Vec<(String, cpal::Device)> {
    let host = cpal::default_host();
    type DeviceName = String;
    let mut devs: Vec<(DeviceName, Device)> = host
        .output_devices()
        .unwrap()
        .map(|dev| {
            (
                dev.name().unwrap_or_else(|_| String::from("<unknown>")),
                dev,
            )
        })
        .collect();
    devs.sort_by(|(n1, _), (n2, _)| n1.cmp(n2));
    devs
}

fn select_output_dev() -> cpal::Device {
    let mut devs = list_output_devs();
    assert!(!devs.is_empty(), "no output devices found!");
    if devs.len() == 1 {
        return devs.remove(0).1;
    }
    println!("Please select the audio device to monitor:");
    devs.iter().enumerate().for_each(|(i, (name, _))| {
        println!("  [{}] {}", i, name);
    });
    loop {
        let mut input = String::new();
        if stdin().lock().read_line(&mut input).is_err() {
            println!("Failed to read line, please try again.");
            continue;
        }
        if let Ok(index) = input.trim().parse::<usize>() {
            if index < devs.len() {
                return devs.remove(index).1;
            }
        }
        println!("Invalid input. Please enter a number from the list.");
    }
}
