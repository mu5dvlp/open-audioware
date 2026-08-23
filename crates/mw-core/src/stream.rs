//! 楽曲ストリーミングのリングバッファと供給源(初期構築仕様『§4.3 楽曲再生』
//! 『§4.7 デコードとリサンプリング』『§5.2 スレッドモデル』)。
//!
//! # なぜ mw-core にスレッドを持たせないか
//!
//! 初期構築仕様『§5.2』の図には「デコードスレッド」が登場するが、**このモジュールは
//! スレッドを一切生成しない**。mw-core は「オフラインレンダリングだけで完結し、
//! テストの主戦場になる」層(`crates/mw-core/CLAUDE.md`)であり、スレッドを埋め込むと
//! テストが途端に難しくなる(タイミング依存のフレークが避けられない)。
//!
//! 代わりに、リングバッファを埋める処理を [`MusicStreamProducer::pump`] として公開し、
//! **スレッドを回す責務は外側(後続作業の mw-ffi)に委ねる**。mw-ffi 側は専用スレッドで
//! `pump` を定期的に(あるいは条件変数等で起こされて)呼び出す想定。テストは `pump` を
//! 直接呼ぶだけでスレッド無しに全経路(供給・シーク調停・アンダーラン)を通せる。
//!
//! # 全体構成
//!
//! - 生産側 [`MusicStreamProducer`]: `pump(&mut decoder)`(`decoder` は
//!   [`crate::decode::MusicDecoder`])でデコードし、`rtrb` の SPSC リングバッファへ
//!   詰める。デコードスレッド(mw-ffi)から呼ばれる想定。
//! - 消費側 [`StreamingMusicSource`]: [`crate::music::MusicFrameSource`] を実装する。
//!   `read` は音声コールバックから呼ばれるため、アロケーション・ロック・パニック経路を
//!   一切踏まない(`rtrb::Consumer::pop_partial_slice`/`read_chunk` は割り当て無しで使える)。
//!
//! # シークの調停(エポックの ack 方式)
//!
//! 素直に「シーク要求 → デコーダをシークして詰め直す」だけでは壊れる。理由:
//!
//! - 音声スレッド(消費側)は待てない(§5.3: ブロッキング禁止)。
//! - `rtrb` は**消費側しか pop できない**。生産側はリングバッファに残った古い PCM を
//!   自分では掃除できない。
//! - 結果、素朴な実装ではシーク直後に古い位置の PCM が数フレーム鳴ってしまう。
//!
//! そこで次のプロトコルで解く:
//!
//! - `seek_target` / `seek_epoch`(**消費側が書く**): [`StreamingMusicSource::request_seek`]
//!   が target を書いてから epoch を進める。
//! - `acked_epoch`(**生産側が書く**): [`MusicStreamProducer::pump`] が epoch の変化に
//!   気づいたら、まずリングバッファへ残っている**古い PCM を消費側自身が掃除**
//!   (`request_seek` の中で即座に `drain` する。これが一次防御)。そのうえで生産側は
//!   デコーダをシークし終えてから ack する(`acked_epoch = seek_epoch`)。
//! - 消費側の `read`: `acked_epoch != seek_epoch` の間は**リングバッファを読み捨てながら
//!   0 を返す**(= 無音。呼び出し側はアンダーランとして扱う)。ack が揃ってから通常の
//!   読み出しに戻る。
//!
//! `request_seek` 自身の掃除だけで大半のケースは解決するが、「消費側が掃除した直後、
//! 生産側がまだ epoch の変化に気づく前に、もう1回ぶんの古い PCM を押し込んでしまう」
//! 競合が原理的に残る(生産側は `pump` の先頭で一度だけ epoch を確認するため、確認前に
//! 開始していた `pump` 呼び出しは古い位置のままデコードを続けてしまいうる)。
//! `read` 側の「ack が揃うまで読み捨て続ける」処理は、この**取りこぼし分の二次防御**として
//! 機能する: 音声コールバックは(デコードスレッドの周期とは無関係に)継続的に `read` を
//! 呼び続けるため、ack が観測されるまでの間に紛れ込んだ古い PCM も読み捨てられる。
//! ack が観測された時点で初めて「それ以降キューに積まれるのは新しい位置の PCM だけ」が
//! 保証される(生産側はシークを完了させてから ack を書く実装にしてあるため)。
//!
//! ## メモリオーダリング
//!
//! `seek_target`/`seek_epoch` は消費側だけが書き、生産側だけが読む。`acked_epoch` は
//! その逆(生産側だけが書き、消費側だけが読む)。どちらも単一の書き手・複数(または単一)の
//! 読み手というメッセージパッシングの形なので、`clock.rs::RenderedFrameCounter` と同じ
//! 片方向の Release/Acquire で十分であり、`MusicClockPublisher` のような seqlock は不要
//! (複数フィールドの整合を取る必要が無いため)。`seek_target` は `Relaxed` で書き、
//! `seek_epoch` を `Release` で書く: 生産側が `seek_epoch` を `Acquire` で読んで変化に
//! 気づけば、それより前(プログラム順序上)に書かれた `seek_target` も必ず見える
//! (Release-Acquire のペアが「これより前の書き込みが見える」を保証するのは
//! この epoch 変数についてだけで十分で、`seek_target` 自体を Acquire/Release にする
//! 必要はない)。`acked_epoch` も同じ理屈で Release(生産側)/Acquire(消費側)。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rtrb::RingBuffer;

