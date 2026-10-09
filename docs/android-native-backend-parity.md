# Android の自前の出力(AAudio)と cpal 版のパリティ確認表

`backend-native` の Android 実装(`crates/mw-backend/src/native_backend/android.rs`、
AUDIOWARE-DEPS-PLAN.md ステップ4-3)へ差し替える前に、**cpal 版とその周りが黙ってやっていたこと**を
一覧にし、自前の出力が同じことをしているかを判定した表。末尾に iOS(`native_backend/apple.rs`)の
同じ表を短く付ける。

🔴 **なぜこの表があるか。** iOS で自前の出力を実機に出したところ、cpal の**外側**にあった
`ios_interruption.rs`(割り込み・背面・メディアサービスのリセットからの復帰)に相当するものが無く、
スリープのあと音が止まった(`ed0236d` で修正)。差し替えで壊れるのは、差し替えた本体よりも
**本体の周りが暗黙に担っていた役目**のほうだった。ステップ4-4(cpal の削除)の前に、
同じ見落としを Android で起こさないための確認項目にする。

- 調べた範囲: cpal **0.18.1**(`Cargo.lock` で固定中)の `src/host/aaudio/{mod.rs,convert.rs,java_interface/}`、
  ndk 0.9.0 の `audio.rs`、本リポジトリの `cpal_backend.rs` / `android_context.rs` /
  `mw-ffi` の内部再オープン(`handle.rs` / `reopen.rs` / `ffi.rs::mw_poll_events`)/
  `docs/adr/0002-p3-11-async-reopen.md` / `docs/measurement-m1.md` §6・§9 /
  `docs/history/08-2026-09-04.md`(M3 の Android 実機確認)。
- 方法はコードを読むだけ(ビルド・実機は使っていない)。行番号は 2026-10-09 時点(`6aeb558`)。
- 判定の凡例: **同じ** / **違う**(やり方が違う。良し悪しは備考)/ **していない** / **両方していない**(cpal 版にも無い)。

---

## 1. Android: 何を観測して、何が起きたら、何をしているか

### 1.1 ストリームの開き方

| # | 項目 | cpal 版(+ 周り) | 自前(`android.rs`) | 判定 |
|---|---|---|---|---|
| A1 | 出力先 | `default_output_device()` は `Device(None)`(cpal `aaudio/mod.rs:196`)= `device_id` を指定しない = システムの既定ルート | `setDeviceId` を呼ばない(`android.rs:485-495`) | 同じ |
| A2 | 形式・チャンネル | f32 / 2ch(`cpal_backend.rs::find_f32_stereo_config`)| `PCM_FLOAT` / `CHANNELS`(=2)(`android.rs:487-488`) | 同じ |
| A3 | 🔴 **サンプルレート** | **48kHz を明示的に要求する**。cpal の既定構成は Java を見ずに固定の候補表から作られ(`default_supported_configs`)、`try_with_standard_sample_rate` が 48k を選ぶ。列挙へ落ちた場合も 48k → 44.1k の順(`cpal_backend.rs:374-`)。結果 `AAudioStreamBuilder_setSampleRate(48000)`(cpal `aaudio/mod.rs:285`)。端末のレートが違えば **AAudio が変換する**。実機(SH-M16)では 48kHz(`measurement-m1.md` §9.2) | `setSampleRate(0)` = **端末の既定レート**をそのまま採り、開いた後に `getSampleRate` で読む(`android.rs:489-493, 549-555`) | **違う** |
| A4 | 性能モード | `realtime` feature で `LowLatency`(cpal `aaudio/mod.rs:298-301`、本リポジトリの `mw-backend/Cargo.toml` で Android だけ有効)| `AAUDIO_PERFORMANCE_MODE_LOW_LATENCY`(`android.rs:494`) | 同じ |
| A5 | 性能モードが通ったかの確認 | 最初のデータコールバックで `performance_mode()` を読み、LowLatency でなければ `ErrorKind::RealtimeDenied` を error callback へ流す(cpal `aaudio/mod.rs:417-440`)。本リポジトリではそれが `StreamError { Backend }` としてログとイベントに出る | 確認しない。開いた後の実効値(性能モード・共有モード・burst・バッファ長)もログに出さない(出すのはレートとチャンネル数だけ。`android.rs:583-588`) | **していない** |
| A6 | 共有 / 排他モード | 指定しない(既定 = SHARED)| 指定しない(既定 = SHARED) | 同じ。排他モードは `measurement-m1.md` §6.5 で「目標に届かなければ次の手」として保留中 |
| A7 | usage / content type / input preset | 指定しない(既定 = `USAGE_MEDIA`) | 指定しない | 同じ |
| A8 | コールバックの長さ(`framesPerDataCallback`)| `BufferSize::Default` なので指定しない(AAudio 任せ。実機で 96 frames = 2ms、§9.2) | 指定しない | 同じ |
| A9 | 開始の待ち方 | `request_start` の後、`Starting` から抜けるまで最大 2 秒待つ(cpal `aaudio/mod.rs:761-774`) | `AAudioStream_requestStart` だけで返る(`android.rs:569`) | 違う(影響は小さい。開いた直後の数コールバックで時刻の外挿が未確定になるのは両者同じ — A16) |
| A10 | Java 側への依存 | `ndk_context` が要る。cpal 0.18.1 で出力に関わるのは `AudioManager::get_mixer_bursts`(システムプロパティ `aaudio.mixer_bursts`。cpal `aaudio/mod.rs:526`)だけ。未初期化だと panic するので、本リポジトリは `JNI_OnLoad` → `android_context::ensure_initialized` で Application Context を登録している(`cpal_backend.rs:215`) | Java を一切使わない(AAudio の C API だけ) | 違う(依存が減る方向。4-4 で `android_context.rs`・`jni`・`ndk-context`・`jni_entry.rs` を消せる) |
| A11 | 最低 API | AAudio = API 26+(ndk 経由で `libaaudio` をリンク) | `#[link(name = "aaudio")]` で API 26+ | 同じ |

