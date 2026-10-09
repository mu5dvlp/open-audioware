# mw-backend

出力デバイス抽象(`Backend` trait)と、その cpal 実装(`CpalBackend`)。
`mw-core` にのみ依存する(依存の向きは `mw-backend → mw-core`。ワークスペース全体の方針は
`../../CLAUDE.md` を参照)。`mw-ffi` からのみ利用される。

## 現状(M1)

- `backend::Backend` — `open(&mut self, renderer: Renderer, events: Arc<EventQueue>)` /
  `close(&mut self)` / `is_open(&self)`。実装はパニックせず、失敗は必ず `BackendError`
  で返す。`renderer` は**値渡し(ムーブ)**。`Renderer` はコールバックスレッド専用の
  排他所有物としてストリームクロージャへムーブされる(M0 時点の `Arc<Renderer>` 共有から
  変更。`crates/mw-core/src/renderer.rs` のドキュメント参照。ゲームスレッドは
  `mw_core::CommandSender` / `mw_core::ReclaimReceiver` という別ハンドル経由でのみ
  音声スレッドとやり取りする)。`events` は初期構築仕様『§4.6 イベント通知』のキュー
  (M2-6)。`CpalBackend` はこれをストリームのエラー通知経路(`err_fn`、**音声スレッドとは
  別の**非リアルタイムスレッド)から `push_side_channel` で `StreamError` イベントを積むために
  使う(`cpal_backend::classify_stream_error` が `cpal::ErrorKind` を
  `mw_core::StreamErrorReason` へ丸める)。この経路は §5.3 の対象外なので `Mutex` を
  使ってよい——実際に音声コールバック本体(`build_output_stream` のデータコールバック)
  から呼ぶのは引き続き `Renderer::render` のみ(下記「設計意図」参照)。
- `backend::last_callback_frames` / `backend::sample_rate` — オーディオコールバックが実際に
  受け取ったフレーム数と、ネゴシエートされたサンプルレート。**I/O バッファ長の実測値**で、
  iOS の `AVAudioSession` の申告値を裏取りするために使う(`docs/measurement-m1.md` §8.7)。
  音声スレッドはアトミックストア1回だけ行い、読むのはゲームスレッド
  (リアルタイム安全性規約に抵触しない)。初期構築仕様 M3「出力レイテンシ問い合わせ」の第一歩。
- `backend::BackendError` — デバイス無し・対応構成無し・ストリーム構築/開始失敗・
  二重 open・未 open close、の6種。
- `backend::log_output_latency_once` / `backend::log_new_output_underruns` —
  🔴 **2026-09-23 に `CpalBackend` の固有メソッドから `Backend` トレイトの必須メソッドへ
  上げた。** `mw-ffi` がバックエンドを `Box<dyn Backend + Send>` で持つようになり
  (下記「テストダブルを差せる形にした」)、`dyn` 越しに呼べる必要が生じたため。
  **既定実装は付けていない** —— 将来の oboe / RemoteIO 実装が「気付かずにログを失う」
  のを避けるため(実装しなければコンパイルが通らない)。どちらも**ゲームスレッド専用**
  (`mw_log!` はアロケーションとロックを伴う)。
- `cpal_backend::CpalBackend` — 既定の出力デバイスに **f32 ステレオ**のストリームを開く。
  対応する構成が無い場合(モノ/サラウンド専用デバイス、非対応サンプルフォーマット等)は
  `BackendError::NoSupportedStreamConfig` を返す(サンプルフォーマット変換は未実装、将来課題)。
  デバイスが実際にネゴシエートしたサンプルレートは、コールバックが動き出す
  (`stream.play()`)前に `Renderer::set_sample_rate` で確定させる(バス/ボイスのランプの
  ミリ秒→サンプル数換算に必要。初期構築仕様 §4.1)。オーディオコールバック内では
  cpal の `OutputCallbackInfo::timestamp().playback`(`StreamInstant`)を ns 化して
  `Renderer::render` の `buffer_start_host_time_ns` 引数へ渡す(初期構築仕様『§4.4』の
  デバイスタイムスタンプ相関、M2-5)。呼ぶのは引き続き `Renderer::render` のみ
  (下記「設計意図」の制約を参照。`StreamInstant` を読むのは cpal が既に計算済みの
  構造体を見るだけで、mw-core への別呼び出しを増やしたわけではない)。

