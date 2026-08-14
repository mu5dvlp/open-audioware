//! 出力デバイス抽象(初期構築仕様 §5.1: 「将来の oboe / RemoteIO 直叩き実装もここに並べる」)。

use std::fmt;
use std::sync::Arc;

use mw_core::Renderer;

/// `Backend` の操作で発生しうるエラー。
///
/// FFI 境界(mw-ffi)ではこれを `MwResult` の負のエラーコードへ writeする。
/// バックエンド内部でパニックさせず、必ずこの型で失敗を報告する(§4.8 の思想を
/// mw-backend 内でも先取りする)。
#[derive(Debug)]
pub enum BackendError {
    /// 既定の出力デバイスが見つからない(デバイス無し環境。CI・ヘッドレス環境等)。
    NoOutputDevice,
    /// M0 が対応する f32 ステレオの出力構成が見つからない。
    NoSupportedStreamConfig,
    /// cpal のストリーム構築に失敗した。
    BuildStreamFailed(String),
    /// cpal のストリーム開始(`play`)に失敗した。
    PlayStreamFailed(String),
    /// 既に開いているバックエンドへ再度 `open` した。
    AlreadyOpen,
    /// 開いていないバックエンドを `close` した。
    NotOpen,
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::NoOutputDevice => write!(f, "no default output device"),
            BackendError::NoSupportedStreamConfig => {
                write!(f, "no supported f32 stereo output stream config")
            }
            BackendError::BuildStreamFailed(msg) => write!(f, "failed to build stream: {msg}"),
            BackendError::PlayStreamFailed(msg) => write!(f, "failed to start stream: {msg}"),
            BackendError::AlreadyOpen => write!(f, "backend is already open"),
            BackendError::NotOpen => write!(f, "backend is not open"),
        }
    }
}

impl std::error::Error for BackendError {}

/// 出力デバイスを開閉し、内部でオーディオコールバックから `mw-core` のレンダラを駆動する抽象。
///
/// 実装はスレッドセーフであること(§5.4)。`open`/`close` はゲームスレッドから
/// 呼ばれる想定だが、コールバック自体は OS が生成する専用の音声スレッドで実行される。
pub trait Backend {
    /// 出力ストリームを開き、再生を開始する。
    ///
    /// 既に開いている場合は `Err(BackendError::AlreadyOpen)` を返す(パニックしない)。
    /// 冪等性(二重 init の扱い)は呼び出し元の mw-ffi が担う(§4.8)。
    fn open(&mut self, renderer: Arc<Renderer>) -> Result<(), BackendError>;

    /// ストリームを停止して閉じる。
    ///
    /// 開いていない場合は `Err(BackendError::NotOpen)` を返す(パニックしない)。
    fn close(&mut self) -> Result<(), BackendError>;

    /// 現在ストリームが開いているか。
    fn is_open(&self) -> bool;
}
