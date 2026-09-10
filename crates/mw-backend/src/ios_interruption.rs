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
//! ## 実機で判明したこと(調査記録は `docs/history/09-2026-09-05.md`)
//!
//! 上の実装方針は、その後の実機報告(R12 / R13 / R24 / V14)で**3箇所が否定された**。
//! いま実装が満たしている不変条件は次の4つで、**どれも実機でしか確かめられなかった**:
//!
//! 1. **`Began` は必ずしも飛んでこない。** バックグラウンド遷移では
//!    `AVAudioSessionInterruptionNotification` の Began が届かないことがある(R12)。
//!    そのため `UIApplicationDidEnterBackgroundNotification` を監視して
//!    [`InterruptionState::Backgrounded`] へ倒し、前面復帰で必ず復帰を試みる。
//! 2. **`NewDeviceAvailable` も復帰を要求する。** 当初は「新しい機器が生えただけなら
//!    ストリームは止まらない」と判断して除外していたが、Bluetooth 再接続で無音になる
//!    実機報告(R13)がこれを否定した([`RouteChangeReason::requires_recovery`])。
//!    ⚠️ ただし `CategoryChange` は**意図的に除外したまま** —— 復帰処理自身が
//!    カテゴリ変更を誘発して自己ループする。
//! 3. **`pause()`→`play()` が `Ok` を返しても、鳴っているとは限らない。**(R24)
//!    `AudioOutputUnitStart` の成功と音声コールバックの再開は別事象なので、
//!    **コールバックが実際に前進したかを実測して確認する**
//!    ([`confirm_recovery_progress`] / `RECOVERY_WAIT_SCHEDULE_MS`)。
//! 4. 🔴 **通知が1つも来ないまま止まることがある。**(V14、2026-09-10)
//!    実機の診断で「音源のロードも `mw_se_play` も全部成功しているのに無音、アプリを
//!    再起動すると直る」が確定した。上の1〜3はすべて**OS から通知が来ること**が前提で、
//!    通知の来ない止まり方には何も反応しない。そこで通知に頼らず、
//!    **コールバックが進んでいるかを定期的に実測して、止まっていたら復帰する**
//!    ウォッチドッグを入れた([`OutputStallDetector`] / `spawn_output_stall_watchdog`)。
//!    📌 **原因を問わない直し方にしたのが要点。** 「なぜ止まったか」を突き止めて拾う
//!    通知を足すやり方は R12 / R13 で2度やり直しており、通知の網羅は原理的に終わらない。
//!
//! 🔴 **どこまで疑って何が否定されたかの全記録は
//! [`docs/history/09-2026-09-05.md`](../../../docs/history/09-2026-09-05.md) にある。**
//! 同じ症状が再発したらそこから読むこと(**このモジュール doc には積み増さない**——
//! 400行まで膨らんで実装本体に匹敵していたのを 2026-09-05 に移した)。
//!
//! 📌 **本ファイル中の「…」による節参照は、すべてその調査記録の節を指す**
//! (「ルート変化」/「`DidBecomeActive` 安全網の前提が崩れていたケース」/
//! 「実機ログで R12 の修正を確認」/「Bluetooth 再接続で無音になるケース」/
//! 「`pause()`→`play()` が Ok を返しても無音のままだったケース」の5つ)。
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
//! 「割り込みで止まった → 再開要求 → 実際に再開できた」の一連の流れ、
//! 「ベニンな(実際には中断していない)アクティブ化では何もしない」ガード、
//! 「`OldDeviceUnavailable` と `NewDeviceAvailable` が復帰を要求し、それ以外の
//! ルート変化 reason(`CategoryChange` を含む——自己誘発ループ防止のため意図的に除外。
//! 「Bluetooth 再接続で無音になるケース」参照)は状態を一切変えない」ガード
//! ([`RouteChangeReason::requires_recovery`] / [`InterruptionState::on_route_changed`])、
//! そして調査記録の R12 節で足した
//! 「Began が一度も来ないままバックグラウンドへ行っても、前面復帰したら復帰を試みる」
//! ([`InterruptionState::Backgrounded`] / [`InterruptionState::on_app_entered_background`])
//! の4つをカバーする。`DidEnterBackground` が実機で本当に飛んでくるか、`pause()`→`play()`
//! の順序で `AudioOutputUnitStart` が実際に音を復活させるかは実機検証でしか確認できない
//! ——前者は上の「実機ログで R12 の修正を確認」節で確認済み。`NewDeviceAvailable`
//! を復帰要求に追加したことが Bluetooth 再接続で実際に無音を解消するか、また
//! `AVAudioSessionRouteChangeNotification` がそもそも実機で発火しているかは、
//! 「Bluetooth 再接続で無音になるケース」節に書いた次の実機テストでのみ確認できる。
//!
//! **5つ目: `pause()`→`play()` が実際にコールバックを前進させたかの判定
//! ([`confirm_recovery_progress`])。** これも `InterruptionState` と同じ理由で
//! CoreAudio 非依存の純粋な関数として切り出し、「成功が1回目で確認できる」「途中の
//! 再試行で確認できる」「最後まで確認できない」の3ケースを単体テストで固定化した
//! (調査記録「`pause()`→`play()` が Ok を返しても無音のままだったケース」
//! 参照)。ただし `AudioOutputUnitStart` 後に実際に何 ms で音声コールバックが再開する
//! かという実機の生の値そのものは自動テスト不可——`RECOVERY_WAIT_SCHEDULE_MS` の
//! 妥当性(20msで足りるか、370ms待っても復帰しない実機ケースがあるか)は次の実機
//! テストで確認する。
//!
//! **6つ目(P0-6, 2026-09-03): 復帰確認をワーカースレッドへ委譲したことでメインスレッドの
//! ブロックが実際に解消したか。** `attempt_recovery` が `confirm_recovery_progress` の
//! 呼び出しを `std::thread::spawn` した専用スレッドへ委譲するようになったこと自体は
//! コードの構造として自動テストで確認できない(`imp` モジュールは iOS/tvOS 専用の
//! `#[cfg]` 配下にあり、CoreAudio/UIKit 実体が無いホスト環境ではそもそもコンパイル
//! 対象に入らない——本ファイル冒頭 doc 参照。`cargo check --target aarch64-apple-ios`
//! でのコンパイル可否は確認済みだが、それは「型が合う」ことの確認であって「実機で
//! メインスレッドが本当にブロックされなくなったか」の確認ではない)。**次の実機テストで
//! 確認すべきこと**: (a) 割り込み終了・ルート変化・前面復帰それぞれで音が正しく復帰する
//! こと(既存の確認事項と同じ)に加え、(b) 前面復帰の瞬間に UI が実際にヒッチしなくなった
//! ことを Instruments 等の Main Thread 計測、または単純に手触りで確認すること
//! (是正前は Xcode の Time Profiler で `UIApplicationDidBecomeActiveNotification` の
//! ハンドラ内に370ms級のブロックが見えていたはず——是正後はそれが消えていることを
//! 確認する)。
//!
//! **7つ目(V14, 2026-09-10): 出力停止ウォッチドッグの判定([`OutputStallDetector`])。**
//! 他の3つと同じく OS 非依存の値型として切り出したので、「進み続けている間は黙っている」
//! 「連続して止まっていたら要求する」「1回では要求しない」「要求後はクールダウンする」
//! 「止まっているのが正常な状態(`Interrupted`/`Backgrounded`/`RecoveryPending`)では
//! 見張らない」「`RecoveryFailed` は見張る(R24 の穴)」「見張らない状態を挟んでも
//! 数えた分を持ち越さない」の7ケースをホストの単体テストで固定化した。
//! ⚠️ **実機でしか確かめられないのは「実際に音が戻るか」**——ウォッチドッグが叩くのは
//! 既存の `attempt_recovery` そのものなので、復帰の効き自体は上の1〜3と同じ土俵に乗る。
//! 閾値([`WATCHDOG_STALL_SAMPLES`] × [`WATCHDOG_POLL_INTERVAL_MS`] = 1秒)が
//! 短すぎて正常時に誤検知しないかも、実機でのみ確認できる。

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

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
    /// 実際にバックグラウンドへ遷移した(`UIApplicationDidEnterBackgroundNotification`)。
    /// `AVAudioSessionInterruptionNotification` の Began が(何らかの理由で)一度も
    /// 飛んでこないままバックグラウンドへ行った場合の安全網——調査記録「
    /// `DidBecomeActive` 安全網の前提が崩れていたケース」参照。前面復帰時
    /// ([`InterruptionState::on_app_became_active`])に無条件で `RecoveryPending` へ遷移する。
    Backgrounded,
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
    /// 割り込み中だった(`Interrupted`)、前回の復帰に失敗していた(`RecoveryFailed`)、
    /// または実際にバックグラウンドへ行っていた(`Backgrounded`。調査記録「
    /// `DidBecomeActive` 安全網の前提が崩れていたケース」参照)場合のみ復帰を試みる。
    /// それ以外(`Running`/`RecoveryPending`/`Recovered`)では何もしない——実際には
    /// 中断していないアクティブ化(Control Center・通知バナー等)のたびに
    /// `pause()`→`play()` を呼び直すと、通常プレイ中に不要な音切れを生むため
    /// (モジュール doc「実装方針」参照)。このガードは `Backgrounded` を足した後も
    /// 変わらず有効——`Backgrounded` は `DidEnterBackground` が実際に飛んできたときにしか
    /// 立たない状態で、Control Center・通知バナーでは飛んでこない
    /// ([`InterruptionState::on_app_entered_background`] 参照)。
    pub fn on_app_became_active(self) -> Self {
        match self {
            Self::Interrupted | Self::RecoveryFailed | Self::Backgrounded => Self::RecoveryPending,
            other => other,
        }
    }

    /// アプリが実際にバックグラウンドへ入った(`UIApplicationDidEnterBackgroundNotification`)。
    ///
    /// どの状態からでも `Backgrounded` へ遷移する——割り込み中(`Interrupted`)に
    /// バックグラウンドへ行くケース(電話に出たままホームへ戻る等)も普通にあり、
    /// その場合でも「実際に背面へ回った」という最新の事実を優先する
    /// (`on_interruption_began` の「最新の事実を優先」と同じ設計)。
    ///
    /// この通知は Control Center の引き下ろし・通知バナー表示では飛んでこない
    /// (アプリは前面のままで、これらで飛ぶのは `UIApplicationWillResignActiveNotification`
    /// まで)。そのため `on_app_became_active` の既存ガード(実際には中断していない
    /// アクティブ化での不要な音切れ回避)を壊さずに、「本当に背面へ行った」ケースだけを
    /// 判別子として使える(調査記録「`DidBecomeActive` 安全網の前提が
    /// 崩れていたケース」参照)。
    pub fn on_app_entered_background(self) -> Self {
        Self::Backgrounded
    }

    /// ルート変化(`AVAudioSessionRouteChangeNotification`)。`reason` は
    /// [`RouteChangeReason::requires_recovery`] で復帰要否を判断済みの値を渡す。
    ///
    /// 復帰が要る reason(`OldDeviceUnavailable` と `NewDeviceAvailable`。モジュール doc
    /// 「ルート変化」および「Bluetooth 再接続で無音になるケース」参照)
    /// ならどの状態からでも `RecoveryPending` へ(割り込み中に新しい割り込みが来る
    /// のと同じ「最新の事実を優先」設計)。**要らない reason は現状を一切変えない**
    /// (`self` をそのまま返す)——正常なルート切替(ヘッドフォン挿し込み等)のたびに
    /// `Interrupted`/`RecoveryFailed` を握りつぶして未解決の割り込みを覆い隠さないため、
    /// かつ `Running`/`Recovered` を無意味に触らないため。
    pub fn on_route_changed(self, reason: RouteChangeReason) -> Self {
        if reason.requires_recovery() {
            Self::RecoveryPending
        } else {
            self
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

    /// 出力停止のウォッチドッグ([`OutputStallDetector`])が、この状態のときに
    /// 「コールバックが進んでいない = 異常」と判断してよいか。
    ///
    /// 🔴 **止まっているのが正常な状態では見張らない。**
    /// - `Interrupted`: OS が意図的に止めている(割り込み終了で復帰する)。
    /// - `Backgrounded`: こちらから [`crate::ios_session::deactivate`] で手放した
    ///   (D6。前面復帰で復帰する)。
    /// - `RecoveryPending`: 通知ハンドラがこれから復帰を試みる。横から二重に叩かない。
    ///
    /// 逆に **`RecoveryFailed` は見張る対象に含める**——「復帰を試みたが空振りし、
    /// 次の `DidBecomeActive` を待つしかない」というのが調査記録 R24 で問題になった
    /// 状態そのもので、**アプリを前面に置いたままだと永久に誰も再試行しない**。
    /// ウォッチドッグはまさにこの穴を塞ぐために入れた(モジュール doc の不変条件4)。
    pub fn allows_stall_watchdog(self) -> bool {
        matches!(self, Self::Running | Self::Recovered | Self::RecoveryFailed)
    }
}

impl Default for InterruptionState {
    fn default() -> Self {
        Self::new()
    }
}

/// `AVAudioSessionRouteChangeReason` を薄く写した、OS 非依存の値(テストのため)。
///
/// imp 側(iOS/tvOS のみ)が実際の `AVAudioSessionRouteChangeReason`(objc2 型)から
/// これへ変換する橋渡し役——判定ロジック本体([`Self::requires_recovery`])はここに置き、
/// objc2 型に依存せず単体テストできるようにする(`InterruptionState` と同じ設計)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteChangeReason {
    /// 直前まで使っていたデバイスが無くなった(例: Bluetooth 切断・イヤホン抜け)。
    /// 実機報告「Bluetooth を解除すると SE が鳴らなくなる」に対応する reason。
    OldDeviceUnavailable,
    /// 新しいデバイスが使えるようになった(例: Bluetooth 接続・イヤホン挿し込み)。
    /// 実機報告「Bluetooth を再接続すると SE が鳴らなくなる」(R13)に対応する reason
    /// ——当初は「音が途切れず自動的に継続するのが通例」としてここに反応しない判断
    /// だったが、実機で否定された(調査記録「Bluetooth 再接続で無音になる
    /// ケース」参照)。
    NewDeviceAvailable,
    /// オーディオカテゴリが変わった。`ios_session::configure()` 自身が
    /// `setCategory_error` 経由で引き起こしうる。
    CategoryChange,
    /// ルートが明示的にオーバーライドされた。
    Override,
    /// 上記以外(`Unknown`/`WakeFromSleep`/`NoSuitableRouteForCategory`/
    /// `RouteConfigurationChange` 等)。
    Other,
}