- `host_time::host_time_ns` — ホスト単調時刻を ns で返す(初期構築仕様『§4.4』, M2-5)。
  macOS/iOS/tvOS は `mach_absolute_time` + `mach_timebase_info`、Android(と CI 専用の
  Linux)は `clock_gettime(CLOCK_MONOTONIC)`。**cpal 0.18.2 の `StreamInstant` を
  各ホストがどう生成しているかを実装まで確認したうえで、同じ式をそのまま再現している**
  (調査結果と根拠は `host_time.rs` のモジュール doc に記載済み)。これにより
  `mw_host_time_ns()`(FFI)が返す値と `OutputCallbackInfo` のデバイスタイムスタンプが
  直接比較可能になる。`mw-ffi::mw_host_time_ns` がここへ薄く委譲する。

- `ios_session::configure` — iOS / tvOS で AVAudioSession(カテゴリ・希望サンプルレート・
  希望 I/O バッファ長)を設定し、採用された実値をログへ出す。`CpalBackend::open` の冒頭、
  **cpal に触る前に**呼ぶ(iOS の cpal はデバイス列挙をセッションの現在状態から作るため)。
  iOS / tvOS 以外では何もしない。初期構築仕様 §14 のリスク表は Obj-C シムを想定していたが、
  cpal 0.18 が `objc2-avf-audio` を持ち込むため Rust から直接設定している(詳細は同ファイルの
  モジュールコメント)。設定値はすべて【仮】で、変更が要るのはこのファイル1枚。

- `ios_interruption`(M3)— iOS / tvOS の `AVAudioSessionInterruptionNotification` /
  `UIApplicationDidBecomeActiveNotification` を監視し、割り込み終了時・アプリのアクティブ化時に
  セッション再アクティブ化 + ストリーム再始動(`pause()`→`play()`)を試みる。実機報告
  「バックグラウンドから戻ると SE だけ無音になる」の修正(cpal 0.18.2 の iOS 実装は
  interruption 通知を監視しておらず、`Stream::play()` の内部フラグも OS 主導の停止に
  追随しないため、単純な再呼び出しでは復帰しない——詳細はモジュール doc)。`CpalBackend`
  はストリームを `Arc<cpal::Stream>` として持ち、この監視の復帰ハンドラと共有する。
  復帰ロジックは `InterruptionState`(OS 呼び出しを含まない純粋な状態機械)として切り出し、
  単体テストで遷移を固定化してある(実機の割り込みそのものは自動テスト不可のため)。
  `mw_core::Event::AudioInterruptionBegan`/`AudioInterruptionEnded { recovered }`
  として `mw_poll_events` 経由で C# 側からも観測できる。iOS / tvOS 以外では no-op。
  Android の AAudio disconnect は同じ経路では塞げない(iOS のような同一ストリームへの
  `pause()`→`play()` 再試行は、AAudio の切断が ndk/AAudio の API 契約上終端的
  〔`AudioError::Disconnected` のドキュメント「the stream cannot be used after the
  device is disconnected」〕なため原理的に効かない)。cpal の AAudio ホストは切断を
  `err_fn`(`cpal_backend.rs::build_output_stream` の一般エラー通知経路)経由で
  `Event::StreamError { reason: DeviceUnavailable }` として報告し
  (`classify_stream_error` の単体テストで固定化済み)、**それを受けての内部再オープン
  (初期構築仕様『§6』案A)は `mw-ffi` 側に実装済み**(`crates/mw-ffi/src/reopen.rs`
  ——バックオフ付きの再試行判定 `ReopenPolicy`。`crates/mw-ffi/src/handle.rs::
  Instance::attempt_reopen` ——実際の `CpalBackend::close()`→`Renderer::
  build_with_events()`→`CpalBackend::open()` と状態復元。`mw_poll_events`
  〔`ffi.rs`〕から `#[cfg(not(any(target_os = "ios", target_os = "tvos")))]` で
  ゲーティングして呼ぶため、iOS/tvOS ではこのモジュール自身の `ios_interruption`
  経路と一切競合しない)。経緯・実装判断の詳細は `docs/history/04-2026-08-31.md`
  「M3 完了」参照。⚠️ 実機(Android)での動作確認はまだ行っていない。

