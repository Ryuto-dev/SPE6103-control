//! SPE6103 SCPI backend (SPEC §2 / M1 + M1補足:電圧フィードバック補正はフロント側制御).
//! 115200 8N1, TX terminated with `\n`, RX with `\r\n`.
//! SPEC癖: 書込み無応答→同一トランザクション内で先に `*IDN?` 確認 (他機器への誤送信防止)。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const BAUD: u32 = 115_200;
const V_MAX: f64 = 60.0;
const A_MAX: f64 = 10.0;
const P_MAX: f64 = 300.0;

/// 自動スキャンのヒット結果.
#[derive(Serialize)]
struct ScanHit {
    port: String,
    idn: String,
}

/// 1秒ポーリング用スナップショット (生文字のまま返し、解釈はフロントで行う).
#[derive(Serialize)]
struct Snapshot {
    set_volt: String,
    set_curr: String,
    lim_volt: String,
    lim_curr: String,
    meas_volt: String,
    meas_curr: String,
    meas_pow: String,
    info: String,
    outp: String,
}

/// 設定適用の読戻し結果.
#[derive(Serialize)]
struct ApplyResult {
    volt: String,
    curr: String,
}

/// OVP/OCP設定の読戻し結果 (M3).
#[derive(Serialize)]
struct ProtectResult {
    ovp: String,
    ocp: String,
}

/// プリセット1件 (M3). 保存先はカレント直下 `presets.json`.
#[derive(Serialize, Deserialize, Clone)]
struct Preset {
    id: String,
    name: String,
    volt: f64,
    curr: f64,
    ovp: Option<f64>,
    ocp: Option<f64>,
    note: String,
    updated: String,
}

/// CSVログの状態 (M2). ファイル名はフロント側で `spe6103_YYYYMMDD_HHMMSS.csv` を生成.
struct LogState {
    path: std::path::PathBuf,
    lines: u64,
    bytes: u64,
    rot: u32,
}

static LOG: OnceLock<Mutex<Option<LogState>>> = OnceLock::new();

fn log_slot() -> &'static Mutex<Option<LogState>> {
    LOG.get_or_init(|| Mutex::new(None))
}

/// ログ1ファイルの上限 (暫定10MB、超過時は自動ローテーション).
const LOG_SIZE_LIMIT: u64 = 10 * 1024 * 1024;
const LOG_HEADER: &str = "timestamp_iso,v_set,v_meas,a_set,a_meas,w,mode,outp,info\n";

fn log_dir() -> Result<std::path::PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    Ok(cwd.join("logs"))
}

fn write_header(path: &std::path::Path) -> Result<u64, String> {
    use std::fs::OpenOptions;
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("create log: {e}"))?;
    f.write_all(LOG_HEADER.as_bytes())
        .map_err(|e| format!("write header: {e}"))?;
    Ok(LOG_HEADER.len() as u64)
}

/// ログ開始. `name` は `spe6103_YYYYMMDD_HHMMSS.csv` 形式を想定 (パストラバーサル防止のため basename のみ使用).
#[tauri::command]
fn log_start(name: String) -> Result<String, String> {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err("ファイル名が不正です".into());
    }
    if !name.ends_with(".csv") {
        return Err("拡張子は.csvのみ".into());
    }
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir logs: {e}"))?;
    let path = dir.join(&name);
    let bytes = write_header(&path)?;
    let mut slot = log_slot().lock().map_err(|e| format!("lock: {e}"))?;
    *slot = Some(LogState {
        path: path.clone(),
        lines: 0,
        bytes,
        rot: 0,
    });
    Ok(path.to_string_lossy().into_owned())
}

#[derive(Serialize)]
struct LogStatus {
    path: String,
    lines: u64,
    bytes: u64,
    rotated: bool,
}

