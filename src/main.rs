use audio_visualizer::dynamic::live_input::AudioDevAndCfg;
use audio_visualizer::dynamic::window_top_btm::{open_window_connect_audio, TransformFn};
use buttplug::client::ButtplugClientDevice;
use buttplug::{
    client::{ButtplugClient, ScalarValueCommand},
    core::connector::new_json_ws_client_connector,
};
use cpal::{
    traits::{DeviceTrait, HostTrait},
    Device,
};
use futures::executor;
use lowpass_filter::lowpass_filter;
use std::cell::RefCell;
use std::io::{stdin, BufRead};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

static mut TEST_CLIENT_DEVICE: OnceLock<Arc<ButtplugClientDevice>> = OnceLock::new();
static mut LAST_TIME: RefCell<Option<Instant>> = RefCell::new(None);

fn callback_fn(x: &[f32], sampling_rate: f32) -> Vec<f32> {
    let mut data_f32 = x.iter().copied().collect::<Vec<_>>();
    lowpass_filter(&mut data_f32, sampling_rate, 80.0);

    unsafe {
        let last_run = LAST_TIME.borrow().unwrap();
        let potential_past_instant = last_run + Duration::from_millis(100);
        let current_instant = Instant::now();
        if potential_past_instant > current_instant {
            // Return unmodified lowpass filter data
            return data_f32;
        }

        let mut first_waveform = *data_f32.last().unwrap() as f64;
        first_waveform = f64::abs(first_waveform);
        first_waveform *= 10.0;

        let computed_intensity = f64::min(
            first_waveform,
            1.0,
        );
        let buttplug_continuation =
            TEST_CLIENT_DEVICE
                .get()
                .unwrap()
                .vibrate(&ScalarValueCommand::ScalarValue(computed_intensity));
        LAST_TIME.replace(Some(Instant::now()));

        let _ = executor::block_on(buttplug_continuation);
    }

    data_f32
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let connector = new_json_ws_client_connector("ws://localhost:12345/buttplug");
    let client = ButtplugClient::new("subwoofer");
    client.connect(connector).await?;

    // TODO: Actually handle errors
    client.start_scanning().await?;
    client.stop_scanning().await?;

    unsafe {
        TEST_CLIENT_DEVICE
            .set(Arc::clone(&client.devices()[0]))
            .unwrap();
        LAST_TIME.replace(Some(Instant::now()));
    }

    let default_out_dev = select_output_dev();
    let default_out_config = default_out_dev.default_output_config().unwrap().config();
    let default_dev_name = default_out_dev.name()?;
    println!("Using default output device: {}", default_dev_name);

    open_window_connect_audio(
        "Live Audio Lowpass Filter View",
        None,
        None,
        None,
        None,
        "time (seconds)",
        "Amplitude (with Lowpass filter)",
        AudioDevAndCfg::new(Some(default_out_dev), Some(default_out_config)),
        // lowpass filter
        TransformFn::Basic(callback_fn),
    );

    client.disconnect().await?;

    Ok(())
}

/// Helps to select available output devices.
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

/// Helps to select the default output device.
fn select_output_dev() -> cpal::Device {
    let mut devs = list_output_devs();
    assert!(!devs.is_empty(), "no output devices found!");
    if devs.len() == 1 {
        return devs.remove(0).1;
    }
    println!();
    devs.iter().enumerate().for_each(|(i, (name, dev))| {
        println!(
            "  [{}] {} {:?}",
            i,
            name,
            dev.default_output_config().unwrap()
        );
    });
    let mut input = String::new();
    stdin().lock().read_line(&mut input).unwrap();
    let index = (&input[0..1]).parse::<usize>().unwrap();
    devs.remove(index).1
}
