//! 音声コールバック経路などから発生するイベント(初期構築仕様『§4.6 イベント通知』, M2-6)。
//!
//! - C → C# のコールバックはしない(初期構築仕様『§2 決定事項サマリ』M4, 確定)。
//!   C# 側が毎フレーム `mw_poll_events` でポーリングする(mw-ffi 側の実装)。
//! - イベントキューは固定容量([`crate::config::Config::event_queue_capacity`]、
//!   【仮】既定 64)。溢れた場合は**古いものから破棄**し、破棄数を次のポーリングで
//!   報告する(黙って落とさない)。
//!
//! # なぜ2系統の書き込み経路があるか
//!
//! イベントの発生源は大きく2つに分かれる:
//!
//! - **音声スレッド**(`mixer.rs::Mixer::render` 経由。`Underrun`/`MusicEnded`/
//!   `MusicLooped`/`ClipperEngaged`)。ここは §5.3 によりロック取得が禁止されるため、
//!   [`EventQueue::push_realtime`] は完全にロックフリー・アロケーション無しでなければならない。
//! - **それ以外の非リアルタイムスレッド**(例: cpal のエラーコールバック。
//!   `crates/mw-backend/src/cpal_backend.rs` の `err_fn` から `StreamError` を積む。
//!   音声スレッドとは別経路であり §5.3 の対象外——既存コードのコメント
//!   「音声スレッドではなく cpal のエラー通知経路から呼ばれる」参照)。ここは
//!   [`EventQueue::push_side_channel`] を使い、`Mutex` で守ってよい。
//!
//! 音声スレッドの書き手は `Mixer` ただ1つに限られる(単一書き手前提)一方、
//! `push_side_channel` は複数の非リアルタイムスレッドから呼ばれても安全なよう
//! `Mutex` で直列化する。両者は完全に独立したストレージを持ち、
//! [`EventQueue::drain`] が読み出し時に両方をマージして返す
//! (音声スレッド系列を優先して読み切ってから非リアルタイム経路を足す。
//! 両者を跨いだ時系列マージは行わない——用途上ほぼ影響が無いため)。
//!
//! # なぜロックフリーの書き込みが「上書き」で実現できるか
//!
//! `rtrb`(コマンドキュー・回収キューで使っている SPSC リングバッファ)は
//! **消費側しか pop できない**という制約があり、そのままでは「満杯になったら
//! 書き手が最古のエントリを破棄する」という初期構築仕様『§4.6』の要件を実現できない
//! (書き手には消費済み位置が見えないため)。
//!
//! そこで本実装は、書き手が「次に書く論理位置」(`write_index`, 単調増加)だけを
//! 進め、対応するスロットへ**常に無条件で上書き**する設計にした。読み手は自分の
//! 読み取り位置(`read_index`)と現在の `write_index` の差を見て、差が容量を超えていれば
//! 「読めないまま上書きされた」件数を逆算し、破棄数として報告する
//! ([`EventQueue::drain`])。書き手は読み手の進捗を一切知る必要が無く、
//! 満杯判定も一切行わない——これにより音声スレッド側は「常に書くだけ」の
//! 最も単純な形になり、ロック・条件分岐によるブロッキングの余地が無くなる。
//!
//! 各スロットは `kind`(種別)と `payload`(付随データ)を1本の `AtomicU64` に
//! 上位8bit/下位56bit で詰めて持つ(`Event::to_bits`/`from_bits`)。**個々のフィールドを
//! 別々の atomic にしなかった**のは、そうすると「新しい kind と古い payload が混ざった」
//! ような不整合なスナップショットを読みうるため(`clock.rs::MusicClockPublisher` が
//! 複数 atomic を seqlock で束ねているのと同じ課題)。1スロット=1回の `AtomicU64` の
//! load/store に単純化すれば、そもそも「複数フィールドの整合を取る」問題自体が
//! 発生しない(1回の読み書きは定義上不可分)。追加の seqlock 機構が丸ごと不要になる
//! ぶん実装が単純になるため、こちらを採用した。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// 出力ストリームのエラー理由(`Event::StreamError` の付随データ)。
///
/// 特定のオーディオバックエンド(cpal 等)の詳細な分類をそのまま持ち込まず、
/// C# 側が実用的に分岐できる粒度へ意図的に丸めてある(詳細な原因は開発ビルドの
/// ログ〔`mw_log!`〕に残る。イベント側は「今後どう振る舞うべきか」の判断材料に絞る)。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorReason {
    /// 上記以外、またはバックエンド固有で分類しきれないエラー。
    Unknown = 0,
    /// 出力デバイス/ホストに到達できない(切断・使用中・ホスト不在)。
    DeviceUnavailable = 1,
    /// ルート変化等でストリーム構成が無効になり、再構築が必要。
    Reconfigured = 2,
    /// OS がデバイスへのアクセスを拒否した。
    PermissionDenied = 3,
    /// 上記以外のバックエンド内部エラー。
    Backend = 4,
}

