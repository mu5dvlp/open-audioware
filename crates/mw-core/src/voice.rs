//! ボイスプール(初期構築仕様 §4.2, 【仮】)。
//!
//! - 同時発音数は既定 64([`crate::config::Config::max_voices`])。
//! - 枯渇時は最も古いボイスをスティールする。スティールされたボイスは即座に消音せず、
//!   「尾」スロット([`VoicePool`] の `tails`)へ移し、既定ランプで滑らかにフェードアウト
//!   させながら解放する(M13: 停止・スティールもランプを通す。不連続ゼロを作らない)。
//! - ボイスは PCM データ参照(`Arc<SoundData>`)+ 再生位置 + 音量(ランプ)+ バス +
//!   ループ区間(オプション、下記)を持つ。
//!
//! `VoicePool` はコンストラクタで固定長のバッキング配列を確保した後は、
//! 音声コールバック経路から一切のアロケーションを行わない(§5.3)。
//!
//! # ループ再生([`VoicePool::set_loop`])
//!
//! ホールド音(継続音)のような「押している間ずっと鳴り続ける SE」向けに、ボイス単位で
//! ループ区間を設定できる(初期構築仕様『§4.2』を拡張)。意味論は
//! [`crate::music::MusicVoice::set_loop`] と揃えてある(2つの流儀を生まないため):
//!
//! - 区間は `(開始フレーム, 終了フレーム)` の組(終了は排他的境界)。
//! - `start >= end` の不正な区間は無視し、ループ無し(`None`)として扱う
//!   (リアルタイム安全性のため、不正入力でもパニックしない)。
//! - 解除は `None` を渡す。
//!
//! **`MusicVoice` と異なる点**: 楽曲側の選曲プレビュー用ループは折り返しの瞬間に
//! フェードアウト→フェードインを挟む(任意の位置がループ点になりうるため、
//! クリックノイズを消す目的)。SE のループは「素材自体がループ点で連続するように
//! 作られている継続音」を想定しており、折り返しにクロスフェードを挟まない
//! (単純に位置を巻き戻すだけ。既存の `volume`(`Ramp`)は明示的な `stop`/スティールの
//! フェードアウト専用のまま——ループ折り返しはこれを一切使わない)。

use std::sync::Arc;

use crate::bus::BusId;
use crate::format::CHANNELS;
use crate::ramp::Ramp;
use crate::sound::SoundData;

/// 1ボイスの状態。
#[derive(Debug)]
struct Voice {
    /// 発音時に払い出された不透明なシリアル(0 は「未使用スロット」)。
    /// `tails`(スティール尾)のスロットは外部から直接アドレスされないため、
    /// 常に 0 のままでよい。
    serial: u64,
    /// 再生中の PCM の発行元 ID(`SoundStorage` が払い出した `SoundId.0`)。
    /// `StopVoicesUsingSound` の照合に使う。`Arc::as_ptr` のポインタ比較ではなく
    /// 単調増加する不透明 ID で比較することで、割り当て解放後にアドレスが再利用される
    /// (ABA 問題)可能性を構造的に排除している。
    sound_id: u64,
    sound: Option<Arc<SoundData>>,
    position_frames: usize,
    bus: BusId,
    volume: Ramp,
    /// ループ区間 `(開始フレーム, 終了フレーム)`。`None` ならループしない
    /// (`MusicVoice::loop_region` と同じ意味論。モジュール doc「ループ再生」参照)。
    loop_region: Option<(u64, u64)>,
    /// 明示的な停止 / スティールによるフェードアウト中か。
    /// 自然終了(PCM を最後まで再生し終えた)とは区別する — 自然終了はランプ不要
    /// (§4.2 の対象は「変化」と「停止」であり、単なるデータ末尾到達ではない)。
    stopping: bool,
    /// 挿入順(スティール時の「最古」判定に使う単調増加カウンタ)。
    age: u64,
}

