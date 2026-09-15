#!/usr/bin/env python3
"""SPE6103 SCPI疎通プローブ (SPEC §2準拠)
使い方: pip3 install pyserial && python3 probe_spe6103.py [--port /dev/cu.wchusbserial...]
本体電源ON＋USB接続状態で実行。送信=\\n, 応答=\\r\\n, 115200 8N1。
"""
import argparse, glob, sys, time

BAUD = 115200

def q(ser, cmd, timeout=1.0):
    ser.reset_input_buffer()
    ser.write((cmd + "\n").encode())
    ser.flush()
    deadline = time.time() + timeout
    buf = b""
    while time.time() < deadline:
        n = ser.in_waiting
        if n:
            buf += ser.read(n)
            if b"\n" in buf:
                break
        else:
            time.sleep(0.02)
    return buf.decode(errors="replace").strip()

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", default=None)
    args = ap.parse_args()
    try:
        import serial
        from serial.tools import list_ports
    except ImportError:
        print("pyserialがありません: pip3 install pyserial", file=sys.stderr)
        return 1
    if args.port:
        ports = [args.port]
    else:
        ports = [p.device for p in list_ports.comports()]
        # CH340優先
        ports.sort(key=lambda d: 0 if ("wchusb" in d or "usbserial" in d or "CH340" in d) else 1)
        if not ports:
            ports = sorted(glob.glob("/dev/cu.*") + glob.glob("/dev/ttyUSB*"))
    print(f"候補ポート: {ports}")
    for dev in ports:
        try:
            import serial as S
            ser = S.Serial(dev, BAUD, timeout=0.2)
        except Exception as e:
            print(f"[{dev}] open失敗: {e}")
            continue
        try:
            time.sleep(0.3)
            idn = q(ser, "*IDN?")
            print(f"[{dev}] *IDN? -> {idn!r}")
            if "SPE6103" not in idn and "OWON" not in idn:
                ser.close()
                continue
            print(f"★ SPE6103発見: {dev} : {idn}")
            for cmd in ["VOLT?", "CURR?", "VOLT:LIM?", "CURR:LIM?",
                        "MEAS:VOLT?", "MEAS:CURR?", "MEAS:POW?",
                        "MEAS:ALL?", "MEAS:ALL:INFO?", "OUTP?"]:
                print(f"  {cmd} -> {q(ser, cmd)!r}")
            ser.close()
            return 0
        except Exception as e:
            print(f"[{dev}] 通信失敗: {e}")
            try: ser.close()
            except Exception: pass
    print("SPE6103が見つかりません。本体電源ON＋USB接続を確認してください。")
    return 2

if __name__ == "__main__":
    raise SystemExit(main())
