//! 予約発音のソート済みキューと、ホスト時刻 → バッファ内オフセットの変換
//! (初期構築仕様『§4.5 スケジュール発音』, 確定, M2-5)。
//!
//! 「予約はソート済みキューで保持し、該当バッファのレンダリング時にバッファ内オフセット
//! サンプル位置から発音する(バッファ境界への丸めはしない — 丸めるとキャリブレーションの
//! 計測値そのものが汚れる)」——本モジュールはこの2点を提供する。
//!
//! # 丸め方向(【確定】この関数のコメントを読まずに変更しないこと)
//!
//! [`offset_within_buffer`] は「予約時刻とバッファ先頭時刻の差(ns)× サンプルレート ÷
//! 10^9」を**切り捨て**る。四捨五入でも切り上げでもなく切り捨てを選んだ理由:
//!
//! - 切り捨てなら「予約時刻以降で最初に到来するサンプル」を常に選ぶ。四捨五入だと
//!   端数が 0.5 サンプルを超えるかどうかで「予約時刻より早いサンプル」を選んでしまう
//!   ことがある——キャリブレーション用途では「指定時刻より早く鳴った」方が
//!   「システムが指定より速く反応した」という誤った(実際より良い)印象を与えるため、
//!   常に「指定時刻以降で最短距離のサンプル」という非対称な下限保証にしてある
//! - 整数演算(`u128` 経由)のみを使い、浮動小数点の丸め誤差を混ぜない
//! - 予約時刻を1サンプル分(`1_000_000_000 / sample_rate` ns)ずつ動かすと、
//!   得られるオフセットも単調に1ずつ増える(`tests` の
//!   `offset_advances_by_exactly_one_sample_per_sample_period` で固定化)
//!
//! 誤差は常に `[0, 1)` サンプル分の**遅れ**方向にのみ生じる(早まることはない)。

/// `frames` フレーム分の再生時間をナノ秒で返す(整数演算、切り捨て)。
///
/// `sample_rate == 0`(まだデバイスがオープンされていない)は 0 を返す
/// (音声コールバック経路からの呼び出しでパニックさせないため。`ms_to_samples` と同じ流儀)。
pub(crate) fn buffer_duration_ns(frames: usize, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    ((frames as u128 * 1_000_000_000u128) / sample_rate as u128) as u64
}

/// `target_ns` がバッファ先頭時刻 `buffer_start_ns` から何サンプル後かを返す(切り捨て)。
///
/// `target_ns <= buffer_start_ns`(予約時刻が既に過去)の扱いは呼び出し側の責務
/// (この関数は常に非負の差分として計算する。`saturating_sub` により逆転しても
/// 0 になるだけでパニックはしない)。丸め方向はモジュール doc を参照。
pub(crate) fn offset_within_buffer(
    target_ns: u64,
    buffer_start_ns: u64,
    sample_rate: u32,
) -> usize {
    if sample_rate == 0 {
        return 0;
    }
    let delta_ns = target_ns.saturating_sub(buffer_start_ns);
    ((delta_ns as u128 * sample_rate as u128) / 1_000_000_000u128) as usize
}

/// 固定容量の、`host_time_ns` 昇順ソート済みキュー(初期構築仕様『§4.5』)。
///
/// # リアルタイム安全性
///
/// `Mixer::render`(音声コールバック経路)から [`Self::try_insert`]/[`Self::pop_front`]
/// の両方が呼ばれる。[`Self::with_capacity`](コンストラクタ、ゲームスレッド側)でのみ
/// `Vec::with_capacity` により固定長のバッキング領域を確保し、以後は
/// `entries.len() < capacity` の場合にしか `insert` を呼ばない([`Self::try_insert`]が
/// 容量超過を事前に弾く)。`Vec::insert`/`remove` は空き容量がある限り再アロケーションを
/// 起こさない(要素のシフトのみ)という `Vec` の保証に依拠している——
/// `tests/realtime_safety.rs` の統合テストでこの経路のアロケーションがゼロであることを
/// 実測で固定化してある。
pub(crate) struct ScheduleQueue<T> {
    /// `(host_time_ns, payload)` を `host_time_ns` 昇順に保つ。同時刻は挿入順(FIFO)。
    entries: Vec<(u64, T)>,
    capacity: usize,
}

impl<T> ScheduleQueue<T> {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// 昇順を保って挿入する。容量が尽きていれば挿入せず `false` を返す
    /// (呼び出し元が超過をカウントする。黙って捨てない——初期構築仕様 §4.2 と同じ流儀)。
    pub(crate) fn try_insert(&mut self, host_time_ns: u64, payload: T) -> bool {
        if self.entries.len() >= self.capacity {
            return false;
        }
        let pos = self.entries.partition_point(|(t, _)| *t <= host_time_ns);
        self.entries.insert(pos, (host_time_ns, payload));
        true
    }

