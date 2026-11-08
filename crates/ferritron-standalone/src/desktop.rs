//! Audio and MIDI devices on Windows (WASAPI) and Linux (ALSA), through cpal
//! and midir: the default output, the default input for effects, and every
//! MIDI input.

use crate::engine::{Renderer, Shared};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Running device streams and MIDI connections; dropping stops them.
pub struct Devices {
    _output: cpal::Stream,
    _input: Option<cpal::Stream>,
    _midi: Vec<midir::MidiInputConnection<()>>,
}

pub fn start(shared: &'static Shared) -> Result<Devices, String> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no audio output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let channels = supported.channels() as usize;
    let sample_rate = supported.sample_rate().0;
    if supported.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!("the output device's format is {:?}; Ferritron needs 32-bit float", supported.sample_format()));
    }
    if !shared.activate(sample_rate as f64) {
        return Err("the plugin could not be activated".into());
    }
    let config: cpal::StreamConfig = supported.into();
    let mut renderer = Renderer::new(shared);
    let output = device
        .build_output_stream(&config, move |data: &mut [f32], _| renderer.render(data, channels), |e| eprintln!("audio output: {e}"), None)
        .map_err(|e| e.to_string())?;
    output.play().map_err(|e| e.to_string())?;

    // Effects process the default input, if it runs at the output's rate.
    let input = if shared.instrument {
        None
    } else {
        host.default_input_device().and_then(|device| {
            let supported = device.default_input_config().ok()?;
            if supported.sample_rate().0 != sample_rate || supported.sample_format() != cpal::SampleFormat::F32 {
                eprintln!("audio input: {} Hz / {:?} does not match the output; no input", supported.sample_rate().0, supported.sample_format());
                return None;
            }
            let channels = supported.channels() as usize;
            let stream = device
                .build_input_stream(&supported.into(), move |data: &[f32], _| shared.push_input(data, channels), |e| eprintln!("audio input: {e}"), None)
                .ok()?;
            stream.play().ok()?;
            Some(stream)
        })
    };

    let mut midi = Vec::new();
    if let Ok(probe) = midir::MidiInput::new("Ferritron") {
        for port in probe.ports() {
            let Ok(input) = midir::MidiInput::new("Ferritron") else { continue };
            if let Ok(connection) = input.connect(&port, "ferritron-in", move |_, bytes, _| shared.midi(bytes), ()) {
                midi.push(connection);
            }
        }
    }
    Ok(Devices { _output: output, _input: input, _midi: midi })
}