impl StreamErrorReason {
    /// パック済み `payload` から復元する。未知の値は `Unknown` にフォールバックする
    /// (`EventQueue` 経由の読み出しで理論上到達しないはずだが、パニックはしない)。
    fn from_raw(raw: u32) -> Self {
        match raw {
            1 => Self::DeviceUnavailable,
            2 => Self::Reconfigured,
            3 => Self::PermissionDenied,
            4 => Self::Backend,
            _ => Self::Unknown,
        }
    }
}

/// 音声コールバック経路などから発生するイベント(初期構築仕様『§4.6』)。
///
/// 可変長データは一切持たない。バリアントごとの付随データは固定サイズの数値のみ
/// (mw-ffi 側の blittable 表現 `MwEvent` への変換は `mw-ffi/src/event.rs` を参照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// 出力ルートの変化(初期構築仕様『§6 テンプレートとの連携ポイント』
    /// 「ルート変化イベントの経路」)。テンプレート側のオフセット自動再較正用。
    ///
    /// iOS / tvOS では `crates/mw-backend/src/ios_interruption.rs` が
    /// `AVAudioSessionRouteChangeNotification` を監視して積む(reason を問わず毎回、
    /// M3)。付随データは持たない(reason で復帰要否を判断するロジックは
    /// `ios_interruption.rs` 側に閉じている)。Android では未配線(M3 未着手)。
    RouteChanged,
    /// アンダーラン。`frames` は集約後のフレーム数(`mixer.rs::Mixer::report_underrun`
    /// のコメント参照。1コールバックごとではなく、連続するアンダーランをまとめて
    /// 報告する)。
    Underrun { frames: u32 },
    /// 楽曲の自然終了(総フレーム数に到達)。
    MusicEnded,
    /// 楽曲のループ折り返し。`restart_frame` は折り返し先(曲頭からのフレーム位置)。
    MusicLooped { restart_frame: u64 },
    /// 出力ストリームのエラー。音声スレッドとは別経路(cpal のエラーコールバック等)
    /// から積まれる([`EventQueue::push_side_channel`])。
    StreamError { reason: StreamErrorReason },
    /// Master 段のソフトクリッパが動作した。**開発ビルドのみ発火**
    /// (呼び出し元の `mixer.rs::Mixer::render` が `cfg!(debug_assertions)` で判定する)。
    ClipperEngaged,
    /// OS 主導のオーディオ割り込みが始まった(M3)。iOS の
    /// `AVAudioSessionInterruptionNotification`(`AVAudioSessionInterruptionTypeBegan`)
    /// 相当——電話着信・Siri・他アプリの音声に加え、Background Audio 機能を持たない
    /// アプリがバックグラウンドへ遷移した場合もここに含まれる(実機報告「ホームに
    /// 戻ると SE だけ無音になる」の原因。`crates/mw-backend/src/ios_interruption.rs`
    /// のモジュール doc 参照)。この時点で出力ストリームは(OS 側の都合で)鳴らなく
    /// なっている可能性が高い。
    AudioInterruptionBegan,
    /// 割り込みが終わった、またはミドルウェアが独自に(アプリのアクティブ化を契機に)
    /// 復帰を試みた(M3)。`recovered` はストリーム再開(AVAudioSession 再アクティブ化 +
    /// 出力ユニット再始動)を試みて成功したかどうか。**割り込み終了時に OS が
    /// 再開不要(`AVAudioSessionInterruptionOptionShouldResume` 無し)と判断した場合は
    /// 復帰を試みず `recovered: false` で積む**(何もしていないので「成功していない」
    /// が正確)。
    ///
    /// **iOS/tvOS 専用ではない**: Android(AAudio)切断からのミドルウェア内部再オープン
    /// (`crates/mw-ffi/src/handle.rs::Instance::attempt_reopen`、初期構築仕様『§6』案A、
    /// M3 完了時点で追加)が、再オープンに**成功した**/バックオフを使い切って
    /// **諦めた**、いずれの結果もこのバリアントを再利用して通知する
    /// (`Instance::notify_reopen_outcome`)。新しいバリアントを追加しなかった理由:
    /// 「ストリームレベルの復帰を試みた結果」という意味(`recovered: bool` で
    /// 十分表現できる)が両者で完全に一致するため。iOS/tvOS では
    /// `ios_interruption.rs` の経路のみ、それ以外では `mw-ffi::reopen` の経路のみが
    /// コンパイルされる(`cfg` で排他)ので、C# 側が発生源(OS 割り込み復帰か
    /// デバイス再オープンか)を区別する必要は無い——「音がまた鳴ったか/鳴らなく
    /// なったままか」という受け手にとっての意味は共通のため。
    /// [`Event::AudioInterruptionBegan`] に相当する「開始」通知は Android 側には
    /// 積まない——切断そのものは既に `Event::StreamError { reason:
    /// StreamErrorReason::DeviceUnavailable }` として通知済みで、その役目を兼ねている。
    AudioInterruptionEnded { recovered: bool },
}