impl Voice {
    const fn empty() -> Self {
        Self {
            serial: 0,
            sound_id: 0,
            sound: None,
            position_frames: 0,
            bus: BusId::Se,
            volume: Ramp::new(0.0),
            loop_region: None,
            stopping: false,
            age: 0,
        }
    }

    fn is_active(&self) -> bool {
        self.sound.is_some()
    }

    /// 1フレーム分の (L, R) を返し、再生位置を進める。データ終端に達していれば
    /// 無音を返す(添字パニックを避けるため `SoundData::frame` の `get` 系経由)。
    ///
    /// ループ区間が設定されている場合、位置が終了フレームへ到達した時点(終了は
    /// 排他的境界)で開始フレームへ折り返してから読む(モジュール doc「ループ再生」)。
    fn next_frame(&mut self) -> (f32, f32) {
        let Some(sound) = self.sound.as_ref() else {
            return (0.0, 0.0);
        };
        if let Some((loop_start, loop_end)) = self.loop_region
            && self.position_frames as u64 >= loop_end
        {
            // `usize::try_from` が失敗する(loop_start が usize に収まらない)ことは
            // 実運用上まず起こらないが、パニックせず安全側(末尾扱い = 以後は無音)に倒す。
            self.position_frames = usize::try_from(loop_start).unwrap_or(usize::MAX);
        }
        match sound.frame(self.position_frames) {
            Some((l, r)) => {
                self.position_frames += 1;
                (l, r)
            }
            None => (0.0, 0.0),
        }
    }

    /// 自然終了(最後まで鳴りきった)かどうか。
    ///
    /// 🔴 **ループ区間が設定されている間は決して自然終了しない。** [`Voice::next_frame`] の
    /// 折り返しは「次に読むとき」に行う遅延方式なので、バッファ境界がちょうどループ終端に
    /// 揃った回では、折り返す前に `position_frames == loop_end` の状態で回収判定
    /// ([`Voice::ready_to_reap`])が走り、**ループ中のボイスが黙って回収されて無音になる**。
    ///
    /// これは机上の懸念ではなく、ホールド保持音(2秒 / 48kHz = 96000 フレーム)で
    /// **必ず**起きる —— 96000 は DSP バッファ 256 の倍数なので、1周目の直後に境界が揃う
    /// (2026-09-02、クライアント側の実装者が `begin=0,end=20` の20フレーム音源で
    ///  20回 `mix_frame` → `reap` すると `reclaimed=1` になることを実測して発見した)。
    ///
    /// ループの停止は明示的な stop でのみ行う、が正しい意味論。
    fn finished_naturally(&self) -> bool {
        match self.sound.as_ref() {
            Some(sound) => self.loop_region.is_none() && self.position_frames >= sound.frames,
            // 音源が無いスロットはループ設定に関わらず回収してよい(取り違えると枠が枯れる)。
            None => true,
        }
    }

    fn ready_to_reap(&self) -> bool {
        self.is_active()
            && (self.finished_naturally() || (self.stopping && self.volume.is_settled()))
    }
}

/// スティール発生時、盗まれた側の「尾」スロットが枯渇していた場合の最終手段の結果。
/// 通常運用では発生しない(尾スロット数は `max_voices` を確保しているため)。
pub enum StealOutcome {
    /// 新しいボイスを空きスロットへ割り当てた(スティール無し)。
    Assigned,
    /// 最古のボイスをスティールし、尾スロットへフェードアウト用に退避した。
    Stolen,
    /// 尾スロットも枯渇していたため、退避できなかった([`VoicePool::render`] 側が
    /// 回収キュー送出などの安全な後始末を行う。理論上到達しない防御的経路)。
    TailPoolExhausted,
}

/// 固定容量のボイスプール。
pub struct VoicePool {
    primary: Vec<Voice>,
    tails: Vec<Voice>,
    next_age: u64,
}

impl VoicePool {
    pub fn new(max_voices: usize, steal_tail_capacity: usize) -> Self {
        let mut primary = Vec::with_capacity(max_voices);
        primary.resize_with(max_voices, Voice::empty);
        let mut tails = Vec::with_capacity(steal_tail_capacity);
        tails.resize_with(steal_tail_capacity, Voice::empty);
        Self {
            primary,
            tails,
            next_age: 1,
        }
    }

