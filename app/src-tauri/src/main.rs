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

/// V/A範囲＋電力の送信前ブロック (M1/M4/M5共通).
fn check_va(volt: f64, curr: f64) -> Result<(), String> {
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
    Ok(())
}

fn scpi_apply(port: &str, volt: f64, curr: f64, timeout: Duration) -> Result<ApplyResult, String> {
    check_va(volt, curr)?;
    let mut p = open_port(port)?;
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

fn scpi_outp(port: &str, on: bool, timeout: Duration) -> Result<String, String> {
    let mut p = open_port(port)?;
    std::thread::sleep(Duration::from_millis(200));
    ensure_spe6103(&mut p, timeout)?;
    let cmd = if on { "OUTP ON" } else { "OUTP OFF" };
    p.write_all(format!("{cmd}\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    p.flush().map_err(|e| format!("flush: {e}"))?;
    std::thread::sleep(Duration::from_millis(300));
    query(&mut p, "OUTP?", timeout)
}

/// 設定V/Aの適用. 送信前ブロック (60V/10A/300W) + `*IDN?` ガード + 読戻し確認.
#[tauri::command]
fn psu_apply(
    port: String,
    volt: f64,
    curr: f64,
    timeout_ms: Option<u64>,
) -> Result<ApplyResult, String> {
    scpi_apply(
        &port,
        volt,
        curr,
        Duration::from_millis(timeout_ms.unwrap_or(1500)),
    )
}

/// 出力ON/OFF. `*IDN?` ガード + 読戻し確認.
#[tauri::command]
fn psu_outp(port: String, on: bool, timeout_ms: Option<u64>) -> Result<String, String> {
    scpi_outp(&port, on, Duration::from_millis(timeout_ms.unwrap_or(1500)))
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

/* ================= M5 サーバーモード (stdのみHTTP + qrcode) =================
 * Lv0 監視のみ / Lv1 Lv0＋出力OFF(デフォルト) / Lv2 フル操作(要明示ON＋トークン)。
 * 0.0.0.0 Bind (LAN内利用想定・WAN公開非推奨はREADME参照)。操作ログを残す。 */

use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// LAN側IPの取得. UDP connectはパケットを送らない.
fn lan_ip() -> String {
    if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(a) = s.local_addr() {
                let ip = a.ip().to_string();
                if !ip.starts_with("127.") {
                    return ip;
                }
            }
        }
    }
    "127.0.0.1".into()
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn make_token() -> String {
    use std::hash::{Hash, Hasher};
    static N: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (t, std::process::id(), N.fetch_add(1, Ordering::Relaxed)).hash(&mut h);
    format!("{:016x}{:016x}", h.finish(), (t & 0xffff_ffff_ffff_ffff) as u64)
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

fn device_snapshot_json(device: &str) -> Result<String, String> {
    let timeout = Duration::from_millis(1200);
    let mut p = open_port(device)?;
    let mut g = |cmd: &str| query(&mut p, cmd, timeout);
    let sv = g("VOLT?")?;
    let sa = g("CURR?")?;
    let lv = g("VOLT:LIM?")?;
    let la = g("CURR:LIM?")?;
    let mv = g("MEAS:VOLT?")?;
    let ma = g("MEAS:CURR?")?;
    let mp = g("MEAS:POW?")?;
    let info = g("MEAS:ALL:INFO?")?;
    let outp = g("OUTP?")?;
    Ok(format!(
        "{{\"set_volt\":\"{}\",\"set_curr\":\"{}\",\"lim_volt\":\"{}\",\"lim_curr\":\"{}\",\"meas_volt\":\"{}\",\"meas_curr\":\"{}\",\"meas_pow\":\"{}\",\"info\":\"{}\",\"outp\":\"{}\"}}",
        escape_json(&sv),
        escape_json(&sa),
        escape_json(&lv),
        escape_json(&la),
        escape_json(&mv),
        escape_json(&ma),
        escape_json(&mp),
        escape_json(&info),
        escape_json(&outp)
    ))
}

#[derive(Clone)]
struct SrvCfg {
    device: String,
    level: u8,
    token: String,
    public: String,
    log: std::path::PathBuf,
    ops: std::sync::Arc<Mutex<u64>>,
}

fn ops_log(log: &std::path::Path, ops: &std::sync::Arc<Mutex<u64>>, who: &str, msg: &str) {
    let line = format!("[{}] {} {}", epoch_secs(), who, msg);
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        use std::io::Write as _;
        let mut f = f;
        let _ = writeln!(f, "{line}");
    }
    if let Ok(mut n) = ops.lock() {
        *n += 1;
    }
}

fn http_resp(s: &mut TcpStream, code: u16, text: &str, ctype: &str, body: &str) {
    let h = format!(
        "HTTP/1.1 {code} {text}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(h.as_bytes());
    let _ = s.write_all(body.as_bytes());
}

fn read_http(s: &TcpStream) -> Option<(String, String, HashMap<String, String>, Vec<u8>)> {
    s.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
    let mut s = s; // &TcpStream は Copy。Readにはmut束縛が必要
    let mut buf: Vec<u8> = Vec::new();
    let mut one = [0u8; 1];
    let start = Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(5) || buf.len() > 65536 {
            return None;
        }
        match s.read(&mut one) {
            Ok(1) => {
                buf.push(one[0]);
                if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
                    break;
                }
            }
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => return None,
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let mut lines = head.split("\r\n");
    let req = lines.next().unwrap_or("");
    let mut sp = req.split_whitespace();
    let method = sp.next().unwrap_or("").to_uppercase();
    let target = sp.next().unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    for l in lines {
        if l.is_empty() {
            break;
        }
        if let Some(i) = l.find(':') {
            headers.insert(l[..i].trim().to_lowercase(), l[i + 1..].trim().to_string());
        }
    }
    // &TcpStream に対する read は共有参照でよい (Read for &TcpStream).
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len.min(65536)];
    let mut got = 0;
    while got < body.len() {
        match s.read(&mut body[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                if start.elapsed() > Duration::from_secs(5) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    body.truncate(got);
    Some((method, target, headers, body))
}

fn authed(target: &str, headers: &HashMap<String, String>, token: &str) -> bool {
    if headers.get("x-token").map(|v| v == token).unwrap_or(false) {
        return true;
    }
    if let Some(q) = target.split_once('?').map(|(_, q)| q) {
        for kv in q.split('&') {
            if let Some(v) = kv.strip_prefix("token=") {
                if v == token {
                    return true;
                }
            }
        }
    }
    false
}

const DASH: &str = r##"<!doctype html><html lang="ja"><head><meta charset="utf-8" />
<meta name="viewport" content="width=device-width,initial-scale=1" />
<title>SPE6103 Lv%%LEVEL%%</title>
<style>body{font-family:system-ui,sans-serif;margin:16px}#v{font-size:40px;font-weight:bold}#mode{font-size:24px}button{padding:10px 16px;margin:4px}input{width:90px;padding:8px}</style>
</head><body><h2>SPE6103 サーバー (Lv%%LEVEL%%)</h2>
<div id="v">-- V / -- A / -- W</div><div id="mode">--</div><div id="op"></div>
<div>%%CTLS%%</div>
<script>
const tok = new URLSearchParams(location.search).get('token') || '';
async function st(){
  try{
    const r = await fetch('/api/status'); const s = await r.json();
    document.getElementById('v').textContent = s.meas_volt + ' V / ' + s.meas_curr + ' A / ' + s.meas_pow + ' W';
    document.getElementById('mode').textContent = s.info + ' / ' + s.outp + ' (設定' + s.set_volt + 'V/' + s.set_curr + 'A)';
  }catch(e){ document.getElementById('mode').textContent = '無応答'; }
}
async function post(path, body){
  const q = tok ? '?token=' + encodeURIComponent(tok) : '';
  const r = await fetch(path + q, { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Token': tok }, body: JSON.stringify(body) });
  document.getElementById('op').textContent = r.status + ' ' + (await r.text());
  st();
}
setInterval(st, 2000); st();
</script></body></html>"##;

fn controls_html(level: u8) -> &'static str {
    match level {
        0 => "<p>監視のみ</p>",
        1 => "<button onclick=\"post('/api/outp',{on:false})\">出力OFF</button>",
        _ => "<button onclick=\"post('/api/outp',{on:true})\">出力ON</button><button onclick=\"post('/api/outp',{on:false})\">出力OFF</button><br />V <input id=\"v\" type=\"number\" step=\"0.01\" /> A <input id=\"a\" type=\"number\" step=\"0.001\" /><button onclick=\"post('/api/apply',{volt:parseFloat(document.getElementById('v').value),curr:parseFloat(document.getElementById('a').value)})\">適用</button>",
    }
}

fn qr_svg(url: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(url).map_err(|e| format!("qr: {e}"))?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .build())
}

fn handle_client(stream: TcpStream, cfg: &SrvCfg) {
    let who = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    let mut s = stream;
    let Some((method, target, headers, body)) = read_http(&s) else {
        return;
    };
    let (path, _q) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target.as_str(), ""),
    };
    match (method.as_str(), path) {
        ("GET", "/") | ("GET", "/index.html") => {
            let html = DASH
                .replace("%%LEVEL%%", &cfg.level.to_string())
                .replace("%%CTLS%%", controls_html(cfg.level));
            http_resp(&mut s, 200, "OK", "text/html; charset=utf-8", &html);
        }
        ("GET", "/qr") => match qr_svg(&cfg.public) {
            Ok(svg) => http_resp(&mut s, 200, "OK", "image/svg+xml", &svg),
            Err(e) => http_resp(&mut s, 500, "ERR", "text/plain", &e),
        },
        ("GET", "/api/status") => match device_snapshot_json(&cfg.device) {
            Ok(j) => http_resp(&mut s, 200, "OK", "application/json", &j),
            Err(e) => http_resp(&mut s, 503, "ERR", "application/json", &format!("{{\"error\":\"{}\"}}", escape_json(&e))),
        },
        ("POST", "/api/outp") => {
            let on: Option<bool> = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("on").and_then(|o| o.as_bool()));
            let Some(on) = on else {
                http_resp(&mut s, 400, "ERR", "text/plain", "bad body");
                return;
            };
            if cfg.level == 0 {
                ops_log(&cfg.log, &cfg.ops, &who, "DENY outp (Lv0)");
                http_resp(&mut s, 403, "ERR", "text/plain", "Lv0は監視のみ");
                return;
            }
            if cfg.level == 1 && on {
                ops_log(&cfg.log, &cfg.ops, &who, "DENY outp ON (Lv1はOFFのみ)");
                http_resp(&mut s, 403, "ERR", "text/plain", "Lv1は出力OFFのみ");
                return;
            }
            if cfg.level == 2 && !authed(&target, &headers, &cfg.token) {
                ops_log(&cfg.log, &cfg.ops, &who, "DENY outp (token)");
                http_resp(&mut s, 403, "ERR", "text/plain", "token required");
                return;
            }
            match scpi_outp(&cfg.device, on, Duration::from_millis(1500)) {
                Ok(r) => {
                    ops_log(&cfg.log, &cfg.ops, &who, &format!("outp on={on} -> {r}"));
                    http_resp(&mut s, 200, "OK", "application/json", &format!("{{\"outp\":\"{}\"}}", escape_json(&r)));
                }
                Err(e) => http_resp(&mut s, 500, "ERR", "application/json", &format!("{{\"error\":\"{}\"}}", escape_json(&e))),
            }
        }
        ("POST", "/api/apply") => {
            if cfg.level != 2 || !authed(&target, &headers, &cfg.token) {
                ops_log(&cfg.log, &cfg.ops, &who, "DENY apply (Lv2+token required)");
                http_resp(&mut s, 403, "ERR", "text/plain", "Lv2+token required");
                return;
            }
            let v: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(_) => {
                    http_resp(&mut s, 400, "ERR", "text/plain", "bad body");
                    return;
                }
            };
            let (volt, curr) = (
                v.get("volt").and_then(|x| x.as_f64()),
                v.get("curr").and_then(|x| x.as_f64()),
            );
            let (Some(volt), Some(curr)) = (volt, curr) else {
                http_resp(&mut s, 400, "ERR", "text/plain", "bad body");
                return;
            };
            match scpi_apply(&cfg.device, volt, curr, Duration::from_millis(1500)) {
                Ok(r) => {
                    ops_log(&cfg.log, &cfg.ops, &who, &format!("apply {volt}V {curr}A"));
                    http_resp(
                        &mut s,
                        200,
                        "OK",
                        "application/json",
                        &format!(
                            "{{\"volt\":\"{}\",\"curr\":\"{}\"}}",
                            escape_json(&r.volt),
                            escape_json(&r.curr)
                        ),
                    );
                }
                Err(e) => {
                    let code = if e.contains("範囲外") || e.contains("超過") {
                        400
                    } else {
                        500
                    };
                    http_resp(&mut s, code, "ERR", "application/json", &format!("{{\"error\":\"{}\"}}", escape_json(&e)));
                }
            }
        }
        _ => http_resp(&mut s, 404, "ERR", "text/plain", "not found"),
    }
}

fn run_server(
    bind: &str,
    cfg: SrvCfg,
) -> std::io::Result<(std::sync::Arc<AtomicBool>, std::thread::JoinHandle<()>, u16)> {
    let listener = TcpListener::bind(bind)?;
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let h = std::thread::spawn(move || {
        while !stop2.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => handle_client(s, &cfg),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    });
    Ok((stop, h, port))
}

struct StoredServer {
    stop: std::sync::Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    url: String,
    level: u8,
    log: std::path::PathBuf,
    ops: std::sync::Arc<Mutex<u64>>,
}

static SERVER: OnceLock<Mutex<Option<StoredServer>>> = OnceLock::new();

fn server_slot() -> &'static Mutex<Option<StoredServer>> {
    SERVER.get_or_init(|| Mutex::new(None))
}

#[derive(Serialize)]
struct ServerInfo {
    url: String,
    lan: String,
    port: u16,
    level: u8,
    token: String,
    qr: String,
    log: String,
}

/// サーバーモード開始. LAN内利用想定 (0.0.0.0 Bind). Lv2は allow_lv2 明示＋トークン必須.
#[tauri::command]
fn server_start(
    port: u16,
    level: u8,
    allow_lv2: bool,
    device: String,
) -> Result<ServerInfo, String> {
    if level > 2 {
        return Err("levelは0-2".into());
    }
    if level == 2 && !allow_lv2 {
        return Err("Lv2フル操作には明示的な有効化が必要です".into());
    }
    let mut slot = server_slot().lock().map_err(|e| format!("lock: {e}"))?;
    if slot.is_some() {
        return Err("サーバー起動中です".into());
    }
    // 起動前に対象機器を確認 (誤Bind先での操作防止).
    {
        let mut p = open_port(&device)?;
        std::thread::sleep(Duration::from_millis(200));
        ensure_spe6103(&mut p, Duration::from_millis(1500))?;
    }
    let lan = lan_ip();
    let token = make_token();
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir logs: {e}"))?;
    let log = dir.join(format!("server_{}.log", epoch_secs()));
    let ops = std::sync::Arc::new(Mutex::new(0u64));
    if port == 0 {
        return Err("ポート番号を指定してください (例: 8000)".into());
    }
    let public = if level == 2 {
        format!("http://{lan}:{port}/?token={token}")
    } else {
        format!("http://{lan}:{port}/")
    };
    let qr = qr_svg(&public)?;
    let cfg = SrvCfg {
        device,
        level,
        token: token.clone(),
        public: public.clone(),
        log: log.clone(),
        ops: ops.clone(),
    };
    let (stop, handle, actual) =
        run_server(&format!("0.0.0.0:{port}"), cfg).map_err(|e| format!("bind失敗: {e}"))?;
    debug_assert_eq!(actual, port);
    *slot = Some(StoredServer {
        stop,
        handle: Some(handle),
        url: public.clone(),
        level,
        log: log.clone(),
        ops: ops.clone(),
    });
    ops_log(
        &log,
        &ops,
        "server",
        &format!("start Lv{level} {public}"),
    );
    Ok(ServerInfo {
        url: public,
        lan,
        port: actual,
        level,
        token: if level == 2 { token } else { String::new() },
        qr,
        log: log.to_string_lossy().into_owned(),
    })
}

/// サーバーモード停止.
#[tauri::command]
fn server_stop() -> Result<String, String> {
    let mut slot = server_slot().lock().map_err(|e| format!("lock: {e}"))?;
    match slot.take() {
        Some(st) => {
            st.stop.store(true, Ordering::Relaxed);
            if let Some(h) = st.handle {
                let _ = h.join();
            }
            let n = st.ops.lock().map(|n| *n).unwrap_or(0);
            Ok(format!("{} ({}ops)", st.log.to_string_lossy(), n))
        }
        None => Err("サーバーは起動していません".into()),
    }
}

#[derive(Serialize)]
struct ServerStatus {
    running: bool,
    url: String,
    level: u8,
    ops: u64,
}

#[tauri::command]
fn server_status() -> Result<ServerStatus, String> {
    let slot = server_slot().lock().map_err(|e| format!("lock: {e}"))?;
    match slot.as_ref() {
        Some(st) => Ok(ServerStatus {
            running: true,
            url: st.url.clone(),
            level: st.level,
            ops: st.ops.lock().map(|n| *n).unwrap_or(0),
        }),
        None => Ok(ServerStatus {
            running: false,
            url: String::new(),
            level: 0,
            ops: 0,
        }),
    }
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
            server_start,
            server_stop,
            server_status,
            log_start,
            log_append,
            log_stop
        ])
        .run(tauri::generate_context!())
        .expect("failed to run tauri app");
}

