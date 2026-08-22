# M1計測手順 — SE再生レイテンシのA/B計測

このドキュメントは、初期構築仕様の §1「成功基準」・§8「テスト戦略」・§10「M1. SE 再生」・
§11「エージェント運用」で定義された **M1 最重要ゲート**(実機での外部録音による遅延計測)を
ユーザーと協働で実施するための手順書。

**第1回の計測は 2026-08-22 に実施済み。結果は §7 を参照。**
§1〜§6 は手順・目標値・準備状況を定義する部分で、計測のたびに再利用する。
§7 以降に実測結果を追記していく。

---

## 1. 目的と成功基準

初期構築仕様 §1「成功基準」より:

| 項目 | 目標 |
|---|---|
| SE トリガー → 出力 | **iOS ≤ 20ms** / **Android ≤ 40ms**(Unity 既定実装との A/B で有意に改善していること) |

初期構築仕様 §10「M1. SE 再生」より:

> 実機で Unity 実装との遅延 A/B を外部録音で計測し、目標値(§1)を満たす。
> 満たさない場合はここで M6(バックエンド直叩き)への置換を判断する。
>
> **M1 の A/B 計測が最重要のゲート。** ここで Unity 実装に対する優位が出なければ、
> このミドルウェアの存在意義(§1)が崩れるため、先に進まず原因を潰すか計画を見直す。

## 2. 計測方式: 外部録音(素朴な方法)

初期構築仕様 §8「テスト戦略」より、M1 の実機計測は**テンプレート非依存**の外部録音方式を使う
(テンプレート統合後の判定計測ハーネスによる計測は M2 以降)。

### 2.1 原理

1. 実機の画面タップ(または任意の明示的なトリガー、例: ボタン)と**同時に** SE 再生要求を発行する
2. 別端末(スマートフォン等)のマイクで、**タップ操作音(または視覚的な合図)**と
   **スピーカーから出る SE の音**の両方を録音する
3. 録音波形をPC上の波形編集ソフト(Audacity 等)で開き、トリガーの立ち上がりから
   SE 音の立ち上がりまでの時間差を目視・波形解析で読み取る

この方式は OS のオーディオスタック内部を計測できないため、あくまで「体感できる遅延」の
実測値になる。ミリ秒単位の高精度が必要なため、録音環境の選定(§4)が重要。

### 2.2 トリガーの明確化(重要)

タップそのものは無音のため、録音上でトリガー時刻を特定できる**明示的な音または光**が要る。
以下のいずれかを推奨する:

- **方式A(視覚)**: 画面タップの瞬間に画面を白反転させ、録画動画のフレームからトリガー時刻を
  特定する(スロー撮影 240fps 以上推奨。音声との同期はカメラのマイクで行う)
- **方式B(聴覚)**: タップと同時に**別チャンネル**(例: ヘッドホン端子から別録音機材へ直結、
  または別デバイスでのクリック音)で基準クリックを鳴らし、それとSE音の間隔を測る

**方式A(スロー動画 + 内蔵マイク)が機材要件が低く、まず着手しやすい。**
より高精度が必要になった場合は方式Bへ切り替える(オーディオインターフェースでの
ライン録音を推奨。§4.2)。

## 3. A/B 比較手順

同一シナリオを2実装で計測し、有意な改善(§1 目標値を満たすこと)を確認する。

### 3.1 比較対象

| 実装 | 説明 |
|---|---|
| **A: Unity 既定実装** | `AudioSource.PlayOneShot` によるSE再生(テンプレート仕様「オーディオ抽象化」の
`UnityAudioService` 相当。比較のベースライン) |
| **B: 本ミドルウェア** | `Mw.Native.MwNative.PlaySe`(本リポジトリ、初期構築仕様の核心) |

### 3.2 手順

1. `unity-sample` に A/B 両方の再生ボタンを持つ最小シーンを用意する(**準備済み**。
   `unity-sample/Assets/Measurement/MeasurementScene.unity`。§6 参照)