    /// 新規ボイスを発音する。空きが無ければ最古のボイスをスティールする。
    ///
    /// `default_ramp_samples` はスティールされた側のフェードアウトに使うランプ長。
    /// 戻り値は診断用([`StealOutcome`])。実際にスティールされたボイスが持っていた
    /// `Arc<SoundData>` の解放は尾スロットのフェード完了後、通常の回収経路
    /// (`VoicePool::render` → 呼び出し元の reclaim キュー)で行われる。
    pub fn play(
        &mut self,
        voice_serial: u64,
        sound_id: u64,
        sound: Arc<SoundData>,
        bus: BusId,
        volume: f32,
        default_ramp_samples: u32,
    ) -> StealOutcome {
        let age = self.next_age;
        self.next_age = self.next_age.wrapping_add(1).max(1);

        if let Some(slot) = self.primary.iter_mut().find(|v| !v.is_active()) {
            *slot = Voice {
                serial: voice_serial,
                sound_id,
                sound: Some(sound),
                position_frames: 0,
                bus,
                volume: Ramp::new(volume),
                loop_region: None,
                stopping: false,
                age,
            };
            return StealOutcome::Assigned;
        }

        // 枯渇: 最古のボイスを探す。
        let oldest_index = self
            .primary
            .iter()
            .enumerate()
            .min_by_key(|(_, v)| v.age)
            .map(|(i, _)| i);

        let Some(oldest_index) = oldest_index else {
            // primary は容量 0(構成ミス)。空きスロット探索と同じ扱いで諦める。
            return StealOutcome::TailPoolExhausted;
        };

        if let Some(tail_slot) = self.tails.iter_mut().find(|v| !v.is_active()) {
            // 既存の再生状態を尾スロットへ退避し、既定ランプで 0 へ向けてフェードアウトさせる。
            let old = std::mem::replace(&mut self.primary[oldest_index], Voice::empty());
            let mut tail = old;
            tail.stopping = true;
            tail.volume.set_target(0.0, default_ramp_samples);
            *tail_slot = tail;

            self.primary[oldest_index] = Voice {
                serial: voice_serial,
                sound_id,
                sound: Some(sound),
                position_frames: 0,
                bus,
                volume: Ramp::new(volume),
                loop_region: None,
                stopping: false,
                age,
            };
            StealOutcome::Stolen
        } else {
            // 理論上到達しない(尾スロット数 >= max_voices)。安全側: 新規ボイスは諦めて
            // 既存の再生を継続する(無音化や Arc ドロップを audio thread で起こさない)。
            StealOutcome::TailPoolExhausted
        }
    }

    pub fn stop(&mut self, voice_serial: u64, ramp_samples: u32) {
        if let Some(v) = self
            .primary
            .iter_mut()
            .find(|v| v.is_active() && v.serial == voice_serial)
        {
            v.stopping = true;
            v.volume.set_target(0.0, ramp_samples);
        }
    }

    pub fn set_volume(&mut self, voice_serial: u64, volume: f32, ramp_samples: u32) {
        if let Some(v) = self
            .primary
            .iter_mut()
            .find(|v| v.is_active() && v.serial == voice_serial && !v.stopping)
        {
            v.volume.set_target(volume, ramp_samples);
        }
    }

    /// ボイスのループ区間を設定・解除する(モジュール doc「ループ再生」)。
    ///
    /// `start >= end` の不正な区間は無視し、ループ無し(`None`)として扱う
    /// (`MusicVoice::set_loop` と同じ意味論。リアルタイム安全性のためパニックしない)。
    /// `set_volume` と同じフィルタ(発音中かつ `stopping`〔明示停止/スティールの
    /// フェードアウト中〕でないボイスのみが対象)——フェードアウトして消えていく
    /// ボイスに新しいループ区間を設定しても意味を持たないため。
    ///
    /// 無効・既に終了したボイス ID は静かに無視される(`stop`/`set_volume` と同じ、
    /// opaque serial 方式の制約。§5.2)。
    pub fn set_loop(&mut self, voice_serial: u64, region: Option<(u64, u64)>) {
        let region = match region {
            Some((start, end)) if start < end => Some((start, end)),
            _ => None,
        };
        if let Some(v) = self
            .primary
            .iter_mut()
            .find(|v| v.is_active() && v.serial == voice_serial && !v.stopping)
        {
            v.loop_region = region;
        }
    }

