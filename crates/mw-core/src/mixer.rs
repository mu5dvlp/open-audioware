//! ミキサ(初期構築仕様 §4.1「ミキサ」/ §5.2)。
//!
//! 「アクティブボイス合算 → バス音量 → Master → クリッパ」の順で処理する。
//! 音声スレッド相当経路(`Mixer::render`)でのアロケーション・ロックは禁止(§5.3)。
//! コンストラクタ([`build`])でのみ固定長バッファ・キューを確保する。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::bus::{ALL_BUSES, BUS_COUNT, BusId, BusSet};
use crate::clipper::SoftClipper;
use crate::clock::MusicClockPublisher;
use crate::command::{Command, ScheduledSe};
use crate::config::Config;
use crate::format::CHANNELS;
use crate::music::{MusicFrameSource, MusicState, MusicVoice};
use crate::ramp::ms_to_samples;
use crate::schedule::{ScheduleQueue, buffer_duration_ns, offset_within_buffer};
use crate::sound::SoundData;
use crate::stream::{self, MusicStreamProducer, StreamingMusicSource};
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

/// 楽曲の予約再生(初期構築仕様『§4.3』, M2-5)の保留状態。
///
/// 楽曲ボイスは同時に1本のみ(初期構築仕様『§2』M14)なので、SE 予約
/// ([`ScheduleQueue`])と違ってキューは要らず、単一スロットで足りる。
#[derive(Debug, Default)]
struct MusicSchedule {
    /// 予約中のホスト時刻。発火 or 破棄すると `None` に戻る。
    pending_host_time_ns: Option<u64>,
    /// 直近の予約が、到来時点でプリロール未完了(`MusicState::Loading`)だったために
    /// 繰り下げられたか。発火するとリセットされる。
    ///
    /// 初期構築仕様『§4.3』【仮】: 「プリロール完了前の予約はエラーではなく
    /// 『準備完了後、可能な最速時刻』へ繰り下げてイベントで通知する」。イベント通知
    /// そのもの(§4.6)は後続作業(M2-6)なので、今回は繰り下げた事実をこのフラグで
    /// 問い合わせられる形にしてある([`Mixer::music_schedule_deferred`])。
    deferred: bool,
}

impl MusicSchedule {
    fn schedule(&mut self, host_time_ns: u64) {
        self.pending_host_time_ns = Some(host_time_ns);
        self.deferred = false;
    }

