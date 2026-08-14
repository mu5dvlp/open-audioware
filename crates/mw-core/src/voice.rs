//! ボイスプール(初期構築仕様 §4.2, 【仮】)。
//!
//! - 同時発音数は既定 64([`crate::config::Config::max_voices`])。
//! - 枯渇時は最も古いボイスをスティールする。スティールされたボイスは即座に消音せず、
//!   「尾」スロット([`VoicePool`] の `tails`)へ移し、既定ランプで滑らかにフェードアウト
//!   させながら解放する(M13: 停止・スティールもランプを通す。不連続ゼロを作らない)。
//! - ボイスは PCM データ参照(`Arc<SoundData>`)+ 再生位置 + 音量(ランプ)+ バスを持つ。
//!
//! `VoicePool` はコンストラクタで固定長のバッキング配列を確保した後は、
//! 音声コールバック経路から一切のアロケーションを行わない(§5.3)。

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
            stopping: false,
            age: 0,
        }
    }

    fn is_active(&self) -> bool {
        self.sound.is_some()
    }

    /// 1フレーム分の (L, R) を返し、再生位置を進める。データ終端に達していれば
    /// 無音を返す(添字パニックを避けるため `SoundData::frame` の `get` 系経由)。
    fn next_frame(&mut self) -> (f32, f32) {
        let Some(sound) = self.sound.as_ref() else {
            return (0.0, 0.0);
        };
        match sound.frame(self.position_frames) {
            Some((l, r)) => {
                self.position_frames += 1;
                (l, r)
            }
            None => (0.0, 0.0),
        }
    }

    fn finished_naturally(&self) -> bool {
        match self.sound.as_ref() {
            Some(sound) => self.position_frames >= sound.frames,
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