    /// 現在のループ区間(テスト・診断用)。ボイスが存在しない、または未設定の場合は `None`。
    pub fn loop_region(&self, voice_serial: u64) -> Option<(u64, u64)> {
        self.primary
            .iter()
            .find(|v| v.is_active() && v.serial == voice_serial)
            .and_then(|v| v.loop_region)
    }

    /// `sound_id`(`SoundStorage` が払い出した不透明 ID)が一致する再生中の全ボイスを停止する
    /// (`mw_sound_release` から発行される `StopVoicesUsingSound` の処理。§5「Arc の所有権」)。
    pub fn stop_all_using_sound(&mut self, sound_id: u64, ramp_samples: u32) {
        for v in self.primary.iter_mut().chain(self.tails.iter_mut()) {
            if v.is_active() && !v.stopping && v.sound_id == sound_id {
                v.stopping = true;
                v.volume.set_target(0.0, ramp_samples);
            }
        }
    }

    /// 1フレーム分をミックスする。`bus_volume` は当該フレームでの各バスのランプ値
    /// (呼び出し元が1フレームにつき1回だけ進めて渡す。バス側は複数ボイスに対して
    /// 共有される単一のランプなので、ここでは進めない)。
    ///
    /// 戻り値は (L, R) の未クリップ・未マスタ音量適用の合算値。
    pub fn mix_frame(&mut self, bus_volume: &[f32; crate::bus::BUS_COUNT]) -> (f32, f32) {
        let mut l_sum = 0.0f32;
        let mut r_sum = 0.0f32;
        for v in self.primary.iter_mut().chain(self.tails.iter_mut()) {
            if !v.is_active() {
                continue;
            }
            let (l, r) = v.next_frame();
            let voice_vol = v.volume.advance();
            // Master へ直接アサインされたボイスはサブバスの二重適用を避けるため
            // ここでは常に係数 1.0(Master 自体の音量は Mixer 側で最後に1回だけ適用する)。
            let bus_gain = if matches!(v.bus, BusId::Master) {
                1.0
            } else {
                bus_volume[v.bus.index()]
            };
            l_sum += l * voice_vol * bus_gain;
            r_sum += r * voice_vol * bus_gain;
        }
        (l_sum, r_sum)
    }

    /// 再生終了(自然終了 or 停止ランプ完了)したボイスを回収する。
    /// 保持していた `Arc<SoundData>` を `on_reclaim` へ渡す(呼び出し元が回収キューへ送る)。
    pub fn reap<F: FnMut(Arc<SoundData>)>(&mut self, mut on_reclaim: F) {
        for v in self.primary.iter_mut().chain(self.tails.iter_mut()) {
            if v.ready_to_reap() {
                if let Some(arc) = v.sound.take() {
                    on_reclaim(arc);
                }
                v.serial = 0;
                v.sound_id = 0;
                v.position_frames = 0;
                v.stopping = false;
            }
        }
    }

    /// 現在アクティブなボイス数(primary のみ。テスト・診断用)。
    pub fn active_primary_count(&self) -> usize {
        self.primary.iter().filter(|v| v.is_active()).count()
    }

    /// 現在アクティブな尾スロット数(テスト・診断用)。
    pub fn active_tail_count(&self) -> usize {
        self.tails.iter().filter(|v| v.is_active()).count()
    }
}

/// インターリーブ出力へ書き込むためのフレーム幅(常にステレオ)。
pub const FRAME_WIDTH: usize = CHANNELS;