/// スロット1件のペイロードに使えるビット数(残り8bitは種別タグ)。
const PAYLOAD_BITS: u32 = 56;
const PAYLOAD_MASK: u64 = (1u64 << PAYLOAD_BITS) - 1;

impl Event {
    fn to_bits(self) -> u64 {
        let (tag, payload): (u8, u64) = match self {
            Event::RouteChanged => (0, 0),
            Event::Underrun { frames } => (1, frames as u64),
            Event::MusicEnded => (2, 0),
            // 56bit あれば 48kHz でも数万年分のフレーム数を表現できるため、実運用では
            // マスクによる切り捨ては起こらない(万一起きても panic はしない)。
            Event::MusicLooped { restart_frame } => (3, restart_frame & PAYLOAD_MASK),
            Event::StreamError { reason } => (4, reason as u64),
            Event::ClipperEngaged => (5, 0),
            Event::AudioInterruptionBegan => (6, 0),
            Event::AudioInterruptionEnded { recovered } => (7, recovered as u64),
        };
        ((tag as u64) << PAYLOAD_BITS) | (payload & PAYLOAD_MASK)
    }

    fn from_bits(bits: u64) -> Self {
        let tag = (bits >> PAYLOAD_BITS) as u8;
        let payload = bits & PAYLOAD_MASK;
        match tag {
            0 => Event::RouteChanged,
            1 => Event::Underrun {
                frames: payload as u32,
            },
            2 => Event::MusicEnded,
            3 => Event::MusicLooped {
                restart_frame: payload,
            },
            4 => Event::StreamError {
                reason: StreamErrorReason::from_raw(payload as u32),
            },
            5 => Event::ClipperEngaged,
            6 => Event::AudioInterruptionBegan,
            // 7、および理論上到達しない値(このスロットへ書き込むのは常にこのモジュール
            // 自身の `to_bits` だけなので tag は 0..=7 のいずれかのはず)の防御的既定。
            _ => Event::AudioInterruptionEnded {
                recovered: payload != 0,
            },
        }
    }
}