2. 同一の SE 音源(短いクリック系、立ち上がりが明瞭な波形を推奨)を両実装にロードする
   (**準備済み**。`ClickSeGenerator`(§6)がコードで合成し、両実装に同一波形を渡す)
3. 実機を録音環境に固定し、同一条件(同じ端末・同じ距離・同じ音量設定)で:
   - A実装で10回タップ→計測
   - B実装で10回タップ→計測
4. 各10回の遅延(トリガーから音の立ち上がりまでの時間)を記録し、中央値・ばらつき(分散)を
   比較する
5. B(本ミドルウェア)がAより有意に短く、かつ§1の目標値(iOS ≤20ms / Android ≤40ms)を
   満たすことを確認する

### 3.3 記録フォーマット(例)

```
端末: iPhone <機種名>, iOS <バージョン>
録音機材: <スマホ内蔵 / オーディオIF 等>
SE音源: <ファイル名>, 長さ <ms>

| 試行 | A(Unity既定) [ms] | B(ミドルウェア) [ms] |
|---|---|---|
| 1 | ... | ... |
| ... | | |
| 中央値 | | |
```

### 3.4 解析の自動化(準備済み)

波形・フレームの目視読み取りは不要。`tools/measurement/analyze_ab_video.py` が
動画から自動で遅延を算出する(ffmpeg / ffprobe が必要。Python 標準ライブラリのみ使用):

```
python3 tools/measurement/analyze_ab_video.py <Aの動画> --label A
python3 tools/measurement/analyze_ab_video.py <Bの動画> --label B
```

- 白フラッシュは映像輝度(YAVG)の立ち上がり、SE は 2kHz バンドパス後の RMS の
  立ち上がりで検出し、タップごとの遅延と中央値・ばらつきを出力する
  (タップの操作音は帯域外なので誤検出しない)
- **1モード1動画**で撮影する(A の連続タップで1本、B で1本)
- **iPhone のスロー動画は「オリジナル」のまま AirDrop すること**。Photos アプリで
  スロー編集を適用して書き出すと時間軸が歪み、解析できない
- 検出原理の自己検証: `python3 tools/measurement/analyze_ab_video.py --self-test`
  (既知の遅延 50/70ms を合成動画に埋め込み、誤差なく復元できることを確認済み)

## 4. 実機組み込みビルド手順

### 4.1 iOS

**現状(2026-08-15 時点で準備完了)**: 以下がすべてローカルで実行・確認済み。
**ユーザーがやることは Xcode を開いて Team を選び、実機に Run するだけ**の状態にしてある。

- `make build-ios` → `unity/Runtime/Plugins/iOS/MwFfi.xcframework` 生成(確認済み)
- `make bindgen` → `unity/Runtime/Generated/NativeMethods.g.cs` 最新化(確認済み)
- `unity-sample/Assets/Measurement/` に A/B 計測用シーン一式を実装済み(§6)
- Unity バッチモードで iOS 向け Xcode プロジェクトを書き出し済み:
  `unity-sample/Build/iOS/Unity-iPhone.xcodeproj`
  (`Measurement.EditorTools.IosXcodeExporter` が `PlayerSettings.iOS.appleEnableAutomaticSigning = true`
  ・`CODE_SIGN_STYLE = Automatic` を設定済み。`PRODUCT_BUNDLE_IDENTIFIER` は既定の
  `com.DefaultCompany.unity-sample` のまま — 変更が必要なら Xcode 上で構わない)
- MwFfi.xcframework は `Build/iOS/Frameworks/com.mu5dvlp.audio-middleware/Runtime/Plugins/iOS/`
  配下に正しく埋め込まれていることを確認済み

手順(再実行する場合):

