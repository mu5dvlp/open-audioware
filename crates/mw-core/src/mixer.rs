//! ミキサ(初期構築仕様 §4.1「ミキサ」/ §5.2)。
//!
//! 「アクティブボイス合算 → バス音量 → Master → クリッパ」の順で処理する。
//! 音声スレッド相当経路(`Mixer::render`)でのアロケーション・ロックは禁止(§5.3)。
//! コンストラクタ([`build`])でのみ固定長バッファ・キューを確保する。

use std::sync::{Arc, Mutex};

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::bus::{ALL_BUSES, BUS_COUNT, BusId, BusSet};
use crate::clipper::SoftClipper;
use crate::command::Command;
use crate::config::Config;
use crate::format::CHANNELS;
use crate::ramp::ms_to_samples;
use crate::sound::SoundData;
use crate::voice::{StealOutcome, VoicePool};

/// ゲームスレッド側から音声スレッドへコマンドを送るハンドル。
///
/// 複数スレッドから並行に呼ばれても安全なよう、内部の `rtrb::Producer` を
/// `Mutex` で包む(§5.4: 「全関数スレッドセーフ」。これはゲームスレッド側のコードであり、
/// 音声コールバック経路の対象外なので `Mutex` を使ってよい。§5.3 が禁止するのは
/// 音声スレッド側でのロック取得のみ)。
pub struct CommandSender {
    producer: Mutex<Producer<Command>>,
}

impl CommandSender {
    /// コマンドをキューへ積む。キューが満杯の場合は `false`(呼び出し元がエラーとして
    /// 扱う。初期構築仕様 §4.2「次のコールバックで必ず発音」を守るため、黙って捨てない)。
    pub fn send(&self, command: Command) -> bool {
        let mut guard = self
            .producer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.push(command) {
            Ok(()) => true,
            Err(PushError::Full(_)) => false,
        }
    }
}

/// 音声スレッドが手放した `Arc<SoundData>` をゲームスレッド側で回収するハンドル。
///
/// 「コールバック内での Arc ドロップ禁止(解放はゲームスレッド側の回収経路で)」を
/// 実現する受け口。呼び出し側(mw-ffi)が FFI 呼び出しの合間に `drain()` を呼び、
/// 実際のデアロケーションをゲームスレッド上で発生させる。
pub struct ReclaimReceiver {
    consumer: Consumer<Arc<SoundData>>,
}

impl ReclaimReceiver {
    /// 溜まっている分をすべて回収してドロップする。ゲームスレッド専用
    /// (音声コールバック経路からは絶対に呼ばないこと)。
    pub fn drain(&mut self) {
        while self.consumer.pop().is_ok() {}
    }

    /// 現在キューに溜まっている回収待ち件数(テスト・診断用)。
    pub fn pending_len(&self) -> usize {
        self.consumer.slots()
    }
}

/// 音声スレッド側で回収待ちの `Arc` を送るハンドル(`Mixer` 内部専用)。
struct ReclaimSender {
    producer: Producer<Arc<SoundData>>,
}

impl ReclaimSender {
    /// キューへ送る。満杯の場合、audio thread 上での Arc ドロップを避けるため
    /// あえてリークする(`mem::forget`)。既定の `reclaim_queue_capacity` は
    /// 同時に回収待ちになりうる最大数へ十分な余裕を持たせてあり、通常運用では
    /// 到達しない防御的経路(【仮】。M2 で発生頻度を計測し、必要なら容量を見直す)。
    fn send_or_leak(&mut self, arc: Arc<SoundData>) {
        if let Err(PushError::Full(leaked)) = self.producer.push(arc) {
            std::mem::forget(leaked);
        }
    }
}

/// mw-core の最上位ミキサ。音声コールバックから駆動される(単一スレッド専用の
/// 排他所有物として `mw-backend` のストリームクロージャへムーブされる想定)。
pub struct Mixer {
    buses: BusSet,
    voices: VoicePool,
    clipper: SoftClipper,
    command_consumer: Consumer<Command>,
    reclaim: ReclaimSender,
    sample_rate: u32,
    default_ramp_ms: f32,
}

