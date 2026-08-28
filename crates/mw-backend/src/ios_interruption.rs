//! iOS / tvOS の AVAudioSession 割り込み・アプリライフサイクルからの復帰(初期構築仕様
//! M3「割り込み処理」)。
//!
//! ## 実機バグの原因
//!
//! iOS はアプリが(Background Audio 機能を持たないまま)バックグラウンドへ移行すると
//! `AVAudioSession` を非アクティブ化し、RemoteIO の出力ユニットを止める。ところが
//! cpal 0.18.1 の iOS 実装(`coreaudio::ios::session_event_manager`。ソースを確認済み)は
//! ルート変化(`AVAudioSessionRouteChangeNotification`)とメディアサービスの喪失/リセット
//! しか監視しておらず、**`AVAudioSessionInterruptionNotification` を一切監視していない**。
//! そのため OS がストリームを止めても、ミドルウェア側には何も通知されず、
//! `mw-core::Renderer::render` を呼ぶ主体である `cpal::Stream` は「鳴っているつもり」の
//! まま固まる。SE はこのストリーム経由なので鳴らなくなり、Unity の `AudioSource` 経由の
//! ボイスは Unity 自身のセッション管理で生き残る——実機報告「ホームへ戻ると SE だけ
//! 無音になる」の裏付けと一致する。
//!
//! さらに厄介なことに、cpal 0.18.1 の `Stream::play()`(iOS 実装、`coreaudio/ios/mod.rs`
//! の `StreamTrait::play` を確認済み)は内部の `playing: bool` フラグが既に `true` なら
//! `AudioOutputUnitStart` を呼ばずに即座に return する。OS が横から出力ユニットを
//! 止めても cpal 側のこのフラグは追随しない(cpal 自身がそれを検知する経路を
//! 持たないため)。**つまり単純に `stream.play()` を呼び直すだけでは復帰しない**——
//! 先に `stream.pause()` を呼んでフラグを `false` に戻してから `play()` を呼ぶ必要がある
//! (`AudioOutputUnitStart`/`Stop` はどちらも、既に動いている/止まっているユニットに
//! 対して呼んでも安全というのが CoreAudio の一般的な契約。この経路は音声スレッドでは
//! ない非リアルタイムスレッドから呼ぶので、`pause`/`play` 内部の `Mutex` ロックも
//! §5.3 の対象外)。
//!
//! ## 実装方針(cpal の抽象と Objective-C ランタイムの兼ね合い)
//!
//! 選択肢は主に3つあった:
//!
//! 1. **cpal の `Stream`/`Backend` を丸ごと作り直す(閉じて再オープン)。**
//!    `mw-backend::Backend::open` は `mw_core::Renderer` を値渡し(ムーブ)で受け取り、
//!    音声コールバックのクロージャへ排他的に所有させる設計(`crates/mw-backend/CLAUDE.md`
//!    「M0 からの変更点」)。一度ムーブした `Renderer` を後から取り戻す経路が無いため、
//!    ストリームを閉じて作り直すと Renderer ごと(=全ボイス・バス音量・楽曲の再生位置)
//!    失われる。これを避けるには `mw-ffi::handle::Instance` 側にも大きな再設計が要る
//!    (「M3 で必須実装」と初期構築仕様§14のリスク表に書かれている AAudio 再オープンの
//!    本丸そのもの)。今回のスコープ(実機報告の再現・修正)を超えるため見送った。
//! 2. **最小の Obj-C シムを xcframework に同梱する**(初期構築仕様§14 が当初想定していた
//!    経路)。cpal 0.18 は既に `objc2` 系クレート(`objc2-avf-audio`/`objc2-foundation`/
//!    `block2`)を iOS 実装の依存に持ち込んでいる(`ios_session.rs` が既に同じ理由で
//!    直接利用している)。Obj-C ファイルを追加すると依存ツリーが増えないという利点を
//!    捨てることになり、`crates/mw-backend/CLAUDE.md`「§1 M1【確定】コア・プラットフォーム
//!    層とも Rust で書き、OS 依存部のみ薄いシムを許容」の精神にも反する。
//! 3. **(採用)同じ `cpal::Stream` を維持したまま、`objc2-foundation` の
//!    `NSNotificationCenter` へ Rust から直接 observer を登録し、復帰時に
//!    `pause()`→`play()` を呼び直す。** cpal 自身の `session_event_manager.rs` が
//!    ルート変化等で全く同じパターン(ブロックベースの observer 登録)を使っており、
//!    実績のある手法をそのまま踏襲できる。`Renderer` の所有権を一切動かさないため、
//!    ボイス・バス音量・楽曲の再生位置はすべて割り込みを跨いで自然に保持される
//!    (=「実機報告のバグを直す」というスコープに対して最小の変更で済む)。
//!
//! 3 を選んだ。監視する通知は2つ:
//!
//! - `AVAudioSessionInterruptionNotification`(電話着信等の「真の」割り込み。Began/Ended
//!   を判別でき、Ended の `userInfo` に `AVAudioSessionInterruptionOptionShouldResume` が
//!   立っていれば復帰を試みる)。バックグラウンド遷移も Background Audio 機能が無ければ
//!   ここで Began が飛んでくる(Apple のドキュメント・WWDC のセッション「Improving Your
//!   App's Audio」で説明されている挙動)。
//! - `UIApplicationDidBecomeActiveNotification`(安全網)。上記の Began は確実に飛んでくる
//!   一方、**対応する Ended がバックグラウンド遷移のケースでは確実に飛んでくる保証が無い**
//!   ——実機報告はまさにこれが疑われる。`objc2-ui-kit` を新規依存に追加せずに済むよう、
//!   通知名は `objc2_foundation::ns_string!` で文字列リテラルから直接作る
//!   (`ios_session.rs` と同じ「依存ツリーを増やさない」方針)。
//!   **ただし割り込み中(`InterruptionState::Interrupted`/`RecoveryFailed`)でない限り
//!   何もしない**([`InterruptionState::on_app_became_active`] 参照)。通知センターの
//!   Control Center 表示やバナー通知でも `DidBecomeActive` は飛んでくるが、これらは
//!   実際には音声セッションを中断しないため、無条件に `pause()`→`play()` すると
//!   通常プレイ中に不要な音切れを生む(このガードが必要な理由)。
//!
//! ## パニック安全性
//!
//! Objective-C ランタイムから直接呼ばれるブロックの内部で panic が Rust スタックを
//! 越えて Obj-C フレームを巻き戻すのは未定義動作になりうる。`std::panic::catch_unwind`
//! で内容を必ず包む(`mw-ffi` が FFI 境界の外へ panic を漏らさないのと同じ考え方、
//! `crates/mw-ffi/CLAUDE.md` 参照)。ここは音声スレッドではない(§5.3 の対象外)ので
//! `catch_unwind` のコスト自体は問題にならない。
//!
//! ## 自動テストで守れる範囲・守れない範囲
//!
//! 実機の割り込み・バックグラウンド遷移そのものは自動テストで再現できない
//! (OS が送る通知、`AudioUnit` の実際の再始動はどちらもシミュレートできない)。
//! そのため復帰ロジックは [`InterruptionState`](OS API 呼び出しを一切含まない純粋な
//! 状態機械)として切り出し、遷移だけをこのファイル末尾の `tests` で固定化している。
//! 「割り込みで止まった → 再開要求 → 実際に再開できた」の一連の流れと、
//! 「ベニンな(実際には中断していない)アクティブ化では何もしない」ガードの両方を
//! カバーする。OS 通知が実機で本当に発火するか、`pause()`→`play()` の順序で
//! `AudioOutputUnitStart` が実際に音を復活させるかは実機検証でしか確認できない。