    /// このバッファで発火すべきか判定する。発火するならバッファ内オフセット
    /// (サンプル数)を返し、内部状態を「発火済み(予約なし)」に戻す。
    ///
    /// - 予約が無い、またはまだ未来(`target_ns >= buffer_end_ns`)なら `None`。
    /// - 到来していても `voice_state` が `Loading`(プリロール未完了)なら、
    ///   予約は保持したまま `deferred` を立てて `None` を返す——次回以降の `render` で
    ///   `Ready` になり次第、必ず「過去」判定(オフセット 0)で即座に発火する。
    /// - 到来していて `voice_state` が `Ready` なら発火(オフセットを返す)。
    /// - 到来していて `voice_state` が `Playing`/`Paused`(既に別経路で状態が変わった)
    ///   場合は、発火させても `MusicVoice::play` 自体が no-op になるだけなので、
    ///   静かに予約を破棄する(`deferred` は立てない——プリロール未完了が理由ではないため)。
    fn take_due_offset(
        &mut self,
        buffer_start_ns: u64,
        buffer_end_ns: u64,
        sample_rate: u32,
        voice_state: MusicState,
    ) -> Option<usize> {
        let target_ns = self.pending_host_time_ns?;
        if target_ns >= buffer_end_ns {
            return None;
        }

        if voice_state == MusicState::Loading {
            self.deferred = true;
            return None;
        }

        self.pending_host_time_ns = None;
        self.deferred = false;

        if voice_state != MusicState::Ready {
            return None;
        }

        let offset = if target_ns <= buffer_start_ns {
            0
        } else {
            offset_within_buffer(target_ns, buffer_start_ns, sample_rate)
        };
        Some(offset)
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
    /// 唯一の楽曲ボイス(初期構築仕様『§2』M14)。
    music_voice: MusicVoice,
    /// 楽曲 PCM の供給元。実体はデコードスレッド(後続作業)から `pump` される
    /// リングバッファの消費側(`stream.rs`)。
    music_source: StreamingMusicSource,
    music_schedule: MusicSchedule,
    /// 予約 SE(初期構築仕様『§4.5』)のソート済みキュー。固定容量
    /// (`Config::schedule_queue_capacity`)。
    se_schedule: ScheduleQueue<ScheduledSe>,
    /// `se_schedule` が満杯で挿入できなかった回数(`clipper.rs` の動作回数カウントと
    /// 同じ流儀。イベント通知への昇格は M2-6 以降)。
    se_schedule_overflow_count: AtomicU64,
    /// 音楽クロックの相関点(初期構築仕様『§4.4』)。音声スレッドが書き、
    /// ゲームスレッドが `Arc` の複製経由でロック無しに読む(seqlock、`clock.rs`)。
    music_clock: Arc<MusicClockPublisher>,
}

/// [`Mixer`] とゲームスレッド側ハンドル一式を構築する。
///
/// `sample_rate` は出力デバイスが実際に開かれた時点のレート(§4.7 の推奨は 48kHz)。
/// バックエンド側がデバイスをオープンした直後、音声コールバックが動き出す前に
/// 確定させる想定(cpal 実装は `CpalBackend::open` 内で行う)。
///
/// 戻り値は `(Mixer, コマンド送信, Arc 回収, 楽曲PCM供給の生産側, 音楽クロックの読み手)`。
/// `MusicStreamProducer` を駆動するデコードスレッドの起動は mw-ffi 側の後続作業
/// (M2-5 の時点では誰も `pump` を呼ばないため、楽曲ボイスは供給が無いまま
/// `MusicState::Loading` に留まる——これは「まだロード API が無い」という現状を
/// 正直に反映した状態であり、誤魔化しではない)。
pub fn build(
    config: Config,
    sample_rate: u32,
) -> (
    Mixer,
    CommandSender,
    ReclaimReceiver,
    MusicStreamProducer,
    Arc<MusicClockPublisher>,
) {
    let (command_producer, command_consumer) =
        RingBuffer::<Command>::new(config.command_queue_capacity);
    let (reclaim_producer, reclaim_consumer) =
        RingBuffer::<Arc<SoundData>>::new(config.reclaim_queue_capacity);
    let (music_producer, music_source) = stream::channel(config, sample_rate);
    let music_clock = Arc::new(MusicClockPublisher::new());

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
        music_voice: MusicVoice::new(sample_rate),
        music_source,
        music_schedule: MusicSchedule::default(),
        se_schedule: ScheduleQueue::with_capacity(config.schedule_queue_capacity),
        se_schedule_overflow_count: AtomicU64::new(0),
        music_clock: Arc::clone(&music_clock),
    };
    let sender = CommandSender {
        producer: Mutex::new(command_producer),
    };
    let receiver = ReclaimReceiver {
        consumer: reclaim_consumer,
    };
    (mixer, sender, receiver, music_producer, music_clock)
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
            Command::SeSchedule {
                host_time_ns,
                entry,
            } => {
                if !self.se_schedule.try_insert(host_time_ns, entry) {
                    // 溢れた予約は発音されない(黙って捨てるのではなく計測する。
                    // `Config::schedule_queue_capacity` のドキュメント参照)。
                    self.se_schedule_overflow_count
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            Command::MusicPlayScheduled { host_time_ns } => {
                self.music_schedule.schedule(host_time_ns);
            }
            Command::MusicSeek { frames } => {
                self.music_voice.seek(frames, &mut self.music_source);
            }
        }
    }