impl RouteChangeReason {
    /// この reason で復帰(セッション再アクティブ化 + `pause()`→`play()`)を試みるべきか。
    ///
    /// **`OldDeviceUnavailable` と `NewDeviceAvailable` が `true`。** 判断根拠は調査記録
    /// 「ルート変化」および「Bluetooth 再接続で無音になるケース」に詳述——
    /// 要約すると、当初は `OldDeviceUnavailable` だけが Apple のドキュメント上「直前まで
    /// 使えていたものが無くなった」ことを意味すると判断していたが、`NewDeviceAvailable`
    /// (Bluetooth 再接続等)でも音が自動的には継続せず無音になることが実機報告(R13)で
    /// 確認され、対象に追加した。`CategoryChange` は `ios_session::configure()` 自身が
    /// 引き起こしうる自己誘発ループの懸念があるため引き続き除外し、`Override`/`Other`
    /// は実機報告が無いため据え置いている。
    pub fn requires_recovery(self) -> bool {
        matches!(self, Self::OldDeviceUnavailable | Self::NewDeviceAvailable)
    }
}

/// [`confirm_recovery_progress`] が「1回目の確認」から「最後の再試行」まで辿る待機
/// スケジュール(ミリ秒)。CoreAudio に一切依存しない定数——`InterruptionState`/
/// `RouteChangeReason` と同じ理由で、macOS/Linux でもコンパイル・テストできるように
/// `imp`(iOS/tvOS 専用)モジュールの外に置いてある(調査記録「
/// `pause()`→`play()` が Ok を返しても無音のままだったケース」参照)。
///
/// 各要素は「その回で(2回目以降は追加の `pause()`→`play()` を呼んだ後に)待つ時間」。
/// index 0(20ms)は追加の呼び出し無し——呼び出し側が既に済ませた1回目の
/// `pause()`→`play()` の結果を確認するだけなので、1回目で復帰していれば追加の
/// コストは20ms待つだけで済む。全部空振りした場合の合計待機時間は
/// 20+50+100+200 = 370ms(依頼書の「合計 400ms 程度を上限に」の範囲)。
pub const RECOVERY_WAIT_SCHEDULE_MS: [u64; 4] = [20, 50, 100, 200];

/// [`RECOVERY_WAIT_SCHEDULE_MS`] を1ステップずつ辿りながら、音声コールバックが実際に
/// 前進した(=本当に鳴り出した)かどうかを確認する。CoreAudio / cpal に一切触れない
/// 純粋な関数——OS 呼び出しを伴う副作用(2回目以降の `pause()`→`play()` の呼び直し、
/// 指定時間のブロッキング待機、現在のカウンタの読み出し)はすべて呼び出し側から
/// クロージャで注入する形にしてあり、`InterruptionState`/`RouteChangeReason` と同じ
/// 「判定ロジックはここに閉じ込め、副作用は呼び出し側」という設計に倣った
/// (調査記録「`pause()`→`play()` が Ok を返しても無音のままだったケース」
/// 参照)。これにより本ファイル末尾の `tests` で macOS 上でも固定化できる。
///
/// - `ticks_before`: 呼び出し側が1回目の `pause()`→`play()` を呼ぶ**前**に読んでおいた
///   カウンタの値(`CpalBackend::callback_ticks` 相当)。
/// - `read_ticks`: 現在のカウンタ値を読む。
/// - `retry`: 2回目以降の `pause()`→`play()` の呼び直し(1回目の呼び出しは呼び出し側が
///   既に済ませている前提——`RECOVERY_WAIT_SCHEDULE_MS` の doc参照)。
/// - `sleep_ms`: 指定ミリ秒だけブロックする(`attempt_recovery` の doc「待機は
///   `std::thread::sleep` を使う」参照)。
///
/// 戻り値: `Some((attempt, waited_ms))`——`attempt` は進行が確認できた試行番号
/// (1始まり。1なら追加の再試行無しで確認できた)、`waited_ms` はそこまでの累計待機
/// ミリ秒。`RECOVERY_WAIT_SCHEDULE_MS` を最後まで辿っても進行が確認できなければ
/// `None`(呼び出し側は復帰失敗として扱う)。
pub fn confirm_recovery_progress(
    ticks_before: u64,
    mut read_ticks: impl FnMut() -> u64,
    mut retry: impl FnMut(),
    mut sleep_ms: impl FnMut(u64),
) -> Option<(u32, u64)> {
    let mut waited_ms = 0u64;
    for (index, &wait_ms) in RECOVERY_WAIT_SCHEDULE_MS.iter().enumerate() {
        if index > 0 {
            retry();
        }
        sleep_ms(wait_ms);
        waited_ms += wait_ms;
        if read_ticks() != ticks_before {
            return Some((index as u32 + 1, waited_ms));
        }
    }
    None
}

