//! Last-chance records for a foreground console closing. Console close/logoff
//! can terminate Windows processes before async shutdown or panic logging runs.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::OnceLock;

struct LifecycleLog {
    file: File,
    controls: [String; 5],
    panic: String,
}

static LOG: OnceLock<LifecycleLog> = OnceLock::new();

pub(super) fn install() {
    let Some(home) = crate::util::home_dir() else {
        return;
    };
    let directory = home.join(".codexify/logs");
    let opened = std::fs::create_dir_all(&directory).and_then(|()| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("codexify.log"))
    });
    let Ok(file) = opened else {
        tracing::warn!("could not open Windows process lifecycle log");
        return;
    };
    let pid = std::process::id();
    let controls = [
        "CTRL_C_EVENT",
        "CTRL_BREAK_EVENT",
        "CTRL_CLOSE_EVENT",
        "CTRL_LOGOFF_EVENT",
        "CTRL_SHUTDOWN_EVENT",
    ]
    .map(|event| format!("[process {pid}] Windows console event {event}\n"));
    let _ = LOG.set(LifecycleLog {file, controls, panic:format!("[process {pid}] Rust panic; inspect stderr or the service log for its source location\n")});
    // This observer never handles/suppresses the event. Existing Tokio handlers
    // and Windows' default close behavior retain their normal semantics.
    if unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(observe_control), 1)
    } == 0
    {
        tracing::warn!(error=%std::io::Error::last_os_error(), "could not observe Windows console close events");
    }
}

unsafe extern "system" fn observe_control(event: u32) -> i32 {
    let index = match event {
        0 => 0,
        1 => 1,
        2 => 2,
        5 => 3,
        6 => 4,
        _ => return 0,
    };
    if let Some(log) = LOG.get() {
        // The file and messages are prepared before registration: no allocator,
        // application mutex, tracing subscriber or async runtime is needed here.
        let mut file = &log.file;
        let _ = file.write_all(log.controls[index].as_bytes());
    }
    0
}

pub(super) fn record_panic() {
    if let Some(log) = LOG.get() {
        let mut file = &log.file;
        let _ = file.write_all(log.panic.as_bytes());
    }
}