    /// このフレームで発火すべき予約 SE をすべて処理する(バッファ内オフセット精度、
    /// 初期構築仕様『§4.5』)。`se_schedule` は昇順ソート済みなので、先頭が
    /// 「このバッファの範囲外(まだ未来)」になった時点で打ち切ってよい。
    ///
    /// 予約時刻が既にバッファ先頭以前(過去)だった場合はオフセット 0(=このバッファの
    /// 最初のフレーム)として扱う——取りこぼして無音にするのではなく、発見可能な
    /// 最速のサンプルで即座に発音する【仮、この関数のコメントで判断根拠を明示】。
    /// 遅延が起きるのはコマンドキューを跨ぐ配送遅延そのものが原因であり、ここで
    /// 追加の遅延を上乗せしない方が実害が小さいと判断した(初期構築仕様 §4.2 の
    /// 「次のコールバックで必ず発音される」という即時発音の保証と同じ考え方)。
    fn fire_due_se(
        &mut self,
        frame_index: usize,
        buffer_start_ns: u64,
        buffer_end_ns: u64,
        default_ramp_samples: u32,
    ) {
        loop {
            let Some(front) = self.se_schedule.front() else {
                break;
            };
            let target_ns = front.0;
            if target_ns >= buffer_end_ns {
                break; // まだこのバッファの範囲外(未来)。
            }
            let offset = if target_ns <= buffer_start_ns {
                0
            } else {
                offset_within_buffer(target_ns, buffer_start_ns, self.sample_rate)
            };
            if offset > frame_index {
                break; // まだこのフレームには早い。
            }
            let Some((_, se)) = self.se_schedule.pop_front() else {
                break; // 直前に front() で存在確認済みのため到達しない防御的分岐。
            };
            self.voices.play(
                se.voice_serial,
                se.sound_id,
                se.sound,
                se.bus,
                se.volume,
                default_ramp_samples,
            );
        }
    }

    /// `output` はインターリーブされた f32 ステレオバッファ。`buffer_start_host_time_ns` は
    /// このバッファの**先頭フレーム**が実際に DAC から出力される(と予測される)ホスト
    /// 単調時刻(初期構築仕様『§4.4』)。`mw-backend` が cpal の
    /// `OutputCallbackInfo::timestamp().playback` から求めて渡す想定
    /// (`crates/mw-backend/src/host_time.rs` のモジュール doc に、cpal の
    /// `StreamInstant` と同じ時計であることの調査結果を記載してある)。
    ///
    /// 処理順序:
    /// 1. コマンド消化(SE 予約・楽曲予約再生・楽曲シークも含む)
    /// 2. 楽曲ボイスのレンダリング(予約発火があれば「発火前」「`play()`」「発火後」の
    ///    2回に分けてサンプル精度の開始位置を実現する。§4.5 と同じ「バッファ境界に
    ///    丸めない」方針を楽曲側にも適用する)。この時点では `output` に Bgm バス音量・
    ///    Master 未適用の生 PCM が入る(3で読み直して合成する)
    /// 3. 通常のフレームループ: 予約 SE の発火・バス音量の前進・SE ボイス合算 + 2の
    ///    生 PCM(Bgm バス音量適用)を加算・Master・クリッパ
    /// 4. 音楽クロックの相関点を公開(このバッファの処理結果を反映した最新値。
    ///    不連続があれば公開前に世代を進める)
    ///
    /// # リアルタイム安全性
    /// ロック取得・ヒープアロケーション・ブロッキング IO・パニック経路は禁止(§5.3)。
    /// `output.len()` が `CHANNELS` の倍数でなくても整数除算で切り捨てるだけでパニックしない。
    pub fn render(&mut self, output: &mut [f32], buffer_start_host_time_ns: u64) {
        self.drain_commands();

        let frames = output.len() / CHANNELS;
        let default_ramp = self.default_ramp_samples();
        let duration_ns = buffer_duration_ns(frames, self.sample_rate);
        let buffer_end_ns = buffer_start_host_time_ns.saturating_add(duration_ns);

        // --- 楽曲ボイス ---
        // `Loading` → `Ready` の遷移は実際には `MusicVoice::render` の中(このすぐ下)で
        // 起きるが、予約発火の判定はそれより先に行う必要がある。ここで先取りしないと
        // 「データが揃った、まさにその render 呼び出し」では常に `Loading` のまま判定されて
        // しまい、繰り下げ発火が実際より1バッファぶん余計に遅れてしまう
        // (`is_ready()` は `&self` で読めるので、`render` を呼ばずに覗ける)。
        let mut voice_state = self.music_voice.state();
        if voice_state == MusicState::Loading && self.music_source.is_ready() {
            voice_state = MusicState::Ready;
        }
        let fire_offset = self.music_schedule.take_due_offset(
            buffer_start_host_time_ns,
            buffer_end_ns,
            self.sample_rate,
            voice_state,
        );
        let music_outcome = match fire_offset {
            Some(offset) => {
                let split = (offset * CHANNELS).min(output.len());
                let (before, after) = output.split_at_mut(split);
                let outcome_before = self.music_voice.render(before, &mut self.music_source);
                self.music_voice.play();
                let outcome_after = self.music_voice.render(after, &mut self.music_source);
                outcome_before.merge(outcome_after)
            }
            None => self.music_voice.render(output, &mut self.music_source),
        };
        if music_outcome.discontinuity {
            // 新しい相関点が確定する前に世代を進める(`bump_generation` のドキュメント参照)。
            self.music_clock.bump_generation();
        }

        // --- SE ボイス + 楽曲の合成 ---
        for frame_index in 0..frames {
            self.fire_due_se(
                frame_index,
                buffer_start_host_time_ns,
                buffer_end_ns,
                default_ramp,
            );

            let mut bus_volume = [0.0f32; BUS_COUNT];
            for id in ALL_BUSES {
                bus_volume[id.index()] = self.buses.get_mut(id).volume.advance();
            }

            let (se_l, se_r) = self.voices.mix_frame(&bus_volume);

            let base = frame_index * CHANNELS;
            // 2で書き込んだ楽曲の生 PCM をここで読み直し、Bgm バス音量を適用してから
            // SE 分に加算する(この読み出しの直後に同じ位置へ最終値を上書きする)。
            let bgm_gain = bus_volume[BusId::Bgm.index()];
            let raw_music_l = output.get(base).copied().unwrap_or(0.0);
            let raw_music_r = output.get(base + 1).copied().unwrap_or(0.0);

            let mut l = se_l + raw_music_l * bgm_gain;
            let mut r = se_r + raw_music_r * bgm_gain;

            let master_volume = bus_volume[BusId::Master.index()];
            l *= master_volume;
            r *= master_volume;

            let (cl, cr) = self.clipper.process_stereo(l, r);

            if let Some(slot) = output.get_mut(base) {
                *slot = cl;
            }
            if let Some(slot) = output.get_mut(base + 1) {
                *slot = cr;
            }
        }

        let reclaim = &mut self.reclaim;
        self.voices.reap(|arc| reclaim.send_or_leak(arc));

        // --- 音楽クロックの相関点を公開 ---
        // このバッファを処理し終えた「今」の位置とバッファ末尾(= 次バッファ先頭)の
        // ホスト時刻を組にする。予約再生がこのバッファの途中で発火した場合も、
        // 発火後の実際の位置・状態が反映された最新の相関点になる。
        self.music_clock.publish(
            self.music_voice.position_frames(),
            buffer_end_ns,
            self.sample_rate,
            self.music_voice.state() == MusicState::Playing,
        );
    }

