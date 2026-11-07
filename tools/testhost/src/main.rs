//! `ferritron-testhost <bundle> [--seconds N] [--param NORMALIZED] [--instances N] [--gui SHOT_DIR [--click X Y] [--scale S] [--resize W H]
//!     [--drag X0 Y0 X1 Y1] [--move X Y] [--wheel X Y NOTCHES] [--leave] [--bmp]]`
//! `ferritron-testhost <plugin.clap> --render` (render mode and latency)
//!
//! Runs a VST3 bundle with a constant input and prints the output whenever it
//! changes. Handy for checking hot reload without a DAW. With `--gui`, opens
//! the plugin's editor instead, changes the first parameter from the host,
//! clicks at (X, Y) in the editor and saves screenshots (PNG; BMP with
//! `--bmp`) to SHOT_DIR. `--scale` makes the host announce a content scale
//! factor; `--resize` resizes the editor afterwards (physical pixels, clamped
//! by the plugin). `--drag`, `--move`, `--wheel` and `--leave` are mouse
//! steps run in order after the click (logical pixels), followed by one more
//! screenshot. `--instances N` runs N instances of the plugin in this
//! process, each told it sits on its own track ("Track 1", ...), for plugins
//! whose instances work together.

#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

#[cfg(not(windows))]
fn main() {
    eprintln!("ferritron-testhost runs on Windows only for now");
}

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(windows)]
unsafe extern "C" {
    fn ferritron_testhost_run(
        binary: *const u16,
        seconds: f64,
        param: f64,
        gui: *const u16,
        x: i32,
        y: i32,
        scale: f64,
        resize_w: i32,
        resize_h: i32,
        pulse_at: f64,
    ) -> i32;
    fn ferritron_testhost_shared_runtime(a: *const u16, b: *const u16) -> i32;
    #[cfg(have_clap)]
    fn ferritron_testhost_clap_gui(binary: *const u16, shots: *const u16, x: i32, y: i32, scale: f64, resize_w: i32, resize_h: i32) -> i32;
    #[cfg(have_clap)]
    fn ferritron_testhost_clap_render(binary: *const u16) -> i32;
}

#[cfg(windows)]
fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
fn main() -> ExitCode {
    // `--shared-runtime A.dll B.dll`: two plugin libraries must share one Runtime.
    let all: Vec<String> = std::env::args().skip(1).collect();
    if all.first().map(String::as_str) == Some("--shared-runtime") && all.len() == 3 {
        let (a, b) = (wide(all[1].as_ref()), wide(all[2].as_ref()));
        let code = unsafe { ferritron_testhost_shared_runtime(a.as_ptr(), b.as_ptr()) };
        return if code == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }
    let mut args = std::env::args().skip(1);
    let (mut bundle, mut seconds, mut param, mut gui, mut click) = (None, 10.0, -1.0, None, (0, 0));
    let (mut scale, mut resize, mut pulse_at, mut render) = (0.0, (0, 0), -1.0, false);
    let mut script: Vec<String> = Vec::new();
    // Read by the C++ host; set before it starts any thread.
    let set_env = |name: &str, value: &str| unsafe { std::env::set_var(name, value) };
    while let Some(arg) = args.next() {
        let numbers = |count: usize, args: &mut dyn Iterator<Item = String>| -> String {
            (0..count).map(|_| args.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0).to_string()).collect::<Vec<_>>().join(",")
        };
        match arg.as_str() {
            "--instances" => {
                let n: u32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1);
                set_env("FERRITRON_TESTHOST_INSTANCES", &n.max(1).to_string());
            }
            "--drag" => script.push(format!("drag:{}", numbers(4, &mut args))),
            "--move" => script.push(format!("move:{}", numbers(2, &mut args))),
            "--wheel" => script.push(format!("wheel:{}", numbers(3, &mut args))),
            "--leave" => script.push("leave".into()),
            "--bmp" => set_env("FERRITRON_TESTHOST_BMP", "1"),
            "--seconds" => seconds = args.next().and_then(|v| v.parse().ok()).unwrap_or(seconds),
            "--param" => param = args.next().and_then(|v| v.parse().ok()).unwrap_or(param),
            "--gui" => gui = args.next(),
            "--render" => render = true,
            "--click" => {
                let mut n = || args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                click = (n(), n());
            }
            "--pulse-param-at" => pulse_at = args.next().and_then(|v| v.parse().ok()).unwrap_or(-1.0),
            "--scale" => scale = args.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
            "--resize" => {
                let mut n = || args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                resize = (n(), n());
            }
            _ => bundle = Some(PathBuf::from(arg)),
        }
    }
    if !script.is_empty() {
        set_env("FERRITRON_TESTHOST_SCRIPT", &script.join(";"));
    }
    let Some(bundle) = bundle else {
        eprintln!("usage: ferritron-testhost <bundle.vst3> [--seconds N] [--param NORMALIZED]");
        return ExitCode::FAILURE;
    };
    let gui: Option<Vec<u16>> = gui.map(|d| std::ffi::OsStr::new(&d).encode_wide().chain(Some(0)).collect());
    let gui_ptr = gui.as_ref().map_or(std::ptr::null(), |g| g.as_ptr());
    // A .clap file is the binary itself; for CLAP only the editor checks exist.
    if bundle.extension().is_some_and(|e| e == "clap") {
        #[cfg(have_clap)]
        {
            let wide: Vec<u16> = bundle.as_os_str().encode_wide().chain(Some(0)).collect();
            if render {
                let code = unsafe { ferritron_testhost_clap_render(wide.as_ptr()) };
                return if code == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE };
            }
            if gui_ptr.is_null() {
                eprintln!("for .clap files, pass --gui SHOT_DIR or --render");
                return ExitCode::FAILURE;
            }
            let code = unsafe { ferritron_testhost_clap_gui(wide.as_ptr(), gui_ptr, click.0, click.1, scale, resize.0, resize.1) };
            return if code == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE };
        }
        #[cfg(not(have_clap))]
        {
            eprintln!("built without CLAP_SDK_DIR: CLAP checks are unavailable");
            return ExitCode::FAILURE;
        }
    }
    let name = bundle.file_name().expect("bundle path has a file name");
    let binary = bundle.join("Contents").join("x86_64-win").join(name);
    let wide: Vec<u16> = binary.as_os_str().encode_wide().chain(Some(0)).collect();
    let code =
        unsafe { ferritron_testhost_run(wide.as_ptr(), seconds, param, gui_ptr, click.0, click.1, scale, resize.0, resize.1, pulse_at) };
    if code == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
