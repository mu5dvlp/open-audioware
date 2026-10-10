//! 音声コールバックから駆動される最上位レンダラ。
//!
//! 初期構築仕様 §5.2(確定): 音声スレッド(OS のオーディオコールバック)は
//! 「コマンド消化 → デコード済みリングバッファからミックス → 出力」を行う。
//! 実体([`crate::mixer::Mixer`])はミキサ・ボイスプール・バス・クリッパを持ち、
//! [`Renderer::render`] の中で §5.3 のリアルタイム安全性規約に従って駆動される。
//!
//! ## 所有権の設計(M0 からの変更点)
//!
//! M0 では `Arc<Renderer>` をゲームスレッドと音声スレッドの双方で共有していたが、
//! M1 ではミキサ状態(ボイスプール・バス・コマンドキューの受信側)は音声コールバック
//! スレッドの排他所有物とし、`Backend::open` へ**値渡し(ムーブ)**する設計に変更した。
//! ゲームスレッド側は [`crate::mixer::CommandSender`](= コマンド送信の口)と
//! [`crate::mixer::ReclaimReceiver`](Arc 回収)という別ハンドル経由でのみ音声スレッドと
//! やり取りする。これにより、音声コールバック内で `Mutex`/`UnsafeCell` 等の
//! 追加の同期プリミティブが一切不要になる(単一の書き手のみが `&mut self` で触る)。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::clock::{MusicClockPublisher, RenderedFrameCounter};
use crate::config::Config;
use crate::event::EventQueue;
use crate::format::CHANNELS;
use crate::mixer::{self, CommandSender, Mixer, ReclaimReceiver};
use crate::music::MusicState;
use crate::stream::MusicStreamProducer;

/// mw-core の最上位レンダラ。
///
/// `mw-backend` の `Backend` 実装がオーディオコールバックからこれを排他所有し、
/// [`Renderer::render`] を呼んでインターリーブ済み f32 ステレオバッファを埋める。
pub struct Renderer {
    mixer: Mixer,
    frame_counter: RenderedFrameCounter,
    /// 出力デバイスの実サンプルレートが [`Renderer::set_sample_rate`] で確定したか。
    /// `Renderer` は単一スレッドが `&mut self` で排他所有する設計(モジュール doc
    /// 「所有権の設計」)なので、ただの `bool` で足りる(atomic は要らない)。
    sample_rate_confirmed: bool,
    /// 確定前に [`Renderer::render`] が呼ばれたことを、ゲームスレッドへ一度だけ
    /// 知らせる合図。音声スレッドは確定前の各 `render` で `store(true, Relaxed)`
    /// するだけ(ロック・アロケーション無し)。[`Renderer::
    /// provisional_sample_rate_warning_handle`] で取った複製をゲームスレッドが
    /// 任意のタイミングで読み、ログに出す(`mw-ffi::handle::Instance::
    /// log_provisional_sample_rate_warning_once` 参照)。
    provisional_sample_rate_warning: Arc<AtomicBool>,
}

impl Renderer {
    /// `Renderer` と、ゲームスレッド側で使うハンドル一式
    /// (コマンド送信 [`CommandSender`] / Arc 回収 [`ReclaimReceiver`] / 楽曲PCM供給の
    /// 生産側 [`MusicStreamProducer`] / 音楽クロックの読み手 [`MusicClockPublisher`])を
    /// 構築する。
    ///
    /// `sample_rate` は出力デバイスが実際に開いたレート(バス/ボイスのランプの
    /// ミリ秒 → サンプル数換算に使う。§4.1)。デバイスオープン前は §4.7 推奨の
    /// 48kHz 等、暫定値を渡しておき、オープン後に [`Renderer::set_sample_rate`] で
    /// 確定させてよい(音声コールバックが動き出す前に呼ぶこと)。
    ///
    /// 戻り値は `(Renderer, コマンド送信, Arc 回収, 楽曲PCM供給の生産側, 音楽クロックの読み手,
    /// イベントキューの読み書きハンドル, BGM 専用ハンドル一式)`。後ろ4つは `mixer::build` の
    /// ドキュメント参照(M2-5, M2-6, M4-3)。
    pub fn build(
        config: Config,
        sample_rate: u32,
    ) -> (
        Renderer,
        CommandSender,
        ReclaimReceiver,
        MusicStreamProducer,
        Arc<MusicClockPublisher>,
        Arc<EventQueue>,
        mixer::BgmHandles,
    ) {
        let (mixer, sender, reclaim, music_producer, music_clock, events, bgm) =
            mixer::build(config, sample_rate);
        (
            Renderer {
                mixer,
                frame_counter: RenderedFrameCounter::new(),
                sample_rate_confirmed: false,
                provisional_sample_rate_warning: Arc::new(AtomicBool::new(false)),
            },
            sender,
            reclaim,
            music_producer,
            music_clock,
            events,
            bgm,
        )
    }