/// [`Mixer`] とゲームスレッド側ハンドルの3点セットを構築する。
///
/// `sample_rate` は出力デバイスが実際に開かれた時点のレート(§4.7 の推奨は 48kHz)。
/// バックエンド側がデバイスをオープンした直後、音声コールバックが動き出す前に
/// 確定させる想定(cpal 実装は `CpalBackend::open` 内で行う)。
pub fn build(config: Config, sample_rate: u32) -> (Mixer, CommandSender, ReclaimReceiver) {
    let (command_producer, command_consumer) =
        RingBuffer::<Command>::new(config.command_queue_capacity);
    let (reclaim_producer, reclaim_consumer) =
        RingBuffer::<Arc<SoundData>>::new(config.reclaim_queue_capacity);

    let mixer = Mixer {
        buses: BusSet::new(),
        voices: VoicePool::new(config.max_voices, config.steal_tail_capacity),
        clipper: SoftClipper::new(config.clipper_threshold),
        command_consumer,
        reclaim: ReclaimSender {
            producer: reclaim_producer,
        },
        sample_rate,
        default_ramp_ms: config.default_ramp_ms,
    };
    let sender = CommandSender {
        producer: Mutex::new(command_producer),
    };
    let receiver = ReclaimReceiver {
        consumer: reclaim_consumer,
    };
    (mixer, sender, receiver)
}

impl Mixer {
    fn default_ramp_samples(&self) -> u32 {
        ms_to_samples(self.default_ramp_ms, self.sample_rate)
    }

    fn drain_commands(&mut self) {
        while let Ok(command) = self.command_consumer.pop() {
            self.apply_command(command);
        }
    }

    fn apply_command(&mut self, command: Command) {
        let default_ramp = self.default_ramp_samples();
        match command {
            Command::PlaySe {
                voice_serial,
                sound_id,
                sound,
                bus,
                volume,
            } => {
                let outcome =
                    self.voices
                        .play(voice_serial, sound_id, sound, bus, volume, default_ramp);
                if matches!(outcome, StealOutcome::TailPoolExhausted) {
                    // 尾スロットも枯渇(理論上到達しない)。新規発音は諦めるが、
                    // 少なくとも既存の再生・Arc 所有権には一切触れない安全側の分岐。
                }
            }
            Command::StopVoice { voice_serial } => {
                self.voices.stop(voice_serial, default_ramp);
            }
            Command::SetVoiceVolume {
                voice_serial,
                volume,
            } => {
                self.voices.set_volume(voice_serial, volume, default_ramp);
            }
            Command::StopVoicesUsingSound { sound_id } => {
                self.voices.stop_all_using_sound(sound_id, default_ramp);
            }
            Command::SetBusVolume { bus, volume } => {
                self.buses
                    .get_mut(bus)
                    .volume
                    .set_target(volume, default_ramp);
            }
            Command::BusFade { bus, target, ms } => {
                let samples = ms_to_samples(ms, self.sample_rate);
                self.buses.get_mut(bus).volume.set_target(target, samples);
            }
        }
    }

    /// `output` はインターリーブされた f32 ステレオバッファ。
    ///
    /// # リアルタイム安全性
    /// ロック取得・ヒープアロケーション・ブロッキング IO・パニック経路は禁止(§5.3)。
    /// `output.len()` が `CHANNELS` の倍数でなくても整数除算で切り捨てるだけでパニックしない。
    pub fn render(&mut self, output: &mut [f32]) {
        self.drain_commands();

        let frames = output.len() / CHANNELS;
        for frame_index in 0..frames {
            let mut bus_volume = [0.0f32; BUS_COUNT];
            for id in ALL_BUSES {
                bus_volume[id.index()] = self.buses.get_mut(id).volume.advance();
            }

            let (mut l, mut r) = self.voices.mix_frame(&bus_volume);

            let master_volume = bus_volume[BusId::Master.index()];
            l *= master_volume;
            r *= master_volume;

            let (cl, cr) = self.clipper.process_stereo(l, r);

            let base = frame_index * CHANNELS;
            if let Some(slot) = output.get_mut(base) {
                *slot = cl;
            }
            if let Some(slot) = output.get_mut(base + 1) {
                *slot = cr;
            }
        }

        let reclaim = &mut self.reclaim;
        self.voices.reap(|arc| reclaim.send_or_leak(arc));
    }

    /// クリッパが動作(閾値超過)した累計回数(開発ビルドでの動作検知。§4.1)。
    pub fn clipper_engaged_count(&self) -> u64 {
        self.clipper.engaged_count()
    }