/// 出力停止ウォッチドッグの観測間隔(ms)。
///
/// 音声コールバックは 5ms 前後のバッファ長で回り続ける([`crate::ios_session`])ので、
/// 250ms あれば「進んでいれば必ず進んでいる」と言える(数十回ぶん)。短くしても
/// 検知が早くなるだけで精度は上がらず、ゲームスレッド外とはいえ無駄に起きるだけ。
pub const WATCHDOG_POLL_INTERVAL_MS: u64 = 250;

/// 「止まった」と判定するまでに必要な、**連続して進んでいない**観測の回数。
///
/// 250ms × 4 = **1秒**進まなければ停止とみなす。⚠️ 1回で判定しないのは、観測の
/// タイミングとコールバックの周期がたまたま噛み合わない・OS が一瞬スケジュールを
/// 落とす、といった正常な揺れで誤検知しないため。
pub const WATCHDOG_STALL_SAMPLES: u32 = 4;

/// 復帰を試みた直後に、次の停止判定を再開するまで空ける観測の回数。
///
/// 250ms × 8 = **2秒**。[`confirm_recovery_progress`] 自身が最大 370ms 待つうえ、
/// `AudioOutputUnitStart` からコールバックが立ち上がるまでにも猶予が要る。
/// 🔴 **クールダウンが無いと、復帰できない状況で `pause()`→`play()` を撃ち続ける**
/// (ログも埋まる)。
pub const WATCHDOG_COOLDOWN_SAMPLES: u32 = 8;

/// 音声コールバックが止まったままになっていないかを見張る、純粋な検知器。
///
/// ## なぜ要るか(実機報告 V14、2026-09-10)
///
/// 実機の診断レポートで「**音源のロードも `mw_se_play` も全部成功しているのに無音**、
/// アプリを再起動すると直る」という状態が確定した(SE 発音16/待ち0/失敗0/委譲0、
/// `outputLatencyNs` は非 0 = コールバックは**起動直後に一度は走っている**)。
/// つまり出力コールバックが途中で止まり、**そのまま誰も気付かない**。
///
/// 既存の復帰経路(割り込み・ルート変化・前面復帰)は、いずれも **OS から通知が来ること**を
/// 前提にしている。通知が来ないまま止まった場合——例えば `CategoryChange`
/// (自己誘発ループを避けるため [`RouteChangeReason::requires_recovery`] が意図的に
/// 除外している)でユニットが止まった場合——**何も再試行しない**。
/// 調査記録 R24 が「アプリを前面に置いたままでは永久に無音のままだったはず」と書いた
/// 穴が、通知の来ないケースとして残っていた。
///
/// ## 何をするか
///
/// [`WATCHDOG_POLL_INTERVAL_MS`] ごとにコールバックカウンタを観測し、
/// [`WATCHDOG_STALL_SAMPLES`] 回連続で進んでいなければ「止まった」と判定する。
/// 判定後は [`WATCHDOG_COOLDOWN_SAMPLES`] 回ぶん判定を止める(撃ち続けない)。
///
/// 🔴 **原因を問わない直し方**である点が肝。「なぜ止まったか」を突き止めて個別に
/// 通知を拾い足すやり方は R12 / R13 で2度やり直しており(`Began` が来ない /
/// `NewDeviceAvailable` でも止まる)、**通知の網羅は原理的に終わらない**。
/// 「進んでいないなら直す」なら、まだ知らない止まり方にも効く。
///
/// ⚠️ 「止まっているのが正常」な状態では見張らない
/// ([`InterruptionState::allows_stall_watchdog`])。
///
/// OS API に一切触れない値型なので、`InterruptionState` / [`confirm_recovery_progress`] と
/// 同じくホスト(macOS/Linux)の単体テストで固定化できる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputStallDetector {
    /// 直近の観測で読んだカウンタ値。
    last_ticks: u64,
    /// 連続して進んでいない観測の回数。
    stalled_samples: u32,
    /// 残りクールダウン観測回数(0 なら判定する)。
    cooldown_samples: u32,
}

impl OutputStallDetector {
    /// 初期状態。`last_ticks` は 0 から始まる——**起動直後に一度もコールバックが
    /// 走らなかった場合もカウンタは 0 のままなので、そのまま停止として検知できる**
    /// (「一度も鳴り始めなかった」も直す対象に含める)。
    pub const fn new() -> Self {
        Self {
            last_ticks: 0,
            stalled_samples: 0,
            cooldown_samples: 0,
        }
    }

    /// 1回ぶんの観測。復帰を試みるべきなら `true` を返す(実際に試みるのは呼び出し側)。
    ///
    /// - `state`: いまの割り込み状態。見張ってよい状態でなければ計測をリセットする
    ///   ——復帰直後に「割り込み中に進まなかったぶん」を持ち越して誤検知しないため。
    /// - `ticks`: `CpalBackend::callback_ticks` の現在値(単調増加)。
    pub fn observe(&mut self, state: InterruptionState, ticks: u64) -> bool {
        if !state.allows_stall_watchdog() {
            self.last_ticks = ticks;
            self.stalled_samples = 0;
            return false;
        }

        if self.cooldown_samples > 0 {
            self.cooldown_samples -= 1;
            self.last_ticks = ticks;
            self.stalled_samples = 0;
            return false;
        }

        if ticks != self.last_ticks {
            self.last_ticks = ticks;
            self.stalled_samples = 0;
            return false;
        }

        self.stalled_samples += 1;
        if self.stalled_samples < WATCHDOG_STALL_SAMPLES {
            return false;
        }

        self.stalled_samples = 0;
        self.cooldown_samples = WATCHDOG_COOLDOWN_SAMPLES;
        true
    }

    /// 停止と判定するまでに実際に経過する時間(ms)。ログ用。
    pub const fn stall_threshold_ms() -> u64 {
        WATCHDOG_POLL_INTERVAL_MS * WATCHDOG_STALL_SAMPLES as u64
    }
}

impl Default for OutputStallDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// `AVAudioSessionInterruptionNotification` / `UIApplicationDidBecomeActiveNotification` /
/// `UIApplicationDidEnterBackgroundNotification` / `AVAudioSessionRouteChangeNotification` の
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
    /// `callback_ticks` は `CpalBackend` が持つ「音声コールバックが呼ばれた回数」の
    /// 単調増加カウンタ(`Arc` 共有)——`pause()`→`play()` が実際にコールバックを
    /// 前進させたかを実測するために使う(調査記録「`pause()`→`play()` が
    /// Ok を返しても無音のままだったケース」参照)。
    ///
    /// `CpalBackend::open` から、ストリームを `play()` した直後に呼ぶこと
    /// (`cpal_backend.rs` 参照)。
    #[cfg(any(target_os = "ios", target_os = "tvos"))]
    pub fn new(
        stream: Arc<cpal::Stream>,
        events: Arc<EventQueue>,
        callback_ticks: Arc<AtomicU64>,
    ) -> Self {
        Self {
            inner: imp::Watcher::new(stream, events, callback_ticks),
        }
    }

    /// iOS / tvOS 以外では何もしない。
    #[cfg(not(any(target_os = "ios", target_os = "tvos")))]
    pub fn new(
        _stream: Arc<cpal::Stream>,
        _events: Arc<EventQueue>,
        _callback_ticks: Arc<AtomicU64>,
    ) -> Self {
        Self {}
    }
}

#[cfg(any(target_os = "ios", target_os = "tvos"))]
mod imp {
    use std::panic::{self, AssertUnwindSafe};
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use block2::RcBlock;
    use cpal::traits::StreamTrait;
    use mw_core::{Event, EventQueue};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
    use objc2_avf_audio::{
        AVAudioSessionInterruptionNotification, AVAudioSessionInterruptionOptionKey,
        AVAudioSessionInterruptionOptions, AVAudioSessionInterruptionType,
        AVAudioSessionInterruptionTypeKey, AVAudioSessionRouteChangeNotification,
        AVAudioSessionRouteChangeReason, AVAudioSessionRouteChangeReasonKey,
    };
    use objc2_foundation::{NSNotification, NSNotificationCenter, NSNumber, NSString, ns_string};

    use super::{
        InterruptionState, OutputStallDetector, RECOVERY_WAIT_SCHEDULE_MS, RouteChangeReason,
        WATCHDOG_POLL_INTERVAL_MS, confirm_recovery_progress,
    };

    /// ウォッチドッグの待機を刻む単位(ms)。[`WATCHDOG_POLL_INTERVAL_MS`] をこの粒度で
    /// 分割して眠り、そのたびに停止要求を確認する —— `Drop`(= `CpalBackend::close`)が
    /// 最大でもこの時間しか待たされないようにするため。
    const WATCHDOG_SHUTDOWN_SLICE_MS: u64 = 25;