use crate::config::Config;
use crate::decode::{DecodeError, MusicDecoder};
use crate::format::CHANNELS;
use crate::music::MusicFrameSource;
use crate::ramp::ms_to_samples;

/// 総フレーム数「不明」を表すセンチネル値。`u64::MAX` フレーム(見積もり上限)は
/// 現実的な楽曲長では起こり得ないため、`Option<u64>` を素の `AtomicU64` に落とし込む
/// 際の番兵として安全に使える。
const UNKNOWN_TOTAL_FRAMES: u64 = u64::MAX;

/// 生産側・消費側で共有するシーク調停用の状態(モジュール doc「シークの調停」参照)。
///
/// フィールドごとに書き手が1つに定まるよう設計してある(コメント参照)。
#[derive(Debug)]
struct SharedState {
    /// 消費側が書く。シーク先フレーム。
    seek_target: AtomicU64,
    /// 消費側が書く。`request_seek` のたびに1つ進む。
    seek_epoch: AtomicU64,
    /// 生産側が書く。`pump` がシークを処理し終えるたびに `seek_epoch` の値へ合わせる。
    acked_epoch: AtomicU64,
    /// 生産側が書く。デコーダから得た総フレーム数(`UNKNOWN_TOTAL_FRAMES` は不明を表す)。
    total_frames: AtomicU64,
    /// 生産側が書く。デコーダがコンテナ末尾に達し、これ以上詰める新規データが無いこと。
    eof: AtomicBool,
    /// 生産側が書く。直近の `pump` でデコードエラーが起きたこと(状態として保持するのみ。
    /// イベント通知(`StreamError`)への昇格は後続作業。初期構築仕様『§4.6』)。
    has_error: AtomicBool,
}

impl SharedState {
    fn new() -> Self {
        Self {
            seek_target: AtomicU64::new(0),
            seek_epoch: AtomicU64::new(0),
            acked_epoch: AtomicU64::new(0),
            total_frames: AtomicU64::new(UNKNOWN_TOTAL_FRAMES),
            eof: AtomicBool::new(false),
            has_error: AtomicBool::new(false),
        }
    }
}

/// [`MusicStreamProducer::pump`] 1回分の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PumpOutcome {
    /// このリングバッファへ実際に積んだフレーム数。
    pub pushed_frames: usize,
    /// この呼び出しでデコーダがコンテナ末尾に達した(= 以後は詰めるものが無い)。
    pub reached_eof: bool,
}