    /// [`Renderer::build`] と同じだが、新しい `EventQueue` を作らず呼び出し側が渡した
    /// `Arc` を使う([`mixer::build_with_events`] のドキュメント参照)。
    ///
    /// **M3「Android(AAudio)切断復旧」案A専用の入口。** ミドルウェア内部でストリームを
    /// 再オープンする際、`mw-ffi::handle::Instance` はこちらを使って `Renderer` を
    /// 丸ごと作り直しつつ、`mw_poll_events` の読み出し先(`Arc<EventQueue>`)の identity
    /// は変えない。
    pub fn build_with_events(
        config: Config,
        sample_rate: u32,
        events: Arc<EventQueue>,
    ) -> (
        Renderer,
        CommandSender,
        ReclaimReceiver,
        MusicStreamProducer,
        Arc<MusicClockPublisher>,
        Arc<EventQueue>,
        mixer::BgmHandles,
    ) {
        let (mixer, sender, reclaim, music_producer, music_clock, events, bgm) =
            mixer::build_with_events(config, sample_rate, events);
        (
            Renderer {
                mixer,
                frame_counter: RenderedFrameCounter::new(),
                sample_rate_confirmed: false,
                provisional_sample_rate_warning: Arc::new(AtomicBool::new(false)),
            },
            sender,
            reclaim,
            music_producer,
            music_clock,
            events,
            bgm,
        )
    }

    /// `output` はインターリーブされた f32 ステレオバッファ(`len` は `frames * CHANNELS`)。
    /// `buffer_start_host_time_ns` はこのバッファの先頭フレームが実際に DAC から
    /// 出力される(と予測される)ホスト単調時刻(初期構築仕様『§4.4』。
    /// `mixer.rs::Mixer::render` のドキュメント参照)。
    ///
    /// # リアルタイム安全性
    ///
    /// この関数はオーディオコールバックから直接呼ばれる想定。
    /// ロック取得・ヒープアロケーション・ブロッキング IO・パニック経路は禁止(§5.3)。
    pub fn render(&mut self, output: &mut [f32], buffer_start_host_time_ns: u64) {
        // サンプルレートが未確定のまま render が走るのは、バックエンド実装が
        // `Backend::open` で `set_sample_rate` を呼び忘れたことの兆候(過去に実際に
        // 起きた——仮置きのレートのまま音楽クロックが変換し続けるバグになる)。
        // `AtomicBool` への `store` 1回だけなのでロック・アロケーションを伴わず、
        // §5.3 に抵触しない。確定後は分岐がそのまま外れるので恒常的なコストは無い。
        if !self.sample_rate_confirmed {
            self.provisional_sample_rate_warning
                .store(true, Ordering::Relaxed);
        }

        self.mixer.render(output, buffer_start_host_time_ns);

        // `output.len()` が CHANNELS の倍数でない場合でも、整数除算で切り捨てるだけで
        // パニックはしない。
        let frames = (output.len() / CHANNELS) as u64;
        self.frame_counter.add(frames);
    }

    /// これまでにレンダリングした総フレーム数(音楽クロックの土台。§4.4。M2 で拡張)。
    pub fn rendered_frames(&self) -> u64 {
        self.frame_counter.get()
    }