1. `make build-ios` で最新の `unity/Runtime/Plugins/iOS/MwFfi.xcframework` を生成する
2. `make bindgen` で C# バインディングを最新化する
3. Unity バッチモードでシーンと Xcode プロジェクトを再生成する(いずれも Unity 起動のため
   `tools/with-unity-lock.sh` 経由でロックを取得すること):
   - `Measurement.EditorTools.MeasurementSceneBuilder.Build`
     (`unity-sample/Assets/Measurement/MeasurementScene.unity` を作成 / 上書き)
   - `Measurement.EditorTools.IosXcodeExporter.Build`
     (`unity-sample/Build/iOS/` に Xcode プロジェクトを書き出す。Editor メニューの
     `Measurement/Export iOS Xcode Project` からも実行可)
4. `unity-sample/Build/iOS/Unity-iPhone.xcodeproj` を Xcode で開き、Signing & Capabilities で
   自分の Team を選び、実機を接続して Run する(自動署名は有効化済み。端末接続・Team 選択は
   ユーザー作業、初期構築仕様 §11)
5. AVAudioSession のカテゴリ・バッファ長設定は未実装(初期構築仕様 §14 リスク表、M3 で
   Obj-C シムを実装予定)。**M1 時点では OS 既定のセッション設定のまま計測する**
   (これ自体が M1 実測値に影響する可能性があり、計測結果と合わせて記録すること)

**TODO(【未定】)**:
- 実機ビルド・インストールの自動化(fastlane 等)は未整備。手動 Xcode 操作が前提
  (今回自動署名 + Xcode プロジェクト書き出しまでは自動化した。Team 選択と実機への
  インストール操作自体は Apple ID のサインイン状態に依存するためユーザー作業のまま)
- AVAudioSession 未設定時の実際のバッファ長・レイテンシ特性は未計測
- `unity-sample/Build/` は `.gitignore` 対象(ビルド成果物はコミットしない方針、CLAUDE.md)。
  再取得する環境では上記手順3を再実行すること

### 4.2 Android

**現状**: `make build-android` で `cargo-ndk` による `arm64-v8a` の `.so` 生成まで確認済み
(API level 26、AAudio 対応。CI・ローカル双方でビルド成功)。

手順:

1. `make build-android` で最新の `unity/Runtime/Plugins/Android/libs/arm64-v8a/libmw_ffi.so`
   を生成する
2. `make bindgen` で C# バインディングを最新化する
3. Unity で Android ビルドターゲットに切り替え、実機(要 USB デバッグ有効化)へビルド・
   インストールする
4. cpal の Android 実装は `ndk`/`jni` クレート経由で AAudio を直接叩く(`crates/mw-backend/CLAUDE.md`
   参照)。パフォーマンスモード(low-latency / exclusive)の明示設定は未検証 —
   計測結果が目標値(≤40ms)に届かない場合、ここが M6(oboe 直叩き)判断の主要な検討材料になる

**TODO(【未定】)**:
- 実機ビルド・インストールの自動化は未整備。手動 Unity Build & Run が前提
- 端末の機種依存(AAudio 対応状況、performance mode の可否)によりばらつきが出うる。
  初期構築仕様の未決事項「プロファイル基準端末」に沿った基準端末の選定が必要

## 5. 判定とM6への分岐

計測結果が目標値を**満たす場合**: M1完了。M2(楽曲とクロック)へ進む。

計測結果が目標値を**満たさない場合**: 初期構築仕様 §10 の表・§14 リスク表に従い、
以下を切り分ける:

1. cpal のバッファサイズ・レイテンシ設定が既定のままで最適化されていない可能性
   → `mw_init(config)` 相当のバッファ長設定を調整して再計測(現状 `Config` に
   バッファ長を直接指定するFFI経路は未実装。【TODO】M1後半 or M2冒頭で追加を検討)
2. cpal 自体の抽象化コストが大きい可能性
   → Android は **oboe 直叩き**、iOS は **RemoteIO 直叩き**への置換を検討
   (`Backend` トレイト経由で差し替え可能な設計に既にしてある。`crates/mw-backend/CLAUDE.md`)