/// リングバッファの生産側(初期構築仕様『§5.2』のデコードスレッドに相当する処理を提供する。
/// ただしスレッドそのものは持たない。モジュール doc 参照)。
pub struct MusicStreamProducer {
    producer: rtrb::Producer<f32>,
    shared: Arc<SharedState>,
    /// 直近に処理し終えた(ack 済みの)epoch。`shared.seek_epoch` と比較して新しい
    /// シーク要求の有無を判定する、生産側専用のローカルキャッシュ(共有はしない)。
    observed_epoch: u64,
}

impl MusicStreamProducer {
    /// デコーダから読めるだけ読み、リングバッファへ詰める。
    ///
    /// - シーク要求(`seek_epoch` の変化)を検知したら、まずデコーダをシークし
    ///   (`MusicDecoder::seek` がサンプル境界への合わせ込みを行う)、ack してから
    ///   このリングバッファへの詰め込みへ進む。
    /// - リングバッファの空き分だけをまとめて `decoder.read` に渡す
    ///   (`rtrb::Producer::write_chunk` でアロケーション無しに直接書き込む)。
    /// - デコードエラーは状態として保持し(`has_error`)、`Err` を返す。
    ///   イベント通知(`StreamError`)への昇格は後続作業(モジュール doc 参照)。
    ///
    /// このメソッドはデコードスレッド(mw-ffi)から呼ばれる想定で、音声コールバックの
    /// リアルタイム制約(§5.3)の対象外(ヒープアロケーションを伴ってよい)。
    pub fn pump(&mut self, decoder: &mut dyn MusicDecoder) -> Result<PumpOutcome, DecodeError> {
        self.shared.total_frames.store(
            decoder.total_frames().unwrap_or(UNKNOWN_TOTAL_FRAMES),
            Ordering::Relaxed,
        );

        if let Err(e) = self.handle_seek_if_requested(decoder) {
            self.shared.has_error.store(true, Ordering::Relaxed);
            return Err(e);
        }

        let free_frames = self.producer.slots() / CHANNELS;
        if free_frames == 0 {
            return Ok(PumpOutcome {
                pushed_frames: 0,
                reached_eof: self.shared.eof.load(Ordering::Relaxed),
            });
        }

        let n_samples = free_frames * CHANNELS;
        let mut chunk = match self.producer.write_chunk(n_samples) {
            Ok(chunk) => chunk,
            // 単一生産者スレッドの設計上、直前に読んだ空き数から縮むことは無い
            // (消費側の pop は空きを増やす方向にしか働かない)。理論上到達しない
            // 防御的分岐として、パニックせずに「今回は何もしない」を返す。
            Err(_) => return Ok(PumpOutcome::default()),
        };
        let (first, second) = chunk.as_mut_slices();

        let mut pushed_frames = 0usize;
        let mut decode_err = None;

        match decoder.read(first) {
            Ok(n) => {
                pushed_frames += n;
                // first を丁度使い切った場合のみ second へ進む(足りなかった = EOF or
                // エラーであり、その状態で second をさらに読もうとするのは無意味)。
                if n * CHANNELS == first.len() && !second.is_empty() {
                    match decoder.read(second) {
                        Ok(n2) => pushed_frames += n2,
                        Err(e) => decode_err = Some(e),
                    }
                }
            }
            Err(e) => decode_err = Some(e),
        }

        // エラー時も、それまでにデコードできたぶんは無駄にせずコミットする。
        chunk.commit(pushed_frames * CHANNELS);

        if let Some(e) = decode_err {
            self.shared.has_error.store(true, Ordering::Relaxed);
            return Err(e);
        }

        let reached_eof = pushed_frames < n_samples / CHANNELS;
        if reached_eof {
            self.shared.eof.store(true, Ordering::Relaxed);
        }
        Ok(PumpOutcome {
            pushed_frames,
            reached_eof,
        })
    }