    /// 最も早い予約(先頭)を覗き見る。
    pub(crate) fn front(&self) -> Option<&(u64, T)> {
        self.entries.first()
    }

    /// 最も早い予約を取り出す。
    pub(crate) fn pop_front(&mut self) -> Option<(u64, T)> {
        if self.entries.is_empty() {
            None
        } else {
            Some(self.entries.remove(0))
        }
    }

    /// `matches` に一致する未発火の予約をすべて取り除く(`StopVoice`/`StopVoicesUsingSound` によるキャンセル用途)。
    ///
    /// 取り除いた要素の所有権は捨てずに `on_removed` へそのまま渡す——
    /// 呼び出し元(`Mixer::apply_command`)はここで `T`(`ScheduledSe`、内部に
    /// `Arc<SoundData>` を持つ)を直接 drop してはならない。音声スレッド上での
    /// Arc ドロップは §5.3 のリアルタイム安全性規約違反になるため、`on_removed` の中で
    /// 回収キュー(`ReclaimSender::send_or_leak`)へ転送する契約(`mixer.rs` 参照)。
    ///
    /// # リアルタイム安全性
    ///
    /// `Vec::remove` は要素のシフトのみで再アロケーションを起こさない
    /// (`try_insert` と同じ根拠、モジュール doc 参照)。一致した要素をその場で
    /// 取り除きながら前へ詰めるだけなので、`entries` の相対順序(= `host_time_ns` 昇順)は
    /// 保たれる——挿入と違って新しい順序判断が要らないため、この操作自体が不変条件を
    /// 崩すことはない。件数は固定容量(`capacity`)で頭打ちのため、最悪 O(capacity^2) の
    /// シフトコストも音声コールバック1回あたりでは無視できる規模に収まる。
    pub(crate) fn remove_where<F, R>(&mut self, mut matches: F, mut on_removed: R)
    where
        F: FnMut(&T) -> bool,
        R: FnMut(T),
    {
        let mut i = 0;
        while i < self.entries.len() {
            let hit = self
                .entries
                .get(i)
                .is_some_and(|(_, payload)| matches(payload));
            if hit {
                let (_, payload) = self.entries.remove(i);
                on_removed(payload);
                // 後続要素が index i へ詰まるので i は進めない。
            } else {
                i += 1;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_duration_ns_matches_frames_over_rate() {
        assert_eq!(buffer_duration_ns(1_000, 1_000), 1_000_000_000);
        assert_eq!(buffer_duration_ns(480, 48_000), 10_000_000);
        assert_eq!(buffer_duration_ns(0, 48_000), 0);
    }

    #[test]
    fn buffer_duration_ns_is_zero_when_sample_rate_unset() {
        assert_eq!(buffer_duration_ns(256, 0), 0);
    }

    #[test]
    fn offset_within_buffer_is_zero_at_buffer_start() {
        assert_eq!(offset_within_buffer(1_000, 1_000, 48_000), 0);
    }

    #[test]
    fn offset_within_buffer_floors_not_rounds() {
        // 48kHz の1サンプル周期は 1e9/48000 = 20833.333... ns。
        // 端数(0.333...)を四捨五入すれば 0 に丸まるはずの delta でも、
        // 切り捨てポリシーにより「1サンプル未満はまだ 0 サンプル目」を保つ
        // (=時刻ちょうどのサンプル、またはそれ以前には決してならない)。
        assert_eq!(offset_within_buffer(1_000 + 20_833, 1_000, 48_000), 0);
        // 1サンプル周期を超えた瞬間に 1 へ進む(四捨五入なら 20_417 ns 付近で
        // 既に 1 になってしまうところ、切り捨てはちょうど周期を超えるまで 0 のまま)。
        assert_eq!(offset_within_buffer(1_000 + 20_834, 1_000, 48_000), 1);
    }

    #[test]
    fn offset_advances_by_exactly_one_sample_per_sample_period() {
        // 1000Hz なら1サンプル = 1_000_000ns ちょうど(端数無し)なので、
        // 「1サンプルずつずらすと1サンプルずつずれる」ことを厳密に固定化できる。
        for n in 0u64..64 {
            let target = 5_000_000_000u64 + n * 1_000_000;
            assert_eq!(
                offset_within_buffer(target, 5_000_000_000, 1_000),
                n as usize,
                "n={n}"
            );
        }
    }

    #[test]
    fn offset_within_buffer_saturates_to_zero_when_target_is_before_start() {
        // この関数自体は呼び出し側が「過去」を弾く前提だが、逆転してもパニックはしない。
        assert_eq!(offset_within_buffer(100, 1_000, 48_000), 0);
    }

    #[test]
    fn try_insert_keeps_ascending_order_regardless_of_insertion_order() {
        let mut q: ScheduleQueue<u32> = ScheduleQueue::with_capacity(8);
        assert!(q.try_insert(300, 3));
        assert!(q.try_insert(100, 1));
        assert!(q.try_insert(200, 2));

        assert_eq!(q.pop_front(), Some((100, 1)));
        assert_eq!(q.pop_front(), Some((200, 2)));
        assert_eq!(q.pop_front(), Some((300, 3)));
        assert_eq!(q.pop_front(), None);
    }

    #[test]
    fn try_insert_is_fifo_for_equal_timestamps() {
        let mut q: ScheduleQueue<u32> = ScheduleQueue::with_capacity(8);
        assert!(q.try_insert(100, 1));
        assert!(q.try_insert(100, 2));
        assert!(q.try_insert(100, 3));

        assert_eq!(q.pop_front(), Some((100, 1)));
        assert_eq!(q.pop_front(), Some((100, 2)));
        assert_eq!(q.pop_front(), Some((100, 3)));
    }

    #[test]
    fn try_insert_rejects_beyond_capacity_without_dropping_existing_entries() {
        let mut q: ScheduleQueue<u32> = ScheduleQueue::with_capacity(2);
        assert!(q.try_insert(100, 1));
        assert!(q.try_insert(200, 2));
        assert!(!q.try_insert(300, 3), "capacity is exhausted");
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop_front(), Some((100, 1)));
        assert_eq!(q.pop_front(), Some((200, 2)));
    }

    #[test]
    fn front_does_not_remove() {
        let mut q: ScheduleQueue<u32> = ScheduleQueue::with_capacity(4);
        q.try_insert(100, 1);
        assert_eq!(q.front(), Some(&(100, 1)));
        assert_eq!(q.front(), Some(&(100, 1)));
        assert_eq!(q.len(), 1);
    }

    // --- remove_where (StopVoice / StopVoicesUsingSound のキャンセル用途) ---

    #[test]
    fn remove_where_removes_only_matching_entries_and_hands_them_to_on_removed() {
        let mut q: ScheduleQueue<u32> = ScheduleQueue::with_capacity(8);
        q.try_insert(100, 1);
        q.try_insert(200, 2);
        q.try_insert(300, 3);
        q.try_insert(400, 2); // 同じ payload 値が複数回一致してもよい

        let mut removed = Vec::new();
        q.remove_where(|payload| *payload == 2, |payload| removed.push(payload));

        assert_eq!(
            removed,
            vec![2, 2],
            "both matching entries must be handed to on_removed"
        );
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop_front(), Some((100, 1)));
        assert_eq!(q.pop_front(), Some((300, 3)));
        assert_eq!(q.pop_front(), None);
    }

    #[test]
    fn remove_where_preserves_ascending_order_of_survivors() {
        let mut q: ScheduleQueue<u32> = ScheduleQueue::with_capacity(8);
        for (t, v) in [(100, 1), (200, 2), (300, 3), (400, 4), (500, 5)] {
            assert!(q.try_insert(t, v));
        }
        // 先頭・中間・末尾の混在パターンで取り除く(不変条件が壊れやすい境界)。
        q.remove_where(|payload| matches!(payload, 1 | 3 | 5), |_| {});

        assert_eq!(q.len(), 2);
        // 生き残った要素が host_time_ns 昇順のままであることを直接検証する
        // (削除後に fire_due_se が「先頭が未来なら打ち切る」最適化を続けて安全に使える条件)。
        assert_eq!(q.pop_front(), Some((200, 2)));
        assert_eq!(q.pop_front(), Some((400, 4)));
        assert_eq!(q.pop_front(), None);
    }

    #[test]
    fn remove_where_with_no_match_leaves_queue_untouched() {
        let mut q: ScheduleQueue<u32> = ScheduleQueue::with_capacity(8);
        q.try_insert(100, 1);
        q.try_insert(200, 2);

        let mut removed = Vec::new();
        q.remove_where(|payload| *payload == 999, |payload| removed.push(payload));

        assert!(removed.is_empty());
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop_front(), Some((100, 1)));
        assert_eq!(q.pop_front(), Some((200, 2)));
    }

    #[test]
    fn remove_where_can_empty_the_queue_entirely() {
        let mut q: ScheduleQueue<u32> = ScheduleQueue::with_capacity(4);
        q.try_insert(100, 1);
        q.try_insert(200, 1);
        q.try_insert(300, 1);

        let mut removed_count = 0;
        q.remove_where(|_| true, |_| removed_count += 1);

        assert_eq!(removed_count, 3);
        assert_eq!(q.len(), 0);
        assert_eq!(q.front(), None);
    }
}
