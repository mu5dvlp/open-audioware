//! cpal を使った `Backend` 実装。
//!
//! 初期構築仕様 M6(【仮】): 立ち上げは cpal で macOS Editor / iOS / Android を1系統で扱う。
//! 計測後、必要なら Android を oboe 直叩き、iOS を RemoteIO 直叩きに置換する可能性がある
//! (その際もこの `Backend` trait 経由で差し替えられるようにしてある)。

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig, SupportedStreamConfig};
use mw_core::{CHANNELS, Event, EventQueue, Renderer, StreamErrorReason};

use crate::backend::{Backend, BackendError};
use crate::ios_interruption;
use crate::underrun::OutputUnderrunTracker;

/// 既定の出力デバイスに f32 ステレオストリームを開く `Backend` 実装。
///
/// デバイスが無い環境(CI・ヘッドレスマシン等)では `open` が
/// `BackendError::NoOutputDevice` を返す。パニックはしない。
#[derive(Default)]
pub struct CpalBackend {
    /// `Arc` で持つ理由: iOS の割り込み監視([`ios_interruption::Watcher`])が
    /// OS 通知ハンドラ(非リアルタイムスレッド)から同じストリームの `pause()`/`play()`
    /// を呼び直せるよう、参照を共有する必要があるため(`ios_interruption.rs` の
    /// モジュール doc「実装方針」参照)。iOS / tvOS 以外では単なる所有権共有としてのみ
    /// 使う(`Watcher` は no-op)。
    stream: Option<Arc<cpal::Stream>>,
    /// iOS / tvOS: `AVAudioSessionInterruptionNotification` /
    /// `UIApplicationDidBecomeActiveNotification` の監視・復帰処理(M3)。
    /// それ以外の OS では no-op(`ios_interruption.rs` 参照)。`stream` と1対1で
    /// 生成・破棄する(`open`/`close` を参照)。
    ios_interruption: Option<ios_interruption::Watcher>,
    /// 音声スレッドが書き、ゲームスレッドが読む「直近のコールバックのフレーム数」。
    /// 詳細は [`Backend::last_callback_frames`]。
    callback_frames: Arc<AtomicU32>,
    /// 音声コールバックが実際に呼ばれた回数(単調増加、`fetch_add` のみ)。
    ///
    /// `ios_interruption::Watcher` が「`pause()`→`play()` の戻り値」ではなく
    /// 「コールバックが実際に前進したか」を実測するために使う
    /// (`ios_interruption.rs` 調査記録「`pause()`→`play()` が Ok を返しても
    /// 無音のままだったケース」参照)。`callback_frames` と役割が近いが、こちらは
    /// 「呼ばれた回数そのもの」を見る専用のカウンタにしてある——`callback_frames`
    /// (直近のフレーム数)の値を比較する方式だと、同じバッファ長のコールバックが
    /// 連続した場合に「進んだかどうか」を値の変化だけでは区別できない。単調増加の
    /// カウンタなら、値が変わっていれば必ず「少なくとも1回呼ばれた」ことを意味する。
    callback_ticks: Arc<AtomicU64>,
    /// 音声スレッドが書き、ゲームスレッドが読む「直近の出力レイテンシ(ns)」。
    /// 詳細は [`Backend::output_latency_ns`]。
    output_latency_ns: Arc<AtomicU64>,
    /// [`Backend::log_output_latency_once`] が既にログを出したか。
    /// 1オープンにつき1回だけ出す(`close` でリセットする)。
    logged_output_latency: AtomicBool,
    /// オープン時にネゴシエートしたサンプルレート(未オープンなら 0)。
    sample_rate: u32,
    /// 出力コールバックのアンダーラン(の疑い)検知の累計回数。
    /// 詳細は [`crate::underrun`] モジュール doc / [`Backend::output_underrun_count`]。
    output_underrun_count: Arc<AtomicU64>,
    /// 直近にアンダーラン(の疑い)を検知したコールバックのホスト単調時刻(ns)。
    /// [`Backend::last_output_underrun_host_time_ns`] 参照。
    last_output_underrun_host_time_ns: Arc<AtomicU64>,
    /// 直近まで連続して検知した回数。[`Backend::consecutive_output_underrun_count`] 参照。
    consecutive_output_underrun_count: Arc<AtomicU32>,
    /// [`Backend::log_new_output_underruns`] が直近にログへ出した
    /// `output_underrun_count` の値(まだログしていなければ 0)。1オープンにつき
    /// 何度でも呼べる(`logged_output_latency` と異なり `AtomicBool` ではなく値そのもの
    /// を持つ——「初回だけ」ではなく「新しく増えた分だけ都度」ログしたいため)。
    logged_output_underrun_count: AtomicU64,
}

