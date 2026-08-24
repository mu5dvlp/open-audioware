//! 楽曲のデコードスレッド(初期構築仕様『§5.2 スレッドモデル』)。
//!
//! `mw_core::stream` のモジュール doc にある通り、mw-core は意図的にスレッドを
//! 一切生成しない。`MusicStreamProducer::pump` を定期的に(あるいは起こされて)
//! 呼ぶ責務は mw-ffi 側にある、とそこに明記されている——それがこのモジュール。
//!
//! ## スレッドは `mw_init` で1本だけ、デコーダはチャネル越しに差し替える
//!
//! `mw_music_set` のたびにスレッドを立て直す設計も考えられるが、曲切り替えは
//! プレイ中にも起こりうる操作であり、そのたびに OS のスレッド生成コスト
//! (スタック確保・カーネル呼び出し)を払うのは無駄が大きい。そこで
//! `mw_init` で1本だけ立てて `mw_shutdown` まで生かし続け、新しい曲は
//! [`mpsc::Sender<Box<dyn MusicDecoder + Send>>`] 経由でこのスレッドへ「差し替え」
//! として渡す方式にした(オーケストレータの決定)。`MusicStreamProducer` は
//! このスレッドへムーブしたまま使い回す。
//!
//! ## 待ち方: 条件変数ではなく短い `sleep` によるポーリング
//!
//! - このスレッドが起きるべき理由は2つある: (1) 新しいデコーダが届いた
//!   (`mw_music_set`)、(2) リングバッファに空きができた(音声コールバックが
//!   消費した)。後者は音声コールバック側(§5.3: ロック取得禁止)から通知できない
//!   ため、そもそも条件変数で賄えるのは前者だけ。両方を1つの Condvar に
//!   束ねようとすると「デコーダ到着で起きたのに実は空きが無い」
//!   「空きができたはずなのに誰も notify しない」を作り込む余地が増えるだけで、
//!   単純なポーリングに対して得られる利点(消費電力・レイテンシ)が
//!   この用途では小さい。
//! - デコード処理自体(Symphonia の1パケット読み)は数百µs〜数ms程度で、
//!   ビジーウェイトにするほどシビアなタイミング精度は要らない。
//!
//! ## ポーリング間隔の根拠(【仮】、[`POLL_INTERVAL`] の1箇所に集約)
//!
//! リングバッファ容量は `preroll_ms`(既定 100ms)の4倍 = 400ms 分
//! (`mw_core::stream::channel` 参照)。音声コールバックは定常再生時 1x 実時間で
//! このリングバッファを消費するので、**ポーリング間隔がリングバッファの持続時間
//! より十分短ければ枯れない**(pump は毎回「空いているだけ」詰め直すので、
//! 間隔が短いほど枯渇までの余裕(=許容できるスケジューリング遅延)が大きくなる)。
//! [`POLL_INTERVAL`] の 10ms は 400ms の 1/40 —— OS のスケジューリング遅延や
//! デコード自体の処理時間を差し引いても十分な安全マージンがあり、かつ
//! 「ビジースピンではない」と言える長さとして選んだ。実測(M2 後半)で見直す。
//!
//! ## パニック安全性(初期構築仕様 §5.4 の精神をデコードスレッドにも適用)
//!
//! Symphonia は外部クレートであり、壊れた入力(いたずら・破損ファイル)に対して
//! パニックしうる前提で扱う。`pump` の呼び出しを `catch_unwind` で受け止め、
//! パニックした場合はプロセスを巻き込まず `StreamError` イベント
//! (`mw_core::Event`/`EventQueue::push_side_channel`)に変換する——
//! `mw-backend::CpalBackend` の cpal `err_fn` が同じことをしているのと同じ思想。
//! パニックしたデコーダは内部状態が不定になりうるため以後使い回さず捨てる
//! (次の `mw_music_set` を待つ)。一方 [`MusicStreamProducer`] 自体は使い回して
//! 問題ない: `rtrb::WriteChunk` は commit されなかった書き込みを `Drop` で
//! 安全に破棄する設計(`rtrb` のソース確認済み)なので、`pump` の途中(chunk を
//! commit する前)でパニックしても内部のリングバッファは壊れない。

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mw_core::{Event, EventQueue, MusicDecoder, MusicStreamProducer, StreamErrorReason};

/// `pump()` を呼ぶ間隔(【仮】)。根拠はモジュール doc 参照。ここを書き換えれば
/// デコードスレッド全体の挙動に反映される。
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 新しいデコーダをデコードスレッドへ渡すハンドル(`mw_music_set` から使う)。
pub type DecoderSender = mpsc::Sender<Box<dyn MusicDecoder + Send>>;