use std::sync::Arc;

use mw_core::EventQueue;

/// 割り込みからの復帰を表す状態機械(OS API 呼び出しを一切含まない、純粋な値型)。
///
/// 実機の割り込みそのものは自動テストで再現できないが、遷移ロジックはここに切り出す
/// ことで単体テストに固定化できる(モジュール doc「自動テストで守れる範囲」参照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptionState {
    /// 割り込みなし。ストリームは通常どおり鳴っているはず。
    Running,
    /// 割り込み中(OS がストリームを止めている、またはその可能性が高い)。
    Interrupted,
    /// 復帰(セッション再アクティブ化 + ストリーム再始動)を試みるべき状態になった。
    RecoveryPending,
    /// 復帰を試み、成功した。
    Recovered,
    /// 復帰を試みたが失敗した。次にアプリがアクティブになったタイミングで再試行する
    /// ([`InterruptionState::on_app_became_active`])。
    RecoveryFailed,
}

impl InterruptionState {
    /// 割り込み無し(通常再生中)の初期状態。
    pub const fn new() -> Self {
        Self::Running
    }

    /// 割り込みが始まった(`AVAudioSessionInterruptionTypeBegan`)。
    ///
    /// どの状態からでも `Interrupted` へ遷移する——復帰を試みている最中に新しい割り込みが
    /// 割り込んできた場合(電話中にさらに着信が来る等)も、最新の「止まっている」事実を
    /// 優先する。
    pub fn on_interruption_began(self) -> Self {
        Self::Interrupted
    }

