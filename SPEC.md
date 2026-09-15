# SPE6103 Control Software — SPEC v0.5（正本）

> OWON SPE6103 用・メーカーソフト代替。Win/Mac/Linux対応、ネイティブ動作＋サーバーモード同居。
> このファイルを正本として仕様を詰める。

## 1. 製品概要

* 目的：メーカー配布ソフトの代替。基本操作・グラフ・ログ・プリセットを分かりやすく、日本語対応。
* 対象機種：**SPE6103のみ先行**（0-60V / 0-10A / 300W）。`*IDN?` で機種確認し、他機種は弾く。将来SPEシリーズ等に拡大。
* 主用途：手動実験、バッテリー（Li-ion/LiPo、汎用CC/CV）充電。
* 公開形態：GitHub公開＋Releases配布（サイト不要）。LICENSEはMIT予定。日英対応。

## 2. ハード前提（確定）

* OWON SPE6103、分解能 10mV / 1mA、2.8inch LCD、本体10ステップシーケンス有。
* PC接続：CH340仮想COM（VID:PID `1A86:7523`）、**SCPI 115200 8N1、送信 `\n`、応答 `\r\n`**。
* SCPI（Programming Manual＋実機検証情報より）：
  `*IDN?` `*RST` `MEAS:VOLT?` `MEAS:CURR?` `MEAS:POW?` `MEAS:ALL?` `MEAS:ALL:INFO?`
  `OUTP {ON|OFF|0|1}` `OUTP?` `VOLT <v>` `VOLT?` `CURR <a>` `CURR?`
  `VOLT:LIM <v>` `VOLT:LIM?` `CURR:LIM <a>` `CURR:LIM?` `SYST:REM` `SYST:LOC`
* 既知の癖：
  - 実測更新は約0.3s周期。ポーリングはそれ以上間隔で。連続一致2回で安定判定しない（3回以上推奨）。
  - 出力立上がり1秒超。`OUTP ON`・設定変更直後の実測はランプ途中。
  - 応答読みは `read_until(\n)`。固定長読みはタイムアウト待ちで遅くなる。
  - 再挿抜でCOM番号が変わる。記憶ポート open失敗時は同一トランザクション内で再スキャン＋リトライ。
  - 書込みコマンドは応答なしのため、同一トランザクション内で先に `*IDN?` 確認（他機器への誤送信防止）。
  - `SYST:STAT?` は現行FWで `ERR` のため不使用。状態は `MEAS:ALL:INFO?`（OVP/OCP/OTP＋CV/CC/待機/故障）で取得。

## 3. アーキテクチャ（確定：Tauri v2）

* **Tauri v2（Rustバックエンド＋Webフロント）＋サーバーモード同居**（確定）。
  - 通常：ネイティブ直結（シリアル直接）。
  - サーバーモードON：`:8000` 等でWeb UI公開、スマホをグラフ端末・遠隔端末に。
  - 将来：Piに同バイナリ載せでUSB→Wi-Fiブリッジ（headless運用＋systemd）。将来PWA化しiPhone Web Push通知も検討。
* 試作はPythonでSCPI癖の検証を先行してもよいが、**公開バイナリはPyInstaller素出し禁止**（Defender誤検知の常連のため）。
  公開は Tauri原則。無料範囲での署名のみ（§5参照）。
* APIは言語非依存に固定（REST/WS想定）。フロント乗せ替え可能に。

想定構成：

```
src-tauri/ (Rust: serial/SCPI driver, state machine, logger, server)
src/       (Web UI: dashboard, graph, preset, battery, sequence, i18n ja/en)
```

## 4. 機能仕様（MVP全部入り、実装順 M1→M5）

### M1 接続・基本操作・保護
* 起動→自動スキャン（`*IDN?`でSPE6103確認）→ワンクリック接続。手入力不要。
* 状態機械：USB-Link有無 / PSU応答有無（本体電源OFF検出）/ OUTP ON-OFF / CV-CC-待機-故障。
* 基本パネル（日英・大表示）：設定V/A、実測V/A/W、CV/CC表示、出力ON（誤操作防止：確認or長押し）、`SYST:REM`自動。
* 保護：OVP/OCP設定・表示、故障時赤バナー。60V/10A/300W超は送信前ブロック。
* 電圧フィードバック補正モード（数mVの実測ズレ対策、ON/OFF可。詳細は §4 補足を参照）。

### M1 補足：電圧フィードバック補正モード（closed-loop）
* 目的：`VOLT <v>` 設定に対し `MEAS:VOLT?` が数mVずれる個体差・負荷特性を吸収し、目標電圧に寄せる。
  実測例：5.000V設定→4.980V実測。