impl CpalBackend {
    pub fn new() -> Self {
        Self {
            stream: None,
            ios_interruption: None,
            callback_frames: Arc::new(AtomicU32::new(0)),
            callback_ticks: Arc::new(AtomicU64::new(0)),
            output_latency_ns: Arc::new(AtomicU64::new(0)),
            logged_output_latency: AtomicBool::new(false),
            sample_rate: 0,
            output_underrun_count: Arc::new(AtomicU64::new(0)),
            last_output_underrun_host_time_ns: Arc::new(AtomicU64::new(0)),
            consecutive_output_underrun_count: Arc::new(AtomicU32::new(0)),
            logged_output_underrun_count: AtomicU64::new(0),
        }
    }
}

impl Backend for CpalBackend {
    /// 実機デバッグで「実際にどれだけの出力レイテンシが申告されているか」を起動ログから
    /// 追えるようにするため、1オープンにつき1回だけ出す
    /// (`crates/mw-ffi/src/handle.rs::Instance::log_buffer_info_once` と同じ動機・設計)。
    ///
    /// コールバックがまだ1度も走っておらず [`Backend::output_latency_ns`] が 0(未計測)
    /// を返す間は何もせず、次に呼ばれた機会に持ち越す。
    fn log_output_latency_once(&self) {
        if self.logged_output_latency.load(Ordering::Relaxed) {
            return;
        }
        let latency_ns = self.output_latency_ns();
        if latency_ns == 0 {
            return;
        }
        self.logged_output_latency.store(true, Ordering::Relaxed);
        let latency_ms = latency_ns as f64 / 1_000_000.0;
        crate::mw_log!(
            "[mw-backend] output latency (measured, cpal playback - callback timestamp): \
             {latency_ns} ns = {latency_ms:.3} ms"
        );
    }

    /// 検知の定義は [`crate::underrun`] モジュール doc 参照。
    ///
    /// [`Backend::log_output_latency_once`] と異なり「初回だけ」ではなく、**呼ぶたびに
    /// 前回ログ時からの増分があればその都度**出す——アンダーランは起動時に1回きりの
    /// 情報ではなく、実運用中いつ何回起きたかを追いたいテレメトリのため。
    fn log_new_output_underruns(&self) {
        let current = self.output_underrun_count.load(Ordering::Relaxed);
        let last_logged = self.logged_output_underrun_count.load(Ordering::Relaxed);
        if current <= last_logged {
            return;
        }
        self.logged_output_underrun_count
            .store(current, Ordering::Relaxed);
        let new_count = current - last_logged;
        let consecutive = self
            .consecutive_output_underrun_count
            .load(Ordering::Relaxed);
        crate::mw_log!(
            "[mw-backend] output underrun suspected: +{new_count} since last check \
             (cumulative={current}, consecutive={consecutive})"
        );
    }