    /// 出力デバイスのサンプルレートを確定させる。音声コールバックが動き出す前
    /// (`Backend::open` がストリームを `play()` する前)に一度だけ呼ぶこと。
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.mixer.set_sample_rate(sample_rate);
        self.sample_rate_confirmed = true;
    }

    /// [`Renderer::render`] が確定前に呼ばれたことがあるかを読むための複製を返す
    /// (ゲームスレッド用)。
    ///
    /// 🔴 他の同種ハンドル([`Renderer::se_schedule_overflow_counter`] 等)と同じ理由で、
    /// `Backend::open` へこの `Renderer` をムーブする**前に**取ること——ムーブ後は
    /// `Renderer` 自身に触れない。
    pub fn provisional_sample_rate_warning_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.provisional_sample_rate_warning)
    }

    pub fn sample_rate(&self) -> u32 {
        self.mixer.sample_rate()
    }

    /// Master 段のソフトクリッパが動作(閾値超過)した累計回数(§4.1 の動作検知)。
    pub fn clipper_engaged_count(&self) -> u64 {
        self.mixer.clipper_engaged_count()
    }

    /// 現在アクティブな(primary スロットの)ボイス数。テスト・診断用。
    pub fn active_voice_count(&self) -> usize {
        self.mixer.active_voice_count()
    }

    /// 現在の楽曲ボイスの状態(初期構築仕様『§4.3』)。
    pub fn music_state(&self) -> MusicState {
        self.mixer.music_state()
    }

    /// 現在の BGM ボイスの状態(初期構築仕様『§2』M14, M4-3)。
    pub fn bgm_state(&self) -> MusicState {
        self.mixer.bgm_state()
    }

    /// 直近の楽曲予約再生が、到来時点でプリロール未完了だったために繰り下げられたか
    /// (初期構築仕様『§4.3』【仮】)。発火すると `false` に戻る。
    pub fn music_schedule_deferred(&self) -> bool {
        self.mixer.music_schedule_deferred()
    }

    /// 予約 SE のキューが満杯で挿入できず、発音されなかった累計件数(初期構築仕様『§4.5』)。
    pub fn se_schedule_overflow_count(&self) -> u64 {
        self.mixer.se_schedule_overflow_count()
    }

    /// 上のカウンタを**ゲームスレッドから読むための複製**を返す
    /// ([`crate::mixer::Mixer::se_schedule_overflow_counter`] への委譲)。
    /// 🔴 `Backend::open` へこの `Renderer` をムーブする**前に**取ること。
    pub fn se_schedule_overflow_counter(&self) -> Arc<AtomicU64> {
        self.mixer.se_schedule_overflow_counter()
    }

    /// 音楽クロックの発行ハンドルへの複製を返す
    /// ([`crate::mixer::Mixer::music_clock_handle`] への委譲。同じ理由で
    /// 🔴 `Backend::open` へこの `Renderer` をムーブする**前に**取ること)。
    pub fn music_clock_handle(&self) -> Arc<MusicClockPublisher> {
        self.mixer.music_clock_handle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer_for_test() -> Renderer {
        let (renderer, _sender, _reclaim, _music_producer, _music_clock, _events, _bgm) =
            Renderer::build(Config::default(), 48_000);
        renderer
    }

    #[test]
    fn render_writes_silence_when_nothing_is_playing() {
        let mut renderer = renderer_for_test();
        let mut buffer = vec![1.0_f32; 256 * CHANNELS];
        renderer.render(&mut buffer, 0);
        assert!(buffer.iter().all(|&sample| sample == 0.0));
    }

    #[test]
    fn render_advances_frame_counter_by_frame_count_not_sample_count() {
        let mut renderer = renderer_for_test();
        let mut buffer = vec![0.0_f32; 128 * CHANNELS];
        renderer.render(&mut buffer, 0);
        assert_eq!(renderer.rendered_frames(), 128);
    }

    #[test]
    fn render_accumulates_across_multiple_callbacks() {
        let mut renderer = renderer_for_test();
        let mut buffer = vec![0.0_f32; 64 * CHANNELS];
        for i in 0..10u64 {
            renderer.render(&mut buffer, i * 1_000_000);
        }
        assert_eq!(renderer.rendered_frames(), 640);
    }

    #[test]
    fn render_handles_empty_buffer_without_panicking() {
        let mut renderer = renderer_for_test();
        let mut buffer: Vec<f32> = Vec::new();
        renderer.render(&mut buffer, 0);
        assert_eq!(renderer.rendered_frames(), 0);
    }

    #[test]
    fn render_handles_buffer_length_not_a_multiple_of_channel_count() {
        let mut renderer = renderer_for_test();
        let mut buffer = vec![0.0_f32; 5]; // 5 は CHANNELS(2) の倍数ではない
        renderer.render(&mut buffer, 0);
        assert_eq!(renderer.rendered_frames(), 2);
    }

    // --- build_with_events / 委譲アクセサの回帰テスト -------------------------------
    //
    // ⚠️ どれも「一行の委譲」に見えるが、<b>委譲先を取り違えても誰も気付かない</b>種類の
    // コードで、テストが無いと壊れたことが分からない。

    /// 🔴 <b>渡した `events` の identity を変えないこと</b>が契約
    /// (`Instance::attempt_reopen` は再オープンの前後で `mw_poll_events` の読み出し先を
    /// 差し替えずに済ませる前提でこれに依存している —— P3-11)。
    #[test]
    fn build_with_events_keeps_the_identity_of_the_passed_event_queue() {
        let events = Arc::new(EventQueue::new(64));

        let (_renderer, _sender, _reclaim, _producer, _clock, returned, _bgm) =
            Renderer::build_with_events(Config::default(), 48_000, Arc::clone(&events));

        assert!(
            Arc::ptr_eq(&events, &returned),
            "build_with_events が別の EventQueue を返しています(再オープン後に \
             mw_poll_events の読み出し先がずれます)"
        );
    }

    #[test]
    fn set_sample_rate_is_readable_back() {
        let mut renderer = renderer_for_test();
        assert_eq!(renderer.sample_rate(), 48_000);

        renderer.set_sample_rate(44_100);

        assert_eq!(renderer.sample_rate(), 44_100);
    }

    // --- 仮置きのサンプルレートのまま render した場合の警告(`provisional_sample_rate_warning_handle`) ---

    /// `set_sample_rate` を一度も呼ばずに `render` すると、警告の合図が立つ
    /// (バックエンド実装が確定を呼び忘れたまま描画を始めたことの検知)。
    #[test]
    fn rendering_before_set_sample_rate_raises_the_provisional_warning() {
        let mut renderer = renderer_for_test();
        let warning = renderer.provisional_sample_rate_warning_handle();
        assert!(!warning.load(std::sync::atomic::Ordering::Relaxed));

        let mut buffer = vec![0.0_f32; 16 * CHANNELS];
        renderer.render(&mut buffer, 0);

        assert!(warning.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// `render` の前に `set_sample_rate` を呼んでいれば、警告は一度も立たない
    /// (`CpalBackend::open` 等、正しく確定させてから描画を始める実装の挙動)。
    #[test]
    fn set_sample_rate_before_rendering_keeps_the_provisional_warning_clear() {
        let mut renderer = renderer_for_test();
        let warning = renderer.provisional_sample_rate_warning_handle();

        renderer.set_sample_rate(44_100);
        let mut buffer = vec![0.0_f32; 16 * CHANNELS];
        for _ in 0..4 {
            renderer.render(&mut buffer, 0);
        }

        assert!(!warning.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// 🔴 <b>ゲームスレッド用に取り出した合図が、レンダラ側が立てる実体と同じであること。</b>
    /// [`se_schedule_overflow_counter_is_the_same_object_the_renderer_counts_with`] と
    /// 同じ理由の回帰テスト——ここが別物になると、警告が<b>永久に立たないまま</b>に見える。
    #[test]
    fn provisional_sample_rate_warning_handle_is_the_same_object_the_renderer_writes_to() {
        let mut renderer = renderer_for_test();

        let first = renderer.provisional_sample_rate_warning_handle();
        let second = renderer.provisional_sample_rate_warning_handle();
        assert!(Arc::ptr_eq(&first, &second), "毎回別の Arc を返しています");

        let mut buffer = vec![0.0_f32; 16 * CHANNELS];
        renderer.render(&mut buffer, 0);

        assert!(
            first.load(std::sync::atomic::Ordering::Relaxed),
            "render が書き込む先と、取り出した複製が食い違っています"
        );
    }

    /// 🔴 <b>ゲームスレッド用に取り出したカウンタが、レンダラ側が増やす実体と同じであること。</b>
    /// ⚠️ ここが別物になると、診断値が<b>ずっと 0 のまま</b>に見える
    /// (`Instance::attempt_reopen` が「Backend::open へムーブする前に取れ」と
    /// 書いているのと同じ罠)。
    #[test]
    fn se_schedule_overflow_counter_is_the_same_object_the_renderer_counts_with() {
        let renderer = renderer_for_test();

        let first = renderer.se_schedule_overflow_counter();
        let second = renderer.se_schedule_overflow_counter();

        assert!(Arc::ptr_eq(&first, &second), "毎回別の Arc を返しています");
        assert_eq!(
            first.load(std::sync::atomic::Ordering::Relaxed),
            renderer.se_schedule_overflow_count()
        );
    }

    /// 🔴 <b>`music_clock_handle` が返す `Arc` が、`Renderer::build` の戻り値
    /// (ゲームスレッド側ハンドル)と同一のオブジェクトであること。</b>
    /// ⚠️ ここが別物になると、`mw-backend::AppleBackend` がこれ経由で呼ぶ
    /// `bump_generation`(補正項の変化を世代へ反映する経路)が、ゲームスレッドが
    /// 実際に読む `MusicClockPublisher` とは違うインスタンスに対して空振りする——
    /// 「世代が進んだように見えるが、読み手には一切伝わらない」という発見しづらい
    /// 事故になる(`se_schedule_overflow_counter_is_the_same_object_the_renderer_counts_with`
    /// と同じ理由)。
    #[test]
    fn music_clock_handle_is_the_same_object_the_build_caller_received() {
        let (mut renderer, _sender, _reclaim, _music_producer, music_clock, _events, _bgm) =
            Renderer::build(Config::default(), 48_000);

        let handle = renderer.music_clock_handle();
        assert!(
            Arc::ptr_eq(&music_clock, &handle),
            "別の Arc を返しています(Mixer 内部の music_clock と食い違う)"
        );

        let mut buffer = vec![0.0_f32; 4 * CHANNELS];
        renderer.render(&mut buffer, 0);

        assert_eq!(
            handle.snapshot().host_time_ns,
            music_clock.snapshot().host_time_ns,
            "同一の Arc なら render の結果が両方から同じように見えるはず"
        );
    }

    /// 何も鳴らしていない状態の見え方を固定する(委譲先の取り違えの検出)。
    #[test]
    fn a_fresh_renderer_reports_nothing_playing() {
        let renderer = renderer_for_test();

        assert_eq!(renderer.active_voice_count(), 0);
        assert_eq!(renderer.clipper_engaged_count(), 0);
        assert_eq!(renderer.se_schedule_overflow_count(), 0);
        assert_eq!(renderer.rendered_frames(), 0);
        assert!(!renderer.music_schedule_deferred());
        // 🔴 楽曲と BGM は<b>別のボイス</b>。どちらも未設定なので Loading から始まる
        // (`MusicState::Loading` の doc「プリロール中、または楽曲が未設定」)。
        assert_eq!(renderer.music_state(), MusicState::Loading);
        assert_eq!(renderer.bgm_state(), MusicState::Loading);
    }

    #[test]
    fn music_clock_handle_reflects_renderer_state_across_the_shared_arc() {
        // `Renderer::build` が返す `Arc<MusicClockPublisher>` は、
        // `Renderer::render`(音声コールバック役)が書き込んだ内容を
        // ゲームスレッド役から直接読める必要がある(M2-5 の核心)。
        let (mut renderer, _sender, _reclaim, _music_producer, music_clock, _events, _bgm) =
            Renderer::build(Config::default(), 1_000);

        let mut buffer = vec![0.0_f32; 10 * CHANNELS];
        renderer.render(&mut buffer, 0);

        let snapshot = music_clock.snapshot();
        assert_eq!(snapshot.host_time_ns, 10_000_000);
        assert_eq!(snapshot.sample_rate, 1_000);
        assert!(!snapshot.is_playing);
        assert_eq!(renderer.music_state(), MusicState::Loading);
        assert!(!renderer.music_schedule_deferred());
        assert_eq!(renderer.se_schedule_overflow_count(), 0);
    }
}