### 1.2 動いている間

| # | 項目 | cpal 版 | 自前 | 判定 |
|---|---|---|---|---|
| A12 | 音声スレッドの優先度 | AAudio が作るコールバックスレッドをそのまま使う。⚠️ **cpal 0.18.1 の AAudio ホストは `audio_thread_priority` を呼ばない**(呼ぶのは ALSA / PipeWire / WASAPI だけ。`realtime` feature は Android では依存を引き込み、性能モードを LowLatency にするだけ) | AAudio のコールバックスレッドをそのまま使う | 同じ |
| A13 | 書き込み前のバッファ | 毎回ゼロで埋めてからユーザーのコールバックを呼ぶ(cpal `aaudio/mod.rs:456-460`) | ゼロ埋めしない。`Renderer::render` が全体を書く前提。**フレーム数・ポインタが異常なとき(`validated_sample_count` が `None`)と panic を捕まえたときは何も書かずに返る**(`android.rs:317-319, 375-379`)| **違う**(異常時に前回の中身が残る = ブツ音・持続音になりうる) |
| A14 | xrun(OS 側のアンダーラン)の数え方 | `AAudioStream_getXRunCount` を**バッファ調整のためだけに**読む(cpal `aaudio/mod.rs:477-`)。数は外へ出さない(`ErrorKind::Xrun` は生成しない)。本リポジトリはコールバック間隔のヒューリスティック(`OutputUnderrunTracker::observe`)で「疑い」を数える | `OutputUnderrunTracker::observe` のみ(`android.rs:362-366`)。`getXRunCount` を読まない | 同じ(どちらも OS の実数を外へ出していない)。Linux 版は OS の実数を `record_reported_underrun` で数えている |
| A15 | 🔴 **xrun が起きたときのバッファ長の自動調整** | `BufferSize::Default` のとき有効。xrun 数が増えるたびに `setBufferSizeInFrames(burst × (mixer_bursts+1))` で1 burst ずつ伸ばす(上限は capacity。cpal `aaudio/mod.rs:477-512`)。初期値は `aaudio.mixer_bursts`(既定 2) | **していない**(モジュール doc に既知の差分として明記。`android.rs:55-60`) | **していない** |
| A16 | 出力時刻(`buffer_start_host_time_ns`)の意味 | `getTimestamp(CLOCK_MONOTONIC)` の対応点を `getFramesWritten` へ線形外挿 = **このバッファの先頭が DAC から出る予測時刻**。取れなければコールバック時刻(cpal `aaudio/convert.rs:32-44`) | 同じ式・同じフォールバック(`android.rs:331-355`、`project_frame_to_ns`) | 同じ。iOS と違い、意味は変わらない(実機校正の取り直しは不要の見込み) |
| A17 | 出力遅延(`output_latency_ns`)| 予測時刻 − コールバック時刻 | 同じ(`android.rs:357-360`) | 同じ |
| A18 | 時計 | `CLOCK_MONOTONIC`(cpal `convert.rs:8-16`)= `host_time_ns()` と同じ | `CLOCK_MONOTONIC` + `host_time_ns()`(`android.rs:209-214, 332`) | 同じ |