/// 固定容量のイベントキュー(初期構築仕様『§4.6』)。
///
/// - 書き込み経路は2系統([`Self::push_realtime`] / [`Self::push_side_channel`]。
///   モジュール doc 参照)。
/// - 読み出しは [`Self::drain`](ゲームスレッド、`mw-ffi::mw_poll_events` から呼ばれる想定。
///   §5.4「全関数スレッドセーフ」のため内部の読み取り位置は `Mutex` で守る——
///   ここは音声スレッドではないのでロックしてよい)。
pub struct EventQueue {
    capacity: usize,
    /// 音声スレッド系列(唯一の書き手は `Mixer`)。各スロットは1回の `AtomicU64`
    /// load/store で読み書きが完結するようパック済み表現を使う(モジュール doc)。
    slots: Box<[AtomicU64]>,
    /// 音声スレッド系列の「次に書く論理位置」(単調増加)。書き手のみが更新する。
    write_index: AtomicU64,
    /// 読み手(ゲームスレッド)の読み取り位置。複数スレッドから同時にポーリングされても
    /// 壊れないよう `Mutex` で守る(§5.4)。
    read_index: Mutex<u64>,
    /// 非リアルタイム経路(cpal のエラーコールバック等)からの書き込み用。
    /// ここは §5.3 の対象外のスレッドなので `Mutex` を使ってよい。
    side_channel: Mutex<VecDeque<Event>>,
    /// `side_channel` が満杯で最古のエントリを破棄した累計回数。`drain` が
    /// 読み出すたびに `swap(0)` でリセットする(「今回新たに判明した破棄数」だけを返すため)。
    side_channel_dropped: AtomicU64,
}

