//! cpal を使った `Backend` 実装。
//!
//! 初期構築仕様 M6(【仮】): 立ち上げは cpal で macOS Editor / iOS / Android を1系統で扱う。
//! 計測後、必要なら Android を oboe 直叩き、iOS を RemoteIO 直叩きに置換する可能性がある
//! (その際もこの `Backend` trait 経由で差し替えられるようにしてある)。

use std::sync::Arc;

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
    fn open(&mut self, renderer: Arc<Renderer>) -> Result<(), BackendError> {
        if self.stream.is_some() {
            return Err(BackendError::AlreadyOpen);
        }

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(BackendError::NoOutputDevice)?;

        let supported_config =
            find_f32_stereo_config(&device).ok_or(BackendError::NoSupportedStreamConfig)?;
        let config: StreamConfig = supported_config.into();

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

fn build_output_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    renderer: Arc<Renderer>,
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
                // ここが音声スレッド上のオーディオコールバック本体。
                // `Renderer::render` はリアルタイム安全性規約(§5.3)を満たす実装である前提。
                renderer.render(data);
            },
            err_fn,
            None,
        )
        .map_err(|e| BackendError::BuildStreamFailed(e.to_string()))
}
