//! cpal を使った `Backend` 実装。
//!
//! 初期構築仕様 M6(【仮】): 立ち上げは cpal で macOS Editor / iOS / Android を1系統で扱う。
//! 計測後、必要なら Android を oboe 直叩き、iOS を RemoteIO 直叩きに置換する可能性がある
//! (その際もこの `Backend` trait 経由で差し替えられるようにしてある)。

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig, SupportedStreamConfig};
use mw_core::{CHANNELS, Renderer};

use crate::backend::{Backend, BackendError};

/// 既定の出力デバイスに f32 ステレオストリームを開く `Backend` 実装。
///
/// デバイスが無い環境(CI・ヘッドレスマシン等)では `open` が
/// `BackendError::NoOutputDevice` を返す。パニックはしない。
#[derive(Default)]
pub struct CpalBackend {
    stream: Option<cpal::Stream>,
    /// 音声スレッドが書き、ゲームスレッドが読む「直近のコールバックのフレーム数」。
    /// 詳細は [`Backend::last_callback_frames`]。
    callback_frames: Arc<AtomicU32>,
    /// オープン時にネゴシエートしたサンプルレート(未オープンなら 0)。
    sample_rate: u32,
}

impl CpalBackend {
    pub fn new() -> Self {
        Self {
            stream: None,
            callback_frames: Arc::new(AtomicU32::new(0)),
            sample_rate: 0,
        }
    }
}

impl Backend for CpalBackend {
    fn open(&mut self, mut renderer: Renderer) -> Result<(), BackendError> {
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

        let stream = build_output_stream(
            &device,
            &config,
            renderer,
            Arc::clone(&self.callback_frames),
        )?;
        stream
            .play()
            .map_err(|e| BackendError::PlayStreamFailed(e.to_string()))?;

        crate::mw_log!("[mw-backend] output stream started");

        self.sample_rate = config.sample_rate;
        self.stream = Some(stream);
        Ok(())
    }

    fn close(&mut self) -> Result<(), BackendError> {
        match self.stream.take() {
            Some(stream) => {
                // `pause` はベストエフォート。stream の drop で確実にコールバックは止まる。
                let _ = stream.pause();
                drop(stream);
                self.sample_rate = 0;
                self.callback_frames.store(0, Ordering::Relaxed);
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

fn build_output_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    mut renderer: Renderer,
    callback_frames: Arc<AtomicU32>,
) -> Result<cpal::Stream, BackendError> {
    let err_fn = |err: cpal::Error| {
        // 音声スレッドではなく cpal のエラー通知経路から呼ばれる(§5.3 の対象外)。
        // M0 では標準エラーへ出力するのみ。開発/リリースの出し分けと
        // `StreamError` イベント化(§4.8, §4.6)は mw-ffi 側の実装(M0 以降)で行う。
        crate::mw_log!("[mw-backend] output stream error: {err}");
    };

    device
        .build_output_stream(
            // `StreamConfig` は `Copy`。呼び出し元との共有を避けるため値で渡す。
            *config,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                // I/O バッファ長の実測用。アトミックストア1回だけで、アロケーション・
                // ロック・IO をしないためリアルタイム安全性規約(§5.3)に抵触しない。
                callback_frames.store((data.len() / CHANNELS) as u32, Ordering::Relaxed);

                // このバッファの先頭フレームが実際に DAC から出力される(と cpal が
                // 予測する)ホスト単調時刻(初期構築仕様『§4.4』の「デバイスのタイムスタンプ
                // API」に相当)。`StreamInstant::as_nanos()` は `crate::host_time::host_time_ns`
                // と同じ時計・同じ式で導出されている(`host_time.rs` のモジュール doc に
                // 調査結果を記載済み)ため、直接比較可能な ns 値としてそのまま渡せる。
                // `u128 → u64` の切り捨ては現実的な稼働時間では発生しない(u64 ns は
                // 約584年ぶん表現できる)。
                let buffer_start_host_time_ns = info.timestamp().playback.as_nanos() as u64;

                // ここが音声スレッド上のオーディオコールバック本体。`renderer` はこの
                // クロージャへムーブ済みで、以後は音声スレッドの単一の書き手が
                // `&mut` で触るだけ(ロックも `Arc` 共有も無い。§5.3)。呼ぶのは
                // `Renderer::render` のみに保つ(`crates/mw-backend/CLAUDE.md` の設計意図。
                // 上の2行は cpal が既に計算済みの構造体を読むだけで、mw-core の別関数を
                // 追加で呼んではいない)。
                // `Renderer::render` はリアルタイム安全性規約(§5.3)を満たす実装である前提。
                renderer.render(data, buffer_start_host_time_ns);
            },
            err_fn,
            None,
        )
        .map_err(|e| BackendError::BuildStreamFailed(e.to_string()))
}