### 1.3 止まる・切れるとき

| # | 項目 | cpal 版 | 自前 | 判定 |
|---|---|---|---|---|
| A19 | 切断(有線の抜き差し・BT の切替)の観測 | AAudio の error callback(ndk 経由)→ `Error::from(AudioError)` → `err_fn` → `classify_stream_error` → `Event::StreamError`(`cpal_backend.rs:513-521, 598-608`) | AAudio の error callback → `error_proc` → `classify_aaudio_error` → `Event::StreamError`(`android.rs:391-410`) | 同じ(経路の形) |
| A20 | 🔴 **どのエラーを「再オープンすべき切断」とみなすか** | cpal が `Disconnected` / `Unavailable` / `NoService` / `InvalidHandle` → `DeviceNotAvailable`、`WouldBlock` / `Timeout` → `DeviceBusy`(cpal `convert.rs:58-84`)。本リポジトリはこの3種をすべて `DeviceUnavailable` に丸める → **内部再オープンの対象**。`Internal` / `InvalidState` は `Reconfigured`(再オープンしない) | `AAUDIO_ERROR_DISCONNECTED`(-899)**だけ**を `DeviceUnavailable`、他はすべて `Backend`(`android.rs:158-163`)。`-881`(`NO_SERVICE`)・`-889`(`UNAVAILABLE`)・`-892`(`INVALID_HANDLE`)・`-885`(`TIMEOUT`)が来ても**再オープンしない** | **違う** |
| A21 | 切断後の再オープン | `mw_poll_events`(ゲームスレッド、毎フレーム)が `StreamError { DeviceUnavailable }` を見て `note_stream_error` → `maybe_reopen`(`ffi.rs:1270-1272, 1297-1300`)→ 3段の再オープン(ADR 0002。`handle.rs:738, 1050, 1639`)。バックオフ 250〜4000ms・5回まで(`reopen.rs::REOPEN_BACKOFF_SCHEDULE_MS`)。音楽は位置を、BGM はトラックとループだけ戻す(`handle.rs:832-925`) | 同じ仕組みに乗る(`make_backend()` が `AndroidBackend` を返すだけ)| 同じ。✅ cpal 版は SH-M16 / Android 11 で有線の引き抜き → 約 0.5 秒で復帰、音とノーツが揃うことを確認済み(`docs/history/08-2026-09-04.md`)。**自前では未確認** |
| A22 | 🔴 **再オープンでサンプルレートが変わったとき** | 常に 48kHz を要求するので**変わらない**(A3) | 端末の既定レートを採るので、**新しいルートのレートが違えば変わりうる**(USB オーディオ・一部の BT 等)。ところが SE はロード時に「その時点の出力レート」へ一括リサンプルして常駐しており(`ffi.rs::load_se`、`handle.rs::backend_sample_rate`)、再オープン後は**ロード済みの SE がずれたレートのまま鳴る**(音程と長さが比率ぶんずれる)。音楽・BGM はデコーダを新しいレートで作り直す(`handle.rs:850, 905`)が、`MusicSeek { frames: clock_before.song_frames }` は前のレートのフレーム数をそのまま渡している(単位の扱いは未確認) | **違う**(cpal 版では起きなかった不具合の入口) |
| A23 | 通知の来ない停止(コールバックが止まったまま、error callback も来ない)| **両方していない**。iOS には `OutputStallDetector` + ウォッチドッグがある(`ios_interruption.rs:530-`。iOS / tvOS 限定)が、Android には無い | 同左 | 両方していない |
| A24 | 閉じ方 | `pause()`(`request_pause` + 待つ)→ ndk の `Drop` が `AAudioStream_close` → その後でコールバックの Box を解放(`cpal_backend.rs:295-`、ndk `audio.rs:1414-`) | `requestStop` → `close` → その後で `CallbackContext` / `ErrorContext` を解放(`android.rs:597-643`) | 同じ。⚠️ 「error callback が別スレッドで走っている最中に close が来たら」の扱いは両者とも AAudio 任せで同じ(C12 で連続の抜き差しを確かめる) |
| A25 | 再オープンの実行場所 | ゲームスレッドの `mw_poll_events` が起点。**アプリが背面にいて Unity のフレームが止まっている間は再オープンしない**(前面に戻ってから) | 同じ | 同じ |