/// 1行追記. サイズ上限超過時は `_rN` ファイルへ自動ローテーション.
#[tauri::command]
fn log_append(line: String) -> Result<LogStatus, String> {
    use std::fs::OpenOptions;
    let mut slot = log_slot().lock().map_err(|e| format!("lock: {e}"))?;
    let st = slot.as_mut().ok_or("ログは開始されていません")?;
    let mut rotated = false;
    if st.bytes + line.len() as u64 + 1 > LOG_SIZE_LIMIT {
        st.rot += 1;
        let stem = st
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "spe6103".into());
        let name = format!("{stem}_r{}.csv", st.rot);
        let path = st.path.with_file_name(name);
        let bytes = write_header(&path)?;
        st.path = path;
        st.lines = 0;
        st.bytes = bytes;
        rotated = true;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&st.path)
        .map_err(|e| format!("open log: {e}"))?;
    f.write_all(line.as_bytes())
        .map_err(|e| format!("append: {e}"))?;
    f.write_all(b"\n").map_err(|e| format!("append: {e}"))?;
    st.lines += 1;
    st.bytes += line.len() as u64 + 1;
    Ok(LogStatus {
        path: st.path.to_string_lossy().into_owned(),
        lines: st.lines,
        bytes: st.bytes,
        rotated,
    })
}

/// ログ停止. パスと行数を返す.
#[tauri::command]
fn log_stop() -> Result<String, String> {
    let mut slot = log_slot().lock().map_err(|e| format!("lock: {e}"))?;
    match slot.take() {
        Some(st) => Ok(format!(
            "{} ({}行)",
            st.path.to_string_lossy(),
            st.lines
        )),
        None => Err("ログは開始されていません".into()),
    }
}

fn open_port(port: &str) -> Result<Box<dyn serialport::SerialPort>, String> {
    serialport::new(port, BAUD)
        .timeout(Duration::from_millis(50))
        .open()
        .map_err(|e| format!("open {port}: {e}"))
}

fn read_line(p: &mut Box<dyn serialport::SerialPort>, timeout: Duration) -> Result<String, String> {
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

fn query(
    p: &mut Box<dyn serialport::SerialPort>,
    cmd: &str,
    timeout: Duration,
) -> Result<String, String> {
    let _ = p.clear(serialport::ClearBuffer::Input);
    p.write_all(format!("{cmd}\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    p.flush().map_err(|e| format!("flush: {e}"))?;
    read_line(p, timeout)
}

/// 書込み前の機種ガード. SPE6103以外には何も送らない.
fn ensure_spe6103(p: &mut Box<dyn serialport::SerialPort>, timeout: Duration) -> Result<String, String> {
    let idn = query(p, "*IDN?", timeout)?;
    if idn.contains("SPE6103") {
        Ok(idn)
    } else {
        Err(format!("SPE6103ではありません: {idn}"))
    }
}

fn sort_ch340_first(ports: &mut Vec<String>) {
    ports.sort_by_key(|d| {
        let l = d.to_lowercase();
        if l.contains("wchusb") || l.contains("usbserial") || l.contains("usb") {
            0
        } else {
            1
        }
    });
}

/// Scan serial ports (CH340 first for convenience).
#[tauri::command]
fn list_serial_ports() -> Vec<String> {
    let mut ports: Vec<String> = serialport::available_ports()
        .map(|ps| ps.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    sort_ch340_first(&mut ports);
    ports
}

/// 自動スキャン: 各ポートに `*IDN?` を投げ、SPE6103を含む最初のポートを返す.
#[tauri::command]
fn psu_scan(timeout_ms: Option<u64>) -> Result<ScanHit, String> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(1200));
    let mut ports: Vec<String> = serialport::available_ports()
        .map(|ps| ps.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    sort_ch340_first(&mut ports);
    for port in ports {
        let mut p = match open_port(&port) {
            Ok(p) => p,
            Err(_) => continue,
        };
        std::thread::sleep(Duration::from_millis(300)); // CH340 settle
        match query(&mut p, "*IDN?", timeout) {
            Ok(idn) if idn.contains("SPE6103") => {
                return Ok(ScanHit { port, idn });
            }
            _ => continue,
        }
    }
    Err("SPE6103が見つかりません。本体電源ON＋USB接続を確認してください。".into())
}

/// Send one SCPI command and read one `\n`-terminated reply (debug用).
#[tauri::command]
fn psu_query(port: String, cmd: String, timeout_ms: Option<u64>) -> Result<String, String> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(1500));
    let mut p = open_port(&port)?;
    query(&mut p, &cmd, timeout)
}

/// 設定V/Aの適用. 送信前ブロック (60V/10A/300W) + `*IDN?` ガード + 読戻し確認.
#[tauri::command]
fn psu_apply(
    port: String,
    volt: f64,
    curr: f64,
    timeout_ms: Option<u64>,
) -> Result<ApplyResult, String> {
    if !(0.0..=V_MAX).contains(&volt) {
        return Err(format!("V範囲外: {volt} (0-{V_MAX}V)"));
    }
    if !(0.0..=A_MAX).contains(&curr) {
        return Err(format!("A範囲外: {curr} (0-{A_MAX}A)"));
    }
    if volt * curr > P_MAX + 1e-9 {
        return Err(format!(
            "電力超過: {volt}V×{curr}A={:.1}W (>300W)",
            volt * curr
        ));
    }
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(1500));
    let mut p = open_port(&port)?;
    std::thread::sleep(Duration::from_millis(200));
    ensure_spe6103(&mut p, timeout)?;
    // 書込みコマンドは応答なし. 間隔をあけて順送.
    p.write_all(format!("VOLT {volt:.3}\n").as_bytes())
        .map_err(|e| format!("write VOLT: {e}"))?;
    p.flush().map_err(|e| format!("flush: {e}"))?;
    std::thread::sleep(Duration::from_millis(200));
    p.write_all(format!("CURR {curr:.3}\n").as_bytes())
        .map_err(|e| format!("write CURR: {e}"))?;
    p.flush().map_err(|e| format!("flush: {e}"))?;
    std::thread::sleep(Duration::from_millis(200));
    let rv = query(&mut p, "VOLT?", timeout)?;
    let ra = query(&mut p, "CURR?", timeout)?;
    Ok(ApplyResult { volt: rv, curr: ra })
}

