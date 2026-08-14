//! init/shutdown ハンドルの管理。
//!
//! 初期構築仕様 §4.8(確定): ハンドルは不透明な整数 ID(ポインタを C# に渡さない)。
//! 二重解放・使用後解放は無効 ID エラーとして検出する(クラッシュさせない)。
//! §6: Unity Editor のドメインリロードに備え、init/shutdown は冪等にする
//! (dylib は常駐前提。ドメインリロードのたびに新規プロセスが立つわけではない)。
//!
//! M0 時点ではインスタンスは同時に1つのみ(グローバルレジストリ)。
//! 複数インスタンスを許すかは未検討(現状の Unity 統合はプロセス内で1つのみ使う想定)。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use mw_backend::{Backend, CpalBackend};
use mw_core::Renderer;

struct Instance {
    handle: u64,
    backend: CpalBackend,
    // M0 では Renderer はまだミキサを持たないが、Backend が保持する Arc の対とし、
    // 将来のコマンドキュー処理(§5.2)から参照できるようここに保持しておく。
    #[allow(dead_code)]
    renderer: Arc<Renderer>,
}

/// ハンドルは 1 から始まる単調増加の不透明 ID。0 は「未割当」を意味する予約値として使わない
/// (呼び出し側が初期化し忘れた `out_handle` を誤ってハンドルだと解釈しにくくするため)。
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

fn registry() -> &'static Mutex<Option<Instance>> {
    static REGISTRY: OnceLock<Mutex<Option<Instance>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(None))
}

pub enum InitOutcome {
    /// 新規に出力ストリームを開いた。
    Opened(u64),
    /// 既に開いていたので同一ハンドルを返す(冪等)。
    AlreadyOpen(u64),
    /// バックエンドのオープンに失敗した(デバイス無し環境等)。
    Failed,
}

/// ミドルウェアを初期化する。既に初期化済みなら同一ハンドルを返す(冪等)。
pub fn init() -> InitOutcome {
    let mut guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(instance) = guard.as_ref() {
        return InitOutcome::AlreadyOpen(instance.handle);
    }

    let renderer = Arc::new(Renderer::new());
    let mut backend = CpalBackend::new();
    if backend.open(Arc::clone(&renderer)).is_err() {
        return InitOutcome::Failed;
    }

    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    *guard = Some(Instance {
        handle,
        backend,
        renderer,
    });
    InitOutcome::Opened(handle)
}

pub enum ShutdownOutcome {
    /// 正常に停止・解放した。
    Closed,
    /// バックエンドのクローズ自体に失敗した(レジストリからは外れるため二重解放にはならない)。
    CloseFailed,
    /// ハンドルが無効(未初期化 / 二重 shutdown / 他インスタンスのハンドル)。
    InvalidHandle,
}

/// ミドルウェアを終了する。無効なハンドルはエラーとして検出し、クラッシュしない。
pub fn shutdown(handle: u64) -> ShutdownOutcome {
    let mut guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let is_valid = guard
        .as_ref()
        .is_some_and(|instance| instance.handle == handle);
    if !is_valid {
        return ShutdownOutcome::InvalidHandle;
    }

    // 見つかったインスタンスをレジストリから外す。`close` が失敗しても状態は握ったままに
    // しない(二重 shutdown は次回呼び出しで ErrInvalidHandle として検出されるため)。
    match guard.take() {
        Some(mut instance) => match instance.backend.close() {
            Ok(()) => ShutdownOutcome::Closed,
            Err(_) => ShutdownOutcome::CloseFailed,
        },
        // 直前の `is_valid` チェックで Some を確認済みのため到達しない防御的分岐。
        None => ShutdownOutcome::InvalidHandle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: これらのテストはグローバルレジストリを共有するため、cpal のデバイス有無に
    // 依存する部分(実際に `Opened` になるかどうか)は環境依存。ここでは
    // 「無効ハンドルの検出」「二重 shutdown の安全性」という契約のみを固定する。

    #[test]
    fn shutdown_with_never_issued_handle_is_invalid() {
        // 十分大きい値を使い、他のテストが払い出した可能性のあるハンドルと衝突しないようにする。
        match shutdown(u64::MAX) {
            ShutdownOutcome::InvalidHandle => {}
            ShutdownOutcome::Closed | ShutdownOutcome::CloseFailed => {
                panic!("unissued handle must not match an existing instance")
            }
        }
    }
}