3. AVAudioSession(iOS)のカテゴリ・バッファ長が未設定であることの影響切り分け
   → M3 で予定している Obj-C シムを前倒しで実装し、再計測

いずれの場合も、**先に進まず原因を潰すか計画を見直す**(初期構築仕様 §10)。

## 6. 準備状況とTODO(2026-08-15 更新)

### 準備済み(実装・ローカル確認済み)

- [x] **計測用シーン**: `unity-sample/Assets/Measurement/MeasurementScene.unity`
      (`Measurement.EditorTools.MeasurementSceneBuilder` がコードから組み立てて保存する。
      手作業でのシーン編集に依存しない)。画面いっぱいの大きなボタン2つ:
      - **ボタンA**(左半分・青): Unity 既定実装(`AudioSource.PlayOneShot`)
      - **ボタンB**(右半分・赤): 本ミドルウェア(`Mw.Native.MwNative.PlaySe`)
      画面上部に現在(直近にタップした)モードを表示する `StatusText`
      (例: `Mode: B (Mw.Native.MwNative.PlaySe)`)を常設。タップと同一フレームで
      画面全体を白フラッシュ(既定2フレーム、`ScreenFlash` コンポーネント)する
- [x] **クリック SE の生成**: `unity-sample/Assets/Measurement/Runtime/ClickSeGenerator.cs`。
      2kHz・約30ms・指数減衰・位相 π/2(コサイン)開始でサンプル0から最大振幅に立ち上がる
      自作波形をコードで合成する(バイナリ資産はコミットしない)。A・B 両実装に
      **完全に同一のサンプル列**を渡す(A は `AudioClip.Create`、B は同じ配列から
      組み立てた 48kHz/16bit モノラル wav バイト列を `LoadSound` に渡す)
- [x] iOS ビルドパイプライン: `make build-ios` → `MwFfi.xcframework`、`make bindgen` →
      C# バインディング、Unity バッチモードでの Xcode プロジェクト書き出しまで実行済み
      (詳細・生成物パスは §4.1)。**自動署名を有効化した状態で書き出し済み**のため、
      ユーザーは Xcode を開いて Team を選び実機で Run するだけでよい
- [x] macOS Editor 上での動作確認: `make unity-test`(EditMode)で
      `ClickSeGeneratorEditModeTests`(波形の健全性・`AudioClip` 生成・
      `MwNative.LoadSound`/`PlaySe` が実際に成功すること)を含む9件すべて green。
      `cargo test --workspace`(69件)・`make lint` も green

### 未解決(ユーザー協働が必要 / 未着手)

- [x] 実機への接続・Team 選択・インストール(2026-08-22 実施。実機は iPad だった。§7.1)
- [x] 外部録音(方式A: スロー動画)で A/B 計測を実施し記録する(§7)
- [ ] **目標値未達の見込みのため §5 の切り分けを行う。第一候補は AVAudioSession 未設定(§7.5)**
- [ ] B が起動から終了まで無音になるセッションがあった件の原因究明(§7.6-1)
- [ ] `handle.rs` が BackendError を握り潰している問題の修正(§7.6-1)
- [ ] `StatusText` を白文字にする(§7.6-2)
- [ ] 初期構築仕様の未決事項「プロファイル基準端末」を確定し、以後の計測はその端末を基準にする
- [ ] (iOS)AVAudioSession シム実装(M3 前倒し検討)
- [ ] Android 側(`make build-android` の再実行・Unity での Android ビルド書き出し・
      AAudio performance mode の明示設定の要否調査)は本ラウンドでは未着手。§4.2 参照

### 計測当日にユーザーがやることの最短手順(iOS)