    pub(super) struct Watcher {
        observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
        /// ウォッチドッグへの停止要求([`Watcher::drop`] で立てる)。
        watchdog_shutdown: Arc<AtomicBool>,
        /// 出力停止ウォッチドッグのスレッド。spawn に失敗した場合は `None`
        /// (ウォッチドッグが無いだけで、既存の通知経由の復帰はそのまま動く)。
        watchdog: Option<thread::JoinHandle<()>>,
    }

    // SAFETY: NSNotificationCenter はスレッドセーフ。ここへ保持する observer トークンは
    // Drop で `removeObserver` するためだけに使う不透明なハンドルで、他のスレッドから
    // 読み書きする内部状態を持たない(cpal 自身の `session_event_manager.rs` の
    // `SessionEventManager` と同じ理由づけ)。
    unsafe impl Send for Watcher {}
    unsafe impl Sync for Watcher {}

    impl Watcher {
        pub(super) fn new(
            stream: Arc<cpal::Stream>,
            events: Arc<EventQueue>,
            callback_ticks: Arc<AtomicU64>,
        ) -> Self {
            let nc = NSNotificationCenter::defaultCenter();
            let state = Arc::new(Mutex::new(InterruptionState::new()));
            let mut observers = Vec::new();

            {
                let state = Arc::clone(&state);
                let stream = Arc::clone(&stream);
                let events = Arc::clone(&events);
                let callback_ticks = Arc::clone(&callback_ticks);
                let block = RcBlock::new(move |notif: NonNull<NSNotification>| {
                    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                        // SAFETY: OS が有効な NSNotification を渡してくる(この block の
                        // 契約)。
                        let notif = unsafe { notif.as_ref() };
                        handle_interruption_notification(
                            notif,
                            &state,
                            &stream,
                            &events,
                            &callback_ticks,
                        );
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
                let block = RcBlock::new(move |_: NonNull<NSNotification>| {
                    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                        handle_entered_background(&state);
                    }));
                    if outcome.is_err() {
                        crate::mw_log!(
                            "[mw-backend] ios_interruption: panic while handling \
                             UIApplicationDidEnterBackgroundNotification (caught at the boundary)"
                        );
                    }
                });
                // UIKit 側の通知名。`objc2-ui-kit` を新規依存に追加せず、文字列リテラルから
                // 直接 NSString を作る(`UIApplicationDidBecomeActiveNotification` と同じ
                // 理由づけ、下記参照)。調査記録「`DidBecomeActive` 安全網の前提が
                // 崩れていたケース」参照——Began が一度も飛んでこないままバックグラウンドへ
                // 行った場合の安全網として、この通知を新たな判別子に使う。
                let name = ns_string!("UIApplicationDidEnterBackgroundNotification");
                // SAFETY: 上記と同様、`addObserverForName_object_queue_usingBlock` の契約に
                // 従う。
                let observer = unsafe {
                    nc.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
                };
                observers.push(observer);
            }

            {
                let state = Arc::clone(&state);
                let stream = Arc::clone(&stream);
                let events = Arc::clone(&events);
                let callback_ticks = Arc::clone(&callback_ticks);
                let block = RcBlock::new(move |_: NonNull<NSNotification>| {
                    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                        handle_became_active(&state, &stream, &events, &callback_ticks);
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

            {
                let state = Arc::clone(&state);
                let stream = Arc::clone(&stream);
                let events = Arc::clone(&events);
                let callback_ticks = Arc::clone(&callback_ticks);
                let block = RcBlock::new(move |notif: NonNull<NSNotification>| {
                    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                        // SAFETY: OS が有効な NSNotification を渡してくる(この block の契約)。
                        let notif = unsafe { notif.as_ref() };
                        handle_route_change_notification(
                            notif,
                            &state,
                            &stream,
                            &events,
                            &callback_ticks,
                        );
                    }));
                    if outcome.is_err() {
                        crate::mw_log!(
                            "[mw-backend] ios_interruption: panic while handling \
                             AVAudioSessionRouteChangeNotification (caught at the boundary)"
                        );
                    }
                });
                // SAFETY: `AVAudioSessionRouteChangeNotification` はプロセス生存中変化しない
                // 静的な通知名。cpal 自身も同じ通知を独立した observer で監視しているが
                // (`session_event_manager.rs`)、NSNotificationCenter は同一通知に対する
                // 複数 observer を問題なく許容する(調査記録「ルート変化」で
                // 確認済み——cpal 側は `error_callback` を呼ぶだけで `AudioUnit`/`playing`
                // フラグには一切触れないため、二重処理にはならない)。
                if let Some(name) = unsafe { AVAudioSessionRouteChangeNotification } {
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
                        "[mw-backend] ios_interruption: AVAudioSessionRouteChangeNotification \
                         is unavailable"
                    );
                }
            }

            // 出力停止ウォッチドッグ(実機報告 V14。`OutputStallDetector` のクラス doc)。
            // observer の登録がすべて済んだ後に起こす —— 先に起こすと、まだ observer が
            // 揃っていない状態で復帰を試みることになりうる。
            let watchdog_shutdown = Arc::new(AtomicBool::new(false));
            let watchdog = spawn_output_stall_watchdog(
                Arc::clone(&state),
                Arc::clone(&stream),
                Arc::clone(&events),
                Arc::clone(&callback_ticks),
                Arc::clone(&watchdog_shutdown),
            );