    fn open(
        &mut self,
        mut renderer: Renderer,
        events: Arc<EventQueue>,
    ) -> Result<(), BackendError> {
        if self.stream.is_some() {
            return Err(BackendError::AlreadyOpen);
        }

        // iOS では cpal のデバイス列挙(チャンネル数・サンプルレート)が AVAudioSession の
        // 現在の状態から作られるため、cpal に触る前に設定しておく必要がある。
        // iOS / tvOS 以外では何もしない。
        crate::ios_session::configure();

        // Android では cpal が Java 側の AudioManager を参照するため、cpal に触る前に
        // ndk_context を初期化しておく必要がある(android_context のモジュール doc 参照)。
        #[cfg(target_os = "android")]
        crate::android_context::ensure_initialized();

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(BackendError::NoOutputDevice)?;

        // 実機でどの構成が提示され、そこから何を選んだのかを追えるよう、常に残す
        // (mw_init 1回につき数行。docs/measurement-m1.md §6.1)。
        log_available_configs(&device);

        let Some(supported_config) = find_f32_stereo_config(&device) else {
            return Err(BackendError::NoSupportedStreamConfig);
        };
        let config: StreamConfig = supported_config.into();

        // 実機で「どの構成でストリームを開いたか」を残す(docs/measurement-m1.md §8.6-1
        // 「起動ログから絶対値を見積もる」の入力)。iOS は AVAudioSession 側の申告値を
        // ios_session::configure が出すが、Android には対応するログが無かった。
        // buffer_size は cpal の既定(BufferSize::Default = OS 任せ)のままで、
        // AAudio が実際に渡してきたフレーム数は初回発音時に
        // `[mw-ffi] audio callback buffer (measured)` として別途出る。
        crate::mw_log!(
            "[mw-backend] output stream config selected: sample_rate={} Hz, channels={}, buffer_size={:?}",
            config.sample_rate,
            config.channels,
            config.buffer_size,
        );

        // ランプのミリ秒→サンプル数換算(初期構築仕様 §4.1)が正しいサンプルレートを
        // 使えるよう、コールバックが動き出す(`stream.play()`)前に確定させる。
        renderer.set_sample_rate(config.sample_rate);

        // `clippy::too_many_arguments` を避けるため、コールバッククロージャへ渡す
        // アトミック群を用途ごとに2つへ束ねる(`CallbackTelemetry`/`OutputUnderrunTracker`)。
        let telemetry = CallbackTelemetry {
            frames: Arc::clone(&self.callback_frames),
            output_latency_ns: Arc::clone(&self.output_latency_ns),
            // `ios_interruption::Watcher::new` へも同じカウンタを渡すため、ここでは
            // clone を渡す(コールバッククロージャへムーブされる分)。
            ticks: Arc::clone(&self.callback_ticks),
        };
        let underrun_tracker = OutputUnderrunTracker::new(
            Arc::clone(&self.output_underrun_count),
            Arc::clone(&self.last_output_underrun_host_time_ns),
            Arc::clone(&self.consecutive_output_underrun_count),
        );

        let stream = build_output_stream(
            &device,
            &config,
            renderer,
            telemetry,
            // `events` はこの後 `ios_interruption::Watcher::new` へも渡すため、ここでは
            // clone を渡す(`err_fn` クロージャへムーブされる分)。
            Arc::clone(&events),
            underrun_tracker,
        )?;
        stream
            .play()
            .map_err(|e| BackendError::PlayStreamFailed(e.to_string()))?;

        crate::mw_log!("[mw-backend] output stream started");

        // ストリームを `Arc` で共有する理由・`ios_interruption` の役割は `CpalBackend` の
        // フィールド doc を参照。iOS / tvOS 以外では no-op(`ios_interruption.rs`)。
        let stream = Arc::new(stream);
        self.ios_interruption = Some(ios_interruption::Watcher::new(
            Arc::clone(&stream),
            events,
            Arc::clone(&self.callback_ticks),
        ));

        self.sample_rate = config.sample_rate;
        self.stream = Some(stream);
        Ok(())
    }