    /// 現在の出力サンプルレート。
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// サンプルレートを変更する(バックエンドがデバイスをオープンした直後、
    /// 音声コールバックが動き出す前に一度だけ呼ぶ想定)。
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate;
    }

    /// テスト・診断用。
    pub fn active_voice_count(&self) -> usize {
        self.voices.active_primary_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sound::SoundData;

    fn constant_sound(frames: usize, value: f32) -> Arc<SoundData> {
        Arc::new(SoundData {
            sample_rate: 48_000,
            frames,
            interleaved: vec![value; frames * CHANNELS],
        })
    }

    #[test]
    fn silence_by_default() {
        let (mut mixer, _sender, _reclaim) = build(Config::default(), 48_000);
        let mut buf = vec![1.0; 32 * CHANNELS];
        mixer.render(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn play_command_produces_sound_on_the_very_next_render_call() {
        let (mut mixer, sender, _reclaim) = build(Config::default(), 48_000);
        assert!(sender.send(Command::PlaySe {
            voice_serial: 1,
            sound_id: 1,
            sound: constant_sound(100, 0.25),
            bus: BusId::Se,
            volume: 1.0,
        }));

        let mut buf = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut buf);
        // 初期構築仕様 §4.2: 「次のオーディオコールバックで必ず発音される」。
        assert_eq!(buf[0], 0.25);
        assert_eq!(buf[1], 0.25);
    }

    #[test]
    fn bus_volume_scales_voice_output_exactly() {
        let (mut mixer, sender, _reclaim) = build(Config::default(), 48_000);
        sender.send(Command::PlaySe {
            voice_serial: 1,
            sound_id: 1,
            sound: constant_sound(1_000, 1.0),
            bus: BusId::Se,
            volume: 1.0,
        });
        // SE バスをただちに 0.5 に設定(既定ランプ 5ms = 240 サンプル @48kHz)。
        sender.send(Command::SetBusVolume {
            bus: BusId::Se,
            volume: 0.5,
        });

        let mut buf = vec![0.0; 300 * CHANNELS];
        mixer.render(&mut buf);
        // ランプが尽きた後(240サンプル以降)は正確に 0.5。
        let last_frame_l = buf[(299) * CHANNELS];
        assert!((last_frame_l - 0.5).abs() < 1e-6);
    }

    #[test]
    fn master_bus_applies_once_not_double_counted_for_direct_master_voices() {
        let (mut mixer, sender, _reclaim) = build(Config::default(), 48_000);
        sender.send(Command::PlaySe {
            voice_serial: 1,
            sound_id: 1,
            sound: constant_sound(1_000, 1.0),
            bus: BusId::Master,
            volume: 1.0,
        });
        sender.send(Command::SetBusVolume {
            bus: BusId::Master,
            volume: 0.5,
        });
        let mut buf = vec![0.0; 300 * CHANNELS];
        mixer.render(&mut buf);
        let last_l = buf[299 * CHANNELS];
        // Master 0.5 のみが1回適用される(サブバス二重適用なら 0.25 になってしまう)。
        assert!((last_l - 0.5).abs() < 1e-6);
    }

    #[test]
    fn exhaustion_steals_and_reclaims_via_reclaim_queue_not_in_place_drop() {
        let config = Config {
            max_voices: 1,
            steal_tail_capacity: 1,
            ..Config::default()
        };
        let (mut mixer, sender, mut reclaim) = build(config, 48_000);

        sender.send(Command::PlaySe {
            voice_serial: 1,
            sound_id: 1,
            sound: constant_sound(10_000, 1.0),
            bus: BusId::Se,
            volume: 1.0,
        });
        let mut buf = vec![0.0; 8 * CHANNELS];
        mixer.render(&mut buf);

        sender.send(Command::PlaySe {
            voice_serial: 2,
            sound_id: 1,
            sound: constant_sound(10_000, 1.0),
            bus: BusId::Se,
            volume: 1.0,
        });
        // 既定ランプ(5ms = 240サンプル @48kHz)を使い切るまでレンダリングする。
        let mut buf2 = vec![0.0; 300 * CHANNELS];
        mixer.render(&mut buf2);

        reclaim.drain();
        assert_eq!(
            mixer.active_voice_count(),
            1,
            "the stealer must occupy the single primary slot"
        );
    }

    #[test]
    fn soft_clipper_engages_when_voices_sum_above_threshold() {
        let (mut mixer, sender, _reclaim) = build(Config::default(), 48_000);
        for i in 0..4u64 {
            sender.send(Command::PlaySe {
                voice_serial: i + 1,
                sound_id: 1,
                sound: constant_sound(10, 0.9),
                bus: BusId::Se,
                volume: 1.0,
            });
        }
        assert_eq!(mixer.clipper_engaged_count(), 0);
        let mut buf = vec![0.0; CHANNELS];
        mixer.render(&mut buf);
        // 0.9 * 4 = 3.6 > 1.0 のはずなのでクリッパが動作している。
        assert!(mixer.clipper_engaged_count() > 0);
        assert!(buf[0] < 3.6);
    }
}