- `underrun::OutputUnderrunTracker`(M3「アンダーラン検知・テレメトリ」)— 音声
  コールバックの間隔が想定より大きく開いたこと(「アンダーラン(の疑い)」)を検知する。
  **`mw_core::Event::Underrun`(楽曲/BGM のデコードリングバッファ枯渇の検知、M2)とは
  別物**——こちらは音声コールバック自体の間隔異常で、OS 側出力バッファの実際の
  枯渇の兆候を指す。cpal 自身のアンダーラン通知(`ErrorKind::Xrun`)は cpal 0.18.2
  時点で本プロジェクトが使う3ホスト(macOS/iOS の `coreaudio`、Android の `aaudio`)
  のどれも生成しないため採用できず(調査結果は `underrun.rs` モジュール doc 参照)、
  代わりに `OutputCallbackInfo::timestamp().callback` の間隔を毎コールバック追跡する
  ヒューリスティックにしてある。`CpalBackend` が累計検知回数・直近検知時刻・連続検知数の
  3値を `Arc<Atomic*>` で持ち(`callback_frames` 等と同じ設計。複数インスタンス・
  複数テストの並列実行で混ざらないよう `static` にはしていない)、
  `Backend::output_underrun_count` 等でゲームスレッドから読める。
  `CpalBackend::log_new_output_underruns`(ゲームスレッド専用、`log_output_latency_once`
  と同じ配線)が新規検知分だけ日和見的にログへ出す。