    fn close(&mut self) -> Result<(), BackendError> {
        match self.stream.take() {
            Some(stream) => {
                // ストリームを実際に止める前に割り込み監視を止める(観測を続けたまま
                // 止まりかけのストリームへ `pause()`/`play()` を呼び直すのを避ける)。
                self.ios_interruption = None;
                // `pause` はベストエフォート。stream の drop で確実にコールバックは止まる。
                let _ = stream.pause();
                drop(stream);
                self.sample_rate = 0;
                self.callback_frames.store(0, Ordering::Relaxed);
                self.callback_ticks.store(0, Ordering::Relaxed);
                self.output_latency_ns.store(0, Ordering::Relaxed);
                self.logged_output_latency.store(false, Ordering::Relaxed);
                self.output_underrun_count.store(0, Ordering::Relaxed);
                self.last_output_underrun_host_time_ns
                    .store(0, Ordering::Relaxed);
                self.consecutive_output_underrun_count
                    .store(0, Ordering::Relaxed);
                self.logged_output_underrun_count
                    .store(0, Ordering::Relaxed);
                Ok(())
            }
            None => Err(BackendError::NotOpen),
        }
    }

    fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    fn last_callback_frames(&self) -> u32 {
        self.callback_frames.load(Ordering::Relaxed)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn output_latency_ns(&self) -> u64 {
        self.output_latency_ns.load(Ordering::Relaxed)
    }

    fn output_underrun_count(&self) -> u64 {
        self.output_underrun_count.load(Ordering::Relaxed)
    }

    fn last_output_underrun_host_time_ns(&self) -> u64 {
        self.last_output_underrun_host_time_ns
            .load(Ordering::Relaxed)
    }

    fn consecutive_output_underrun_count(&self) -> u32 {
        self.consecutive_output_underrun_count
            .load(Ordering::Relaxed)
    }
}

/// M0 が対応する構成(f32・ステレオ)の出力設定を探す。
///
/// モノ/サラウンド出力やサンプルフォーマット変換(i16 等への対応)は将来の課題。
/// 見つからない場合は `None`(呼び出し側がエラーへ変換し、パニックはしない)。
///
/// **デバイス既定の構成を最優先する。** 以前は「列挙の先頭にある f32 ステレオ範囲の
/// 最大レート」を無条件に採っていたが、cpal 0.18 の Android(AAudio)実装は
/// 5512Hz のような低いレートから列挙してくるため、`sample_rate=5512` でストリームを
/// 開こうとして AAudio が `InvalidRate` を返し、**`mw_init` が失敗して B が一度も
/// 鳴らない**状態になっていた(2026-08-23 に実機で確認。docs/measurement-m1.md §6.2)。
/// 既定の構成はそのデバイスが実際に回している構成なので、これを先に見る。
fn find_f32_stereo_config(device: &cpal::Device) -> Option<SupportedStreamConfig> {
    let default_config = default_output_config(device);

    if let Some(config) = default_config
        && config.channels() as usize == CHANNELS
        && config.sample_format() == SampleFormat::F32
    {
        return Some(config);
    }

    // 既定が使えない/f32 ステレオでない場合の後段。優先レートを含む範囲があればそれを使い、
    // 無ければ**最大レートが最も高い**範囲を採る(低レート側の範囲を掴まないため)。
    // Android では既定構成を取得できない(下記 `default_output_config` 参照)ので、
    // 実質この経路を通る。48kHz → 44.1kHz の順に狙う。
    let preferred_rates = [
        default_config.map(|c| c.sample_rate()),
        Some(48_000),
        Some(44_100),
    ];
    let candidates = device
        .supported_output_configs()
        .ok()?
        .filter(|c| c.channels() as usize == CHANNELS && c.sample_format() == SampleFormat::F32);

    let candidates: Vec<_> = candidates.collect();

    for rate in preferred_rates.into_iter().flatten() {
        for range in &candidates {
            if let Some(config) = range.try_with_sample_rate(rate) {
                return Some(config);
            }
        }
    }

    let mut best: Option<SupportedStreamConfig> = None;
    for range in candidates {
        let config = range.with_max_sample_rate();
        if best.is_none_or(|current| config.sample_rate() > current.sample_rate()) {
            best = Some(config);
        }
    }

    best
}

/// デバイス既定の出力構成。取得できなければ `None`。
///
/// **Android では panic を畳む必要がある。** cpal 0.18 の AAudio 実装は
/// `default_output_config` から Java 側の `AudioManager` を参照するため、
/// `ndk_context`(JavaVM + Android Context)が初期化済みであることを前提にしており、
/// 未初期化のプロセスでは `android context was not initialized` で **panic** する。
/// Unity のようなホストアプリのプロセスでは誰もそれを初期化しないため、実機で
/// `mw_init` が `ErrPanic` を返して**ミドルウェアが一切動かない**状態になっていた
/// (2026-08-23。docs/measurement-m1.md §6.2)。
///
/// ここで畳んでおけば、既定構成が取れる OS(macOS / iOS)では従来どおり最優先で使い、
/// Android では列挙からの選択へ素直に落ちる。
/// `ndk_context` を初期化して Java 側の情報(ネイティブレート・frames per burst)まで
/// 使えるようにするのは別タスク(docs/measurement-m1.md §6.5)。
fn default_output_config(device: &cpal::Device) -> Option<SupportedStreamConfig> {
    let result = panic::catch_unwind(AssertUnwindSafe(|| device.default_output_config().ok()));

    match result {
        Ok(config) => config,
        Err(_) => {
            crate::mw_log!(
                "[mw-backend] default_output_config() panicked (Android では ndk_context 未初期化が原因)。\
                 列挙からの選択へフォールバックする"
            );
            None
        }
    }
}

/// デバイスが実際に提示した構成を列挙して残す(`open` から毎回呼ぶ)。実機ではこれが
/// 唯一の手がかりになる場面が2回あった(iOS の 1ch 問題と、Android の 5512Hz 問題)。
///
/// iOS では cpal が `AVAudioSession.outputNumberOfChannels()` に基づく構成しか提示せず、
/// 出力ルート次第(Bluetooth HFP 等)では 1ch しか出てこないことがある。そうなると
/// `find_f32_stereo_config` が必ず None を返し、アプリ起動から終了までミドルウェアが
/// 無音になる。実機ではこのログが唯一の手がかりになる(docs/measurement-m1.md §7.6-1)。
fn log_available_configs(device: &cpal::Device) {
    match device.supported_output_configs() {
        Ok(configs) => {
            let mut found_any = false;
            for config in configs {
                found_any = true;
                crate::mw_log!(
                    "[mw-backend] available output config: channels={} sample_format={:?} \
                     sample_rate={}..{}",
                    config.channels(),
                    config.sample_format(),
                    config.min_sample_rate(),
                    config.max_sample_rate(),
                );
            }
            if !found_any {
                crate::mw_log!("[mw-backend] the device reported no output configs at all");
            }
        }
        Err(err) => crate::mw_log!("[mw-backend] supported_output_configs() failed: {err}"),
    }
}

/// [`build_output_stream`] へ渡すコールバック計測用アトミック群
/// (`clippy::too_many_arguments` を避けるための束ね。§5.3 のリアルタイム安全性には
/// 影響しない——各フィールドは従来どおり個別の `Arc<Atomic*>` のまま)。
struct CallbackTelemetry {
    /// [`CpalBackend::callback_frames`] へ渡す `Arc`。
    frames: Arc<AtomicU32>,
    /// [`CpalBackend::output_latency_ns`] へ渡す `Arc`。
    output_latency_ns: Arc<AtomicU64>,
    /// [`CpalBackend::callback_ticks`] へ渡す `Arc`。
    ticks: Arc<AtomicU64>,
}

fn build_output_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    mut renderer: Renderer,
    telemetry: CallbackTelemetry,
    events: Arc<EventQueue>,
    // コールバック間隔異常(「アンダーラン(の疑い)」)を検知するトラッカー
    // (`crate::underrun` モジュール doc参照)。`prev_*` フィールドは音声コールバック
    // スレッドが単独で所有する非アトミック状態なので、呼び出し側(`CpalBackend::open`)
    // が構築したものをそのままこのクロージャへムーブする(`renderer` と同じ
    // 「単一の書き手」設計、§5.3)。
    mut underrun_tracker: OutputUnderrunTracker,
) -> Result<cpal::Stream, BackendError> {
    let CallbackTelemetry {
        frames: callback_frames,
        output_latency_ns,
        ticks: callback_ticks,
    } = telemetry;
    // ストリーム構成時に確定するサンプルレート。オープン中は変わらないため、
    // アトミックにせずクロージャへそのまま値でムーブする(`Copy`)。
    let sample_rate = config.sample_rate;
    let err_fn = move |err: cpal::Error| {
        // 音声スレッドではなく cpal のエラー通知経路から呼ばれる(§5.3 の対象外)。
        // ここは音声コールバックそのものとは別スレッドなので、`EventQueue` の
        // 非リアルタイム経路(`push_side_channel`、内部で `Mutex` を使う)へ積んでよい
        // (`mw_core::event` モジュール doc「なぜ2系統の書き込み経路があるか」参照)。
        crate::mw_log!("[mw-backend] output stream error: {err}");
        events.push_side_channel(Event::StreamError {
            reason: classify_stream_error(&err),
        });
    };

    device
        .build_output_stream(
            // `StreamConfig` は `Copy`。呼び出し元との共有を避けるため値で渡す。
            *config,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                // I/O バッファ長の実測用。アトミックストア1回だけで、アロケーション・
                // ロック・IO をしないためリアルタイム安全性規約(§5.3)に抵触しない。
                let frames_this_callback = (data.len() / CHANNELS) as u32;
                callback_frames.store(frames_this_callback, Ordering::Relaxed);

                // 復帰確認用のカウンタ(`CpalBackend::callback_ticks` のdoc、
                // `ios_interruption.rs` 調査記録「`pause()`→`play()` が Ok を
                // 返しても無音のままだったケース」参照)。relaxed な加算1回のみ——
                // アロケーション・ロック・IO を伴わないためリアルタイム安全性規約
                // (§5.3)に抵触しない。
                callback_ticks.fetch_add(1, Ordering::Relaxed);

                let timestamp = info.timestamp();

                // このバッファの先頭フレームが実際に DAC から出力される(と cpal が
                // 予測する)ホスト単調時刻(初期構築仕様『§4.4』の「デバイスのタイムスタンプ
                // API」に相当)。`StreamInstant::as_nanos()` は `crate::host_time::host_time_ns`
                // と同じ時計・同じ式で導出されている(`host_time.rs` のモジュール doc に
                // 調査結果を記載済み)ため、直接比較可能な ns 値としてそのまま渡せる。
                // `u128 → u64` の切り捨ては現実的な稼働時間では発生しない(u64 ns は
                // 約584年ぶん表現できる)。
                let buffer_start_host_time_ns = timestamp.playback.as_nanos() as u64;

                // 出力レイテンシの実測用(`Backend::output_latency_ns` のdoc参照)。
                // `playback`(このバッファがスピーカーへ出る予測時刻)と `callback`
                // (このコールバックが呼ばれた時刻)は同一コールバック呼び出し内の
                // 値なので同じ時計を共有しており、引き算に意味がある(cpal
                // `StreamInstant` のdoc参照)。時刻が逆転してもパニックしないよう
                // `saturating_sub` を使う——実機ではまず起こらないはずだが、ホスト側の
                // タイムスタンプ実装のバグ・精度不足で `callback > playback` になった
                // 場合に音声スレッドを絶対にパニックさせないための保険。
                let callback_host_time_ns = timestamp.callback.as_nanos() as u64;
                output_latency_ns.store(
                    buffer_start_host_time_ns.saturating_sub(callback_host_time_ns),
                    Ordering::Relaxed,
                );

                // アンダーラン(の疑い)検知用。整数演算+アトミック操作のみ
                // (ヒープ確保・ロック・IO なし。§5.3。`crate::underrun` モジュール doc参照)。
                underrun_tracker.observe(callback_host_time_ns, frames_this_callback, sample_rate);

                // ここが音声スレッド上のオーディオコールバック本体。`renderer` はこの
                // クロージャへムーブ済みで、以後は音声スレッドの単一の書き手が
                // `&mut` で触るだけ(ロックも `Arc` 共有も無い。§5.3)。呼ぶのは
                // `Renderer::render` のみに保つ(`crates/mw-backend/CLAUDE.md` の設計意図。
                // 上の数行は cpal が既に計算済みの構造体を読んでアトミックストアするだけで、
                // mw-core の別関数を追加で呼んではいない)。
                // `Renderer::render` はリアルタイム安全性規約(§5.3)を満たす実装である前提。
                renderer.render(data, buffer_start_host_time_ns);
            },
            err_fn,
            None,
        )
        .map_err(|e| BackendError::BuildStreamFailed(e.to_string()))
}