/// 出力ON/OFF. `*IDN?` ガード + 読戻し確認.
#[tauri::command]
fn psu_outp(port: String, on: bool, timeout_ms: Option<u64>) -> Result<String, String> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(1500));
    let mut p = open_port(&port)?;
    std::thread::sleep(Duration::from_millis(200));
    ensure_spe6103(&mut p, timeout)?;
    let cmd = if on { "OUTP ON" } else { "OUTP OFF" };
    p.write_all(format!("{cmd}\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    p.flush().map_err(|e| format!("flush: {e}"))?;
    std::thread::sleep(Duration::from_millis(300));
    query(&mut p, "OUTP?", timeout)
}

/// OVP/OCP設定. 送信前ブロック (60V/10A) + `*IDN?` ガード + 読戻し確認 (M3).
#[tauri::command]
fn psu_protect(
    port: String,
    ovp: f64,
    ocp: f64,
    timeout_ms: Option<u64>,
) -> Result<ProtectResult, String> {
    if !(0.0..=V_MAX).contains(&ovp) {
        return Err(format!("OVP範囲外: {ovp} (0-{V_MAX}V)"));
    }
    if !(0.0..=A_MAX).contains(&ocp) {
        return Err(format!("OCP範囲外: {ocp} (0-{A_MAX}A)"));
    }
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(1500));
    let mut p = open_port(&port)?;
    std::thread::sleep(Duration::from_millis(200));
    ensure_spe6103(&mut p, timeout)?;
    p.write_all(format!("VOLT:LIM {ovp:.3}\n").as_bytes())
        .map_err(|e| format!("write OVP: {e}"))?;
    p.flush().map_err(|e| format!("flush: {e}"))?;
    std::thread::sleep(Duration::from_millis(200));
    p.write_all(format!("CURR:LIM {ocp:.3}\n").as_bytes())
        .map_err(|e| format!("write OCP: {e}"))?;
    p.flush().map_err(|e| format!("flush: {e}"))?;
    std::thread::sleep(Duration::from_millis(200));
    let rovp = query(&mut p, "VOLT:LIM?", timeout)?;
    let rocp = query(&mut p, "CURR:LIM?", timeout)?;
    Ok(ProtectResult { ovp: rovp, ocp: rocp })
}

fn preset_file() -> Result<std::path::PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    Ok(cwd.join("presets.json"))
}

fn load_presets() -> Result<Vec<Preset>, String> {
    let path = preset_file()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read presets: {e}"))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).map_err(|e| format!("parse presets: {e}"))
}