- `native_backend::apple::AppleBackend`(AUDIOWARE-DEPS-PLAN.md ステップ4-1〔macOS〕・
  4-2〔iOS/tvOS〕)— macOS の既定出力デバイス(AUHAL、`kAudioUnitSubType_DefaultOutput`)/
  iOS・tvOS のハードウェア I/O(RemoteIO、`kAudioUnitSubType_RemoteIO`)へ AudioUnit で
  直接出力する `Backend` 実装。新しい外部クレートは追加していない——AudioToolbox /
  CoreAudio フレームワークの C API を自前の `extern "C"` 宣言で直接叩く
  (`objc2-audio-toolbox` 等は使わない。それらは `objc2`/`objc2-foundation` を引き込むため
  依存排除の到達点に反する)。iOS/tvOS のルート変化監視だけは `ios_interruption.rs` が
  既に使っている `objc2-foundation`/`block2`(NSNotificationCenter)と
  `objc2-avf-audio`(AVAudioSession)を再利用する(いずれも既存の iOS/tvOS 向け依存。
  新規追加なし)。手順は `AudioComponentFindNext` → `AudioComponentInstanceNew` →
  `AudioUnitSetProperty`(StreamFormat / SetRenderCallback)→ `AudioUnitInitialize` →
  `AudioOutputUnitStart`(iOS/tvOS では直前に `ios_session::configure()` を呼ぶ)。
  タイムスタンプは `AudioTimeStamp.mHostTime` を `host_time::mach_ticks_to_ns`
  (cpal 版の `host_time_ns` と同じ式)で ns 化したものを**相関点**とし、
  `compute_timestamps`(`apple.rs`)でバッファ長(このコールバックの実測フレーム数)+
  **補正項**(`device_extra_latency_ns`。OS ごとに出し方が違う——下記)を加えたものを
  `Renderer::render` の `buffer_start_host_time_ns` として渡す。相関点と補正項は
  意図的に別々の状態として持つ(HANDOFF 0f の統合。cpal 0.18.2 が iOS の `playback` へ
  `AVAudioSession.outputLatency()` を無断で足し、校正済みの音楽クロックが実機で
  約65ms ずれた〔`9fdf287`〕のと同じ事故を、ここでは「補正項だけを独立かつ明示的に
  更新できる」構造にすることで避ける):
  - **macOS**: `open()` で `query_device_extra_latency_frames`
    (`AudioObjectGetPropertyData` で読む `kAudioDevicePropertyLatency` +
    `kAudioDevicePropertySafetyOffset`。cpal 0.18.1 の `get_device_extra_latency_frames`
    と同じ2プロパティ)を1度だけ読み、以後固定(4-1 の既知の差分、デバイス切断監視は未実装)。
  - **iOS/tvOS**: `open()` で `AVAudioSession.outputLatency()`
    (`query_ios_output_latency_ns`)を読んで初期化し、以後は `IosOutputLatencyWatcher`
    (`apple.rs` の `ios_impl` サブモジュール)が `AVAudioSessionRouteChangeNotification`
    を監視してルート(スピーカー/有線/Bluetooth)が変わるたびに読み直す。
    `IOBufferDuration` は別途読まない——バッファ長はレンダーコールバックの実測フレーム数
    (`compute_timestamps` の引数)が既に正確に捉えており、二重計上を避けるため。
    🔴 **cpal 0.18.1(固定中)の iOS 実装は `outputLatency()` を一切読んでいない**
    (`IOBufferDuration` だけ)——この実装は意図的にその意味を変え、`mw-core::Mixer::
    render` と client 側 `NativeMusicClockCore.cs` が最初から要求している
    「DAC 出力時刻の予測」を正しく満たす。`backend-cpal` が既定のままなので本番には
    影響しないが、実際に iOS を `backend-native` へ切り替える段では出力レイテンシの
    前提が変わるため、`AudioOffsetSeconds` 等の実機校正を取り直す必要がある
    (client 側のクラス doc が既に明記している帰結)。
  `OutputUnderrunTracker` は cpal 版と共用。
  **iOS/tvOS の割り込み・バックグラウンド・ルート変化・出力停止からの復帰は cpal 版と
  同じ `ios_interruption::Watcher` を使う**(監視と判断は共有し、復帰の操作だけを
  `ios_interruption::RecoverableOutput` の実装で差し替える。cpal 版は `StreamHandle`
  〔`pause()`/`play()`〕、Apple 版は `UnitControl`〔`AudioOutputUnitStop`/
  `AudioOutputUnitStart`、Start に失敗したら Initialize し直し〕)。Apple 版だけは
  出力を**作り直せる**(`supports_rebuild`)——
  `AVAudioSessionMediaServicesWereResetNotification` を監視し、止め直しを繰り返しても
  進まないときの最後の手段にも使う。
  作り直すのは AudioUnit だけで、`Renderer` を持つコールバックの context は同じ
  ポインタのまま付け替える(サンプルレートは開いたときの値のまま)。AudioUnit は
  `UnitControl` の `Mutex` の中にあり、`close` と復帰が同じ口を通る(閉じた後の操作は
  空振り。音声スレッドはこのロックに触れない)。動かし直し・作り直しのたびに
  `restart_epoch` を進め、音声スレッドが次のコールバックで音楽クロックの世代を進める
  (補正項の変化と同じ経路)。動かし直しの直前に補正項(`outputLatency`)も読み直す。
  **既知の差分**: macOS のデバイス切断・既定出力の変更の監視は未実装、`StreamError` は
  発行しない(4-1 は macOS Editor 専用の開発機バックエンドという前提で許容)。
  `mw-ffi` の切替口(`handle.rs::make_backend`)が `backend-native`
  feature(macOS/iOS/tvOS)で選ぶ。既定の `backend-cpal` では選ばれない。