/// `cpal::Error` を `mw_core::StreamErrorReason`(初期構築仕様『§4.6』)へ分類する。
///
/// cpal 側の `ErrorKind` は `#[non_exhaustive]` かつ詳細度が高い(14種)。C# 側が
/// 実用的に分岐できる粒度(「デバイスに到達できない」「構成が壊れて再構築が要る」
/// 「権限が無い」「その他」)へ意図的に丸める——詳細な原因は上の `mw_log!` で
/// 開発ビルドのログに残るため、イベント側の payload まで cpal 固有の分類を
/// 持ち込む必要は無いと判断した。
fn classify_stream_error(err: &cpal::Error) -> StreamErrorReason {
    use cpal::ErrorKind;
    match err.kind() {
        ErrorKind::DeviceNotAvailable | ErrorKind::HostUnavailable | ErrorKind::DeviceBusy => {
            StreamErrorReason::DeviceUnavailable
        }
        ErrorKind::DeviceChanged | ErrorKind::StreamInvalidated => StreamErrorReason::Reconfigured,
        ErrorKind::PermissionDenied => StreamErrorReason::PermissionDenied,
        _ => StreamErrorReason::Backend,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Android(AAudio)切断の通知経路がまだ生きていることを固定化する回帰テスト。**
    ///
    /// M3「Android の AAudio 切断復旧」調査(2026-08-31、`docs/history/03-2026-08-31.md`
    /// 参照)で、cpal 0.18.2 の AAudio ホスト実装(`~/.cargo/.../cpal-0.18.2/src/host/
    /// aaudio/mod.rs::build_output_stream` の `error_callback`)と ndk 0.9.0
    /// (`~/.cargo/.../ndk-0.9.0/src/audio.rs` の `AudioStreamBuilder::error_callback` doc)
    /// をソースで確認した結果、AAudio が `AAUDIO_ERROR_DISCONNECTED` を出すと
    /// cpal は `ndk::audio::AudioError::Disconnected` → `cpal::ErrorKind::
    /// DeviceNotAvailable`(`cpal-0.18.2/src/host/aaudio/convert.rs::impl From<AudioError>
    /// for Error`)へ変換したうえで、このクレートが `build_output_stream` に渡した
    /// `err_fn` を呼ぶ(iOS のルート変化と同じ経路、M2-6 から存在)。
    ///
    /// つまり **Android の切断通知そのものは既存のコードパスで既に届いている**——
    /// この `classify_stream_error(ErrorKind::DeviceNotAvailable)` が
    /// `StreamErrorReason::DeviceUnavailable` を返し続ける限り、C# 側は
    /// `mw_poll_events` 経由で「デバイスが無くなった」ことを検知できる。
    ///
    /// このテストが守っているのはあくまで**分類ロジック**(cpal のホスト実装や
    /// ndk クレートの実機呼び出しそのものは自動テスト不可)。実機での「AAudio が
    /// 実際に `AAUDIO_ERROR_DISCONNECTED` を出すか」「その後ストリームを再構築すれば
    /// 音が戻るか」(iOS の `pause()`→`play()` とは異なり、AAudio の切断は終端的で
    /// ストリームの作り直しが要る。`ndk-0.9.0/src/audio.rs` の `AudioError::Disconnected`
    /// doc「The stream cannot be used after the device is disconnected. Applications
    /// should stop and close the stream.」参照)は実機検証でしか確認できない。
    #[test]
    fn device_not_available_classifies_as_device_unavailable_the_aaudio_disconnect_path() {
        let err = cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable);
        assert_eq!(
            classify_stream_error(&err),
            StreamErrorReason::DeviceUnavailable
        );
    }

    /// host 不在(PulseAudio/PipeWire/JACK 等が無い)も同じ `DeviceUnavailable` へ丸める
    /// ——C# 側の分岐粒度は「デバイスに到達できない」でひとまとめにする設計(関数 doc参照)。
    #[test]
    fn host_unavailable_also_classifies_as_device_unavailable() {
        let err = cpal::Error::new(cpal::ErrorKind::HostUnavailable);
        assert_eq!(
            classify_stream_error(&err),
            StreamErrorReason::DeviceUnavailable
        );
    }

    /// 一時的にデバイスが使用中(他アプリが握っている等)も同じグループ。
    #[test]
    fn device_busy_also_classifies_as_device_unavailable() {
        let err = cpal::Error::new(cpal::ErrorKind::DeviceBusy);
        assert_eq!(
            classify_stream_error(&err),
            StreamErrorReason::DeviceUnavailable
        );
    }

    /// iOS のルート変化(`AVAudioSessionRouteChangeNotification`)がここへ来ることは
    /// 実機で確認済み(`ios_interruption.rs` 調査記録「ルート変化」参照)。
    /// `Reconfigured` は「再構築すれば直る」という意味なので、iOS はここには来ず
    /// `ios_interruption::Watcher` が別経路で `pause()`→`play()` を試みる
    /// ——`err_fn` 経由のこの分類はあくまでイベント通知用で、iOS の復帰処理自体は
    /// `AVAudioSessionRouteChangeNotification` の直接監視によって行われる(cpal の
    /// `err_fn` はルート変化時に `AudioUnit`/`playing` フラグへ一切触れないため、
    /// 二重処理にはならない。`ios_interruption.rs` 参照)。
    #[test]
    fn stream_invalidated_and_device_changed_classify_as_reconfigured() {
        for kind in [
            cpal::ErrorKind::StreamInvalidated,
            cpal::ErrorKind::DeviceChanged,
        ] {
            let err = cpal::Error::new(kind);
            assert_eq!(classify_stream_error(&err), StreamErrorReason::Reconfigured);
        }
    }

    #[test]
    fn permission_denied_classifies_as_permission_denied() {
        let err = cpal::Error::new(cpal::ErrorKind::PermissionDenied);
        assert_eq!(
            classify_stream_error(&err),
            StreamErrorReason::PermissionDenied
        );
    }

    /// 上記4分類のどれにも当てはまらない残り(`InvalidInput`/`RealtimeDenied`/
    /// `ResourceExhausted`/`UnsupportedConfig` 等)は `Backend` へ丸める
    /// (関数 doc「C# 側が実用的に分岐できる粒度」参照)。代表として1つだけ固定化する
    /// ——将来 cpal が `ErrorKind` へバリアントを追加しても(`#[non_exhaustive]`)、
    /// `_ =>` 分岐がある限りこのテストは影響を受けない。
    #[test]
    fn unmatched_kinds_fall_back_to_backend() {
        let err = cpal::Error::new(cpal::ErrorKind::ResourceExhausted);
        assert_eq!(classify_stream_error(&err), StreamErrorReason::Backend);
    }
}
