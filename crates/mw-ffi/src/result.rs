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
    /// wav のサンプルレートが不正だった(0Hz 等、壊れたファイル)。サンプルレートの不一致
    /// 自体は M2(rubato)でロード時リサンプルするため、もはやここには当たらない
    /// (初期構築仕様 §4.7)。
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
    /// `mw_music_set_loop` の区間が不正だった(`begin >= end` で、かつループ解除
    /// (`begin == 0 && end == 0`)でもない)。`mw_core::MusicVoice::set_loop` は同じ
    /// 状況を黙ってループ無しとして扱う(音声スレッドはパニックできないため、§5.3)が、
    /// FFI 境界(ゲームスレッド経路)では黙って捨てず明示的に拒否する
    /// (`crates/mw-ffi/src/ffi.rs::mw_music_set_loop` 参照)。
    ErrInvalidLoopRegion = -13,
}

// --- P1-6: `mw_core` のデコードエラー型からの変換 ----------------------------------
//
// `mw_core::wav::decode`(SE ロード, `load_se`)と `mw_core::SymphoniaDecoder::open`
// (楽曲/BGM ロード, `mw_music_set`/`mw_bgm_set` の共有実体 `set_music_track`)は
// エラーの種類こそ違う型(`WavError`/`DecodeError`)だが、どちらも「同じ理由付けで
// 数個の `MwResult` エラーコードへ落とし込むだけ」の match だった(呼び出し側3箇所で
// 実質同一の match 式が重複していた)。ここへ集約する——ログ出力(呼び出し元ごとに
// 文言が違う)は呼び出し側に残したまま、コード変換だけをここへ寄せる設計
// (`err.into()` の形で使う)。
impl From<mw_core::WavError> for MwResult {
    fn from(err: mw_core::WavError) -> Self {
        match err {
            mw_core::WavError::InvalidSampleRate(_) => MwResult::ErrUnsupportedSampleRate,
            mw_core::WavError::Resample(_) => MwResult::ErrDecodeFailed,
            mw_core::WavError::UnsupportedFormatTag(_)
            | mw_core::WavError::UnsupportedBitsPerSample(_)
            | mw_core::WavError::UnsupportedChannelCount(_) => MwResult::ErrUnsupportedFormat,
            mw_core::WavError::Truncated
            | mw_core::WavError::NotRiff
            | mw_core::WavError::NotWave
            | mw_core::WavError::MissingFmtChunk
            | mw_core::WavError::MissingDataChunk => MwResult::ErrDecodeFailed,
        }
    }
}

impl From<mw_core::DecodeError> for MwResult {
    fn from(err: mw_core::DecodeError) -> Self {
        match err {
            mw_core::DecodeError::InvalidSampleRate(_) => MwResult::ErrUnsupportedSampleRate,
            mw_core::DecodeError::UnsupportedChannelCount(_) => MwResult::ErrUnsupportedFormat,
            mw_core::DecodeError::Symphonia(_)
            | mw_core::DecodeError::NoAudioTrack
            | mw_core::DecodeError::Resample(_)
            | mw_core::DecodeError::ResetRequired => MwResult::ErrDecodeFailed,
        }
    }
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
        assert!((MwResult::ErrInvalidLoopRegion as i32) < 0);
    }

    // --- P1-6 の変換(WavError / DecodeError → MwResult)---------------------------
    //
    // 🔴 <b>この2つの From は「C# 側が見るエラーコード」を決めている唯一の場所。</b>
    // 腕を1つ取り違えると、たとえば「壊れたファイル」が
    // <c>ErrUnsupportedSampleRate</c> として届き、client 側の分岐が別の道へ行く
    // (しかも<b>どちらも「失敗」なので気付きにくい</b>)。
    // ⚠️ デバイスが要らない純粋関数なので CI でも必ず通る —— 2026-09-23 の
    // カバレッジ確認で「全14腕にテストが1本も無い」と分かったため足した。

    #[test]
    fn wav_error_maps_to_the_documented_result_code() {
        let cases: [(mw_core::WavError, MwResult); 10] = [
            (
                mw_core::WavError::InvalidSampleRate(0),
                MwResult::ErrUnsupportedSampleRate,
            ),
            (
                mw_core::WavError::Resample(mw_core::ResampleError::Construction("x".into())),
                MwResult::ErrDecodeFailed,
            ),
            (
                mw_core::WavError::UnsupportedFormatTag(3),
                MwResult::ErrUnsupportedFormat,
            ),
            (
                mw_core::WavError::UnsupportedBitsPerSample(24),
                MwResult::ErrUnsupportedFormat,
            ),
            (
                mw_core::WavError::UnsupportedChannelCount(6),
                MwResult::ErrUnsupportedFormat,
            ),
            (mw_core::WavError::Truncated, MwResult::ErrDecodeFailed),
            (mw_core::WavError::NotRiff, MwResult::ErrDecodeFailed),
            (mw_core::WavError::NotWave, MwResult::ErrDecodeFailed),
            (
                mw_core::WavError::MissingFmtChunk,
                MwResult::ErrDecodeFailed,
            ),
            (
                mw_core::WavError::MissingDataChunk,
                MwResult::ErrDecodeFailed,
            ),
        ];

        for (err, expected) in cases {
            // ⚠️ `into()` が err を消費するので、ラベルは先に作っておく
            // (WavError / DecodeError は Clone を実装していない)。
            let label = format!("{err:?}");
            let actual: MwResult = err.into();
            assert_eq!(
                actual, expected,
                "WavError::{label} の変換先が変わっています"
            );
        }
    }

    #[test]
    fn decode_error_maps_to_the_documented_result_code() {
        let cases: [(mw_core::DecodeError, MwResult); 6] = [
            (
                mw_core::DecodeError::InvalidSampleRate(0),
                MwResult::ErrUnsupportedSampleRate,
            ),
            (
                mw_core::DecodeError::UnsupportedChannelCount(6),
                MwResult::ErrUnsupportedFormat,
            ),
            (
                mw_core::DecodeError::Symphonia("x".into()),
                MwResult::ErrDecodeFailed,
            ),
            (
                mw_core::DecodeError::NoAudioTrack,
                MwResult::ErrDecodeFailed,
            ),
            (
                mw_core::DecodeError::Resample(mw_core::ResampleError::Processing("x".into())),
                MwResult::ErrDecodeFailed,
            ),
            (
                mw_core::DecodeError::ResetRequired,
                MwResult::ErrDecodeFailed,
            ),
        ];

        for (err, expected) in cases {
            let label = format!("{err:?}");
            let actual: MwResult = err.into();
            assert_eq!(
                actual, expected,
                "DecodeError::{label} の変換先が変わっています"
            );
        }
    }

    /// ⚠️ <b>「デコード失敗」と「フォーマット非対応」を取り違えていないこと。</b>
    /// 両方とも失敗なので、取り違えても<b>テストが無ければ誰も気付かない</b>。
    #[test]
    fn unsupported_format_and_decode_failed_are_not_interchangeable() {
        let format: MwResult = mw_core::WavError::UnsupportedBitsPerSample(24).into();
        let decode: MwResult = mw_core::WavError::NotRiff.into();

        assert_ne!(format, decode);
        assert_eq!(format, MwResult::ErrUnsupportedFormat);
        assert_eq!(decode, MwResult::ErrDecodeFailed);
    }
}