- `native_backend::android::AndroidBackend`(AUDIOWARE-DEPS-PLAN.md ステップ4-3)——
  Android の既定出力へ `libaaudio.so`(API 26+)で直接出力する `Backend` 実装。
  新しい外部クレートは追加していない(`ndk`/`ndk-sys` は使わず自前の `extern "C"`
  宣言)。手順は `AAudio_createStreamBuilder` → `setPerformanceMode`
  (`AAUDIO_PERFORMANCE_MODE_LOW_LATENCY`。cpal の `realtime` feature が Android で
  低遅延を得るために設定していたのと同じ値で、`audio_thread_priority`〔MPL〕への
  依存をこの実装自体は持たない)→ `setDataCallback`/`setErrorCallback` →
  `AAudioStreamBuilder_openStream` → `AAudioStream_requestStart`。
  タイムスタンプは macOS/iOS のような「相関点 + 補正項」の分離を持たない——
  `AAudioStream_getTimestamp` が毎コールバック「フレーム位置 `anchor_frame` が
  ホスト単調時刻 `anchor_time_ns` に出力される」という対応点を返すので、
  これを `AAudioStream_getFramesWritten`(このコールバックが書き込むバッファの
  先頭フレーム位置)へ線形に外挿して予測出力時刻を求める——cpal 0.18.1 の AAudio
  実装(`cpal::host::aaudio::convert::{output_stream_instant,
  stream_instant_from_anchor}`)と**全く同じ式**(iOS と異なり cpal の Android 実装は
  最初から「予測される DAC 出力時刻」を正しく計算しているため、意味を変える必要が
  無い)。外挿が未確定(ストリーム開始直後等)の場合はコールバックの呼び出し時刻
  そのものへフォールバックする(cpal の `now_stream_instant()` フォールバックと同じ)。
  サンプルレートは cpal 版と同じく 48kHz を要求する(開けたなら 48kHz。端末の内部レートが
  違えば AAudio が変換する。再オープンでレートが変わらないので、ロード時に出力レートへ
  リサンプルした SE がずれない)。
  切断(`AAUDIO_ERROR_DISCONNECTED`)と、cpal が `DeviceNotAvailable` / `DeviceBusy` に
  していた `UNAVAILABLE` / `NO_SERVICE` / `INVALID_HANDLE` / `TIMEOUT` / `WOULD_BLOCK` は
  `Event::StreamError { reason: DeviceUnavailable }` として `events` 経由で通知し
  (`INTERNAL` / `INVALID_STATE` は `Reconfigured`。分類は `classify_aaudio_error`)、`mw-ffi` の既存の内部再オープン
  (`handle.rs::Instance::attempt_reopen`、cpal 版の Android 切断経路がこれまで使っていた
  のと同じ仕組み)へそのまま乗る——AppleBackend と異なりここは `events` を使う。
  `android_context`(JavaVM/Context の `ndk_context` 登録)は cpal が Java 側
  `AudioManager` を参照するためだけに要るものなので、この実装は呼ばない
  (AAudio の生 C API 自体はネイティブに閉じており Context を要求しない)。
  cpal 版の `tune_dynamically` と同じく、データコールバックの中で
  `AAudioStream_getXRunCount` が増えたら `setBufferSizeInFrames` で 1 burst ずつ伸ばす
  (上限は capacity。判断は `XrunBufferTuner`)。xrun の実数は
  `OutputUnderrunTracker::record_reported_underrun` で数える(間隔のヒューリスティックと併用)。
  ⚠️ cpal 0.18.1 は成功時の戻り値〔正の値〕を失敗と見なすため実際には1段しか伸ばさない。
  ここは capacity まで伸ばす。
  `open` で性能モード・共有モード・レート・burst・バッファ長・capacity を1回ログに出し、
  LowLatency が通らなければ cpal の `RealtimeDenied` と同じ `StreamError { Backend }` を積む。
  フレーム数・ポインタが異常なとき・panic を捕まえたときは書ける範囲をゼロで埋める。
  `OutputUnderrunTracker` は cpal 版・AppleBackend と共用。`mw-ffi` の切替口が
  `backend-native` feature(Android を含む5OS)で選ぶ。既定の `backend-cpal` では
  選ばれない。純粋ロジック(タイムスタンプの外挿計算・バッファ検証・ゼロ埋めの範囲・エラー分類・
  バッファを伸ばす判断)は
  `target_os` を問わずコンパイルされ、ホスト(macOS)の `cargo test` で固定化している
  (`native_backend::android` モジュール doc参照)。

