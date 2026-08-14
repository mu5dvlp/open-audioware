//! FFI 境界のエラーモデル(初期構築仕様 §4.8, 確定)。
//!
//! 全 FFI 関数はエラーコード(負の整数)返しとする。この列挙が唯一の正であり、
//! csbindgen が C# 側 enum を自動生成する(手書きの宣言ズレを構造的に排除する、M2)。
//! `#[repr(i32)]` の enum は csbindgen が C# `enum ... : int` として自動生成する。

/// FFI 関数の戻り値。`Ok`(0)以外は全て負の整数のエラーコード。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MwResult {
    /// 成功。
    Ok = 0,
    /// 出力引数として渡されたポインタが null だった。
    ErrNullPointer = -1,
    /// ハンドルが無効(未初期化 / 既に shutdown 済み / 他インスタンスのハンドル)。
    ErrInvalidHandle = -2,
    /// 出力バックエンド(cpal ストリーム)のオープンに失敗した。
    ErrBackendOpenFailed = -3,
    /// 出力バックエンドのクローズに失敗した。
    ErrBackendCloseFailed = -4,
    /// FFI 境界内で Rust panic を捕捉した(catch_unwind)。呼び出し元に漏らさない(§5.4)。
    ErrPanic = -5,
    /// `mw_sound_load` の `mode` が M1 未実装(`Music`)、または未知の値だった。
    /// 楽曲モードは M2 で実装する(初期構築仕様 §5.2)。
    ErrUnsupportedSoundMode = -6,
    /// wav のパースに失敗した(RIFF/WAVE 構造が壊れている、`fmt `/`data` チャンクが無い等)。
    ErrDecodeFailed = -7,
    /// wav が 48kHz 以外だった。リサンプルは M2(rubato)で対応予定(初期構築仕様 §4.7)。
    ErrUnsupportedSampleRate = -8,
    /// wav が 16bit PCM でない、またはチャンネル数がモノ/ステレオでない。
    ErrUnsupportedFormat = -9,
    /// 指定されたサウンド ID が存在しない(未ロード / 既に解放済み)。
    ErrInvalidSoundId = -10,
    /// コマンドキューが満杯で発行できなかった。初期構築仕様 §4.2 の
    /// 「次のコールバックで必ず発音される」という保証は、コマンドが実際にキューへ
    /// 積まれたことが前提のため、黙って捨てずにこのエラーを返す。
    ErrCommandQueueFull = -11,
    /// `bus` 引数が固定4本(Master/BGM/SE/Voice)のいずれにも対応しない値だった。
    ErrInvalidBus = -12,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_is_zero_and_errors_are_negative() {
        assert_eq!(MwResult::Ok as i32, 0);
        assert!((MwResult::ErrNullPointer as i32) < 0);
        assert!((MwResult::ErrInvalidHandle as i32) < 0);
        assert!((MwResult::ErrBackendOpenFailed as i32) < 0);
        assert!((MwResult::ErrBackendCloseFailed as i32) < 0);
        assert!((MwResult::ErrPanic as i32) < 0);
        assert!((MwResult::ErrUnsupportedSoundMode as i32) < 0);
        assert!((MwResult::ErrDecodeFailed as i32) < 0);
        assert!((MwResult::ErrUnsupportedSampleRate as i32) < 0);
        assert!((MwResult::ErrUnsupportedFormat as i32) < 0);
        assert!((MwResult::ErrInvalidSoundId as i32) < 0);
        assert!((MwResult::ErrCommandQueueFull as i32) < 0);
        assert!((MwResult::ErrInvalidBus as i32) < 0);
    }
}