### 1.4 OS・アプリのライフサイクル

| # | 項目 | cpal 版 | 自前 | 判定 |
|---|---|---|---|---|
| A26 | オーディオフォーカス(Java の `AudioManager.requestAudioFocus`)| cpal は扱わない。本リポジトリも扱わない(Java 側の仕事を持たない方針。`unity/Runtime` にも無い)| 扱わない | 両方していない |
| A27 | 背面・前面(Activity の `onPause` / `onResume`)| 監視しない。AAudio のストリームは背面でもコールバックが続く(ゲーム側が止めない限り鳴り続ける)| 監視しない | 両方していない |
| A28 | 画面の回転・構成変更 | ストリームは Activity に結び付いていない(Context も Application のもの)ので影響を受けない | 同じ(Context 自体を持たない) | 同じ |
| A29 | 着信・通話 | 何もしない(AAudio とオーディオポリシー任せ) | 同じ | 両方していない |

---

## 2. 4-4(cpal の削除)の前に足すべきもの

cpal 版が**黙ってやっていた**のに自前に無いもののうち、実機で問題になりうるもの。優先度順。

1. 🔴 **サンプルレートの扱いを cpal 版に揃える(A3 / A22)。**
   いちばん素直なのは `AAudioStreamBuilder_setSampleRate(48000)` で 48kHz を要求すること
   (cpal 版と同じ。端末側のレートが違えば AAudio が変換する)。
   端末の既定レートを使い続けたいなら、代わりに「再オープンでレートが変わったら、ロード済みの SE を
   新しいレートで作り直す / 音楽の位置をレート比で換算する」まで要る。前者のほうが小さい。
2. 🔴 **エラーの分類を cpal 版と揃える(A20)。**
   `classify_aaudio_error` で `UNAVAILABLE(-889)` / `NO_SERVICE(-881)` / `INVALID_HANDLE(-892)` /
   `TIMEOUT(-885)` / `WOULD_BLOCK(-884)` も `DeviceUnavailable` にする(`INTERNAL(-896)` /
   `INVALID_STATE(-895)` は `Reconfigured`)。今の分類では、オーディオサーバの再起動など
   切断以外の理由で止まったときに再オープンが走らない。
3. **xrun のときのバッファ長の自動調整(A15)。**
   cpal 版は既定で有効だった。無いと、負荷の重い場面(描画が重い・動画を再生している等)で
   起きた xrun が**ずっと直らずにプチプチが続く**。cpal と同じく、データコールバックの中で
   `AAudioStream_getXRunCount` が増えていたら `AAudioStream_setBufferSizeInFrames` で1 burst 伸ばす
   (cpal もデータコールバックの中で呼んでいる。初期値は `getBufferSizeInFrames / getFramesPerBurst`)。
   併せて xrun の実数を `OutputUnderrunTracker::record_reported_underrun` で数える(A14。Linux 版と同じ)。
