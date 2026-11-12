//! Audio and MIDI devices on Windows (WASAPI) and Linux (ALSA), through cpal
//! and midir: the default output, the default input for effects, every
//! MIDI input, and the one MIDI output the user chose (`ctx.midi_out`).

use crate::engine::{Renderer, Shared};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Running device streams and MIDI connections; dropping stops them.
pub struct Devices {
    _output: cpal::Stream,
    _input: Option<cpal::Stream>,
    _midi: Vec<midir::MidiInputConnection<()>>,
    _midi_out: Option<MidiOut>,
}

/// The MIDI output the plugin's messages go to: the `FERRITRON_MIDI_OUT`
/// variable, else the first line of `standalone/midi-out.txt` in Ferritron's
/// data folder. Nothing chosen: nothing is sent.
fn midi_out_name() -> Option<String> {
    let text = match std::env::var("FERRITRON_MIDI_OUT") {
        Ok(name) => name,
        Err(_) => std::fs::read_to_string(crate::engine::data_dir()?.join("standalone").join("midi-out.txt")).ok()?,
    };
    let name = text.lines().next()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Writes the names of the MIDI outputs found to `standalone/midi-out-ports.txt`,
/// so the user can copy one into `midi-out.txt` (the app has no window for it).
fn write_port_list() {
    let Some(dir) = crate::engine::data_dir().map(|d| d.join("standalone")) else { return };
    let Ok(output) = midir::MidiOutput::new("Ferritron") else { return };
    let names: Vec<String> = output.ports().iter().filter_map(|port| output.port_name(port).ok()).collect();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("midi-out-ports.txt"), names.join("\n"));
}

/// The thread that sends the plugin's MIDI. It alone touches the MIDI output;
/// the audio thread only queues messages (`Shared::midi_out`).
struct MidiOut {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MidiOut {
    fn start(shared: &'static Shared, name: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            // Opened on this thread: a connection need not be Send.
            let Ok(output) = midir::MidiOutput::new("Ferritron") else { return };
            let Some(port) = output.ports().into_iter().find(|port| output.port_name(port).ok().as_deref() == Some(name.as_str())) else {
                eprintln!("ferritron: MIDI output \"{name}\" was not found");
                return;
            };
            let Ok(mut connection) = output.connect(&port, "ferritron-out") else {
                eprintln!("ferritron: MIDI output \"{name}\" could not be opened");
                return;
            };
            while !flag.load(Ordering::Acquire) {
                match shared.midi_out.pop() {
                    Some(message) => {
                        let _ = connection.send(&message.bytes[..message.len as usize]);
                    }
                    None => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        });
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for MidiOut {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
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
    write_port_list();
    let midi_out = midi_out_name().map(|name| MidiOut::start(shared, name));
    Ok(Devices { _output: output, _input: input, _midi: midi, _midi_out: midi_out })
}
