//! SPE6103 SCPI backend (SPEC §2).
//! 115200 8N1, TX terminated with `\n`, RX with `\r\n`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// Scan serial ports (CH340 first for convenience).
#[tauri::command]
fn list_serial_ports() -> Vec<String> {
    let mut ports: Vec<String> = serialport::available_ports()
        .map(|ps| ps.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    ports.sort_by_key(|d| {
        let l = d.to_lowercase();
        if l.contains("wchusb") || l.contains("usbserial") || l.contains("usb") {
            0
        } else {
            1
        }
    });
    ports
}

/// Send one SCPI command and read one `\n`-terminated reply.
#[tauri::command]
fn psu_query(port: String, cmd: String, timeout_ms: Option<u64>) -> Result<String, String> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(1500));
    let mut p = serialport::new(&port, 115_200)
        .timeout(Duration::from_millis(50))
        .open()
        .map_err(|e| format!("open {port}: {e}"))?;
    p.write_all(format!("{cmd}\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    p.flush().map_err(|e| format!("flush: {e}"))?;

    let start = Instant::now();
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if start.elapsed() > timeout {
            return Err("timeout waiting for reply".into());
        }
        match p.read(&mut byte) {
            Ok(1) => {
                buf.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    Ok(String::from_utf8_lossy(&buf).trim().to_string())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![list_serial_ports, psu_query])
        .run(tauri::generate_context!())
        .expect("failed to run tauri app");
}