4. **開いた後の実効値をログに出す(A5)。**
   `AAudioStream_getPerformanceMode` / `getSharingMode` / `getFramesPerBurst` /
   `getBufferSizeInFrames` / `getBufferCapacityInFrames` / `getSampleRate` を `open` で1回だけ出す。
   LowLatency が通らなかった端末の検出は cpal 版では `RealtimeDenied` が担っていた。
   実機で問題が起きたときに唯一の手がかりになる(`measurement-m1.md` §6.2 の教訓)。
5. **異常時は出力バッファをゼロで埋める(A13)。**
   `validated_sample_count` が `None` のとき(ポインタが有効な場合)と、`catch_unwind` が panic を
   捕まえたとき。cpal 版は常にゼロ埋めしていた。

条件付き(実機で症状が出たら):

6. **通知の来ない停止のウォッチドッグ(A23)。** iOS では V14(通知が1つも来ないまま止まる)で必要になった。
   Android では cpal 版でも無かったので「パリティ」ではないが、症状(無音・アプリ再起動で直る)が出たら、
   純粋ロジックの `OutputStallDetector` を流用してゲームスレッド側(`mw_poll_events`)から
   コールバックの前進を見るのが最小の形。

4-4 で一緒に片付けるもの(足すのではなく消す):

- `android_context.rs`・`mw-ffi/src/jni_entry.rs`(`JNI_OnLoad`)・Android 向けの `jni` / `ndk-context` 依存は、
  自前の出力では使わない(A10)。
- `mw-backend/Cargo.toml` の Android 向け `cpal`(`realtime`)のコメントと `measurement-m1.md` §6.5 にある
  「音声コールバックスレッドの優先度も上がる」は、cpal 0.18.1 の AAudio ホストについては当たっていない(A12)。
  cpal を消すときにまとめて直す。

---

## 3. Android 実機での確認項目

`backend-native` で焼いた Android 版で行う。🔴 **すべて `adb logcat` を取りながら**行い、
`[mw-backend] (native/android)` と `[mw-ffi]` の行を残す(「音が戻った」だけでは
どの経路で戻ったかが分からない —— `docs/history/08-2026-09-04.md` の BT の件)。
比較のため、できれば同じ端末で `backend-cpal` 版でも同じ操作をする。

| # | 操作 | OK の条件 | 対応 |
|---|---|---|---|
| C1 | 起動して曲を流す | `output stream started: sample_rate=…` が出る。コールバックのフレーム数が cpal 版と同程度(SH-M16 なら 96 frames 前後)。2節の4 を入れた後なら性能モードが LowLatency | A3 / A4 / A5 |
| C2 | プレイ中に**有線イヤホンを抜く** | `output stream error: -899` → `attempting internal stream reopen` → 成功。約 0.5 秒以内にスピーカーから戻り、**音とノーツが揃っている** | A19 / A21 |
| C3 | プレイ中に**有線イヤホンを挿す** | 音が止まらない、または C2 と同じ経路で戻る(どちらだったかをログで記録) | A19 / A21 |
| C4 | プレイ中に **BT イヤホンを繋ぐ・切る** | 音が戻る。切断イベントが来たのか、ルート変更だけで済んだのかをログで記録 | A19 / A21 |
| C5 | 🔴 **44.1kHz など 48kHz 以外の出力**(USB オーディオ、レートの違う BT 機器)へ切り替えてから SE を鳴らす | SE の音程・長さが変わらない。曲の位置がずれない。※ 2節の1 を入れる前は**ここで壊れる見込み** | A3 / A22 |
| C6 | 重い場面(動画の再生・描画の重い画面)で数分鳴らし続ける | プチプチが続かない。2節の3 を入れた後は xrun 数と `setBufferSizeInFrames` の伸びがログに出る | A14 / A15 |
| C7 | プレイ中に**ホームへ戻り、30 秒以上待ってから戻る** | 戻ったあと音が出る。背面にいる間に鳴り続けたかどうかを cpal 版と比べて同じであること(どちらも背面で止める仕組みは持たない) | A25 / A27 |
| C8 | プレイ中に**画面を消して**1分以上待ち、点けて戻る | C7 と同じ | A25 / A27 |
| C9 | プレイ中に**着信**を受ける(出る / 出ない)、アラームを鳴らす | 終わったあと音が出る。止まった場合は error callback が来たかをログで確認 | A29 / A20 |
| C10 | プレイ中に**別の音楽アプリ**を鳴らす | cpal 版と同じ振る舞い(重なる / 止まる)であること | A26 |
| C11 | プレイ中に端末を**回転**する(回転を許す画面で) | 音が途切れない | A28 |
| C12 | 抜き差しを**10回ほど続けて**行う | クラッシュしない・最後に音が戻る。バックオフを使い切ったら、次の抜き差しで仕切り直すこと | A21 / A24 |
| C13 | (2節の2 を入れた後)開発者オプション等で**オーディオサーバを再起動**できる端末なら行う | `-899` 以外のエラーでも再オープンが走る | A20 |