    /// 割り込みが終わった(`AVAudioSessionInterruptionTypeEnded`)。`should_resume` は
    /// userInfo の `AVAudioSessionInterruptionOptionShouldResume` の有無。
    ///
    /// OS が「再開不要」と言っている場合は復帰を試みず `Running` へ戻す(何もしていない
    /// ので「割り込みは無かった」ときと区別しない)。
    pub fn on_interruption_ended(self, should_resume: bool) -> Self {
        if should_resume {
            Self::RecoveryPending
        } else {
            Self::Running
        }
    }

    /// アプリがアクティブになった(`UIApplicationDidBecomeActiveNotification`)。
    ///
    /// 割り込み中だった(`Interrupted`)、または前回の復帰に失敗していた
    /// (`RecoveryFailed`)場合のみ復帰を試みる。それ以外(`Running`/`RecoveryPending`/
    /// `Recovered`)では何もしない——実際には中断していないアクティブ化(Control
    /// Center・通知バナー等)のたびに `pause()`→`play()` を呼び直すと、通常プレイ中に
    /// 不要な音切れを生むため(モジュール doc「実装方針」参照)。
    pub fn on_app_became_active(self) -> Self {
        match self {
            Self::Interrupted | Self::RecoveryFailed => Self::RecoveryPending,
            other => other,
        }
    }

    /// 復帰(セッション再アクティブ化 + ストリーム再始動)を試みた結果。
    pub fn on_recovery_attempted(self, success: bool) -> Self {
        if success {
            Self::Recovered
        } else {
            Self::RecoveryFailed
        }
    }

    /// 呼び出し側(OS 通知ハンドラ)が実際に `pause()`→`play()` を試みるべきかどうか。
    /// 状態機械自体は OS 呼び出しを一切しない——判断材料を返すだけ。
    pub fn needs_recovery_attempt(self) -> bool {
        matches!(self, Self::RecoveryPending)
    }
}

impl Default for InterruptionState {
    fn default() -> Self {
        Self::new()
    }
}

/// `AVAudioSessionInterruptionNotification` / `UIApplicationDidBecomeActiveNotification` の
/// 監視・復帰処理。iOS / tvOS 以外では何もしない no-op(`ios_session::configure` と同じ
/// 「常に呼んでよい・非対応 OS では素通り」の設計)。
pub struct Watcher {
    // 読み出さない(`Drop` 経由で observer を確実に `removeObserver` させるためだけに
    // 保持する RAII ハンドル)。
    #[cfg(any(target_os = "ios", target_os = "tvos"))]
    #[allow(dead_code)]
    inner: imp::Watcher,
}