    /// クリッパが動作(閾値超過)した累計回数(開発ビルドでの動作検知。§4.1)。
    pub fn clipper_engaged_count(&self) -> u64 {
        self.clipper.engaged_count()
    }

    /// 現在の楽曲ボイスの状態(初期構築仕様『§4.3』)。
    pub fn music_state(&self) -> MusicState {
        self.music_voice.state()
    }

    /// 直近の楽曲予約再生が、到来時点でプリロール未完了だったために繰り下げられたか
    /// (初期構築仕様『§4.3』【仮】)。発火すると `false` に戻る。
    pub fn music_schedule_deferred(&self) -> bool {
        self.music_schedule.deferred
    }

    /// `se_schedule` が満杯で挿入できず、発音されなかった予約 SE の累計件数。
    pub fn se_schedule_overflow_count(&self) -> u64 {
        self.se_schedule_overflow_count.load(Ordering::Relaxed)
    }

    /// 現在キューに残っている予約 SE の件数(診断・テスト用)。
    #[cfg(test)]
    fn se_schedule_len(&self) -> usize {
        self.se_schedule.len()
    }

    /// 現在の出力サンプルレート。
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// サンプルレートを変更する(バックエンドがデバイスをオープンした直後、
    /// 音声コールバックが動き出す前に一度だけ呼ぶ想定)。
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate;
        self.music_voice.set_sample_rate(sample_rate);
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
        let (mut mixer, _sender, _reclaim, _music_producer, _music_clock) =
            build(Config::default(), 48_000);
        let mut buf = vec![1.0; 32 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn play_command_produces_sound_on_the_very_next_render_call() {
        let (mut mixer, sender, _reclaim, _music_producer, _music_clock) =
            build(Config::default(), 48_000);
        assert!(sender.send(Command::PlaySe {
            voice_serial: 1,
            sound_id: 1,
            sound: constant_sound(100, 0.25),
            bus: BusId::Se,
            volume: 1.0,
        }));

        let mut buf = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut buf, 0);
        // 初期構築仕様 §4.2: 「次のオーディオコールバックで必ず発音される」。
        assert_eq!(buf[0], 0.25);
        assert_eq!(buf[1], 0.25);
    }