1. `unity-sample/Build/iOS/Unity-iPhone.xcodeproj` を Xcode で開く
2. Signing & Capabilities で自分の Apple Developer Team を選ぶ(自動署名は有効化済み)
3. iPhone を接続し、ビルドターゲットとして選択して Run(Development ビルド)
4. アプリ起動後、別端末のスロー動画(240fps 推奨)で画面を撮りながら
   A ボタン(青・左半分)を10回ゆっくりタップ → 録画停止(A の動画1本)
5. 同様に B ボタン(赤・右半分)を10回タップして録画(B の動画1本)
   (画面上部の `StatusText` で直前にどちらを押したか常時確認できる)
6. 動画2本を**オリジナルのまま** AirDrop で Mac へ送り、
   `tools/measurement/analyze_ab_video.py`(§3.4)で解析 → §3.3 のフォーマットで記録する

---

## 7. 実測結果(第1回: 2026-08-22)

### 7.1 計測条件

```
端末(計測対象): iPad ※機種名・iOS バージョンは未記録 —— 次回計測時に必ず埋めること
録音機材: iPhone 14 内蔵マイク / スローモーション 1080p 240fps
撮影: 端末を机に置き、手持ちの iPhone 14 で画面と本体スピーカーを同時に撮影
SE音源: ClickSeGenerator が合成する 2kHz・30ms・指数減衰クリック(A/B 完全同一波形)
AVAudioSession: 未設定(OS 既定のまま。§4.1 手順5)
Bluetooth: オン(ただし出力経路には入っていない。§7.4 参照)
解析: tools/measurement/analyze_ab_video.py --min-flash-jump 25
      → フラッシュ 20 回検出 / SE 対応づけ 20 回成功(取りこぼしゼロ)
動画: 1本に A×10 → B×10 を連続収録(実時間 48.06 秒)。
      タップ10回目(21.69s)と11回目(25.69s)の間の 4 秒の空白が A→B の切り替え点。
      A/B の別は 15.0s / 35.0s のフレームを抜き出し、指の位置(青=A / 赤=B)で目視確認した。
```

### 7.2 結果

| 試行 | A: Unity 既定 [ms] | B: ミドルウェア [ms] |
|---|---|---|
| 1 | 191.2 | 31.3 |
| 2 | 174.9 | 3.8 |
| 3 | 204.3 | 39.1 |
| 4 | 176.8 | 6.3 |
| 5 | 177.7 | 32.2 |
| 6 | 197.5 | 28.2 |
| 7 | 187.8 | 38.8 |
| 8 | 213.3 | 5.9 |
| 9 | 184.4 | 21.8 |
| 10 | 190.2 | 17.0 |
| **中央値** | **189.0** | **25.0** |
| 平均 | 189.8 | 22.4 |
| 最小 / 最大 | 174.9 / 213.3 | 3.8 / 39.1 |
| 標準偏差 | 11.8 | 12.9 |

### 7.3 判定

- ✅ **Unity 既定実装に対する有意な改善**: 約 **164ms** 短縮(**7.5倍**)。
  初期構築仕様 §1 の「Unity 既定実装との A/B で有意に改善していること」は明確に達成。
  M1 の最重要ゲート(§10「ここで Unity 実装に対する優位が出なければ存在意義が崩れる」)は通過。
- ⚠️ **iOS ≤20ms は未達の可能性が高い**: 実測中央値 25.0ms。さらに下記の系統誤差により
  真値はこれより大きい(25〜40ms 程度)と見るべき。

**計測方式の系統誤差(重要・以後の計測でも必ず考慮すること)**

アプリはタップと同一フレームで白フラッシュと `PlaySe` を実行するが、フラッシュが画面に
**見える**のは次の垂直同期のあと(60Hz なら 0〜16.7ms 後)。一方 SE 要求はその場で発行される。
したがって

```
実測値 = 音の出た時刻 − フラッシュが見えた時刻 < 実際の「トリガー→出力」
```

