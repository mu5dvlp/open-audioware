//! C ABI 境界の関数本体。
//!
//! 初期構築仕様 §5.4(確定):
//! - 全関数スレッドセーフ・非ブロッキング・エラーコード返し(§4.8)
//! - シンボルは `mw_` プレフィックスで統一
//! - panic 境界(catch_unwind)で FFI から Rust panic を漏らさない
//!
//! このファイルは `build.rs` から csbindgen の入力として読まれ、
//! `unity/Runtime/Generated/NativeMethods.g.cs` を自動生成する。
//! 関数シグネチャを変える際はコメントだけでなく実際の型を変更すること
//! (手書きの宣言ズレを構造的に排除するのが csbindgen 採用の目的、M2)。

use std::panic::{self, AssertUnwindSafe};

use crate::handle as handle_registry;
use crate::handle::{InitOutcome, ShutdownOutcome};
use crate::result::MwResult;

/// ABI バージョン。ABI 互換の破壊は semver メジャーバージョンでのみ許可する(§4.8)。
const ABI_VERSION: u32 = 1;

/// ABI バージョンを返す。C# 側は起動時にこれを期待値と照合すること。
#[unsafe(no_mangle)]
pub extern "C" fn mw_abi_version() -> u32 {
    ABI_VERSION
}

/// ミドルウェアを初期化し、既定の出力デバイスにストリームを開いて再生を開始する。
///
/// 冪等: 既に初期化済みの場合は同一ハンドルを `out_handle` に書き `MwResult::Ok` を返す
/// (Unity Editor のドメインリロード対策、§6)。
///
/// # Safety
/// `out_handle` は書き込み可能な `u64` を指す有効なポインタであるか、null でなければならない。
/// null の場合は書き込みを行わず `MwResult::ErrNullPointer` を返す。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mw_init(out_handle: *mut u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if out_handle.is_null() {
            return None;
        }
        Some(handle_registry::init())
    }));

    match outcome {
        Ok(None) => MwResult::ErrNullPointer,
        Ok(Some(InitOutcome::Opened(h))) | Ok(Some(InitOutcome::AlreadyOpen(h))) => {
            // SAFETY: 上の分岐で null チェック済み。呼び出し規約上、書き込み可能な
            // `u64` を指すポインタであることは呼び出し側の責務(FFI 境界の契約)。
            unsafe {
                *out_handle = h;
            }
            MwResult::Ok
        }
        Ok(Some(InitOutcome::Failed)) => MwResult::ErrBackendOpenFailed,
        Err(_) => MwResult::ErrPanic,
    }
}

/// ミドルウェアを終了し、出力ストリームを停止する。
///
/// 無効なハンドル(未初期化・二重 shutdown・他インスタンスのハンドル)は
/// `MwResult::ErrInvalidHandle` を返す。クラッシュはしない(§4.8)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_shutdown(handle: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| handle_registry::shutdown(handle)));

    match outcome {
        Ok(ShutdownOutcome::Closed) => MwResult::Ok,
        Ok(ShutdownOutcome::CloseFailed) => MwResult::ErrBackendCloseFailed,
        Ok(ShutdownOutcome::InvalidHandle) => MwResult::ErrInvalidHandle,
        Err(_) => MwResult::ErrPanic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_version_is_one() {
        assert_eq!(mw_abi_version(), 1);
    }

    #[test]
    fn init_rejects_null_out_pointer() {
        let result = unsafe { mw_init(std::ptr::null_mut()) };
        assert_eq!(result, MwResult::ErrNullPointer);
    }

    #[test]
    fn shutdown_with_bogus_handle_is_invalid_handle_not_a_crash() {
        let result = mw_shutdown(0xDEAD_BEEF_u64);
        assert_eq!(result, MwResult::ErrInvalidHandle);
    }

    /// M0 の終了条件そのもの: init → shutdown が成功コードを返すこと。
    /// CI やヘッドレス環境では出力デバイスが無いことがあるため、その場合は
    /// クラッシュせずエラーコードを返せていることまでを確認する(§4.8)。
    #[test]
    fn init_then_shutdown_roundtrip_or_gracefully_reports_no_device() {
        let mut handle_out: u64 = 0;
        let init_result = unsafe { mw_init(&mut handle_out as *mut u64) };
        match init_result {
            MwResult::Ok => {
                assert_ne!(handle_out, 0);
                assert_eq!(mw_shutdown(handle_out), MwResult::Ok);
            }
            MwResult::ErrBackendOpenFailed => {
                // デバイス無し環境(CI 等)。落ちずにエラーコードを返せていることが重要。
            }
            other => panic!("unexpected mw_init result: {other:?}"),
        }
    }
}