            Self {
                observers,
                watchdog_shutdown,
                watchdog,
            }
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            // 先にウォッチドッグを止める —— ストリームが閉じられた後に
            // `pause()`→`play()` を撃たせないため。待たされるのは最大でも
            // `WATCHDOG_SHUTDOWN_SLICE_MS`(25ms)。
            self.watchdog_shutdown.store(true, Ordering::Relaxed);
            if let Some(handle) = self.watchdog.take() {
                let _ = handle.join();
            }

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
        state: &Arc<Mutex<InterruptionState>>,
        stream: &Arc<cpal::Stream>,
        events: &Arc<EventQueue>,
        callback_ticks: &Arc<AtomicU64>,
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
                attempt_recovery(
                    state,
                    stream,
                    events,
                    callback_ticks,
                    RecoveryTrigger::InterruptionEnded,
                );
            } else {
                events.push_side_channel(Event::AudioInterruptionEnded { recovered: false });
            }
        }
    }

    /// アプリが実際にバックグラウンドへ入った(`UIApplicationDidEnterBackgroundNotification`)。
    /// ここでは状態を `Backgrounded` へ倒すだけで、復帰は試みない(復帰はあくまで前面復帰
    /// (`handle_became_active`)またはルート変化・割り込み終了の契機で行う)。
    ///
    /// `previous_state` をログへ出す。実機でこの行が1行も出なければ
    /// `DidEnterBackground` 自体が届いていないことが分かる(調査記録「
    /// `DidBecomeActive` 安全網の前提が崩れていたケース」の観測性節参照)。
    fn handle_entered_background(state: &Mutex<InterruptionState>) {
        let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
        let previous_state = *guard;
        *guard = guard.on_app_entered_background();
        drop(guard);
        crate::mw_log!(
            "[mw-backend] app entered background (previous_state={previous_state:?}); will \
             attempt recovery on next activation"
        );

        // 🔴 <b>出力を手放す</b>(ユーザー決定 2026-09-09。D6「バックグラウンド時に解放して」)。
        //    カテゴリ Playback は他アプリの音を止めるので、非アクティブ化して
        //    「もう鳴らさない」ことを OS へ伝えないと、<b>止めた相手の音楽が再開しない</b>
        //    (`ios_session::deactivate` の doc が理由の正)。
        //
        // ⚠️ <b>状態は `Backgrounded` のまま</b>にする —— 前面復帰時の復帰処理
        //    (`handle_became_active` → `attempt_recovery`)が `ios_session::configure()` を
        //    呼び、そこで `setActive(true)` に戻る。ここで状態まで触ると復帰が走らなくなる。
        //
        // 📌 ロックを外してから呼ぶ(Obj-C 呼び出しをロック内へ持ち込まない)。
        crate::ios_session::deactivate();
    }

    /// アプリがアクティブになった(`UIApplicationDidBecomeActiveNotification`)。
    ///
    /// 復帰を試みる場合、遷移前の状態(`previous_state`)をログへ出す——
    /// `previous_state=Backgrounded` なら「Began が一度も来ないままバックグラウンドへ
    /// 行った」今回追加した経路、`Interrupted`/`RecoveryFailed` なら従来の経路
    /// (Began は届いていた)だったと実機ログから判別できる(調査記録「
    /// `DidBecomeActive` 安全網の前提が崩れていたケース」の観測性節参照)。
    fn handle_became_active(
        state: &Arc<Mutex<InterruptionState>>,
        stream: &Arc<cpal::Stream>,
        events: &Arc<EventQueue>,
        callback_ticks: &Arc<AtomicU64>,
    ) {
        let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
        let previous_state = *guard;
        *guard = guard.on_app_became_active();
        let attempt = guard.needs_recovery_attempt();
        drop(guard);
        if attempt {
            crate::mw_log!(
                "[mw-backend] app became active while unresolved (previous_state=\
                 {previous_state:?}); attempting recovery"
            );
            attempt_recovery(
                state,
                stream,
                events,
                callback_ticks,
                RecoveryTrigger::AppBecameActive { previous_state },
            );
        }
    }

    fn handle_route_change_notification(
        notif: &NSNotification,
        state: &Arc<Mutex<InterruptionState>>,
        stream: &Arc<cpal::Stream>,
        events: &Arc<EventQueue>,
        callback_ticks: &Arc<AtomicU64>,
    ) {
        // 通知を受け取った事実そのものを reason の解釈より前にログする。以前は
        // `route_change_reason` が `None` を返すとログより先に `return` していたため、
        // userInfo の取得やキャストに失敗すると何も記録せずに黙って捨てていた
        // (このプロジェクトが何度も踏んできた罠。調査記録「Bluetooth
        // 再接続で無音になるケース」参照)。
        crate::mw_log!("[mw-backend] AVAudioSession route change notification received");
        let Some(reason) = route_change_reason(notif) else {
            crate::mw_log!(
                "[mw-backend] ios_interruption: failed to interpret route change reason from \
                 notification userInfo (missing userInfo/key, or unexpected value type)"
            );
            return;
        };
        crate::mw_log!("[mw-backend] AVAudioSession route changed (reason={reason:?})");

        // `Event::RouteChanged` は reason を問わず毎回発火する(調査記録「
        // ルート変化」参照。テンプレート側のオフセット再較正用で、復帰要否とは別軸)。
        events.push_side_channel(Event::RouteChanged);

        let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
        *guard = guard.on_route_changed(reason);
        let attempt = guard.needs_recovery_attempt();
        drop(guard);
        if attempt {
            attempt_recovery(
                state,
                stream,
                events,
                callback_ticks,
                RecoveryTrigger::RouteChange { reason },
            );
        }
    }

    fn classify_route_change_reason(raw: AVAudioSessionRouteChangeReason) -> RouteChangeReason {
        match raw {
            AVAudioSessionRouteChangeReason::OldDeviceUnavailable => {
                RouteChangeReason::OldDeviceUnavailable
            }
            AVAudioSessionRouteChangeReason::NewDeviceAvailable => {
                RouteChangeReason::NewDeviceAvailable
            }
            AVAudioSessionRouteChangeReason::CategoryChange => RouteChangeReason::CategoryChange,
            AVAudioSessionRouteChangeReason::Override => RouteChangeReason::Override,
            _ => RouteChangeReason::Other,
        }
    }

    fn route_change_reason(notif: &NSNotification) -> Option<RouteChangeReason> {
        let user_info = notif.userInfo()?;
        let key = unsafe { AVAudioSessionRouteChangeReasonKey }?;
        // SAFETY: cpal 自身の `session_event_manager.rs::route_change_error` と同じ
        // reinterpret(`cast_unchecked`)。
        let dict = unsafe { user_info.cast_unchecked::<NSString, AnyObject>() };
        let value = dict.objectForKey(key)?;
        let number = value.downcast_ref::<NSNumber>()?;
        Some(classify_route_change_reason(
            AVAudioSessionRouteChangeReason(number.unsignedIntegerValue()),
        ))
    }

    /// 復帰を試みた起点(観測性のためだけの値。`InterruptionState` の遷移ロジックには
    /// 一切影響しない)。`attempt_recovery` のログに出し、実機でどの通知経由の復帰
    /// だったかを判別できるようにする(調査記録「`DidBecomeActive` 安全網の
    /// 前提が崩れていたケース」の観測性節参照)。
    #[derive(Debug, Clone, Copy)]
    enum RecoveryTrigger {
        /// `AVAudioSessionInterruptionNotification` の Ended(`should_resume` あり)。
        InterruptionEnded,
        /// `UIApplicationDidBecomeActiveNotification`。`previous_state` が
        /// `Backgrounded` なら今回追加した経路(Began 未到達)、`Interrupted`/
        /// `RecoveryFailed` なら従来の経路(Began は届いていた)。
        //
        // フィールドは `{trigger:?}`(導出 `Debug`)経由でしかログに出さない。rustc の
        // dead_code 解析は導出 impl 経由の読み出しを「使用」に数えないため
        // `#[allow(dead_code)]` が要る(`Watcher::inner` フィールドと同じ理由づけ)。
        #[allow(dead_code)]
        AppBecameActive { previous_state: InterruptionState },
        /// `AVAudioSessionRouteChangeNotification`(`reason` は
        /// `RouteChangeReason::OldDeviceUnavailable` または `NewDeviceAvailable` のはず
        /// ——他の reason は `needs_recovery_attempt()` が `false` になるため、そもそも
        /// ここへ来ない)。
        #[allow(dead_code)]
        RouteChange { reason: RouteChangeReason },
        /// 🆕 出力停止ウォッチドッグ(実機報告 V14、2026-09-10)。**OS からの通知は何も
        /// 来ていない** —— 音声コールバックが進まなくなったことを実測だけで検知した経路。
        /// `stalled_ms` は「進んでいない」と判定するまでに実際に経過した時間。
        #[allow(dead_code)]
        OutputStalled { stalled_ms: u64 },
    }

    /// 出力停止ウォッチドッグのスレッドを起こす(実機報告 V14。判定そのものは
    /// [`OutputStallDetector`]、なぜ要るかもそちらのクラス doc が正)。
    ///
    /// ⚠️ **ゲームスレッドでも音声スレッドでもない専用スレッド**で回す。
    /// [`WATCHDOG_POLL_INTERVAL_MS`] ごとにアトミックを2つ読むだけなので負荷は無視できる
    /// (§5.3 の対象外なのは `attempt_recovery` のワーカースレッドと同じ理由)。
    ///
    /// spawn に失敗しても致命傷にしない —— ウォッチドッグが無いだけで、通知経由の
    /// 既存の復帰経路はそのまま動く(`ios_session::configure` と同じ「失敗は記録して続行」)。
    fn spawn_output_stall_watchdog(
        state: Arc<Mutex<InterruptionState>>,
        stream: Arc<cpal::Stream>,
        events: Arc<EventQueue>,
        callback_ticks: Arc<AtomicU64>,
        shutdown: Arc<AtomicBool>,
    ) -> Option<thread::JoinHandle<()>> {
        let spawned = thread::Builder::new()
            .name("mw-ios-output-stall-watchdog".to_owned())
            .spawn(move || {
                let mut detector = OutputStallDetector::new();
                let threshold_ms = OutputStallDetector::stall_threshold_ms();

                while sleep_unless_shutdown(&shutdown, WATCHDOG_POLL_INTERVAL_MS) {
                    let current = *state.lock().unwrap_or_else(|p| p.into_inner());
                    let ticks = callback_ticks.load(Ordering::Relaxed);

                    if !detector.observe(current, ticks) {
                        continue;
                    }

                    crate::mw_log!(
                        "[mw-backend] ios_interruption: output stall detected \
                         (callback_ticks={ticks} unchanged for {threshold_ms}ms, \
                         state={current:?}); attempting recovery"
                    );

                    // 通知ハンドラと二重に走らないよう、先に「これから復帰する」状態にする
                    // (他の経路が `needs_recovery_attempt()` で自分の番だと誤認しないため)。
                    {
                        let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
                        *guard = InterruptionState::RecoveryPending;
                    }

                    attempt_recovery(
                        &state,
                        &stream,
                        &events,
                        &callback_ticks,
                        RecoveryTrigger::OutputStalled {
                            stalled_ms: threshold_ms,
                        },
                    );
                }
            });

        match spawned {
            Ok(handle) => Some(handle),
            Err(err) => {
                crate::mw_log!(
                    "[mw-backend] ios_interruption: failed to spawn the output stall watchdog \
                     ({err}); falling back to notification-driven recovery only"
                );
                None
            }
        }
    }

    /// `total_ms` だけ眠る。ただし [`WATCHDOG_SHUTDOWN_SLICE_MS`] ごとに停止要求を確認し、
    /// 立っていたら即座に `false` を返して切り上げる(`Drop` を待たせないため)。
    ///
    /// 戻り値: 眠り切って**まだ続けてよい**なら `true`、停止要求が来ていたら `false`。
    fn sleep_unless_shutdown(shutdown: &AtomicBool, total_ms: u64) -> bool {
        let mut remaining = total_ms;
        while remaining > 0 {
            if shutdown.load(Ordering::Relaxed) {
                return false;
            }
            let slice = remaining.min(WATCHDOG_SHUTDOWN_SLICE_MS);
            thread::sleep(Duration::from_millis(slice));
            remaining -= slice;
        }
        !shutdown.load(Ordering::Relaxed)
    }

    /// セッション再アクティブ化 + ストリーム再始動を試み、**音声コールバックが実際に
    /// 前進したか**を実測してから成否を判定する。
    ///
    /// `stream.play()` だけでは復帰しない(モジュール doc の「実機バグの原因」参照:
    /// cpal 0.18.1 の内部 `playing` フラグが OS 主導の停止に追随しないため、まず
    /// `pause()` でフラグを倒してから `play()` を呼ぶ)。
    ///
    /// 🔴 **`stream.play()` が `Ok` を返しても実際には無音のままのケースが実機で
    /// 確認された**(調査記録「`pause()`→`play()` が Ok を返しても無音の
    /// ままだったケース」参照)。`Result::Ok` は「`AudioOutputUnitStart` の呼び出し
    /// 自体がエラーを返さなかった」ことしか意味せず、コールバックが実際に再開したかは
    /// 何も保証しない。そのためこの関数は `play()` の戻り値を成否判定に使わない——
    /// `CpalBackend` が持つ単調増加カウンタ `callback_ticks`(このカウンタの意味は
    /// `cpal_backend.rs::CpalBackend::callback_ticks` のdoc参照)を `pause()`→`play()`
    /// の前後で比較し、進んでいなければ [`confirm_recovery_progress`] が
    /// `pause()`→`play()` を呼び直しながら段階的に(`RECOVERY_WAIT_SCHEDULE_MS`)
    /// 再確認する。
    ///
    /// **待機は `std::thread::sleep`(ブロッキング)。** ただし P0-6(2026-09-03 是正)の
    /// 対応として、待機を伴う確認・再試行(`confirm_recovery_progress` とその結果に基づく
    /// 状態更新・イベント発火)は**この関数自身のスレッドでは実行せず、専用のワーカー
    /// スレッドへ丸ごと委譲する**(下記実装参照)。この関数(＝通知ハンドラから同期的に
    /// 呼ばれる部分)が実際に行うのは「1回目の `pause()`→`play()`」までであり、これは
    /// ブロッキング待機を含まない高速な CoreAudio 呼び出しのみなので、呼び出し元の
    /// スレッド(`UIApplicationDidBecomeActiveNotification` の場合は**メインスレッド**
    /// ——`queue: None` で登録した observer は通知を post したスレッド上で同期実行される
    /// 契約であり、UIKit の通知は必ずメインスレッドから post されるため)を実質的に
    /// ブロックしない。
    ///
    /// 🔴 **是正前はここに `confirm_recovery_progress` の呼び出し(=最大370msの
    /// `std::thread::sleep`)が同期的に含まれており、`UIApplicationDidBecomeActiveNotification`
    /// 経由(=メインスレッド)で呼ばれた場合に限り、メインスレッドを最大370ms
    /// ブロックしていた**(調査記録「`pause()`→`play()` が Ok を返しても無音のままだった
    /// ケース」の「待機は `std::thread::sleep` を使う」パラグラフ参照。P0-6, 2026-09-03 是正)。ワーカースレッドへの委譲により
    /// この問題は解消した——ただし実機での体感(ヒッチが本当に消えたか)は自動テストでは
    /// 確認できない(本ファイル末尾「自動テストで守れる範囲・守れない範囲」参照)。
    ///
    /// `trigger`・`stream.play()` の成否・実測による最終判定は必ずログへ出す——
    /// 「`play()` は成功したが実測では確認できなかった」ケースをログだけで判別できる
    /// ようにする(調査記録「`DidBecomeActive` 安全網の前提が崩れていた
    /// ケース」の観測性節、および R24 節の両方を踏襲)。ワーカースレッドへ移した後も
    /// この観測性は変わらない(ログを出す場所が別スレッドになるだけ)。
    fn attempt_recovery(
        state: &Arc<Mutex<InterruptionState>>,
        stream: &Arc<cpal::Stream>,
        events: &Arc<EventQueue>,
        callback_ticks: &Arc<AtomicU64>,
        trigger: RecoveryTrigger,
    ) {
        crate::mw_log!("[mw-backend] ios_interruption: attempting recovery (trigger={trigger:?})");

        // 1回目の pause()→play() の前に読んでおく。この値と比較してカウンタが
        // 進んだかどうかで「本当に鳴り出したか」を判定する(`play()` の戻り値だけでは
        // 分からない——上のdoc参照)。
        let ticks_before = callback_ticks.load(Ordering::Relaxed);

        crate::ios_session::configure();
        if let Err(err) = stream.pause() {
            crate::mw_log!("[mw-backend] ios_interruption: stream.pause() returned Err: {err}");
        }
        match stream.play() {
            Ok(()) => crate::mw_log!(
                "[mw-backend] ios_interruption: stream.play() returned Ok (pause+play, \
                 trigger={trigger:?}); confirming via callback progress"
            ),
            Err(err) => crate::mw_log!(
                "[mw-backend] ios_interruption: stream.play() returned Err: {err} \
                 (trigger={trigger:?}); still confirming via callback progress in case a \
                 retry recovers"
            ),
        }

        // ここから先(待機を伴う確認・再試行・最終的な状態更新とイベント発火)は
        // 専用のワーカースレッドへ委譲する(P0-6)。呼び出し元(通知ハンドラ)のスレッドは
        // ここで即座に返る——`UIApplicationDidBecomeActiveNotification` 経由の場合、
        // それはメインスレッドを指す。
        //
        // `state`/`stream`/`events`/`callback_ticks` はいずれも `Arc` なので `clone` は
        // 参照カウント操作のみ(このスレッド生成自体は §5.3 の対象外の非リアルタイム
        // スレッドから行っているため、ここでのアロケーションは問題にならない)。
        // 元の `state`/`events`(引数の `&Arc<...>`)は spawn 失敗時のフォールバックで
        // 使うため、ワーカースレッドへ渡す分は別名で clone する(シャドーイングして
        // move してしまうとフォールバック側で参照できなくなるため)。
        let worker_state = Arc::clone(state);
        let worker_stream = Arc::clone(stream);
        let worker_events = Arc::clone(events);
        let worker_callback_ticks = Arc::clone(callback_ticks);
        let spawned = thread::Builder::new()
            .name("mw-ios-interruption-recovery".to_owned())
            .spawn(move || {
                let outcome = confirm_recovery_progress(
                    ticks_before,
                    || worker_callback_ticks.load(Ordering::Relaxed),
                    || {
                        // 空振り: pause()→play() を呼び直す。既に停止/再生中のユニットへ
                        // 呼んでも安全というのが CoreAudio の一般的な契約(モジュール doc
                        // 「実機バグの原因」参照)なので、戻り値は無視してよい——最終的な
                        // 成否は `confirm_recovery_progress` の実測で決まる。
                        let _ = worker_stream.pause();
                        let _ = worker_stream.play();
                    },
                    |wait_ms| std::thread::sleep(Duration::from_millis(wait_ms)),
                );

                let success = match outcome {
                    Some((attempt, waited_ms)) => {
                        crate::mw_log!(
                            "[mw-backend] ios_interruption: stream restart confirmed by callback \
                             progress (attempt={attempt}, waited={waited_ms}ms, \
                             trigger={trigger:?})"
                        );
                        true
                    }
                    None => {
                        let total_ms: u64 = RECOVERY_WAIT_SCHEDULE_MS.iter().sum();
                        crate::mw_log!(
                            "[mw-backend] ios_interruption: 🔴 stream restart NOT confirmed — \
                             callback did not advance after {total_ms}ms across {attempts} \
                             attempts (trigger={trigger:?}); the cpal stream likely needs to be \
                             rebuilt (M3 design proper)",
                            attempts = RECOVERY_WAIT_SCHEDULE_MS.len(),
                        );
                        false
                    }
                };

                let mut guard = worker_state.lock().unwrap_or_else(|p| p.into_inner());
                *guard = guard.on_recovery_attempted(success);
                drop(guard);
                worker_events
                    .push_side_channel(Event::AudioInterruptionEnded { recovered: success });
            });
        if let Err(err) = spawned {
            // スレッド生成自体が失敗した(OS リソース枯渇等、極めて稀)。復帰確認を
            // 諦めるしかないが、パニックはしない——次の `DidBecomeActive`/ルート変化・
            // 割り込みが来れば `needs_recovery_attempt()` 経由で再試行されるため
            // (`InterruptionState::RecoveryFailed` と同じ扱いにする)。
            crate::mw_log!(
                "[mw-backend] ios_interruption: failed to spawn recovery worker thread: {err} \
                 (trigger={trigger:?}); giving up on this attempt, a later notification will \
                 retry"
            );
            let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
            *guard = guard.on_recovery_attempted(false);
            drop(guard);
            events.push_side_channel(Event::AudioInterruptionEnded { recovered: false });
        }
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
    use super::{
        InterruptionState, OutputStallDetector, RECOVERY_WAIT_SCHEDULE_MS, RouteChangeReason,
        WATCHDOG_COOLDOWN_SAMPLES, WATCHDOG_STALL_SAMPLES, confirm_recovery_progress,
    };

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

    /// 依頼書が明示した最小シナリオそのもの:実機バグ R12「iOS でホームに戻って復帰すると
    /// 音が出ない」の原因——`AVAudioSessionInterruptionNotification` の Began が
    /// (何らかの理由で)一度も飛んでこないままバックグラウンドへ行くと、`Running` のまま
    /// 変わらず、`on_app_became_active` の当初のガード(`Interrupted`/`RecoveryFailed`
    /// のみ復帰)には引っかからなかった。`UIApplicationDidEnterBackgroundNotification` を
    /// 判別子に足したことで、Began の到達に一切依存せず復帰を要求できることを固定化する
    /// (調査記録「`DidBecomeActive` 安全網の前提が崩れていたケース」参照)。
    #[test]
    fn app_became_active_recovers_when_no_interruption_began_before_backgrounding() {
        let state = InterruptionState::new();
        assert_eq!(state, InterruptionState::Running);

        // Began は一度も来ない。バックグラウンドへ行った事実だけを状態として持つ。
        let state = state.on_app_entered_background();
        assert_eq!(state, InterruptionState::Backgrounded);

        let state = state.on_app_became_active();
        assert_eq!(state, InterruptionState::RecoveryPending);
        assert!(state.needs_recovery_attempt());
    }

    /// 割り込み中(電話中等)にホームへ戻ってバックグラウンドへ行くのも普通にある。
    /// `Backgrounded` で上書きされても、前面復帰時に復帰要求へ到達することを固定化する。
    #[test]
    fn app_entered_background_while_interrupted_still_recovers_on_becoming_active() {
        let state = InterruptionState::new()
            .on_interruption_began()
            .on_app_entered_background();
        assert_eq!(state, InterruptionState::Backgrounded);

        let state = state.on_app_became_active();
        assert_eq!(state, InterruptionState::RecoveryPending);
    }

    /// R12 の経路(バックグラウンド経由)で復帰に失敗しても、従来の割り込み経由
    /// ([`recovery_failure_can_be_retried_when_the_app_becomes_active_again`])と同じく
    /// 次のアクティブ化で再試行できることを固定化する。
    #[test]
    fn recovery_failure_after_backgrounding_can_be_retried_on_becoming_active_again() {
        let state = InterruptionState::new()
            .on_app_entered_background()
            .on_app_became_active();
        assert_eq!(state, InterruptionState::RecoveryPending);

        let state = state.on_recovery_attempted(false);
        assert_eq!(state, InterruptionState::RecoveryFailed);

        let state = state.on_app_became_active();
        assert_eq!(state, InterruptionState::RecoveryPending);
        assert!(state.needs_recovery_attempt());
    }

    #[test]
    fn app_became_active_is_a_no_op_when_nothing_was_interrupted() {
        // Control Center・通知バナー等、実際には中断していないアクティブ化で
        // 毎回ストリームを作り直さないことを固定化する(モジュール doc「実装方針」参照)。
        // `Backgrounded` はここに含めない——それはまさに「実際にバックグラウンドへ
        // 行った」ケースで、復帰を試みるのが正しい(上記の R12 回帰テスト群を参照)。
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

    /// 上と同じ「最新の事実を優先」設計が `Backgrounded` からでも成り立つことを固定化する
    /// (`a_new_interruption_always_wins_even_mid_recovery` を壊さず、`Backgrounded` 分だけ
    /// 別テストとして足す)。
    #[test]
    fn a_new_interruption_always_wins_even_while_backgrounded() {
        assert_eq!(
            InterruptionState::Backgrounded.on_interruption_began(),
            InterruptionState::Interrupted
        );
    }

    /// `on_app_entered_background` はどの状態からでも `Backgrounded` へ遷移する
    /// (`on_interruption_began` の「最新の事実を優先」と同じ設計、モジュール doc
    /// 「`DidBecomeActive` 安全網の前提が崩れていたケース」参照)。
    #[test]
    fn entering_background_always_wins_from_any_state() {
        for state in [
            InterruptionState::Running,
            InterruptionState::Interrupted,
            InterruptionState::RecoveryPending,
            InterruptionState::Recovered,
            InterruptionState::RecoveryFailed,
            InterruptionState::Backgrounded,
        ] {
            assert_eq!(
                state.on_app_entered_background(),
                InterruptionState::Backgrounded
            );
        }
    }

    #[test]
    fn default_state_is_running() {
        assert_eq!(InterruptionState::default(), InterruptionState::Running);
    }

    /// 実機報告その2の最小シナリオ:「Bluetooth を切断すると SE が鳴らなくなる」——
    /// `OldDeviceUnavailable` はどの状態からでも復帰要求(`RecoveryPending`)へ遷移する。
    #[test]
    fn route_changed_with_old_device_unavailable_requests_recovery_from_any_state() {
        for state in [
            InterruptionState::Running,
            InterruptionState::Interrupted,
            InterruptionState::RecoveryPending,
            InterruptionState::Recovered,
            InterruptionState::RecoveryFailed,
            InterruptionState::Backgrounded,
        ] {
            assert_eq!(
                state.on_route_changed(RouteChangeReason::OldDeviceUnavailable),
                InterruptionState::RecoveryPending
            );
        }
    }

    /// 依頼書の警告どおり:正常なルート切替(カテゴリ変更・オーバーライド等)では状態を
    /// 一切変えない——毎回 `pause()`→`play()` すると通常プレイ中に不要な音切れを生むため。
    /// `NewDeviceAvailable`(BT 接続・イヤホン挿し込み)はここに含めない——実機報告 R13で
    /// 復帰が要ることが判明したため
    /// ([`route_changed_with_new_device_available_requests_recovery_from_any_state`] 参照)。
    #[test]
    fn route_changed_with_benign_reasons_does_not_change_state() {
        for reason in [
            RouteChangeReason::CategoryChange,
            RouteChangeReason::Override,
            RouteChangeReason::Other,
        ] {
            for state in [
                InterruptionState::Running,
                InterruptionState::Interrupted,
                InterruptionState::RecoveryPending,
                InterruptionState::Recovered,
                InterruptionState::RecoveryFailed,
                InterruptionState::Backgrounded,
            ] {
                assert_eq!(
                    state.on_route_changed(reason),
                    state,
                    "reason {reason:?} must not change state {state:?}"
                );
            }
        }
    }

    /// 旧名 `only_old_device_unavailable_requires_recovery`。実機報告 R13(Bluetooth
    /// 再接続で無音になる)を受けて `NewDeviceAvailable` も復帰対象に加わったため、
    /// 「old device unavailable だけ」という名前のままでは実態と食い違う。名前を
    /// 実態に合わせて変更した(調査記録「Bluetooth 再接続で無音になる
    /// ケース」参照)。
    #[test]
    fn old_device_unavailable_and_new_device_available_require_recovery_other_reasons_do_not() {
        assert!(RouteChangeReason::OldDeviceUnavailable.requires_recovery());
        assert!(RouteChangeReason::NewDeviceAvailable.requires_recovery());
        assert!(!RouteChangeReason::CategoryChange.requires_recovery());
        assert!(!RouteChangeReason::Override.requires_recovery());
        assert!(!RouteChangeReason::Other.requires_recovery());
    }

    /// 実機報告 R13 の最小シナリオ:「Bluetooth を再接続すると SE が鳴らなくなる」——
    /// `NewDeviceAvailable` はどの状態からでも復帰要求(`RecoveryPending`)へ遷移する
    /// (`route_changed_with_old_device_unavailable_requests_recovery_from_any_state` と
    /// 同じ形。調査記録「Bluetooth 再接続で無音になるケース」参照)。
    #[test]
    fn route_changed_with_new_device_available_requests_recovery_from_any_state() {
        for state in [
            InterruptionState::Running,
            InterruptionState::Interrupted,
            InterruptionState::RecoveryPending,
            InterruptionState::Recovered,
            InterruptionState::RecoveryFailed,
            InterruptionState::Backgrounded,
        ] {
            assert_eq!(
                state.on_route_changed(RouteChangeReason::NewDeviceAvailable),
                InterruptionState::RecoveryPending
            );
        }
    }

    /// `CategoryChange` は引き続き復帰を要求しない——`attempt_recovery` が呼ぶ
    /// `ios_session::configure()` 自身が `setCategory_error` を呼ぶため、ここで復帰に
    /// 反応すると自分自身のカテゴリ再設定をトリガーに拾う自己誘発ループになりうる
    /// (調査記録「ルート変化」および「Bluetooth 再接続で無音になる
    /// ケース」参照)。`NewDeviceAvailable` を復帰対象に追加した際もここは意図的に
    /// 据え置いた設計判断であることを、独立したテストとして固定化する。
    #[test]
    fn category_change_does_not_require_recovery_to_avoid_self_triggered_loop() {
        assert!(!RouteChangeReason::CategoryChange.requires_recovery());
        for state in [
            InterruptionState::Running,
            InterruptionState::Interrupted,
            InterruptionState::RecoveryPending,
            InterruptionState::Recovered,
            InterruptionState::RecoveryFailed,
            InterruptionState::Backgrounded,
        ] {
            assert_eq!(
                state.on_route_changed(RouteChangeReason::CategoryChange),
                state
            );
        }
    }

    /// ルート変化による復帰要求と、割り込みによる復帰要求は同じ `RecoveryPending` へ
    /// 合流する——復帰失敗後の再試行(`on_app_became_active`)がどちらの経路から来た
    /// 場合でも同じように働くことを固定化する。
    #[test]
    fn route_change_recovery_failure_can_be_retried_when_the_app_becomes_active_again() {
        let state = InterruptionState::Running
            .on_route_changed(RouteChangeReason::OldDeviceUnavailable)
            .on_recovery_attempted(false);
        assert_eq!(state, InterruptionState::RecoveryFailed);

        let state = state.on_app_became_active();
        assert_eq!(state, InterruptionState::RecoveryPending);
    }

    // --- `confirm_recovery_progress`(実機 R24 の修正。調査記録「
    // `pause()`→`play()` が Ok を返しても無音のままだったケース」参照)---
    //
    // ここは CoreAudio に一切触れない純粋なロジックなので、`cpal::Stream`/`AtomicU64`
    // すら使わずクロージャだけで実機の「カウンタが進む/進まない」を模擬できる
    // (`InterruptionState`/`RouteChangeReason` のテストと同じ設計思想)。

    /// 依頼書が明示した最小シナリオ1つ目:「成功が1回目に来る」——1回目の
    /// `pause()`→`play()`(呼び出し側が既に済ませている前提)の結果が、最初の待機
    /// (`RECOVERY_WAIT_SCHEDULE_MS[0]` = 20ms)の直後に確認できるケース。
    /// 追加の `retry` は一度も呼ばれないこと(=通常時のコストが20ms待つだけで
    /// 済むこと)も合わせて固定化する。
    #[test]
    fn confirm_recovery_progress_succeeds_on_the_first_check_without_retrying() {
        let ticks_before = 41u64;
        // 1回目の pause()→play()(呼び出し側が既に済ませた分)で既に進んでいた、
        // というシナリオ。
        let ticks_now = ticks_before + 1;
        let mut retry_calls = 0u32;
        let mut waits = Vec::new();

        let outcome = confirm_recovery_progress(
            ticks_before,
            || ticks_now,
            || retry_calls += 1,
            |wait_ms| waits.push(wait_ms),
        );

        assert_eq!(outcome, Some((1, RECOVERY_WAIT_SCHEDULE_MS[0])));
        assert_eq!(retry_calls, 0, "1回目で確認できたら追加の再試行は不要");
        assert_eq!(waits, vec![RECOVERY_WAIT_SCHEDULE_MS[0]]);
    }

    /// 依頼書が明示した最小シナリオ2つ目:「成功が途中(最後より前)に来る」——
    /// 最初の2ステップは空振りし、3回目の再試行後にようやくカウンタが進むケース。
    /// 累計待機時間・`retry` の呼び出し回数(2回)も合わせて固定化する。
    #[test]
    fn confirm_recovery_progress_succeeds_after_a_couple_of_retries() {
        let ticks_before = 100u64;
        // read_ticks の呼び出し回数で「今どのステップの確認中か」を数える:
        // 1回目(index 0, 追加retry無し)・2回目(index 1, retry後)は空振り、
        // 3回目(index 2, retry後)で進む。
        let mut read_calls = 0u32;
        let mut retry_calls = 0u32;

        let outcome = confirm_recovery_progress(
            ticks_before,
            || {
                read_calls += 1;
                if read_calls < 3 {
                    ticks_before
                } else {
                    ticks_before + 1
                }
            },
            || retry_calls += 1,
            |_| {},
        );

        let expected_waited: u64 = RECOVERY_WAIT_SCHEDULE_MS[..3].iter().sum();
        assert_eq!(outcome, Some((3, expected_waited)));
        assert_eq!(
            retry_calls, 2,
            "3回目の確認に到達するまでに2回 retry するはず"
        );
    }

    /// 依頼書が明示した最小シナリオ3つ目:「最後まで確認できない」——実機 R24 の
    /// ログのうち、もし2回目の `AppBecameActive` 経由の再試行も効かなかったら、
    /// という想定シナリオに対応する。スケジュールを最後まで使い切り、`None` を返し、
    /// 呼び出し側が `InterruptionState::RecoveryFailed` として扱えるようにする。
    #[test]
    fn confirm_recovery_progress_gives_up_after_exhausting_the_schedule() {
        let ticks_before = 7u64;
        let mut retry_calls = 0u32;
        let mut waits = Vec::new();

        let outcome = confirm_recovery_progress(
            ticks_before,
            || ticks_before, // 一度も進まない
            || retry_calls += 1,
            |wait_ms| waits.push(wait_ms),
        );

        assert_eq!(outcome, None);
        assert_eq!(
            retry_calls,
            RECOVERY_WAIT_SCHEDULE_MS.len() as u32 - 1,
            "1回目(index 0)は追加retry無しなので、retryはスケジュール長-1回のはず"
        );
        assert_eq!(waits, RECOVERY_WAIT_SCHEDULE_MS.to_vec());
        assert_eq!(
            waits.iter().sum::<u64>(),
            370,
            "合計待機時間は依頼書の目安どおり370ms"
        );
    }

    // ------------------------------------------------------------------
    // 出力停止ウォッチドッグ(実機報告 V14、2026-09-10)。
    // OutputStallDetector のクラス doc「なぜ要るか」参照。
    // ------------------------------------------------------------------

    /// コールバックが進み続けている限り、ウォッチドッグは何も言わない。
    #[test]
    fn watchdog_stays_quiet_while_the_callback_keeps_advancing() {
        let mut detector = OutputStallDetector::new();
        for tick in 1..=100u64 {
            assert!(
                !detector.observe(InterruptionState::Running, tick),
                "正常に鳴っている最中に復帰を要求しています(不要な音切れの原因になります)"
            );
        }
    }

    /// 進まないまま WATCHDOG_STALL_SAMPLES 回観測したら復帰を要求する。
    /// ⚠️ **1回では要求しない**——観測とコールバック周期の噛み合わせによる正常な揺れで
    /// 誤検知しないため。
    #[test]
    fn watchdog_requests_recovery_after_consecutive_stalled_samples() {
        let mut detector = OutputStallDetector::new();
        // まず一度進めて基準を作る。
        assert!(!detector.observe(InterruptionState::Running, 10));

        for sample in 1..WATCHDOG_STALL_SAMPLES {
            assert!(
                !detector.observe(InterruptionState::Running, 10),
                "{sample} 回目で早まって復帰を要求しています"
            );
        }
        assert!(
            detector.observe(InterruptionState::Running, 10),
            "{WATCHDOG_STALL_SAMPLES} 回連続で進んでいないのに復帰を要求していません(V14 が直りません)"
        );
    }

    /// 起動直後に一度もコールバックが走らなかった場合(カウンタが 0 のまま)も検知する。
    #[test]
    fn watchdog_detects_a_callback_that_never_started() {
        let mut detector = OutputStallDetector::new();
        let mut requested = false;
        for _ in 0..WATCHDOG_STALL_SAMPLES {
            requested = detector.observe(InterruptionState::Running, 0);
        }
        assert!(
            requested,
            "一度も鳴り始めなかったケースを取りこぼしています"
        );
    }

    /// 復帰を要求した直後はクールダウンし、撃ち続けない。
    #[test]
    fn watchdog_backs_off_after_requesting_recovery() {
        let mut detector = OutputStallDetector::new();
        // 1回目は基準作り(last_ticks の初期値 0 から 7 へ動く)なので、発火まで
        // STALL_SAMPLES + 1 回ぶん観測が要る。
        let mut fired = false;
        for _ in 0..=WATCHDOG_STALL_SAMPLES {
            fired = detector.observe(InterruptionState::Running, 7);
        }
        assert!(fired, "前提が崩れています(ここで1回発火しているはず)");

        // クールダウン中は、止まったままでも要求しない。
        for sample in 0..WATCHDOG_COOLDOWN_SAMPLES {
            assert!(
                !detector.observe(InterruptionState::Running, 7),
                "クールダウン {sample} 回目で復帰を撃ち直しています(pause/play の連射になります)"
            );
        }

        // 明けたら、また STALL_SAMPLES 回ぶん数え直してから要求する。
        for _ in 1..WATCHDOG_STALL_SAMPLES {
            assert!(!detector.observe(InterruptionState::Running, 7));
        }
        assert!(
            detector.observe(InterruptionState::Running, 7),
            "クールダウン明けに再試行できていません(1回空振りしたら二度と直らなくなります)"
        );
    }

    /// 🔴 止まっているのが正常な状態では見張らない。
    #[test]
    fn watchdog_ignores_states_where_a_stopped_callback_is_expected() {
        for state in [
            InterruptionState::Interrupted,
            InterruptionState::Backgrounded,
            InterruptionState::RecoveryPending,
        ] {
            assert!(!state.allows_stall_watchdog(), "{state:?} を見張っています");

            let mut detector = OutputStallDetector::new();
            for _ in 0..WATCHDOG_STALL_SAMPLES * 3 {
                assert!(
                    !detector.observe(state, 42),
                    "{state:?} 中に復帰を要求しています(OS が止めている/自分で手放した状態です)"
                );
            }
        }
    }

    /// 🔴 `RecoveryFailed` は**見張る対象**(R24 で残っていた穴。クラス doc 参照)。
    #[test]
    fn watchdog_retries_after_a_failed_recovery() {
        assert!(
            InterruptionState::RecoveryFailed.allows_stall_watchdog(),
            "復帰に失敗したまま前面に居座ると、誰も再試行しなくなります(R24 の穴)"
        );

        let mut detector = OutputStallDetector::new();
        let mut requested = false;
        for _ in 0..WATCHDOG_STALL_SAMPLES {
            requested = detector.observe(InterruptionState::RecoveryFailed, 0);
        }
        assert!(requested);
    }

    /// 見張らない状態を挟んでも、そこで数えた分を持ち越さない
    /// (割り込み中に進まなかったぶんで、復帰直後に誤検知しないこと)。
    #[test]
    fn watchdog_does_not_carry_stalled_samples_across_an_interruption() {
        let mut detector = OutputStallDetector::new();
        detector.observe(InterruptionState::Running, 5);

        // 割り込み中は当然進まない。
        for _ in 0..WATCHDOG_STALL_SAMPLES * 2 {
            assert!(!detector.observe(InterruptionState::Interrupted, 5));
        }

        // 復帰直後の1回目で即座に「止まっている」と判定してはいけない。
        assert!(
            !detector.observe(InterruptionState::Running, 5),
            "割り込み中に数えた分を持ち越しています"
        );
    }
}