* 方式：`MEAS:VOLT?` を見て `Vset` を分解能刻み（10mV）で加減算する簡易閉ループ。PIDなし。
  - 補正周期：0.5s以上（実測更新0.3s・立上がり1秒超を考慮。デフォルト1.0s）。
  - 安定判定：目標±許容差（デフォルト±5mV）内に3回連続で入ったら補正停止（ホールド）。
  - 1回の補正量：上限±20mV（=2ステップ）まで。オーバーシュート防止。
  - ガード：出力ON中のみ動作。OFF/故障/無応答で自動休止。Vsetは0-60V範囲内に丸め（10mV単位）。
    電力上限（Vset×Iset≦300W）・OVP下限（Vset≦OVP）を常に満たすこと。範囲外になる補正は打切り＋通知。
* UI：目標V表示、実測V、補正量（ΔmV）、状態（追込中/ホールド/休止）。デフォルトOFF。
* 既知の限界：Vset分解能が10mVのため、理論上±5mV程度の残差は残る。MEAS側の確度依存。
  CC遷移・負荷急変時は追従が遅れる（仕様）。シーケンス/バッテリーMode実行中は排他（同時ON不可、v1）。

### M2 グラフ＋ログ
* グラフ常時表示、起動からのリングバッファで遡及可。V/I/W表示切替（どれがどれか分かる色・凡例固定）、滑らか描画（間引き＋補間、更新と描画分離）。
* 本体電源OFF中は欠損扱い（線を繋がない、ログに空行を書かない）。
* ログはデフォルトOFF、欲しい時だけON。間隔は最小（約0.3s）〜60sで設定可。
  ファイル名自動 `spe6103_YYYYMMDD_HHMMSS.csv`。PSU無応答時は自動一時停止。
* 肥大化防止（暫定、使いながら調整）：メモリはリング＋長時間は間引き保存、DB/CSVにサイズ上限＋自動ローテーション、
  古い高解像度データから間引く方針。しきい値は実測で調整。

### M3 プリセット＋バッテリーMode
* プリセット無制限（JSON/SQLite、検索＋export/import）。
* バッテリーMode（Li-ion/LiPo＋汎用CC/CV）：
  Vset（満充電電圧）、Ilim（充電電流）、終了条件OR（終止電流・最大時間・最大容量Ah）、
  Ah/Wh積算表示、OVP=Vset+α自動、充電中ログ強制ON、終了時自動OFF＋通知＋CSV確定。
  通知はv1.0でOS通知（Tauri notification）まで。音・ダイアログ併用可。
  将来：サーバーをPWA対応しiPhone Web Push通知を検討。
  ※温度保護なし、BMSの代わりにならない旨を明記。

### M4 シーケンス（ホスト側）
* ステップ表形式のみ（V/A/継続時間/ループ、例 12V/1A×60s→5V/0.5A×30s→OFF）。条件分岐なし（v1）。
* 上限は本体最大のみ（別 caps なし）。

### M5 サーバーモード
* 権限はサーバー側で固定（クライアント側で緩和不可）：
  Lv0 監視のみ / Lv1 Lv0＋出力OFF（デフォルト）/ Lv2 フル操作（要明示ON＋警告＋トークン）。
* LAN内のみBind、操作ログ、QR表示。WAN公開は非推奨をREADME明記。

## 5. 非機能・配布・安全

* 対応OS：Win10+ / macOS 13+ / Linux（x64＋ARM/Pi）。
* 署名・誤検知対策（無料範囲のみ。Apple Developer登録なし）：
  Win＝SignPath無料枠（OSS）が使えれば署名、不可なら無署名＋SHA256公開＋Defender誤検知申請＋Actionsビルド証明。
  SmartScreen警告時の回避手順をREADMEに記載。
  Mac＝無署名・無公証前提。右クリック開く／`xattr -d com.apple.quarantine` 手順をREADMEに記載。
  Linux＝AppImage/deb＋SHA256/GPG署名。
  素のPyInstallerバイナリは配布しない。
* i18n：`ja/en.json`分離、OSロケール自動＋手動切替、デフォルト日本語。
* 安全方針：READMEに免責（焼損・発火自己責任、無人フル遠隔禁止、充電はBMS併用）。
  法律面は個人LAN内利用では問題なし（販売・業務提供は別途検討）。
* 性能目安：サンプリング≧0.3s、グラフ60fps描画、メモリはリング＋DBで長時間対応。

## 6. ロードマップ

* M1→M2→M3→M4→M5→v1.0 GitHub公開。M単位で動作確認。
* v1.1以降：他SPE機種対応、WAN（要認証強化）、温度センサ連携、条件分岐シーケンス。
* v2.x実験枠：Apple Home（HomeKit/Matter）対応検討。PiをHAP/Matterブリッジ化しOutletとして公開。
  範囲外（v1.0には含めない）。個人利用前提、販売時のMFi認証は別途。公開は監視＋OFFのみ推奨。

## 7. 要確認（残り）

* [x] Tauri確定 → 確定（おまかせにより決定）
* [x] グラフ保持期間・永続化 → 暫定方針で実装し使いながら調整（肥大化防止あり）
* [x] バッテリー終了時の通知手段 → v1.0はOS通知まで、将来PWA＋Web Push
* [x] Apple Developer登録 → しない。無料範囲のみ、いつでもOK