#[cfg(test)]
mod tests {
    use super::*;

    static TLOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn dev_port() -> String {
        std::env::var("SPE6103_PORT").unwrap_or_else(|_| "COM7".into())
    }

    fn http_raw(port: u16, req: &str) -> String {
        let mut s = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(15)))
            .expect("timeout");
        s.write_all(req.as_bytes()).expect("write");
        let mut out = Vec::new();
        let mut one = [0u8; 1024];
        loop {
            match s.read(&mut one) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&one[..n]),
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn get(port: u16, target: &str) -> String {
        http_raw(
            port,
            &format!("GET {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
        )
    }

    fn post(port: u16, target: &str, body: &str) -> String {
        http_raw(
            port,
            &format!(
                "POST {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    struct TestSrv {
        stop: std::sync::Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    fn start_test_srv(bind_port: u16, level: u8, token: &str) -> TestSrv {
        let log = std::env::temp_dir().join(format!("spe6103_test_{bind_port}.log"));
        let _ = std::fs::remove_file(&log);
        let cfg = SrvCfg {
            device: dev_port(),
            level,
            token: token.into(),
            public: format!("http://127.0.0.1:{bind_port}/"),
            log,
            ops: std::sync::Arc::new(Mutex::new(0)),
        };
        let (stop, handle, actual) =
            run_server(&format!("127.0.0.1:{bind_port}"), cfg).expect("bind");
        assert_eq!(actual, bind_port);
        TestSrv {
            stop,
            handle: Some(handle),
        }
    }

    impl Drop for TestSrv {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    #[test]
    fn lv1_status_and_off_guard() {
        let _g = TLOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let srv = start_test_srv(18081, 1, "unused");
        let st = get(18081, "/api/status");
        assert!(st.starts_with("HTTP/1.1 200"), "status: {st}");
        assert!(st.contains("meas_volt"), "body: {st}");
        // Lv1では出力ONを拒否
        let deny = post(18081, "/api/outp", "{\"on\":true}");
        assert!(deny.starts_with("HTTP/1.1 403"), "deny: {deny}");
        // Lv1では出力OFFを許可
        let off = post(18081, "/api/outp", "{\"on\":false}");
        let off_body = off.clone();
        // 先に復元してからassert (失敗時もONに戻す)
        let back = scpi_outp(&dev_port(), true, Duration::from_millis(1500)).expect("restore ON");
        assert!(off.starts_with("HTTP/1.1 200"), "off: {off_body}");
        assert!(off_body.contains("OFF"), "off: {off_body}");
        assert!(back.contains("ON"), "restore: {back}");
        drop(srv);
    }

    #[test]
    fn lv2_token_flow() {
        let _g = TLOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let srv = start_test_srv(18082, 2, "TOK123");
        // トークンなしapplyは403
        let deny = post(18082, "/api/apply", "{\"volt\":8.4,\"curr\":1.25}");
        assert!(deny.starts_with("HTTP/1.1 403"), "deny: {deny}");
        // トークンあり・同値適用 (状態不変) は200
        let ok = post(
            18082,
            "/api/apply?token=TOK123",
            "{\"volt\":8.4,\"curr\":1.25}",
        );
        assert!(ok.starts_with("HTTP/1.1 200"), "ok: {ok}");
        assert!(ok.contains("8.400"), "ok: {ok}");
        // 範囲外は400
        let bad = post(
            18082,
            "/api/apply?token=TOK123",
            "{\"volt\":61.0,\"curr\":1.0}",
        );
        assert!(bad.starts_with("HTTP/1.1 400"), "bad: {bad}");
        drop(srv);
    }

    #[test]
    fn server_commands_roundtrip() {
        let _g = TLOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        // Lv2は明示的有効化なしでは拒否
        assert!(server_start(18085, 2, false, dev_port()).is_err());
        let info = server_start(18084, 1, false, dev_port()).expect("start");
        assert!(info.url.contains("18084"), "url: {}", info.url);
        assert!(info.qr.contains("<svg"), "qr");
        let st = server_status().expect("status");
        assert!(st.running && st.level == 1, "status");
        let dash = get(18084, "/");
        assert!(dash.contains("出力OFF"), "dash Lv1");
        let done = server_stop().expect("stop");
        assert!(done.contains("ops)"), "stop: {done}");
        assert!(!server_status().expect("status2").running);
    }

    #[test]
    fn lv0_readonly() {        let _g = TLOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let srv = start_test_srv(18083, 0, "unused");
        let st = get(18083, "/api/status");
        assert!(st.starts_with("HTTP/1.1 200"), "status: {st}");
        let dash = get(18083, "/");
        assert!(dash.contains("監視のみ"), "dash readonly");
        let deny = post(18083, "/api/outp", "{\"on\":false}");
        assert!(deny.starts_with("HTTP/1.1 403"), "deny: {deny}");
        drop(srv);
    }
}