#[cfg(test)]
mod tests {
    use super::*;

    fn sound(frames: usize) -> Arc<SoundData> {
        // 全サンプル 1.0 の一定値。ミックス数値検証をしやすくする。
        Arc::new(SoundData {
            sample_rate: 48_000,
            frames,
            interleaved: vec![1.0; frames * CHANNELS],
        })
    }

    fn unity_bus_volume() -> [f32; crate::bus::BUS_COUNT] {
        [1.0; crate::bus::BUS_COUNT]
    }

    #[test]
    fn play_assigns_free_slot_and_mixes_immediately() {
        let mut pool = VoicePool::new(2, 2);
        pool.play(1, 100, sound(4), BusId::Se, 0.5, 240);
        let (l, r) = pool.mix_frame(&unity_bus_volume());
        assert_eq!(l, 0.5);
        assert_eq!(r, 0.5);
    }

    #[test]
    fn exhaustion_steals_oldest_voice_via_ramp_no_discontinuity() {
        let mut pool = VoicePool::new(1, 1);
        pool.play(1, 100, sound(1000), BusId::Se, 1.0, 4);
        // 1フレーム分ミックスして最初のボイスの音量が 1.0 で鳴っていることを確認。
        let (l0, _) = pool.mix_frame(&unity_bus_volume());
        assert_eq!(l0, 1.0);

        // プール枯渇 → スティール。
        let outcome = pool.play(2, 101, sound(1000), BusId::Se, 1.0, 4);
        assert!(matches!(outcome, StealOutcome::Stolen));
        assert_eq!(pool.active_tail_count(), 1);

        // 新ボイスは即座にフル音量(定数 1.0)、旧ボイスは尾スロットでランプ中。
        // 合算値は「新ボイス 1.0 + 旧ボイスの残り音量」なので、単調非増加かつ
        // 4サンプル後には新ボイス分の 1.0 だけに収束する(不連続ゼロなし = 途中でジャンプしない)。
        let mut prev = f32::MAX;
        let mut samples = Vec::new();
        for _ in 0..4 {
            let (l, _r) = pool.mix_frame(&unity_bus_volume());
            assert!(
                l <= prev + 1e-6,
                "combined output must be monotonically non-increasing while stealing fades out"
            );
            prev = l;
            samples.push(l);
        }
        assert_eq!(samples, vec![1.75, 1.5, 1.25, 1.0]);

        pool.reap(|_arc| {});
        assert_eq!(
            pool.active_tail_count(),
            0,
            "tail must be reaped once its ramp settles at 0"
        );
    }

    #[test]
    fn stop_ramps_to_zero_then_becomes_reapable() {
        let mut pool = VoicePool::new(1, 1);
        pool.play(1, 100, sound(1000), BusId::Se, 1.0, 4);
        pool.stop(1, 4);

        let mut last = 1.0;
        for _ in 0..4 {
            let (l, _) = pool.mix_frame(&unity_bus_volume());
            assert!(l <= last + 1e-6, "stop ramp must be non-increasing");
            last = l;
        }
        assert_eq!(last, 0.0);

        let mut reclaimed = 0;
        pool.reap(|_| reclaimed += 1);
        assert_eq!(reclaimed, 1);
        assert_eq!(pool.active_primary_count(), 0);
    }

    #[test]
    fn natural_end_of_data_reaps_without_needing_a_ramp() {
        let mut pool = VoicePool::new(1, 1);
        pool.play(1, 100, sound(2), BusId::Se, 1.0, 240);
        let _ = pool.mix_frame(&unity_bus_volume());
        let _ = pool.mix_frame(&unity_bus_volume());
        // データを使い切った(2フレーム)。
        let mut reclaimed = 0;
        pool.reap(|_| reclaimed += 1);
        assert_eq!(reclaimed, 1);
    }