impl Watcher {
    /// `stream` は復帰時に `pause()`→`play()` を呼び直す対象。`events` は
    /// `Event::AudioInterruptionBegan`/`AudioInterruptionEnded` を積む先
    /// (`EventQueue::push_side_channel`。ここは音声スレッドではないので §5.3 の対象外)。
    ///
    /// `CpalBackend::open` から、ストリームを `play()` した直後に呼ぶこと
    /// (`cpal_backend.rs` 参照)。
    #[cfg(any(target_os = "ios", target_os = "tvos"))]
    pub fn new(stream: Arc<cpal::Stream>, events: Arc<EventQueue>) -> Self {
        Self {
            inner: imp::Watcher::new(stream, events),
        }
    }

    /// iOS / tvOS 以外では何もしない。
    #[cfg(not(any(target_os = "ios", target_os = "tvos")))]
    pub fn new(_stream: Arc<cpal::Stream>, _events: Arc<EventQueue>) -> Self {
        Self {}
    }
}

#[cfg(any(target_os = "ios", target_os = "tvos"))]
mod imp {
    use std::panic::{self, AssertUnwindSafe};
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};

    use block2::RcBlock;
    use cpal::traits::StreamTrait;
    use mw_core::{Event, EventQueue};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
    use objc2_avf_audio::{
        AVAudioSessionInterruptionNotification, AVAudioSessionInterruptionOptionKey,
        AVAudioSessionInterruptionOptions, AVAudioSessionInterruptionType,
        AVAudioSessionInterruptionTypeKey,
    };
    use objc2_foundation::{NSNotification, NSNotificationCenter, NSNumber, NSString, ns_string};

    use super::InterruptionState;

    pub(super) struct Watcher {
        observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    }

    // SAFETY: NSNotificationCenter はスレッドセーフ。ここへ保持する observer トークンは
    // Drop で `removeObserver` するためだけに使う不透明なハンドルで、他のスレッドから
    // 読み書きする内部状態を持たない(cpal 自身の `session_event_manager.rs` の
    // `SessionEventManager` と同じ理由づけ)。
    unsafe impl Send for Watcher {}
    unsafe impl Sync for Watcher {}

    impl Watcher {
        pub(super) fn new(stream: Arc<cpal::Stream>, events: Arc<EventQueue>) -> Self {
            let nc = NSNotificationCenter::defaultCenter();
            let state = Arc::new(Mutex::new(InterruptionState::new()));
            let mut observers = Vec::new();

            {
                let state = Arc::clone(&state);
                let stream = Arc::clone(&stream);
                let events = Arc::clone(&events);
                let block = RcBlock::new(move |notif: NonNull<NSNotification>| {
                    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                        // SAFETY: OS が有効な NSNotification を渡してくる(この block の
                        // 契約)。
                        let notif = unsafe { notif.as_ref() };
                        handle_interruption_notification(notif, &state, &stream, &events);
                    }));
                    if outcome.is_err() {
                        crate::mw_log!(
                            "[mw-backend] ios_interruption: panic while handling \
                             AVAudioSessionInterruptionNotification (caught at the boundary)"
                        );
                    }
                });
                // SAFETY: `AVAudioSessionInterruptionNotification` はプロセス生存中
                // 変化しない静的な通知名。`addObserverForName_object_queue_usingBlock` は
                // block を Objective-C ランタイムが retain する契約どおりに呼ぶ。
                if let Some(name) = unsafe { AVAudioSessionInterruptionNotification } {
                    let observer = unsafe {
                        nc.addObserverForName_object_queue_usingBlock(
                            Some(name),
                            None,
                            None,
                            &block,
                        )
                    };
                    observers.push(observer);
                } else {
                    crate::mw_log!(
                        "[mw-backend] ios_interruption: AVAudioSessionInterruptionNotification \
                         is unavailable"
                    );
                }
            }

            {
                let state = Arc::clone(&state);
                let stream = Arc::clone(&stream);
                let events = Arc::clone(&events);
                let block = RcBlock::new(move |_: NonNull<NSNotification>| {
                    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                        handle_became_active(&state, &stream, &events);
                    }));
                    if outcome.is_err() {
                        crate::mw_log!(
                            "[mw-backend] ios_interruption: panic while handling \
                             UIApplicationDidBecomeActiveNotification (caught at the boundary)"
                        );
                    }
                });
                // UIKit 側の通知名。`objc2-ui-kit` を新規依存に追加せず、文字列リテラルから
                // 直接 NSString を作る(`ns_string!` はコンパイル時定数、実行時アロケーション
                // 無し。モジュール doc「実装方針」参照)。
                let name = ns_string!("UIApplicationDidBecomeActiveNotification");
                // SAFETY: 上記と同様、`addObserverForName_object_queue_usingBlock` の契約に
                // 従う。
                let observer = unsafe {
                    nc.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
                };
                observers.push(observer);
            }

            Self { observers }
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            let nc = NSNotificationCenter::defaultCenter();
            for observer in &self.observers {
                // SAFETY: `observer` はこの `Watcher` が `addObserverForName_...` から
                // 受け取ったトークンそのもの(他で作られたものを渡さない)。
                unsafe { nc.removeObserver(observer.as_ref()) };
            }
        }
    }

    fn handle_interruption_notification(
        notif: &NSNotification,
        state: &Mutex<InterruptionState>,
        stream: &cpal::Stream,
        events: &EventQueue,
    ) {
        let Some(kind) = interruption_type(notif) else {
            return;
        };

        if kind == AVAudioSessionInterruptionType::Began {
            crate::mw_log!("[mw-backend] AVAudioSession interruption began");
            let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
            *guard = guard.on_interruption_began();
            drop(guard);
            events.push_side_channel(Event::AudioInterruptionBegan);
            return;
        }

        if kind == AVAudioSessionInterruptionType::Ended {
            let should_resume = interruption_should_resume(notif);
            crate::mw_log!(
                "[mw-backend] AVAudioSession interruption ended (should_resume={should_resume})"
            );
            let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
            *guard = guard.on_interruption_ended(should_resume);
            let attempt = guard.needs_recovery_attempt();
            drop(guard);
            if attempt {
                attempt_recovery(state, stream, events);
            } else {
                events.push_side_channel(Event::AudioInterruptionEnded { recovered: false });
            }
        }
    }

    fn handle_became_active(
        state: &Mutex<InterruptionState>,
        stream: &cpal::Stream,
        events: &EventQueue,
    ) {
        let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
        *guard = guard.on_app_became_active();
        let attempt = guard.needs_recovery_attempt();
        drop(guard);
        if attempt {
            crate::mw_log!(
                "[mw-backend] app became active while a stream interruption was unresolved; \
                 attempting recovery"
            );
            attempt_recovery(state, stream, events);
        }
    }

    /// セッション再アクティブ化 + ストリーム再始動を試みる。
    ///
    /// `stream.play()` だけでは復帰しない(モジュール doc の「実機バグの原因」参照:
    /// cpal 0.18.1 の内部 `playing` フラグが OS 主導の停止に追随しないため、まず
    /// `pause()` でフラグを倒してから `play()` を呼ぶ)。
    fn attempt_recovery(
        state: &Mutex<InterruptionState>,
        stream: &cpal::Stream,
        events: &EventQueue,
    ) {
        crate::ios_session::configure();
        let _ = stream.pause();
        let success = stream.play().is_ok();
        if !success {
            crate::mw_log!("[mw-backend] ios_interruption: stream restart failed");
        }

        let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
        *guard = guard.on_recovery_attempted(success);
        drop(guard);
        events.push_side_channel(Event::AudioInterruptionEnded { recovered: success });
    }

    fn interruption_type(notif: &NSNotification) -> Option<AVAudioSessionInterruptionType> {
        let user_info = notif.userInfo()?;
        let key = unsafe { AVAudioSessionInterruptionTypeKey }?;
        // SAFETY: `userInfo()` の実体は `NSDictionary<NSString, id>`(Apple のドキュメント
        // 上の契約)。cpal 自身の `session_event_manager.rs::route_change_error` と同じ
        // reinterpret(`cast_unchecked`)。
        let dict = unsafe { user_info.cast_unchecked::<NSString, AnyObject>() };
        let value = dict.objectForKey(key)?;
        let number = value.downcast_ref::<NSNumber>()?;
        Some(AVAudioSessionInterruptionType(
            number.unsignedIntegerValue(),
        ))
    }

    fn interruption_should_resume(notif: &NSNotification) -> bool {
        (|| -> Option<bool> {
            let user_info = notif.userInfo()?;
            let key = unsafe { AVAudioSessionInterruptionOptionKey }?;
            let dict = unsafe { user_info.cast_unchecked::<NSString, AnyObject>() };
            let value = dict.objectForKey(key)?;
            let number = value.downcast_ref::<NSNumber>()?;
            let options = AVAudioSessionInterruptionOptions(number.unsignedIntegerValue());
            Some(options.contains(AVAudioSessionInterruptionOptions::ShouldResume))
        })()
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::InterruptionState;

    /// 依頼書が明示した最小シナリオ:「停止した → 再開要求 → 再開した」。
    #[test]
    fn interruption_began_then_ended_with_resume_then_recovery_succeeds() {
        let state = InterruptionState::new();
        assert_eq!(state, InterruptionState::Running);

        let state = state.on_interruption_began();
        assert_eq!(state, InterruptionState::Interrupted);

        let state = state.on_interruption_ended(true);
        assert_eq!(state, InterruptionState::RecoveryPending);
        assert!(state.needs_recovery_attempt());

        let state = state.on_recovery_attempted(true);
        assert_eq!(state, InterruptionState::Recovered);
        assert!(!state.needs_recovery_attempt());
    }

    #[test]
    fn interruption_ended_without_should_resume_returns_to_running_without_recovery() {
        let state = InterruptionState::Interrupted.on_interruption_ended(false);
        assert_eq!(state, InterruptionState::Running);
        assert!(!state.needs_recovery_attempt());
    }

    #[test]
    fn recovery_failure_can_be_retried_when_the_app_becomes_active_again() {
        let state = InterruptionState::Interrupted
            .on_interruption_ended(true)
            .on_recovery_attempted(false);
        assert_eq!(state, InterruptionState::RecoveryFailed);

        let state = state.on_app_became_active();
        assert_eq!(state, InterruptionState::RecoveryPending);
        assert!(state.needs_recovery_attempt());
    }

    #[test]
    fn app_became_active_recovers_the_exact_reported_bug_scenario() {
        // 実機報告: バックグラウンドへ行くと Began は飛ぶが(cpal がストリームを
        // 「鳴っているつもり」のままにする)、Ended が確実に飛んでくる保証は無い。
        // その場合でも DidBecomeActive が安全網として復帰を要求できること。
        let state = InterruptionState::new().on_interruption_began();
        assert_eq!(state, InterruptionState::Interrupted);

        let state = state.on_app_became_active();
        assert_eq!(state, InterruptionState::RecoveryPending);
    }

    #[test]
    fn app_became_active_is_a_no_op_when_nothing_was_interrupted() {
        // Control Center・通知バナー等、実際には中断していないアクティブ化で
        // 毎回ストリームを作り直さないことを固定化する(モジュール doc「実装方針」参照)。
        for state in [
            InterruptionState::Running,
            InterruptionState::RecoveryPending,
            InterruptionState::Recovered,
        ] {
            assert_eq!(state.on_app_became_active(), state);
        }
    }

    #[test]
    fn a_new_interruption_always_wins_even_mid_recovery() {
        for state in [
            InterruptionState::Running,
            InterruptionState::Interrupted,
            InterruptionState::RecoveryPending,
            InterruptionState::Recovered,
            InterruptionState::RecoveryFailed,
        ] {
            assert_eq!(
                state.on_interruption_began(),
                InterruptionState::Interrupted
            );
        }
    }

    #[test]
    fn default_state_is_running() {
        assert_eq!(InterruptionState::default(), InterruptionState::Running);
    }
}
