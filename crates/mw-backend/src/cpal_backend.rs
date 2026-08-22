//! cpal を使った `Backend` 実装。
//!
//! 初期構築仕様 M6(【仮】): 立ち上げは cpal で macOS Editor / iOS / Android を1系統で扱う。
//! 計測後、必要なら Android を oboe 直叩き、iOS を RemoteIO 直叩きに置換する可能性がある
//! (その際もこの `Backend` trait 経由で差し替えられるようにしてある)。

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
}

impl CpalBackend {
    pub fn new() -> Self {
        Self { stream: None }
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

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(BackendError::NoOutputDevice)?;

        let supported_config = match find_f32_stereo_config(&device) {
            Some(config) => config,
            None => {
                log_available_configs(&device);
                return Err(BackendError::NoSupportedStreamConfig);
            }
        };
        let config: StreamConfig = supported_config.into();

        // ランプのミリ秒→サンプル数換算(初期構築仕様 §4.1)が正しいサンプルレートを
        // 使えるよう、コールバックが動き出す(`stream.play()`)前に確定させる。
        renderer.set_sample_rate(config.sample_rate);

        let stream = build_output_stream(&device, &config, renderer)?;
        stream
            .play()
            .map_err(|e| BackendError::PlayStreamFailed(e.to_string()))?;

        self.stream = Some(stream);
        Ok(())
    }

    fn close(&mut self) -> Result<(), BackendError> {
        match self.stream.take() {
            Some(stream) => {
                // `pause` はベストエフォート。stream の drop で確実にコールバックは止まる。
                let _ = stream.pause();
                drop(stream);
                Ok(())
            }
            None => Err(BackendError::NotOpen),
        }
    }

    fn is_open(&self) -> bool {
        self.stream.is_some()
    }
}

/// M0 が対応する構成(f32・ステレオ)の出力設定を探す。
///
/// モノ/サラウンド出力やサンプルフォーマット変換(i16 等への対応)は将来の課題。
/// 見つからない場合は `None`(呼び出し側がエラーへ変換し、パニックはしない)。
fn find_f32_stereo_config(device: &cpal::Device) -> Option<SupportedStreamConfig> {
    let configs = device.supported_output_configs().ok()?;
    configs
        .filter(|c| c.channels() as usize == CHANNELS && c.sample_format() == SampleFormat::F32)
        .map(|c| c.with_max_sample_rate())
        .next()
}

/// f32 ステレオ構成が見つからなかったときに、デバイスが実際に提示した構成を列挙して残す。
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
                eprintln!(
                    "[mw-backend] available output config: channels={} sample_format={:?} \
                     sample_rate={}..{}",
                    config.channels(),
                    config.sample_format(),
                    config.min_sample_rate(),
                    config.max_sample_rate(),
                );
            }
            if !found_any {
                eprintln!("[mw-backend] the device reported no output configs at all");
            }
        }
        Err(err) => eprintln!("[mw-backend] supported_output_configs() failed: {err}"),
    }
}

fn build_output_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    mut renderer: Renderer,
) -> Result<cpal::Stream, BackendError> {
    let err_fn = |err: cpal::Error| {
        // 音声スレッドではなく cpal のエラー通知経路から呼ばれる(§5.3 の対象外)。
        // M0 では標準エラーへ出力するのみ。開発/リリースの出し分けと
        // `StreamError` イベント化(§4.8, §4.6)は mw-ffi 側の実装(M0 以降)で行う。
        eprintln!("[mw-backend] output stream error: {err}");
    };

    device
        .build_output_stream(
            // `StreamConfig` は `Copy`。呼び出し元との共有を避けるため値で渡す。
            *config,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                // ここが音声スレッド上のオーディオコールバック本体。`renderer` はこの
                // クロージャへムーブ済みで、以後は音声スレッドの単一の書き手が
                // `&mut` で触るだけ(ロックも `Arc` 共有も無い。§5.3)。
                // `Renderer::render` はリアルタイム安全性規約(§5.3)を満たす実装である前提。
                renderer.render(data);
            },
            err_fn,
            None,
        )
        .map_err(|e| BackendError::BuildStreamFailed(e.to_string()))
}