impl EventQueue {
    /// 固定容量で構築する(コンストラクタでのみアロケーションする。`schedule.rs::ScheduleQueue`
    /// と同じ流儀)。`capacity == 0` は 1 に繰り上げる(以後のインデックス演算が
    /// ゼロ除算しないための防御。実運用では `Config::event_queue_capacity` が
    /// 既定 64 を返すため到達しない)。
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            slots: (0..capacity).map(|_| AtomicU64::new(0)).collect(),
            write_index: AtomicU64::new(0),
            read_index: Mutex::new(0),
            side_channel: Mutex::new(VecDeque::with_capacity(capacity)),
            side_channel_dropped: AtomicU64::new(0),
        }
    }

    /// 音声スレッド(唯一の書き手)から呼ぶこと。他のスレッドから呼ぶと、
    /// この構造の単一書き手前提(モジュール doc)が崩れて壊れる。
    ///
    /// リアルタイム安全: ロック取得・アロケーション無し(`AtomicU64` の load/store のみ)。
    /// 満杯かどうかの判定を一切行わず、常に対象スロットへ上書きする
    /// (= 結果的に最古の未読エントリを破棄する。モジュール doc 参照)。
    pub fn push_realtime(&self, event: Event) {
        // 単一書き手前提のため fetch_add は不要(このスレッド以外は書かない)。
        let idx = self.write_index.load(Ordering::Relaxed);
        if let Some(slot) = self.slots.get((idx % self.capacity as u64) as usize) {
            slot.store(event.to_bits(), Ordering::Relaxed);
        }
        // Release: このストアより前のスロット書き込みが、読み手が Acquire で
        // write_index を観測した時点で必ず見えているようにする。
        self.write_index.store(idx + 1, Ordering::Release);
    }

    /// 音声スレッド以外の非リアルタイムスレッドから呼ぶ(例: cpal のエラー
    /// コールバック)。`Mutex` で直列化するため、複数スレッドから呼ばれても安全
    /// (§5.4「全関数スレッドセーフ」)。満杯なら最古のエントリを破棄してから積む。
    pub fn push_side_channel(&self, event: Event) {
        let mut side = self
            .side_channel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if side.len() >= self.capacity {
            side.pop_front();
            self.side_channel_dropped.fetch_add(1, Ordering::Relaxed);
        }
        side.push_back(event);
    }

    /// 溜まっている分を `max` 件を上限として `f` へ渡す。
    ///
    /// 戻り値は `(実際に渡した件数, 今回新たに判明した破棄件数)`。破棄件数は
    /// 「前回までに既に報告済みの分」を含まない、この呼び出しで新たに判明した分だけ
    /// (呼び出し側が累積させる必要はない)。
    ///
    /// 音声スレッド系列を読み切ってから非リアルタイム経路をマージする
    /// (モジュール doc「なぜ2系統」参照。両者を跨いだ時系列マージはしない)。
    pub fn drain(&self, max: usize, mut f: impl FnMut(Event)) -> (usize, u32) {
        let mut read_index = self
            .read_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Acquire: これ以降で読むスロットの内容が、書き手の Release ストア以前に
        // 完了していることを保証する(モジュール doc 参照)。
        let write_index = self.write_index.load(Ordering::Acquire);
        let available = write_index.saturating_sub(*read_index);

        let mut dropped: u64 = 0;
        if available > self.capacity as u64 {
            dropped = available - self.capacity as u64;
            *read_index = write_index - self.capacity as u64;
        }

        let remaining = write_index.saturating_sub(*read_index);
        let n = (remaining as usize).min(max);
        for i in 0..n {
            let idx = *read_index + i as u64;
            let slot_index = (idx % self.capacity as u64) as usize;
            let bits = self
                .slots
                .get(slot_index)
                .map_or(0, |slot| slot.load(Ordering::Relaxed));
            f(Event::from_bits(bits));
        }
        *read_index += n as u64;
        drop(read_index);

        let mut written = n;
        if written < max {
            let mut side = self
                .side_channel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while written < max {
                let Some(event) = side.pop_front() else {
                    break;
                };
                f(event);
                written += 1;
            }
        }
        let side_dropped = self.side_channel_dropped.swap(0, Ordering::Relaxed);

        (
            written,
            (dropped + side_dropped).min(u64::from(u32::MAX)) as u32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_all(queue: &EventQueue, max: usize) -> (Vec<Event>, u32) {
        let mut out = Vec::new();
        let (_, dropped) = queue.drain(max, |e| out.push(e));
        (out, dropped)
    }

    #[test]
    fn to_bits_from_bits_round_trips_every_variant() {
        let variants = [
            Event::RouteChanged,
            Event::Underrun { frames: 12345 },
            Event::MusicEnded,
            Event::MusicLooped {
                restart_frame: 9_876_543,
            },
            Event::StreamError {
                reason: StreamErrorReason::DeviceUnavailable,
            },
            Event::ClipperEngaged,
            Event::AudioInterruptionBegan,
            Event::AudioInterruptionEnded { recovered: true },
            Event::AudioInterruptionEnded { recovered: false },
        ];
        for v in variants {
            assert_eq!(
                Event::from_bits(v.to_bits()),
                v,
                "round trip failed for {v:?}"
            );
        }
    }

    #[test]
    fn stream_error_reason_from_raw_maps_all_abi_values_and_unknown_values() {
        assert_eq!(StreamErrorReason::from_raw(0), StreamErrorReason::Unknown);
        assert_eq!(
            StreamErrorReason::from_raw(1),
            StreamErrorReason::DeviceUnavailable
        );
        assert_eq!(
            StreamErrorReason::from_raw(2),
            StreamErrorReason::Reconfigured
        );
        assert_eq!(
            StreamErrorReason::from_raw(3),
            StreamErrorReason::PermissionDenied
        );
        assert_eq!(StreamErrorReason::from_raw(4), StreamErrorReason::Backend);
        assert_eq!(StreamErrorReason::from_raw(5), StreamErrorReason::Unknown);
        assert_eq!(
            StreamErrorReason::from_raw(u32::MAX),
            StreamErrorReason::Unknown
        );
    }

    #[test]
    fn push_then_drain_returns_events_in_order() {
        let queue = EventQueue::new(4);
        queue.push_realtime(Event::MusicEnded);
        queue.push_realtime(Event::Underrun { frames: 7 });

        let (events, dropped) = drain_all(&queue, 10);
        assert_eq!(
            events,
            vec![Event::MusicEnded, Event::Underrun { frames: 7 }]
        );
        assert_eq!(dropped, 0);
    }

    /// 依頼書のテスト要件3: ポーリングでキューが空になること、2回目のポーリングで
    /// 0件が返ること。
    #[test]
    fn second_poll_after_full_drain_returns_nothing() {
        let queue = EventQueue::new(4);
        queue.push_realtime(Event::MusicEnded);

        let (first, _) = drain_all(&queue, 10);
        assert_eq!(first.len(), 1);

        let (second, dropped) = drain_all(&queue, 10);
        assert!(second.is_empty());
        assert_eq!(dropped, 0);
    }

    /// 依頼書のテスト要件2: 溢れたときに古いものから捨てられ、破棄数が正しく報告されること。
    #[test]
    fn overflow_drops_oldest_first_and_reports_dropped_count() {
        let capacity = 4;
        let queue = EventQueue::new(capacity);
        // 容量(4)を2件超える6件を積む -> 最初の2件(0, 1相当)が上書きされて消える。
        for i in 0..6u32 {
            queue.push_realtime(Event::Underrun { frames: i });
        }

        let (events, dropped) = drain_all(&queue, 10);
        assert_eq!(
            dropped, 2,
            "the 2 oldest entries must be counted as dropped"
        );
        assert_eq!(
            events,
            vec![
                Event::Underrun { frames: 2 },
                Event::Underrun { frames: 3 },
                Event::Underrun { frames: 4 },
                Event::Underrun { frames: 5 },
            ],
            "must keep exactly the newest `capacity` entries, oldest-first"
        );
    }

    /// 依頼書のテスト要件4: 呼び出し側バッファの容量が積まれた件数より少ないとき、
    /// 残りが次回のポーリングで取れること(取りこぼさない)。
    #[test]
    fn partial_drain_leaves_remainder_for_the_next_poll() {
        let queue = EventQueue::new(8);
        for i in 0..5u32 {
            queue.push_realtime(Event::Underrun { frames: i });
        }

        let (first, dropped_first) = drain_all(&queue, 2);
        assert_eq!(
            first,
            vec![Event::Underrun { frames: 0 }, Event::Underrun { frames: 1 }]
        );
        assert_eq!(dropped_first, 0);

        let (second, dropped_second) = drain_all(&queue, 10);
        assert_eq!(
            second,
            vec![
                Event::Underrun { frames: 2 },
                Event::Underrun { frames: 3 },
                Event::Underrun { frames: 4 },
            ],
            "remaining entries must survive to the next poll, not be lost"
        );
        assert_eq!(dropped_second, 0);
    }

    #[test]
    fn side_channel_push_drops_oldest_when_full_and_counts_it() {
        let queue = EventQueue::new(2);
        queue.push_side_channel(Event::StreamError {
            reason: StreamErrorReason::Backend,
        });
        queue.push_side_channel(Event::StreamError {
            reason: StreamErrorReason::DeviceUnavailable,
        });
        // 容量(2)を超える3件目 -> 最古の Backend が破棄される。
        queue.push_side_channel(Event::StreamError {
            reason: StreamErrorReason::PermissionDenied,
        });

        let (events, dropped) = drain_all(&queue, 10);
        assert_eq!(dropped, 1);
        assert_eq!(
            events,
            vec![
                Event::StreamError {
                    reason: StreamErrorReason::DeviceUnavailable
                },
                Event::StreamError {
                    reason: StreamErrorReason::PermissionDenied
                },
            ]
        );
    }

    #[test]
    fn drain_merges_realtime_series_before_side_channel() {
        let queue = EventQueue::new(8);
        queue.push_realtime(Event::MusicEnded);
        queue.push_side_channel(Event::StreamError {
            reason: StreamErrorReason::Unknown,
        });

        let (events, _) = drain_all(&queue, 10);
        assert_eq!(
            events,
            vec![
                Event::MusicEnded,
                Event::StreamError {
                    reason: StreamErrorReason::Unknown
                },
            ]
        );
    }

    #[test]
    fn empty_queue_drains_nothing_without_panicking() {
        let queue = EventQueue::new(4);
        let (events, dropped) = drain_all(&queue, 10);
        assert!(events.is_empty());
        assert_eq!(dropped, 0);
    }

    #[test]
    fn zero_capacity_is_clamped_to_one_without_panicking() {
        let queue = EventQueue::new(0);
        queue.push_realtime(Event::MusicEnded);
        queue.push_realtime(Event::MusicEnded);
        let (events, dropped) = drain_all(&queue, 10);
        assert_eq!(events.len(), 1);
        assert_eq!(dropped, 1);
    }
}
