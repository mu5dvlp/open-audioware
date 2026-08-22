//! init/shutdown ハンドルの管理。
//!
//! 初期構築仕様 §4.8(確定): ハンドルは不透明な整数 ID(ポインタを C# に渡さない)。
//! 二重解放・使用後解放は無効 ID エラーとして検出する(クラッシュさせない)。
//! §6: Unity Editor のドメインリロードに備え、init/shutdown は冪等にする
//! (dylib は常駐前提。ドメインリロードのたびに新規プロセスが立つわけではない)。
//!
//! M0 時点ではインスタンスは同時に1つのみ(グローバルレジストリ)。
//! 複数インスタンスを許すかは未検討(現状の Unity 統合はプロセス内で1つのみ使う想定)。
//!
//! M1 で `Instance` にゲームスレッド側ハンドル(`CommandSender` / `ReclaimReceiver`)と
//! サウンドストレージ・ボイスシリアル採番器を追加した(§5.2「コマンド/イベントキュー」)。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use mw_backend::{Backend, CpalBackend};
use mw_core::{CommandSender, Config, ReclaimReceiver, Renderer, SoundStorage};

/// 出力デバイスが実際にオープンされるまでの暫定サンプルレート(§4.7 推奨の 48kHz)。
/// `CpalBackend::open` がデバイスとネゴシエートした実レートで上書きする
/// (`Renderer::set_sample_rate`、コールバックが動き出す前)。
const PROVISIONAL_SAMPLE_RATE: u32 = 48_000;

pub struct Instance {
    handle: u64,
    backend: CpalBackend,
    pub command_sender: CommandSender,
    pub reclaim_receiver: Mutex<ReclaimReceiver>,
    pub sounds: Mutex<SoundStorage>,
    next_voice_serial: AtomicU64,
}

impl Instance {
    /// 新規ボイスシリアル(不透明な voice id)を1つ払い出す。0 は「未割当」の予約値。
    pub fn next_voice_serial(&self) -> u64 {
        self.next_voice_serial
            .fetch_add(1, Ordering::Relaxed)
            .max(1)
    }

    /// 音声スレッドが手放した `Arc<SoundData>` をゲームスレッド上で回収する。
    /// FFI 呼び出しの合間に日和見的に呼ぶ(§5.4: 非ブロッキング。rtrb の pop は O(1))。
    pub fn drain_reclaimed(&self) {
        let mut guard = self
            .reclaim_receiver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.drain();
    }
}

/// ハンドルは 1 から始まる単調増加の不透明 ID。0 は「未割当」を意味する予約値として使わない
/// (呼び出し側が初期化し忘れた `out_handle` を誤ってハンドルだと解釈しにくくするため)。
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

fn registry() -> &'static Mutex<Option<Instance>> {
    static REGISTRY: OnceLock<Mutex<Option<Instance>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(None))
}

/// 現在初期化済みのインスタンスに対して `f` を実行する。無効ハンドルは `None` を返す。
///
/// レジストリの `Mutex` はゲームスレッド側のコードでのみ取得される(§5.3 が禁止するのは
/// 音声スレッド側でのロック取得のみ。ここは FFI 呼び出し = ゲームスレッド経路)。
pub fn with_instance<T>(handle: u64, f: impl FnOnce(&Instance) -> T) -> Option<T> {
    let guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match guard.as_ref() {
        Some(instance) if instance.handle == handle => Some(f(instance)),
        _ => None,
    }
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

    let (renderer, command_sender, reclaim_receiver) =
        Renderer::build(Config::default(), PROVISIONAL_SAMPLE_RATE);

    let mut backend = CpalBackend::new();
    if let Err(err) = backend.open(renderer) {
        // 実機(特に iOS)では失敗理由が分からないと原因を特定できないため、
        // 具体的な BackendError を必ず残す(docs/measurement-m1.md §7.6-1)。
        // MwResult は粒度が粗い(ErrBackendOpenFailed 一種)ので、詳細はこのログが唯一の手がかりになる。
        eprintln!("[mw-ffi] mw_init: backend open failed: {err}");
        return InitOutcome::Failed;
    }

    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    *guard = Some(Instance {
        handle,
        backend,
        command_sender,
        reclaim_receiver: Mutex::new(reclaim_receiver),
        sounds: Mutex::new(SoundStorage::new()),
        next_voice_serial: AtomicU64::new(1),
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

    #[test]
    fn with_instance_returns_none_for_unknown_handle() {
        assert!(with_instance(u64::MAX, |_| ()).is_none());
    }
}