    /// `sound()` と違い、フレーム `i` の (L, R) が `i` そのものになる一定でない PCM。
    /// ループ折り返しがどの位置から再開したかをサンプル値そのものから検証できる。
    fn indexed_sound(frames: usize) -> Arc<SoundData> {
        let mut interleaved = vec![0.0; frames * CHANNELS];
        for i in 0..frames {
            interleaved[i * CHANNELS] = i as f32;
            interleaved[i * CHANNELS + 1] = i as f32;
        }
        Arc::new(SoundData {
            sample_rate: 48_000,
            frames,
            interleaved,
        })
    }

    #[test]
    fn set_loop_rejects_invalid_region() {
        let mut pool = VoicePool::new(1, 1);
        pool.play(1, 100, sound(1000), BusId::Se, 1.0, 4);

        pool.set_loop(1, Some((10, 10)));
        assert_eq!(pool.loop_region(1), None);

        pool.set_loop(1, Some((10, 5)));
        assert_eq!(pool.loop_region(1), None);

        pool.set_loop(1, Some((5, 10)));
        assert_eq!(pool.loop_region(1), Some((5, 10)));
    }

    #[test]
    fn set_loop_clears_with_none() {
        let mut pool = VoicePool::new(1, 1);
        pool.play(1, 100, sound(1000), BusId::Se, 1.0, 4);
        pool.set_loop(1, Some((5, 10)));
        assert_eq!(pool.loop_region(1), Some((5, 10)));

        pool.set_loop(1, None);
        assert_eq!(pool.loop_region(1), None);
    }

    #[test]
    fn set_loop_on_unknown_voice_is_silently_ignored() {
        let mut pool = VoicePool::new(1, 1);
        pool.play(1, 100, sound(1000), BusId::Se, 1.0, 4);
        // 未知のシリアル: パニックせず、既存ボイスにも影響しない。
        pool.set_loop(999, Some((5, 10)));
        assert_eq!(pool.loop_region(999), None);
        assert_eq!(pool.loop_region(1), None);
    }

    #[test]
    fn set_loop_is_ignored_once_voice_is_stopping() {
        // `set_volume` と同じフィルタ: フェードアウト中のボイスへ新しいループ区間を
        // 設定しても無視される(消えていくボイスに意味を持たせないため)。
        let mut pool = VoicePool::new(1, 1);
        pool.play(1, 100, sound(1000), BusId::Se, 1.0, 4);
        pool.stop(1, 4);
        pool.set_loop(1, Some((5, 10)));
        assert_eq!(pool.loop_region(1), None);
    }

    /// ループ境界をまたいで正しくサンプルが出ること(依頼書のテスト要件)。
    /// 区間 `[5, 10)` でループさせ、位置の実際の遷移をサンプル値(L=フレーム番号)から
    /// 直接検証する: 0,1,2,3,4 (助走) → 5,6,7,8,9 (ループ1周目) → 5,6,7,8,9 (2周目) → ...
    #[test]
    fn loop_region_wraps_and_produces_correct_samples_across_the_boundary() {
        let mut pool = VoicePool::new(1, 1);
        pool.play(1, 100, indexed_sound(20), BusId::Se, 1.0, 4);
        pool.set_loop(1, Some((5, 10)));

        let mut observed = Vec::new();
        for _ in 0..17 {
            let (l, r) = pool.mix_frame(&unity_bus_volume());
            assert_eq!(l, r, "L/R must match for this fixture");
            observed.push(l);
        }

        assert_eq!(
            observed,
            vec![
                0.0, 1.0, 2.0, 3.0, 4.0, // 助走(ループ区間の外)
                5.0, 6.0, 7.0, 8.0, 9.0, // 1周目
                5.0, 6.0, 7.0, 8.0, 9.0, // 2周目
                5.0, 6.0, // 3周目の途中
            ],
            "position must wrap to the loop start exactly at the exclusive end boundary"
        );
    }