となり、本方式は**常に過小評価する**(0〜16.7ms、平均 ~8ms)。B の最小値 3.8ms のような
「速すぎる」値はこれで説明できる。A/B の**差**には同じ誤差が乗るため比較には影響しないが、
**絶対値を §1 の目標値(≤20ms)と突き合わせる際は必ずこの分を織り込むこと**。

より正確な絶対値が必要になった場合は §2.2 の方式B(ライン録音)へ切り替える。

### 7.4 A が 189ms である件

Bluetooth はオンだったが、**出力経路には入っていない**と判断する。同一端末・同一 AVAudioSession で
B が 25ms で鳴っている以上、経路に BT(A2DP なら 100〜200ms)は挟まりようがないため。
したがってこの 189ms は **Unity 既定設定(DSP バッファ・FMOD の内部バッファ)そのものの遅延**である。
§1 のベースラインは「Unity 既定実装」と定義されているので、この値をそのままベースラインとして扱う。

### 7.5 次のアクション(§5 の切り分け)

目標未達の第一候補は §4.1 手順5 に明記のとおり **AVAudioSession が未設定**であること
(iOS 既定の I/O バッファは通常 ~23ms)。§5 の分岐でいえば「3. AVAudioSession のカテゴリ・
バッファ長が未設定であることの影響切り分け」に該当する。

→ **M3 で予定している Obj-C シムを前倒しで実装し、`preferredIOBufferDuration` を 5ms 程度に
設定して再計測する。** cpal 側は `BufferSize::Fixed` を渡したときだけ
`set_audio_session_buffer_size` を呼ぶ実装になっており(cpal 0.18 の iOS 実装)、現状は
`BufferSize::Default` のため**バッファ長の要求を一切していない**。ここも併せて見直す。

### 7.6 計測中に判明した不具合・注意点

1. **B が起動から終了まで無音になるセッションがあった**(同日の1本目の録画。A は10打すべて発音、
   B は10打すべて無音)。そのとき Bluetooth がオンだった。
   `crates/mw-backend/src/cpal_backend.rs` の `find_f32_stereo_config` は **f32 かつ 2ch ちょうど**
   しか受け付けないが、cpal 0.18 の iOS 実装は `AVAudioSession.outputNumberOfChannels()` に
   基づくチャンネル数しか列挙しない。BT 経路(特に HFP はモノラル)では該当構成がゼロになり
   `mw_init` が `ErrBackendOpenFailed` を返す、という筋が通る。**未確認の仮説**であり、
   Bluetooth オン/オフでの再現確認が必要。
   併せて `crates/mw-ffi/src/handle.rs` の `if backend.open(renderer).is_err()` が
   **具体的な BackendError を捨てている**ため実機で原因が特定できない。ログ出力を追加すること。
2. **`StatusText` が黒文字**(`MeasurementSceneBuilder`)で背景も黒のため、録画から A/B を
   判別できなかった(今回は指の位置で判別した)。白文字にすること。
3. 解析スクリプトの白フラッシュ判定が「baseline→peak の輝度差 40 以上」固定で、今回の映像
   (差 34.4)を弾いた。`--min-flash-jump` / `--flash-ratio` オプションを追加して回避可能にした。
   閾値を下げたときは**検出回数がタップ回数と一致するか必ず確認する**こと。
4. **iPhone の写真アプリ経由(AirDrop 含む)で書き出すとスローモーションが焼き込まれる。**
   映像も音声も 8倍に引き伸ばされ(クリックが 2000Hz → 250Hz に下がる)、しかもランプ区間が
   あるため場所によって伸び率が変わり、**時間軸を復元できず解析不能**になる。
   §3.4 の注意書きのとおり、**必ずオリジナル(240fps・実時間)を渡すこと**。
   iPhone 側でスロー編集を解除してから共有するか、USB 接続で「イメージキャプチャ」から読み込む。
   `ffprobe` で **240 fps** と表示されれば正しいファイル。