---

## 4. iOS(`apple.rs`)の同じ表

iOS は `ed0236d` で、cpal 版の周り(`ios_interruption.rs`)と同じ監視を自前の出力にも付けた
(監視と判断は共有し、復帰の操作だけ `RecoverableOutput` で差し替え)。

| # | 項目 | cpal 版(+ 周り) | 自前(`apple.rs`) | 判定 | 実機 |
|---|---|---|---|---|---|
| I1 | セッションの設定(カテゴリ・希望レート・I/O バッファ長) | `ios_session::configure` | 同じ関数を `open` の前に呼ぶ | 同じ | 確認済み(起動・発音) |
| I2 | 割り込み(着信など)の Began / Ended | `ios_interruption::Watcher` → `pause()`/`play()` | 同じ Watcher → `AudioOutputUnitStop` / `Start`(失敗したら Initialize し直し)| 同じ(`ed0236d`) | **未確認**(着信・Siri・アラーム) |
| I3 | 背面・前面(`DidEnterBackground` / `DidBecomeActive`)| Watcher | 同じ Watcher | 同じ(`ed0236d`) | ✅ スリープ・背面からの復帰で音が止まらない・復帰直後のずれが無いことを確認済み |
| I4 | ルート変化(有線・BT の抜き差し)| cpal の `session_event_manager` が `StreamError`(`Reconfigured` / `DeviceUnavailable`)を出す + Watcher が復帰 | Watcher が復帰。`IosOutputLatencyWatcher` が `outputLatency` を読み直す。**`StreamError` は出さない** | 違う(C# 側が iOS の `StreamError` に依存していないことを確認すること) | **未確認** |
| I5 | メディアサービスのリセット | cpal は `StreamError` を出すだけで作り直さない | `MediaServicesWereReset` を監視して AudioUnit を**作り直す**(`supports_rebuild`)| 違う(良くなる方向) | **未確認**(設定 → デベロッパ → メディアサービスのリセット) |
| I6 | 通知の来ない停止 | ウォッチドッグ(`OutputStallDetector`、1秒)| 同じ | 同じ | 間接的に確認(I3)。⚠️ 動画再生がセッションのカテゴリを変えた場合はウォッチドッグ頼みで約1秒の無音が残りうる |
| I7 | 出力時刻の意味 | cpal 0.18.1 は `IOBufferDuration` だけ(`outputLatency` を足さない)| `outputLatency` を補正項として足す = DAC 出力時刻の予測 | **違う(意図的)** | 校正値(出力遅延のオフセット)の取り直しが要る |
| I8 | 再始動のたびの音楽クロックの世代 | — | `restart_epoch` で世代を進める | 足した | 確認済み(I3 のずれ無し) |
| I9 | Control Center・通知バナー | Watcher が `DidBecomeActive` を割り込み中だけ扱う | 同じ | 同じ | **未確認**(音が途切れない・ずれないこと) |
| I10 | macOS(Editor)のデバイス切断・既定出力の変更 | cpal が扱う | 監視しない・`StreamError` も出さない | していない(開発機専用として許容) | — |

---

## 5. この文書の更新

- 2節の項目を実装したら、該当する行の判定を書き換え、実装したコミットを備考に足す。
- 3節の確認を行ったら「実機」の結果(端末名・OS 版・日付)を書き足す。
  確認が済んだ時点で、4-4 に進めるかの判断材料はこの文書にそろう。