fn store_presets(ps: &[Preset]) -> Result<(), String> {
    let path = preset_file()?;
    let raw = serde_json::to_string_pretty(ps).map_err(|e| format!("encode presets: {e}"))?;
    std::fs::write(&path, raw).map_err(|e| format!("write presets: {e}"))
}

/// プリセット一覧 (M3).
#[tauri::command]
fn preset_list() -> Result<Vec<Preset>, String> {
    load_presets()
}

/// プリセット保存 (id一致で上書き、なければ追加). 範囲チェック付き.
#[tauri::command]
fn preset_save(p: Preset) -> Result<Vec<Preset>, String> {
    if p.id.trim().is_empty() || p.name.trim().is_empty() {
        return Err("id/nameが空です".into());
    }
    if !(0.0..=V_MAX).contains(&p.volt) || !(0.0..=A_MAX).contains(&p.curr) {
        return Err("V/A範囲外です".into());
    }
    if p.volt * p.curr > P_MAX + 1e-9 {
        return Err("電力超過(>300W)です".into());
    }
    let mut ps = load_presets()?;
    match ps.iter_mut().find(|x| x.id == p.id) {
        Some(x) => *x = p,
        None => ps.push(p),
    }
    store_presets(&ps)?;
    Ok(ps)
}

/// プリセット削除 (M3).
#[tauri::command]
fn preset_delete(id: String) -> Result<Vec<Preset>, String> {
    let mut ps = load_presets()?;
    ps.retain(|x| x.id != id);
    store_presets(&ps)?;
    Ok(ps)
}

/// プリセット取込 (JSON配列or単体、id一致で上書き).
#[tauri::command]
fn preset_import(json: String) -> Result<Vec<Preset>, String> {
    let mut ps = load_presets()?;
    let incoming: Vec<Preset> = if json.trim_start().starts_with('[') {
        serde_json::from_str(&json).map_err(|e| format!("parse import: {e}"))?
    } else {
        let one: Preset = serde_json::from_str(&json).map_err(|e| format!("parse import: {e}"))?;
        vec![one]
    };
    let mut n = 0;
    for p in incoming {
        if p.id.trim().is_empty() || p.name.trim().is_empty() {
            continue;
        }
        if !(0.0..=V_MAX).contains(&p.volt) || !(0.0..=A_MAX).contains(&p.curr) {
            continue;
        }
        match ps.iter_mut().find(|x| x.id == p.id) {
            Some(x) => *x = p,
            None => ps.push(p),
        }
        n += 1;
    }
    store_presets(&ps)?;
    if n == 0 {
        return Err("取込めるプリセットがありません".into());
    }
    Ok(ps)
}
/// 周期ポーリング用スナップショット (読出しのみのため `*IDN?` ガードなしの高速 path).
/// 接続直後は `psu_scan`/`psu_query(*IDN?)` で機種確認済みであること.
#[tauri::command]
fn psu_poll(port: String, timeout_ms: Option<u64>) -> Result<Snapshot, String> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(1200));
    let mut p = open_port(&port)?;
    Ok(Snapshot {
        set_volt: query(&mut p, "VOLT?", timeout)?,
        set_curr: query(&mut p, "CURR?", timeout)?,
        lim_volt: query(&mut p, "VOLT:LIM?", timeout)?,
        lim_curr: query(&mut p, "CURR:LIM?", timeout)?,
        meas_volt: query(&mut p, "MEAS:VOLT?", timeout)?,
        meas_curr: query(&mut p, "MEAS:CURR?", timeout)?,
        meas_pow: query(&mut p, "MEAS:POW?", timeout)?,
        info: query(&mut p, "MEAS:ALL:INFO?", timeout)?,
        outp: query(&mut p, "OUTP?", timeout)?,
    })
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            list_serial_ports,
            psu_scan,
            psu_query,
            psu_apply,
            psu_outp,
            psu_poll,
            psu_protect,
            preset_list,
            preset_save,
            preset_delete,
            preset_import,
            log_start,
            log_append,
            log_stop
        ])
        .run(tauri::generate_context!())
        .expect("failed to run tauri app");
}