    #[test]
    fn bus_volume_scales_voice_output_exactly() {
        let (mut mixer, sender, _reclaim, _music_producer, _music_clock) =
            build(Config::default(), 48_000);
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
        mixer.render(&mut buf, 0);
        // ランプが尽きた後(240サンプル以降)は正確に 0.5。
        let last_frame_l = buf[(299) * CHANNELS];
        assert!((last_frame_l - 0.5).abs() < 1e-6);
    }

    #[test]
    fn master_bus_applies_once_not_double_counted_for_direct_master_voices() {
        let (mut mixer, sender, _reclaim, _music_producer, _music_clock) =
            build(Config::default(), 48_000);
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
        mixer.render(&mut buf, 0);
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
        let (mut mixer, sender, mut reclaim, _music_producer, _music_clock) = build(config, 48_000);

        sender.send(Command::PlaySe {
            voice_serial: 1,
            sound_id: 1,
            sound: constant_sound(10_000, 1.0),
            bus: BusId::Se,
            volume: 1.0,
        });
        let mut buf = vec![0.0; 8 * CHANNELS];
        mixer.render(&mut buf, 0);

        sender.send(Command::PlaySe {
            voice_serial: 2,
            sound_id: 1,
            sound: constant_sound(10_000, 1.0),
            bus: BusId::Se,
            volume: 1.0,
        });
        // 既定ランプ(5ms = 240サンプル @48kHz)を使い切るまでレンダリングする。
        let mut buf2 = vec![0.0; 300 * CHANNELS];
        mixer.render(&mut buf2, 0);

        reclaim.drain();
        assert_eq!(
            mixer.active_voice_count(),
            1,
            "the stealer must occupy the single primary slot"
        );
    }

    #[test]
    fn soft_clipper_engages_when_voices_sum_above_threshold() {
        let (mut mixer, sender, _reclaim, _music_producer, _music_clock) =
            build(Config::default(), 48_000);
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
        mixer.render(&mut buf, 0);
        // 0.9 * 4 = 3.6 > 1.0 のはずなのでクリッパが動作している。
        assert!(mixer.clipper_engaged_count() > 0);
        assert!(buf[0] < 3.6);
    }

    // =====================================================================
    // M2-5: 予約発音(サンプル精度)とデバイスタイムスタンプ相関
    // =====================================================================

    use crate::decode::{DecodeError, MusicDecoder};

    /// テストで使う既定サンプルレート。1000Hz なら1サンプル = 1_000_000ns ちょうどになり、
    /// 「予約時刻を1サンプルずつずらす」テストの host_time_ns が整数のまま扱える
    /// (`music.rs`/`stream.rs` のテストと同じ理由で選んだ値)。
    const TEST_SAMPLE_RATE: u32 = 1_000;
    const NS_PER_SAMPLE: u64 = 1_000_000_000 / TEST_SAMPLE_RATE as u64;

    fn se_entry(voice_serial: u64, value: f32) -> ScheduledSe {
        ScheduledSe {
            voice_serial,
            sound_id: voice_serial,
            sound: constant_sound(1_000, value),
            bus: BusId::Se,
            volume: 1.0,
        }
    }

    /// 楽曲テスト用のフェイクデコーダ。常に一定値を返す(`music.rs::tests::FakeSource` の
    /// `constant_value` と同じ考え方——フェードインの傾きだけで開始位置を判定できるように
    /// PCM 側は定数にしてある)。総フレーム数は無尽蔵(`None`)。
    struct ConstantDecoder {
        cursor: u64,
        value: f32,
    }

    impl MusicDecoder for ConstantDecoder {
        fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
            let n = out.len() / CHANNELS;
            for i in 0..n {
                out[i * CHANNELS] = self.value;
                out[i * CHANNELS + 1] = self.value;
            }
            self.cursor += n as u64;
            Ok(n)
        }

        fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
            self.cursor = frame;
            Ok(())
        }

        fn total_frames(&self) -> Option<u64> {
            None
        }
    }

    /// `build` 直後の `Mixer` へ、`MusicVoice` が `Ready` になるまで PCM を供給する。
    /// (`producer.pump` でリングバッファを満杯にし、`render` を1回呼んで
    /// `Loading` → `Ready` の遷移を確定させる)。
    fn make_music_ready(mixer: &mut Mixer, producer: &mut MusicStreamProducer, value: f32) {
        let mut decoder = ConstantDecoder { cursor: 0, value };
        producer.pump(&mut decoder).expect("pump must succeed");
        let mut warmup = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut warmup, 0);
        assert_eq!(mixer.music_state(), MusicState::Ready);
    }

    // --- SE 予約: バッファ内オフセットの精度 ---

    #[test]
    fn se_schedule_fires_at_exact_sample_offset_within_buffer() {
        let (mut mixer, sender, _reclaim, _music_producer, _music_clock) =
            build(Config::default(), TEST_SAMPLE_RATE);
        let offset = 7u64;
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: offset * NS_PER_SAMPLE,
            entry: se_entry(1, 1.0),
        }));

        let frames = 16usize;
        let mut buf = vec![-1.0; frames * CHANNELS];
        mixer.render(&mut buf, 0);

        for frame in 0..frames {
            let expected = if (frame as u64) < offset { 0.0 } else { 1.0 };
            assert!(
                (buf[frame * CHANNELS] - expected).abs() < 1e-6,
                "frame {frame}: expected {expected}, got {}",
                buf[frame * CHANNELS]
            );
        }
    }

    #[test]
    fn se_schedule_offset_boundary_values() {
        // 境界値1: バッファ先頭ちょうど。
        {
            let (mut mixer, sender, _reclaim, _mp, _mc) =
                build(Config::default(), TEST_SAMPLE_RATE);
            sender.send(Command::SeSchedule {
                host_time_ns: 0,
                entry: se_entry(1, 1.0),
            });
            let mut buf = vec![0.0; 8 * CHANNELS];
            mixer.render(&mut buf, 0);
            assert!(
                (buf[0] - 1.0).abs() < 1e-6,
                "must fire exactly at frame 0 of the buffer"
            );
        }

        // 境界値2: バッファ末尾ちょうど(最後のフレームだけ発音)。
        {
            let (mut mixer, sender, _reclaim, _mp, _mc) =
                build(Config::default(), TEST_SAMPLE_RATE);
            let frames = 8usize;
            sender.send(Command::SeSchedule {
                host_time_ns: (frames as u64 - 1) * NS_PER_SAMPLE,
                entry: se_entry(1, 1.0),
            });
            let mut buf = vec![0.0; frames * CHANNELS];
            mixer.render(&mut buf, 0);
            for frame in 0..frames - 1 {
                assert_eq!(
                    buf[frame * CHANNELS],
                    0.0,
                    "frame {frame} must still be silent"
                );
            }
            assert!((buf[(frames - 1) * CHANNELS] - 1.0).abs() < 1e-6);
        }

        // 境界値3: バッファをまたぐ(1バッファ目では発音せず、2バッファ目の正しい
        // オフセットで発音する)。
        {
            let (mut mixer, sender, _reclaim, _mp, _mc) =
                build(Config::default(), TEST_SAMPLE_RATE);
            let frames = 8usize;
            let buffer1_duration_ns = frames as u64 * NS_PER_SAMPLE;
            let target_ns = buffer1_duration_ns + 3 * NS_PER_SAMPLE;
            sender.send(Command::SeSchedule {
                host_time_ns: target_ns,
                entry: se_entry(1, 1.0),
            });

            let mut buf1 = vec![0.0; frames * CHANNELS];
            mixer.render(&mut buf1, 0);
            assert!(
                buf1.iter().all(|&s| s == 0.0),
                "must not fire while still in the first buffer's window"
            );

            let mut buf2 = vec![0.0; frames * CHANNELS];
            mixer.render(&mut buf2, buffer1_duration_ns);
            for frame in 0..3 {
                assert_eq!(buf2[frame * CHANNELS], 0.0);
            }
            assert!((buf2[3 * CHANNELS] - 1.0).abs() < 1e-6);
        }
    }

    /// 依頼書のテスト要件2: 予約時刻を1サンプルずつずらしたとき、発音位置も1サンプルずつ
    /// ずれること(バッファ境界への丸めが起きていないことの直接証拠)。
    #[test]
    fn se_schedule_offset_advances_by_exactly_one_sample_per_sample_shift() {
        let frames = 40usize;
        for n in 0..frames as u64 {
            let (mut mixer, sender, _reclaim, _mp, _mc) =
                build(Config::default(), TEST_SAMPLE_RATE);
            sender.send(Command::SeSchedule {
                host_time_ns: n * NS_PER_SAMPLE,
                entry: se_entry(1, 1.0),
            });
            let mut buf = vec![0.0; frames * CHANNELS];
            mixer.render(&mut buf, 0);
            for frame in 0..frames as u64 {
                let expected = if frame < n { 0.0 } else { 1.0 };
                assert!(
                    (buf[frame as usize * CHANNELS] - expected).abs() < 1e-6,
                    "n={n}, frame={frame}: expected {expected}"
                );
            }
        }
    }

    /// 依頼書のテスト要件3: 過去の時刻を予約した場合は破棄せず、発見可能な最速の
    /// サンプル(このバッファの先頭)で即座に発音する(`schedule.rs` モジュール doc の
    /// 丸め方針と同じ判断: 「指定時刻以降で最短距離」を優先し、取りこぼさない)。
    #[test]
    fn se_schedule_in_the_past_fires_immediately_at_offset_zero() {
        let (mut mixer, sender, _reclaim, _mp, _mc) = build(Config::default(), TEST_SAMPLE_RATE);
        // バッファ先頭(10ms 地点)よりずっと前を予約時刻に指定する。
        sender.send(Command::SeSchedule {
            host_time_ns: 0,
            entry: se_entry(1, 1.0),
        });
        let mut buf = vec![0.0; 8 * CHANNELS];
        mixer.render(&mut buf, 10 * NS_PER_SAMPLE);
        assert!(
            (buf[0] - 1.0).abs() < 1e-6,
            "an overdue schedule must fire at the very first frame, not be dropped"
        );
    }

    #[test]
    fn se_schedule_overflow_is_counted_not_silently_dropped() {
        let config = Config {
            schedule_queue_capacity: 2,
            ..Config::default()
        };
        let (mut mixer, sender, _reclaim, _mp, _mc) = build(config, TEST_SAMPLE_RATE);

        for i in 0..2u64 {
            assert!(sender.send(Command::SeSchedule {
                host_time_ns: 1_000 * NS_PER_SAMPLE + i,
                entry: se_entry(i + 1, 1.0),
            }));
        }
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: 1_000 * NS_PER_SAMPLE + 2,
            entry: se_entry(3, 1.0),
        }));

        assert_eq!(mixer.se_schedule_overflow_count(), 0);
        let mut buf = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut buf, 0); // まだ全部未来なので何も発火しない。

        assert_eq!(
            mixer.se_schedule_overflow_count(),
            1,
            "the third schedule must be dropped-but-counted, not silently accepted"
        );
        assert_eq!(mixer.se_schedule_len(), 2);
    }

    // --- 楽曲の予約再生 ---

    #[test]
    fn music_play_scheduled_starts_exactly_at_sample_offset_with_fade_in() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);

        let offset = 6u64;
        sender.send(Command::MusicPlayScheduled {
            host_time_ns: offset * NS_PER_SAMPLE,
        });

        let frames = 20usize;
        let mut buf = vec![-1.0; frames * CHANNELS];
        mixer.render(&mut buf, 0);

        let ramp_len = ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as u64;
        for frame in 0..frames as u64 {
            let l = buf[frame as usize * CHANNELS];
            if frame < offset {
                assert_eq!(
                    l, 0.0,
                    "frame {frame} must be silent before the scheduled start"
                );
                continue;
            }
            let k = frame - offset;
            let expected = if k < ramp_len {
                (k + 1) as f32 / ramp_len as f32
            } else {
                1.0
            };
            assert!(
                (l - expected).abs() < 1e-4,
                "frame {frame}: expected {expected}, got {l}"
            );
        }
        assert_eq!(mixer.music_state(), MusicState::Playing);
    }

    #[test]
    fn music_play_scheduled_before_preroll_completion_is_deferred_then_fires_once_ready() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock) =
            build(Config::default(), TEST_SAMPLE_RATE);

        // わざと pump しない: プリロール未完了(Loading)のまま予約時刻を到来させる。
        assert_eq!(mixer.music_state(), MusicState::Loading);
        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });

        let mut buf = vec![0.0; 10 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert_eq!(mixer.music_state(), MusicState::Loading);
        assert!(
            mixer.music_schedule_deferred(),
            "must be flagged as deferred while preroll is incomplete"
        );
        assert!(buf.iter().all(|&s| s == 0.0));

        // プリロールを満たす。
        let mut decoder = ConstantDecoder {
            cursor: 0,
            value: 1.0,
        };
        producer.pump(&mut decoder).expect("pump must succeed");

        let mut buf2 = vec![-1.0; 10 * CHANNELS];
        mixer.render(&mut buf2, 10_000 * NS_PER_SAMPLE);

        assert_eq!(
            mixer.music_state(),
            MusicState::Playing,
            "must fire on the very first render call where the voice becomes Ready"
        );
        assert!(
            !mixer.music_schedule_deferred(),
            "deferred flag must clear once it actually fires"
        );
        let ramp_len = ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as f32;
        assert!(
            (buf2[0] - 1.0 / ramp_len).abs() < 1e-4,
            "must start immediately at offset 0 of this buffer once ready"
        );
    }

    #[test]
    fn music_play_scheduled_in_the_past_when_already_ready_fires_immediately() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);

        // 予約時刻をこのバッファの先頭よりずっと前に設定(既に過去)。
        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });

        let mut buf = vec![-1.0; 10 * CHANNELS];
        mixer.render(&mut buf, 2_000 * NS_PER_SAMPLE);

        assert_eq!(mixer.music_state(), MusicState::Playing);
        assert!(!mixer.music_schedule_deferred());
        assert!(
            buf[0] > 0.0,
            "an overdue music schedule must start at the very first frame, not be dropped"
        );
    }

    // --- 音楽クロックの相関点(初期構築仕様『§4.4』) ---

    #[test]
    fn render_publishes_music_clock_snapshot_after_every_call() {
        let (mut mixer, sender, _reclaim, mut producer, music_clock) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);

        // Ready のまま(再生していない)render 直後のスナップショット。
        let snap_ready = music_clock.snapshot();
        assert_eq!(snap_ready.song_frames, 0);
        assert_eq!(snap_ready.sample_rate, TEST_SAMPLE_RATE);
        assert!(!snap_ready.is_playing);
        assert_eq!(snap_ready.generation, 0);

        // 次のバッファの先頭で予約再生を発火させる。
        let buffer_frames = 10u64;
        let buffer1_end_ns = buffer_frames * NS_PER_SAMPLE; // make_music_ready のウォームアップ分
        sender.send(Command::MusicPlayScheduled {
            host_time_ns: buffer1_end_ns,
        });

        let mut buf = vec![0.0; buffer_frames as usize * CHANNELS];
        mixer.render(&mut buf, buffer1_end_ns);

        let snap_playing = music_clock.snapshot();
        assert_eq!(
            snap_playing.song_frames, buffer_frames,
            "must reflect the position after this buffer's rendering, not before it"
        );
        assert_eq!(
            snap_playing.host_time_ns,
            buffer1_end_ns + buffer_frames * NS_PER_SAMPLE
        );
        assert!(snap_playing.is_playing);
        assert_eq!(
            snap_playing.generation, 0,
            "starting playback alone is not a discontinuity"
        );
    }

    #[test]
    fn render_bumps_generation_exactly_once_on_seek_discontinuity() {
        let (mut mixer, sender, _reclaim, mut producer, music_clock) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);

        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let mut buf = vec![0.0; 20 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert_eq!(music_clock.snapshot().generation, 0);

        sender.send(Command::MusicSeek { frames: 500 });
        let mut buf2 = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut buf2, 100_000 * NS_PER_SAMPLE);

        let snap = music_clock.snapshot();
        assert_eq!(
            snap.generation, 1,
            "seek must bump the generation exactly once"
        );
        assert_eq!(
            snap.song_frames, 500,
            "the post-seek position must be reflected immediately, not the stale pre-seek one"
        );

        // 追加の render では世代がさらに進まない(不連続はそのバッファだけの出来事)。
        let mut buf3 = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut buf3, 200_000 * NS_PER_SAMPLE);
        assert_eq!(music_clock.snapshot().generation, 1);
    }
}