    /// `seek_epoch` の変化を検知し、シークを処理して ack する。
    fn handle_seek_if_requested(
        &mut self,
        decoder: &mut dyn MusicDecoder,
    ) -> Result<(), DecodeError> {
        // Acquire: これより後ろで読む `seek_target` が、消費側が epoch を書く前に
        // 書いた値として確実に見えるようにする(モジュール doc「メモリオーダリング」)。
        let requested_epoch = self.shared.seek_epoch.load(Ordering::Acquire);
        if requested_epoch == self.observed_epoch {
            return Ok(());
        }
        let target = self.shared.seek_target.load(Ordering::Relaxed);

        decoder.seek(target)?;
        self.observed_epoch = requested_epoch;
        self.shared.eof.store(false, Ordering::Relaxed);
        self.shared.has_error.store(false, Ordering::Relaxed);
        // Release: これより前の「デコーダのシークが完了している」という事実を、
        // 消費側が Acquire で読んだ時点で確実に見えるようにする。ack した**後**にしか
        // このリングバッファへ新しい PCM を積まない(このメソッドの呼び出し元
        // `pump` を参照)ため、消費側は ack を見た時点で「これ以降キューに積まれるのは
        // 新しい位置の PCM だけ」を信頼してよい。
        self.shared
            .acked_epoch
            .store(requested_epoch, Ordering::Release);
        Ok(())
    }
}

/// リングバッファの消費側。[`MusicFrameSource`] を実装し、音声コールバックから呼ばれる。
pub struct StreamingMusicSource {
    consumer: rtrb::Consumer<f32>,
    shared: Arc<SharedState>,
    /// `is_ready` がプリロール完了とみなすために必要な最小バッファ済みフレーム数。
    preroll_frames: usize,
}

impl StreamingMusicSource {
    /// 現在リングバッファに溜まっているフレーム数(診断・テスト用)。
    pub fn buffered_frames(&self) -> usize {
        self.consumer.slots() / CHANNELS
    }

    /// 直近の `pump` でデコードエラーが発生し、その状態が保持されたままか。
    ///
    /// イベント通知(`StreamError`)への昇格は後続作業(初期構築仕様『§4.6』)。
    /// 現時点では問い合わせ専用の状態として公開するに留める。
    pub fn has_stream_error(&self) -> bool {
        self.shared.has_error.load(Ordering::Relaxed)
    }

    /// 今リングバッファにあるものを全て読み捨てる(掃除)。
    ///
    /// アロケーション無し: `read_chunk` で確保済み領域を直接 `commit_all` するだけで、
    /// どこにもコピーしない。
    fn drain_all_available(&mut self) {
        let avail = self.consumer.slots();
        if avail == 0 {
            return;
        }
        if let Ok(chunk) = self.consumer.read_chunk(avail) {
            chunk.commit_all();
        }
    }
}

impl MusicFrameSource for StreamingMusicSource {
    fn read(&mut self, out: &mut [f32]) -> usize {
        // Acquire: 一致していれば、生産側がこの epoch のシークを完了させてから
        // 積んだ PCM だけがこの後キューに残っている(モジュール doc 参照)。
        let acked = self.shared.acked_epoch.load(Ordering::Acquire);
        let target = self.shared.seek_epoch.load(Ordering::Acquire);
        if acked != target {
            // 調停中: 生産側がまだ古い位置のつもりで押し込んだかもしれない PCM を
            // 読み捨てながら無音を返す(モジュール doc「シークの調停」の二次防御)。
            self.drain_all_available();
            return 0;
        }

        if self.shared.has_error.load(Ordering::Relaxed) {
            return 0;
        }

        // `out` の長さがフレーム境界に揃っていなくてもパニックしない(`MusicVoice::render`
        // と同じ流儀)。半端な末尾サンプルはそもそも書かず、呼び出し側が前もって
        // 埋めておいた値(通常は 0.0)のまま残す。
        let usable_len = out.len() - out.len() % CHANNELS;
        let (filled, _unfilled) = self.consumer.pop_partial_slice(&mut out[..usable_len]);
        filled.len() / CHANNELS
    }

    fn total_frames(&self) -> Option<u64> {
        let raw = self.shared.total_frames.load(Ordering::Relaxed);
        if raw == UNKNOWN_TOTAL_FRAMES {
            None
        } else {
            Some(raw)
        }
    }