/// デコードスレッドを1本立てる。
///
/// 戻り値は `(デコーダ送信ハンドル, 停止フラグ, join ハンドル)`。`mw_shutdown` は
/// 停止フラグを立てたうえで join ハンドルを必ず join すること(スレッドリークを
/// 防ぐ。最大で [`POLL_INTERVAL`] 分だけ待たされうるが、これは一度きりの
/// 終了処理であり毎フレーム呼ぶ非ブロッキング関数群〔初期構築仕様『§5.4』〕の
/// 対象外)。
pub fn spawn(
    mut producer: MusicStreamProducer,
    events: Arc<EventQueue>,
) -> (DecoderSender, Arc<AtomicBool>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Box<dyn MusicDecoder + Send>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);

    let join_handle = thread::spawn(move || {
        // 現在再生中の曲のデコーダ。`None` は「まだ何も設定されていない」
        // (`mw_music_set` 未呼び出し、またはパニックで捨てた直後)。
        let mut current: Option<Box<dyn MusicDecoder + Send>> = None;
        // 直近の pump() が Err を返した状態を既に `StreamError` として報告済みか。
        // 同じ壊れたデコーダに対して毎ポーリング(10ms間隔)積み続けると、固定容量
        // (既定64)のイベントキューをあっという間に溢れさせて他の重要なイベント
        // (MusicEnded 等)を押し出してしまう。新しいデコーダに差し替わったら
        // 忘れてよい(次のエラーはまた1回だけ報告する)。
        let mut error_already_reported = false;

        while !stop_for_thread.load(Ordering::Relaxed) {
            // 溜まっていれば最新のものを採用する(mw_music_set を連打された場合、
            // 一番新しい要求を優先するのが自然な挙動のため)。
            while let Ok(next) = rx.try_recv() {
                current = Some(next);
                error_already_reported = false;
            }

            if let Some(decoder) = current.as_mut() {
                let decoder_ref: &mut dyn MusicDecoder = decoder.as_mut();
                // AssertUnwindSafe: `producer`/`decoder_ref` への `&mut` 借用はデフォルトでは
                // UnwindSafe ではない(catch_unwind の一般的な制約)。パニック時に
                // 中途半端な状態が残ってもこのスレッド内で完結して処理する
                // (モジュール doc「パニック安全性」参照)ため、ここでのアサートは妥当。
                let pump_result =
                    panic::catch_unwind(AssertUnwindSafe(|| producer.pump(decoder_ref)));
                match pump_result {
                    Ok(Ok(_outcome)) => {
                        error_already_reported = false;
                    }
                    Ok(Err(decode_err)) => {
                        if !error_already_reported {
                            mw_backend::mw_log!(
                                "[mw-ffi] decode thread: pump failed: {decode_err}"
                            );
                            events.push_side_channel(Event::StreamError {
                                reason: StreamErrorReason::Backend,
                            });
                            error_already_reported = true;
                        }
                    }
                    Err(panic_payload) => {
                        mw_backend::mw_log!(
                            "[mw-ffi] decode thread: pump panicked: {}",
                            panic_message(&panic_payload)
                        );
                        events.push_side_channel(Event::StreamError {
                            reason: StreamErrorReason::Backend,
                        });
                        // 内部状態が不定なデコーダは使い回さない(次の mw_music_set を
                        // 待つ)。`producer`(リングバッファ)自体はパニック後も安全に
                        // 使い回せる(モジュール doc 参照)ため、ここでは破棄しない。
                        current = None;
                        error_already_reported = true;
                    }
                }
            }
            // current が None(未設定)の間は何もせず、ただ待つ(busy-spin しない)。

            thread::sleep(POLL_INTERVAL);
        }
    });

    (tx, stop, join_handle)
}