- `native_backend::linux::LinuxBackend`(AUDIOWARE-DEPS-PLAN.md ステップ4-4 の前提)——
  Linux の ALSA `default` へ `libasound` の C API(自前の `extern "C"` +
  `#[link(name = "asound")]`。新しいクレートは追加していない)で出力する `Backend` 実装。
  コールバック駆動ではなく、**専用スレッドが `Renderer::render` → ブロッキングの
  `snd_pcm_writei` を回す**(cpal の ALSA ホストと同じ方式)。`snd_pcm_set_params` は
  FLOAT_LE・インターリーブ・2ch・48kHz・`soft_resample=1`・総レイテンシ
  `TARGET_LATENCY_US`(【仮】20ms)。バッファ先頭の出力時刻は `host_time_ns()` +
  `snd_pcm_delay`(`playback_start_ns`)、出力レイテンシはその遅延分。xrun は
  `snd_pcm_recover` で復旧して `OutputUnderrunTracker::record_reported_underrun` で数える。
  🔴 **時計を持たない `null` デバイスでは `writei` が待たないので、`throttle_sleep_ns` で
  「書いたフレーム数が実時間 + バッファ長を超えたら眠る」**(外すと 1 コア空回りする)。
  実デバイスでは発動しない。デバイスが無ければ `open` は `NoOutputDevice` で返る。
  **既知の差分**: 音声スレッドの優先度は上げない / デバイス切断・既定出力変更の監視は無い
  (書き込みが致命的に失敗したときだけ `Event::StreamError` を積んでスレッドを終える)。
  純粋ロジックは `target_os` を問わずホストの `cargo test` で固定。`null` デバイスでの
  動作確認は `#[ignore]` のテスト(`ALSA_CONFIG_PATH` を `pcm.!default { type null }` だけの
  設定ファイルへ向けて `cargo test -p mw-backend -- --ignored`)。

## 設計意図

- 初期構築仕様 M6(【仮】): 立ち上げは cpal で macOS Editor / iOS / Android を1系統に揃える。
  計測の結果 cpal で遅延目標(iOS ≤ 20ms / Android ≤ 40ms)に届かない場合、
  Android は oboe 直叩き、iOS は RemoteIO 直叩きに置換する可能性がある。
  その置き換えは `Backend` trait を実装する新しい struct を追加するだけで済むようにしてある
  — `mw-ffi` 側は `Backend` トレイトオブジェクト(または将来的にジェネリクス)越しにしか
  バックエンドを触らない設計を維持すること。
  🔴 **2026-09-23 に実際にそうなった**(それまで `Instance.backend` は具象型
  `CpalBackend` を直接持っていた)—— `mw-ffi` は `Box<dyn Backend + Send>` で持つ。
  きっかけは置き換えではなく**テスト**: CI にもビルドマシンにもオーディオデバイスが
  無い場面で `CpalBackend::open()` が必ず失敗し、内部再オープンの段2・段3
  (`mw-ffi::handle::run_reopen_worker` / `finalize_reopen_success` /
  `teardown_orphaned_reopen`)へ到達する手段が他に無かった。
  ⚠️ **リアルタイム安全性への影響は無い** —— 音声コールバックは `Instance` を経由せず、
  `Renderer` は `Backend::open` へムーブ済み。動的ディスパッチが乗るのはゲームスレッドの
  FFI 呼び出しだけで、そこは元々 FFI 越えのコストを払っている。
- オーディオコールバック(`build_output_stream` に渡すクロージャ)は音声スレッド上で実行される。
  ここから呼ぶのは `Renderer::render` のみに保つこと
  (`mw-core/CLAUDE.md` のリアルタイム安全性規約がこの経路にも適用される)。

## Android ビルド時の依存について

`cpal` 0.18 は Android で `ndk` / `ndk-context` / `jni` クレート経由で AAudio を直接叩く
実装を持ち、追加の feature flag は不要(`oboe` クレートには依存しない)。
`cargo ndk` でのクロスビルドに特別な設定は要らない(`make build-android` 参照)。