    fn is_ready(&self) -> bool {
        let acked = self.shared.acked_epoch.load(Ordering::Acquire);
        let target = self.shared.seek_epoch.load(Ordering::Acquire);
        if acked != target {
            return false;
        }
        self.buffered_frames() >= self.preroll_frames || self.shared.eof.load(Ordering::Relaxed)
    }

    fn request_seek(&mut self, frame: u64) {
        // 一次防御: 今リングバッファにあるものは全部消費側自身の手で掃除する
        // (rtrb は消費側しか pop できないため、これができるのは消費側だけ)。
        self.drain_all_available();

        // Relaxed で先に書く: 続く `seek_epoch` の Release ストアが、この書き込みを
        // 追い越して他スレッドから先に観測されることは無い(モジュール doc 参照)。
        self.shared.seek_target.store(frame, Ordering::Relaxed);
        let next_epoch = self
            .shared
            .seek_epoch
            .load(Ordering::Relaxed)
            .wrapping_add(1);
        self.shared.seek_epoch.store(next_epoch, Ordering::Release);
    }
}

/// リングバッファと生産側・消費側のハンドルを構築する(`mixer::build` と同じ形)。
///
/// - `config.preroll_ms` から `sample_rate` を使ってプリロール量(フレーム数)を決める
///   ([`ms_to_samples`]。`ramp.rs`/`MusicVoice` と同じ ms → サンプル数変換の流儀)。
/// - リングバッファ容量はプリロール量の4倍【仮】。プリロール分に加えて、デコード
///   スレッドがプリロール完了後も先読みを続けられる余裕を持たせるための倍率で、
///   実測(M2 後半)で見直す。容量は必ず [`CHANNELS`] の倍数にする(ステレオの
///   フレーム境界が折り返し位置をまたいでも壊れないようにするため。モジュール内の
///   各所で「積む/読む量は常に CHANNELS の倍数」という不変条件を前提にしている)。
pub fn channel(config: Config, sample_rate: u32) -> (MusicStreamProducer, StreamingMusicSource) {
    let preroll_frames = ms_to_samples(config.preroll_ms, sample_rate) as u64;
    let capacity_frames = (preroll_frames * 4).max(1);
    let capacity_samples = (capacity_frames as usize) * CHANNELS;

    let (producer, consumer) = RingBuffer::<f32>::new(capacity_samples);
    let shared = Arc::new(SharedState::new());

    let producer_handle = MusicStreamProducer {
        producer,
        shared: Arc::clone(&shared),
        observed_epoch: 0,
    };
    let consumer_handle = StreamingMusicSource {
        consumer,
        shared,
        preroll_frames: preroll_frames as usize,
    };
    (producer_handle, consumer_handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::SymphoniaDecoder;
    use crate::wav::golden::make_pcm16_wav;

    /// テスト用のフェイクデコーダ(`music.rs::tests::FakeSource` と同じ考え方)。
    /// フレーム番号をそのまま値として返すので、順序・シーク位置をサンプル値から検証できる。
    struct FakeDecoder {
        total_frames: Option<u64>,
        cursor: u64,
        remaining_budget: usize,
        seek_calls: Vec<u64>,
        seek_err: bool,
    }

    impl FakeDecoder {
        fn new(total_frames: Option<u64>) -> Self {
            Self {
                total_frames,
                cursor: 0,
                remaining_budget: usize::MAX,
                seek_calls: Vec::new(),
                seek_err: false,
            }
        }
    }

    impl MusicDecoder for FakeDecoder {
        fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
            let want = out.len() / CHANNELS;
            let cap_by_total = match self.total_frames {
                Some(total) => (total.saturating_sub(self.cursor)) as usize,
                None => usize::MAX,
            };
            let n = want.min(self.remaining_budget).min(cap_by_total);
            for i in 0..n {
                let v = self.cursor as f32;
                out[i * CHANNELS] = v;
                out[i * CHANNELS + 1] = v;
                self.cursor += 1;
            }
            if self.remaining_budget != usize::MAX {
                self.remaining_budget -= n;
            }
            Ok(n)
        }

        fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
            self.seek_calls.push(frame);
            if self.seek_err {
                return Err(DecodeError::NoAudioTrack);
            }
            self.cursor = frame;
            Ok(())
        }

        fn total_frames(&self) -> Option<u64> {
            self.total_frames
        }
    }

    const TEST_SAMPLE_RATE: u32 = 1_000;

    fn small_config(preroll_ms: f32) -> Config {
        Config {
            preroll_ms,
            ..Config::default()
        }
    }

    /// 生産側・消費側それぞれ別スレッド(デコードスレッド/音声スレッド)に渡す設計
    /// (モジュール doc)であるためには、両ハンドルが `Send` である必要がある。
    /// コンパイル時に固定化しておく(実行時には何もしない)。
    #[test]
    fn producer_and_source_are_send() {
        fn assert_send<T: Send>() {}
        assert_send::<MusicStreamProducer>();
        assert_send::<StreamingMusicSource>();
    }

    #[test]
    fn pump_then_read_returns_pcm_in_order() {
        let (mut producer, mut source) = channel(small_config(5.0), TEST_SAMPLE_RATE);
        let mut decoder = FakeDecoder::new(None);

        let outcome = producer.pump(&mut decoder).expect("pump must succeed");
        assert!(outcome.pushed_frames > 0);

        let mut out = vec![-1.0f32; 4 * CHANNELS];
        let n = source.read(&mut out);
        assert_eq!(n, 4);
        for i in 0..4 {
            assert_eq!(out[i * CHANNELS], i as f32);
            assert_eq!(out[i * CHANNELS + 1], i as f32);
        }
    }

    #[test]
    fn read_returns_fewer_than_requested_without_panicking_when_buffer_is_empty() {
        let (_producer, mut source) = channel(small_config(5.0), TEST_SAMPLE_RATE);
        // 一度も pump していないので空のまま。
        let mut out = vec![9.0f32; 8 * CHANNELS];
        let n = source.read(&mut out);
        assert_eq!(n, 0);

        // 奇数長(CHANNELS の倍数でない)バッファでもパニックしない。
        let mut odd = vec![9.0f32; 7];
        let n2 = source.read(&mut odd);
        assert_eq!(n2, 0);
    }

    #[test]
    fn is_ready_toggles_around_the_preroll_threshold() {
        // プリロール 5ms @ 1000Hz = 5 フレーム、容量はその4倍の20フレーム。
        //
        // `MusicDecoder::read` の契約(要求より少なく返すのは EOF を意味する)を守った
        // フェイクを使う必要があるため、「まだプリロール未満」の状態は
        // 「供給を絞ったデコーダ」ではなく「一度満杯まで詰めたバッファを読み出して
        // プリロール未満まで減らす」ことで作る(EOF ではない状態を保ったまま)。
        let (mut producer, mut source) = channel(small_config(5.0), TEST_SAMPLE_RATE);
        let mut decoder = FakeDecoder::new(None); // 総フレーム数不明 = 供給は無尽蔵

        assert!(
            !source.is_ready(),
            "must not be ready before any PCM has been buffered"
        );

        producer.pump(&mut decoder).expect("pump must succeed");
        assert!(
            source.is_ready(),
            "must become ready once at least preroll_frames are buffered"
        );

        // プリロール(5フレーム)を割り込むまで読み出す(EOF はまだ来ていない)。
        let mut drain = vec![0.0f32; 16 * CHANNELS];
        let drained = source.read(&mut drain);
        assert!(drained >= 16);
        assert!(
            source.buffered_frames() < 5,
            "test setup must leave fewer than preroll_frames buffered"
        );
        assert!(
            !source.is_ready(),
            "must fall back to not-ready once buffered PCM drops below preroll_frames again"
        );

        // 再度 pump すれば無尽蔵デコーダがまた満杯まで詰め直し、ready に戻る。
        producer.pump(&mut decoder).expect("pump must succeed");
        assert!(
            source.is_ready(),
            "must become ready again once refilled above preroll_frames"
        );
    }

    #[test]
    fn is_ready_becomes_true_at_eof_even_below_preroll_for_short_clips() {
        // 総フレーム数がプリロール未満の短い素材(選曲プレビュー等の極端なケース)。
        let (mut producer, source) = channel(small_config(50.0), TEST_SAMPLE_RATE);
        let mut decoder = FakeDecoder::new(Some(2)); // プリロール(50フレーム)よりずっと短い

        producer.pump(&mut decoder).expect("pump must succeed");
        assert!(
            source.is_ready(),
            "a clip shorter than the preroll must still become ready once fully decoded (EOF)"
        );
    }

    #[test]
    fn total_frames_reflects_the_decoder_when_known_or_unknown() {
        let (mut producer, source) = channel(small_config(5.0), TEST_SAMPLE_RATE);
        let mut decoder = FakeDecoder::new(None);
        producer.pump(&mut decoder).expect("pump must succeed");
        assert_eq!(source.total_frames(), None);

        let (mut producer2, source2) = channel(small_config(5.0), TEST_SAMPLE_RATE);
        let mut decoder2 = FakeDecoder::new(Some(123));
        producer2.pump(&mut decoder2).expect("pump must succeed");
        assert_eq!(source2.total_frames(), Some(123));
    }

    #[test]
    fn request_seek_returns_silence_immediately_and_does_not_leak_old_pcm() {
        let (mut producer, mut source) = channel(small_config(5.0), TEST_SAMPLE_RATE);
        let mut decoder = FakeDecoder::new(None);

        // リングバッファを満杯まで詰める(値 0..capacity-1 の「古い」PCM)。
        producer.pump(&mut decoder).expect("pump must succeed");
        let buffered_before = source.buffered_frames();
        assert!(buffered_before > 0);

        source.request_seek(100);

        // pump がまだ一度も動いていない(= ack 前)ので、リングバッファに残っていた
        // 古い PCM は request_seek 自身の掃除で既に空になっているはず。
        assert_eq!(
            source.buffered_frames(),
            0,
            "request_seek must drain stale PCM immediately"
        );

        // ack 前の read は無音を返す(呼び出し側はアンダーランとして扱う)。
        let mut out = vec![-1.0f32; 4 * CHANNELS];
        let n = source.read(&mut out);
        assert_eq!(n, 0, "read must stay silent until the seek is acknowledged");
    }

    #[test]
    fn seek_does_not_leak_stale_pcm_even_when_the_ring_buffer_was_already_full() {
        // 依頼書 §テスト 5 の核心: 「リングバッファに古いデータが溜まった状態でシークしても
        // 漏れない」ことを明示的に検証する。
        let (mut producer, mut source) = channel(small_config(5.0), TEST_SAMPLE_RATE);
        let mut decoder = FakeDecoder::new(None);

        // 満杯まで詰める(値は 0.. の連番。これが「古い」PCM)。
        producer.pump(&mut decoder).expect("pump must succeed");
        assert!(source.buffered_frames() > 0);
        assert_eq!(decoder.cursor, source.buffered_frames() as u64);

        source.request_seek(500);
        // request_seek の掃除で既に空(前のテストと同じ不変条件)。

        // 生産側がまだシークに気づいていない間の read は無音。
        let mut out = vec![-1.0f32; 2 * CHANNELS];
        assert_eq!(source.read(&mut out), 0);

        // 生産側がシークを処理する: デコーダを 500 へシークし、ack し、その位置から
        // 新しい PCM を詰め直す。
        let outcome = producer.pump(&mut decoder).expect("pump must succeed");
        assert_eq!(decoder.seek_calls, vec![500]);
        assert!(outcome.pushed_frames > 0);

        // ack 後の read は、500 から始まる「新しい」PCM だけを返す(0..capacity-1 の
        // 「古い」PCM が一切混ざらないことを値そのもので確認する)。
        let mut after = vec![-1.0f32; 4 * CHANNELS];
        let n = source.read(&mut after);
        assert_eq!(n, 4);
        for i in 0..4 {
            let expected = 500.0 + i as f32;
            assert_eq!(
                after[i * CHANNELS],
                expected,
                "must not observe stale pre-seek PCM"
            );
            assert_eq!(after[i * CHANNELS + 1], expected);
        }
    }

    #[test]
    fn seek_request_during_playback_is_acked_and_resumes_from_the_new_position() {
        let (mut producer, mut source) = channel(small_config(5.0), TEST_SAMPLE_RATE);
        let mut decoder = FakeDecoder::new(None);

        producer.pump(&mut decoder).expect("pump must succeed");
        let mut warm = vec![0.0f32; 2 * CHANNELS];
        assert_eq!(source.read(&mut warm), 2);

        source.request_seek(42);
        producer.pump(&mut decoder).expect("pump must succeed");

        let mut out = vec![0.0f32; 3 * CHANNELS];
        let n = source.read(&mut out);
        assert_eq!(n, 3);
        assert_eq!(out[0], 42.0);
        assert_eq!(out[CHANNELS], 43.0);
        assert_eq!(out[2 * CHANNELS], 44.0);
    }

    #[test]
    fn decode_error_is_recorded_and_read_stays_silent() {
        struct AlwaysErrorsDecoder;
        impl MusicDecoder for AlwaysErrorsDecoder {
            fn read(&mut self, _out: &mut [f32]) -> Result<usize, DecodeError> {
                Err(DecodeError::NoAudioTrack)
            }
            fn seek(&mut self, _frame: u64) -> Result<(), DecodeError> {
                Ok(())
            }
            fn total_frames(&self) -> Option<u64> {
                None
            }
        }

        let (mut producer, mut source) = channel(small_config(5.0), TEST_SAMPLE_RATE);
        let mut decoder = AlwaysErrorsDecoder;

        let err = producer.pump(&mut decoder).unwrap_err();
        assert!(matches!(err, DecodeError::NoAudioTrack));
        assert!(source.has_stream_error());

        let mut out = vec![-1.0f32; 4 * CHANNELS];
        assert_eq!(source.read(&mut out), 0);
    }

    /// wav の実デコード(`SymphoniaDecoder`)と本モジュールを繋いだ end-to-end テスト
    /// (依頼書のテスト要件1: 「wav をストリーミングデコードして、pump → read で
    /// 元の PCM が順序どおり取り出せる」)。
    #[test]
    fn end_to_end_streams_a_real_wav_through_pump_and_read_in_order() {
        const SAMPLE_RATE: u32 = 48_000;
        const FRAME_COUNT: usize = 500;
        let mut samples = Vec::with_capacity(FRAME_COUNT * 2);
        for i in 0..FRAME_COUNT {
            samples.push(i as i16);
            samples.push(-(i as i16));
        }
        let bytes = make_pcm16_wav(SAMPLE_RATE, 2, &samples);
        let mut decoder = SymphoniaDecoder::open(bytes, SAMPLE_RATE).expect("valid wav must open");

        let (mut producer, mut source) = channel(small_config(20.0), SAMPLE_RATE);

        // EOF に達するまで詰め続ける(リングバッファ容量が全フレームより小さい前提で
        // 何度も pump/read を往復する、実運用に近い形)。
        let mut got: Vec<f32> = Vec::with_capacity(FRAME_COUNT);
        loop {
            let outcome = producer.pump(&mut decoder).expect("pump must succeed");
            let mut buf = vec![0.0f32; 64 * CHANNELS];
            let n = source.read(&mut buf);
            for i in 0..n {
                got.push(buf[i * CHANNELS]);
            }
            if outcome.reached_eof && got.len() >= FRAME_COUNT {
                break;
            }
            if outcome.pushed_frames == 0 && n == 0 && !outcome.reached_eof {
                // 空きが無く読むものも無い(あり得ないはずだが、無限ループ防止の保険)。
                if source.buffered_frames() == 0 {
                    break;
                }
            }
        }

        assert_eq!(got.len(), FRAME_COUNT);
        for (i, &v) in got.iter().enumerate() {
            let expected = i as f32 / 32_768.0;
            assert_eq!(v, expected, "frame {i} out of order");
        }
    }
}