/// panic ペイロードから人間が読めるメッセージを取り出す(`&str`/`String` の
/// どちらでパニックしても拾えるようにする。標準的な `std::panic::set_hook` の
/// 実装が使う判定と同じ)。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_core::{CHANNELS, Config, DecodeError};
    use std::time::Instant;

    /// 定数値を返し続けるフェイクデコーダ(`mw_core::stream::tests::FakeDecoder` と
    /// 同じ考え方)。`panic_after` 回目の `read` でパニックさせて、パニック安全性の
    /// 検証に使う。
    struct FakeDecoder {
        value: f32,
        reads: usize,
        panic_after: Option<usize>,
        always_errors: bool,
    }

    impl MusicDecoder for FakeDecoder {
        fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
            self.reads += 1;
            if self.always_errors {
                return Err(DecodeError::NoAudioTrack);
            }
            if Some(self.reads) == self.panic_after {
                panic!("intentional panic for decode_thread test");
            }
            let n = out.len() / CHANNELS;
            for i in 0..n {
                out[i * CHANNELS] = self.value;
                out[i * CHANNELS + 1] = self.value;
            }
            Ok(n)
        }

        fn seek(&mut self, _frame: u64) -> Result<(), DecodeError> {
            Ok(())
        }

        fn total_frames(&self) -> Option<u64> {
            None
        }
    }

    /// 何度もポーリングして `f` が満たされるまで待つ(タイムアウト付き)。
    /// スレッドのタイミング依存テストなので、固定 sleep ではなくポーリングで
    /// 待つことでフレーク耐性を上げる。
    fn wait_until(mut f: impl FnMut() -> bool, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            if f() {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn decoder_set_before_spawn_is_used_and_fills_the_ring_buffer() {
        let (producer, source) = mw_core::stream::channel(Config::default(), 1_000);
        let events = Arc::new(EventQueue::new(4));
        let (tx, stop, handle) = spawn(producer, Arc::clone(&events));

        tx.send(Box::new(FakeDecoder {
            value: 1.0,
            reads: 0,
            panic_after: None,
            always_errors: false,
        }))
        .expect("decode thread must still be receiving");

        assert!(
            wait_until(|| source.buffered_frames() > 0, Duration::from_secs(2)),
            "decode thread must pump PCM into the ring buffer once a decoder is set"
        );

        stop.store(true, Ordering::Relaxed);
        handle.join().expect("decode thread must not panic itself");
    }

    #[test]
    fn no_decoder_set_leaves_the_ring_buffer_empty_without_panicking() {
        let (producer, source) = mw_core::stream::channel(Config::default(), 1_000);
        let events = Arc::new(EventQueue::new(4));
        let (_tx, stop, handle) = spawn(producer, events);

        // 何も送らずしばらく待つ: busy-spin していないことの間接検証も兼ねる
        // (CPU を使い切っていればこの sleep 自体が極端に遅延するはずだが、
        // 決定的な検証ではないため主眼はあくまで「クラッシュしない・詰まらない」)。
        thread::sleep(POLL_INTERVAL * 3);
        assert_eq!(source.buffered_frames(), 0);

        stop.store(true, Ordering::Relaxed);
        handle.join().expect("decode thread must not panic itself");
    }

    #[test]
    fn a_panicking_decoder_is_caught_reported_as_stream_error_and_discarded() {
        let (producer, _source) = mw_core::stream::channel(Config::default(), 1_000);
        let events = Arc::new(EventQueue::new(4));
        let (tx, stop, handle) = spawn(producer, Arc::clone(&events));

        tx.send(Box::new(FakeDecoder {
            value: 1.0,
            reads: 0,
            panic_after: Some(1),
            always_errors: false,
        }))
        .expect("decode thread must still be receiving");

        let reported = wait_until(
            || {
                let mut found = false;
                events.drain(8, |e| {
                    if matches!(e, Event::StreamError { .. }) {
                        found = true;
                    }
                });
                found
            },
            Duration::from_secs(2),
        );
        assert!(
            reported,
            "a panic inside pump() must be caught and reported as a StreamError event, \
             not crash the process"
        );

        stop.store(true, Ordering::Relaxed);
        handle.join().expect(
            "the decode thread itself must survive a panic inside pump() (catch_unwind boundary)",
        );
    }

    #[test]
    fn a_persistently_failing_decoder_reports_stream_error_only_once() {
        let (producer, _source) = mw_core::stream::channel(Config::default(), 1_000);
        let events = Arc::new(EventQueue::new(64));
        let (tx, stop, handle) = spawn(producer, Arc::clone(&events));

        tx.send(Box::new(FakeDecoder {
            value: 1.0,
            reads: 0,
            panic_after: None,
            always_errors: true,
        }))
        .expect("decode thread must still be receiving");

        // 複数ポーリングぶん待つ(常にエラーを返すデコーダなので、抑制が無ければ
        // 毎ポーリングごとに1件ずつ積まれ続けるはず)。
        thread::sleep(POLL_INTERVAL * 5);

        let mut count = 0usize;
        events.drain(64, |e| {
            if matches!(e, Event::StreamError { .. }) {
                count += 1;
            }
        });
        assert_eq!(
            count, 1,
            "a persistently-erroring decoder must be reported once, not flood the event queue"
        );

        stop.store(true, Ordering::Relaxed);
        handle.join().expect("decode thread must not panic itself");
    }

    #[test]
    fn stop_flag_causes_the_thread_to_exit_and_join_promptly() {
        let (producer, _source) = mw_core::stream::channel(Config::default(), 1_000);
        let events = Arc::new(EventQueue::new(4));
        let (_tx, stop, handle) = spawn(producer, events);

        stop.store(true, Ordering::Relaxed);
        let start = Instant::now();
        handle.join().expect("decode thread must not panic itself");
        // POLL_INTERVAL の数倍程度で確実に抜けるはず(busy-spin ではないが、
        // 無限に近い長さで詰まってもいないことの緩い検証)。
        assert!(start.elapsed() < POLL_INTERVAL * 20);
    }
}