    /// ループ設定済みのボイスは、`sound.frames` を超えて延々ループし続けても
    /// 自然終了(`ready_to_reap`)しない——ホールド保持音が明示的な `stop` まで
    /// 鳴り続け続けるための前提。
    #[test]
    fn looping_voice_never_naturally_reaps_while_looping() {
        let mut pool = VoicePool::new(1, 1);
        pool.play(1, 100, indexed_sound(20), BusId::Se, 1.0, 4);
        pool.set_loop(1, Some((0, 4)));

        for _ in 0..100 {
            let _ = pool.mix_frame(&unity_bus_volume());
        }

        let mut reclaimed = 0;
        pool.reap(|_| reclaimed += 1);
        assert_eq!(
            reclaimed, 0,
            "a looping voice must not be reaped just because it has cycled past its data length"
        );
        assert_eq!(pool.active_primary_count(), 1);
    }

    /// 🔴 ループ終端が**音源の長さちょうど**(= 音源全体をループ)で、かつ描画したフレーム数が
    /// その境界にぴったり揃った回の回帰テスト。
    ///
    /// 上の `looping_voice_never_naturally_reaps_while_looping` はループ区間が音源の内側
    /// (20フレーム中の 0..4)だったため `position_frames` が音源長へ到達せず、**この壊れ方を
    /// 踏んでいなかった**(テストは緑のまま、実際には回収されていた)。
    ///
    /// 「音源全体をループ」は最も素直な指定で、ホールド保持音がまさにこれ
    /// (2秒 / 48kHz = 96000 フレームは DSP バッファ 256 の倍数なので**必ず境界が揃う**)。
    #[test]
    fn looping_whole_sound_is_not_reaped_when_buffer_ends_exactly_on_the_loop_end() {
        let mut pool = VoicePool::new(1, 1);
        let frames = 20usize;
        pool.play(1, 100, indexed_sound(frames), BusId::Se, 1.0, 4);
        pool.set_loop(1, Some((0, frames as u64)));

        // 音源長ちょうどぶんだけ描画する(= バッファ境界がループ終端に揃った状態)。
        for _ in 0..frames {
            let _ = pool.mix_frame(&unity_bus_volume());
        }

        let mut reclaimed = 0;
        pool.reap(|_| reclaimed += 1);
        assert_eq!(
            reclaimed, 0,
            "音源全体をループしているボイスが、折り返す前の回収判定で消えてはいけない\
             (これが起きるとホールド保持音が1周目の直後に必ず無音になる)"
        );
        assert_eq!(pool.active_primary_count(), 1);

        // 折り返して先頭から鳴り続けること(無音落ちしていないことの裏取り)。
        // ⚠️ `indexed_sound` は**フレーム0 の値が 0.0** なので、折り返し直後の1フレーム目で
        //    非ゼロを期待してはいけない(最初にそう書いて落ちた)。2フレーム目(値 1.0)を見る。
        let (first_after_wrap, _) = pool.mix_frame(&unity_bus_volume());
        let (second_after_wrap, _) = pool.mix_frame(&unity_bus_volume());
        assert_eq!(
            first_after_wrap, 0.0,
            "折り返し後の1フレーム目は音源の先頭(indexed_sound のフレーム0 = 0.0)のはず"
        );
        assert!(
            second_after_wrap.abs() > 0.0,
            "折り返し後に無音のままになっている(ループが機能していない)"
        );
    }

    #[test]
    fn stop_all_using_sound_stops_every_matching_voice() {
        let mut pool = VoicePool::new(4, 4);
        let shared = sound(1000);
        let shared_id = 200u64;
        pool.play(1, shared_id, Arc::clone(&shared), BusId::Se, 1.0, 4);
        pool.play(2, shared_id, Arc::clone(&shared), BusId::Voice, 1.0, 4);
        pool.play(3, 999, sound(1000), BusId::Se, 1.0, 4); // 別サウンド、影響を受けない

        pool.stop_all_using_sound(shared_id, 1);
        let _ = pool.mix_frame(&unity_bus_volume());

        let mut reclaimed = 0;
        pool.reap(|_| reclaimed += 1);
        assert_eq!(
            reclaimed, 2,
            "only the two voices sharing `shared` must be stopped"
        );
        assert_eq!(pool.active_primary_count(), 1);
    }
}
