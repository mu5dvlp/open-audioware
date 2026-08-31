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
use crate::clock::{BgmStatePublisher, MusicClockPublisher};
use crate::command::{Command, ScheduledSe};
use crate::config::Config;
use crate::event::{Event, EventQueue};
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

/// BGM ボイス(`Mixer::bgm_voice`)の生 PCM を1コールバックぶんまとめて保持せず、
/// 固定長のスタックチャンク単位でレンダリングするためのフレーム数(【仮】)。
///
/// `output`(音声コールバックのバッファ)は既に `music_voice` の生 PCM 保持に
/// 使っているため、BGM 用に二重利用できない。かといって §5.3 によりヒープ確保も
/// できないため、`Vec` ではなくコンパイル時サイズ固定のスタック配列
/// (`[f32; BGM_CHUNK_FRAMES * CHANNELS]`)へチャンク単位で読み直す設計にした
/// (`Mixer::render` のこの定数を使っている箇所のコメント参照)。値を大きくすると
/// `bgm_voice.render` ひいては `MusicFrameSource::read` の呼び出し回数が減る一方、
/// スタック消費量が増える(256フレーム×2ch×4byte = 2KiB)。実測で見直してよい。
const BGM_CHUNK_FRAMES: usize = 256;

/// [`mixer::build`] が返す、BGM(初期構築仕様『§2』M14, M4-3)専用のゲームスレッド側
/// ハンドル一式。既存の6要素タプルにこれ以上要素を増やすと可読性が落ちるため、
/// BGM ぶんだけ1つの構造体にまとめてある。
pub struct BgmHandles {
    /// BGM PCM の供給元(生産側)。実体はデコードスレッド(mw-ffi 側、楽曲用とは
    /// 別にもう1本立てる想定)から `pump` されるリングバッファの生産側(`stream.rs`)。
    pub stream_producer: MusicStreamProducer,
    /// BGM ボイスの状態(初期構築仕様『§4.3』の4状態)をロック無しで読むハンドル。
    /// **クロックではない**——BGM は曲位置・ホスト時刻の相関点を持たない(M14)ため、
    /// `MusicClockPublisher` ではなく状態1本だけを公開する `BgmStatePublisher`
    /// (`clock.rs`)を使う。
    pub state: Arc<BgmStatePublisher>,
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
    /// BGM 用の2本目の楽曲ボイス(初期構築仕様『§2』M14, M4-3)。`music_voice` と
    /// **同じ `MusicVoice` 型をそのまま転用する**——状態機械・フェード・ループの
    /// 実装をまるごと共有できるうえ、「クロックを持たない」という M14 の要件は
    /// `MusicVoice` 自身の責務ではなく、その位置を `music_clock`(下記)へ**公開しない**
    /// という `Mixer::render` 側の配線だけで満たせる(型を分ける理由が無い)。
    bgm_voice: MusicVoice,
    /// BGM PCM の供給元。`music_source` と対になる、独立したストリーミングチャネル
    /// (`stream::channel` をもう一度呼んで作る。`music_source` とリングバッファも
    /// デコードスレッドも完全に別)。
    bgm_source: StreamingMusicSource,
    /// BGM ボイスの状態をゲームスレッドへ公開するハンドル(`BgmHandles::state` と
    /// 同じ `Arc` を共有する)。`music_clock`(音楽クロック)とは別物——BGM は
    /// クロックを持たないため、公開するのは状態1本だけ。
    bgm_state: Arc<BgmStatePublisher>,
    /// 予約 SE(初期構築仕様『§4.5』)のソート済みキュー。固定容量
    /// (`Config::schedule_queue_capacity`)。
    se_schedule: ScheduleQueue<ScheduledSe>,
    /// `se_schedule` が満杯で挿入できなかった回数(`clipper.rs` の動作回数カウントと
    /// 同じ流儀)。初期構築仕様『§4.6』の6種(M2-6 で実装済み)には含まれないため、
    /// イベント通知への昇格は行っていない(問い合わせ API 経由のままでよいと判断)。
    se_schedule_overflow_count: AtomicU64,
    /// 音楽クロックの相関点(初期構築仕様『§4.4』)。音声スレッドが書き、
    /// ゲームスレッドが `Arc` の複製経由でロック無しに読む(seqlock、`clock.rs`)。
    music_clock: Arc<MusicClockPublisher>,
    /// イベント通知(初期構築仕様『§4.6』)。音声スレッドはここへ `push_realtime` する
    /// だけで、実際の解放・破棄数の計算は読み手(ゲームスレッド、`event.rs` 参照)側で行う。
    events: Arc<EventQueue>,
    /// アンダーランの集約中カウンタ(`report_underrun` 参照)。コールバックをまたいで
    /// 蓄積し、まとめて1件のイベントにする。
    underrun_accumulator: u32,
    /// `underrun_accumulator` がこの値に達したら、まだアンダーランが収まっていなくても
    /// 一度報告する(初期構築仕様『§4.6』, `Config::underrun_report_threshold_frames`)。
    underrun_report_threshold_frames: u32,
}

/// [`Mixer`] とゲームスレッド側ハンドル一式を構築する。
///
/// `sample_rate` は出力デバイスが実際に開かれた時点のレート(§4.7 の推奨は 48kHz)。
/// バックエンド側がデバイスをオープンした直後、音声コールバックが動き出す前に
/// 確定させる想定(cpal 実装は `CpalBackend::open` 内で行う)。
///
/// 戻り値は `(Mixer, コマンド送信, Arc 回収, 楽曲PCM供給の生産側, 音楽クロックの読み手,
/// イベントキューの読み書きハンドル)`。
/// `MusicStreamProducer` を駆動するデコードスレッドの起動は mw-ffi 側の後続作業
/// (M2-5 の時点では誰も `pump` を呼ばないため、楽曲ボイスは供給が無いまま
/// `MusicState::Loading` に留まる——これは「まだロード API が無い」という現状を
/// 正直に反映した状態であり、誤魔化しではない)。
///
/// `Arc<EventQueue>` は書き込み・読み出しの両方を1つの型に集約してある(`event.rs`
/// モジュール doc 参照)。呼び出し側(mw-ffi)はこの同じ `Arc` を、音声スレッド以外の
/// 発生源(cpal のエラーコールバック等)からの `push_side_channel` と、ゲームスレッドの
/// `drain`(ポーリング)の両方に使い回す。
pub fn build(
    config: Config,
    sample_rate: u32,
) -> (
    Mixer,
    CommandSender,
    ReclaimReceiver,
    MusicStreamProducer,
    Arc<MusicClockPublisher>,
    Arc<EventQueue>,
    BgmHandles,
) {
    let (command_producer, command_consumer) =
        RingBuffer::<Command>::new(config.command_queue_capacity);
    let (reclaim_producer, reclaim_consumer) =
        RingBuffer::<Arc<SoundData>>::new(config.reclaim_queue_capacity);
    let (music_producer, music_source) = stream::channel(config, sample_rate);
    // BGM(初期構築仕様『§2』M14, M4-3)専用の独立したストリーミングチャネル。
    // `music_source`/`music_producer` とはリングバッファもデコード進行も完全に別。
    let (bgm_producer, bgm_source) = stream::channel(config, sample_rate);
    let music_clock = Arc::new(MusicClockPublisher::new());
    let bgm_state = Arc::new(BgmStatePublisher::new());
    let events = Arc::new(EventQueue::new(config.event_queue_capacity));

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
        bgm_voice: MusicVoice::new(sample_rate),
        bgm_source,
        bgm_state: Arc::clone(&bgm_state),
        se_schedule: ScheduleQueue::with_capacity(config.schedule_queue_capacity),
        se_schedule_overflow_count: AtomicU64::new(0),
        music_clock: Arc::clone(&music_clock),
        events: Arc::clone(&events),
        underrun_accumulator: 0,
        underrun_report_threshold_frames: config.underrun_report_threshold_frames,
    };
    let sender = CommandSender {
        producer: Mutex::new(command_producer),
    };
    let receiver = ReclaimReceiver {
        consumer: reclaim_consumer,
    };
    let bgm_handles = BgmHandles {
        stream_producer: bgm_producer,
        state: bgm_state,
    };
    (
        mixer,
        sender,
        receiver,
        music_producer,
        music_clock,
        events,
        bgm_handles,
    )
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
                // 既に発火して `voices` へ移った分はこれで止まるが、まだ発火していない
                // 予約(`se_schedule`)は `voices` の外にあるため別途取り消す(依頼書
                // 「症状」参照——キャリブレーション画面のキャンセルでメトロノームが
                // 鳴り続ける退行の直接原因)。取り除いた `Arc<SoundData>` はここで
                // drop せず回収キューへ回す(§5.3、`ScheduleQueue::remove_where` 参照)。
                let reclaim = &mut self.reclaim;
                self.se_schedule.remove_where(
                    |se| se.voice_serial == voice_serial,
                    |se| reclaim.send_or_leak(se.sound),
                );
            }
            Command::SetVoiceVolume {
                voice_serial,
                volume,
            } => {
                self.voices.set_volume(voice_serial, volume, default_ramp);
            }
            Command::StopVoicesUsingSound { sound_id } => {
                self.voices.stop_all_using_sound(sound_id, default_ramp);
                // `voices` 側だけでは不十分: `se_schedule` に残る同じ音源への未発火予約は
                // 解放済み `SoundData` を後から鳴らそうとしてしまう(依頼書参照)。
                // ここでも取り除いた分は回収キュー経由でのみ解放する。
                let reclaim = &mut self.reclaim;
                self.se_schedule.remove_where(
                    |se| se.sound_id == sound_id,
                    |se| reclaim.send_or_leak(se.sound),
                );
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
            Command::MusicPrepare => {
                self.music_voice.prepare();
            }
            Command::MusicSeek { frames } => {
                self.music_voice.seek(frames, &mut self.music_source);
            }
            Command::MusicPause => {
                self.music_voice.pause();
            }
            Command::MusicResumeAt { frames } => {
                self.music_voice.resume_at(frames, &mut self.music_source);
            }
            Command::MusicStop => {
                self.music_voice.stop();
            }
            Command::MusicSetLoop { region } => {
                self.music_voice.set_loop(region);
            }
            Command::BgmPrepare => {
                self.bgm_voice.prepare();
            }
            Command::BgmSeek { frames } => {
                self.bgm_voice.seek(frames, &mut self.bgm_source);
            }
            Command::BgmPlay => {
                self.bgm_voice.play();
            }
            Command::BgmStop => {
                self.bgm_voice.stop();
            }
            Command::BgmSetLoop { region } => {
                self.bgm_voice.set_loop(region);
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
        while let Some(&(target_ns, _)) = self.se_schedule.front() {
            // `front()` の不変借用はこのパターンマッチの中で完結させる(タプル先頭の
            // `u64` だけをコピーして即座に手放す)。`front` を束縛して後段で使う書き方
            // だと、ループ本体末尾の `pop_front()`(可変借用)まで不変借用が生き続けて
            // しまい借用チェックに落ちる(clippy::while_let_loop 対応、CI 1.98 化に伴う
            // 修正)。
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

        // --- イベント通知(初期構築仕様『§4.6』, M2-6) ---
        // `MusicRenderOutcome` は M2-5 までに既に実装済みの戻り値をそのまま使う
        // (新たな検知ロジックは作らない。依頼書のとおり)。
        if music_outcome.ended {
            self.events.push_realtime(Event::MusicEnded);
        }
        if music_outcome.looped {
            self.events.push_realtime(Event::MusicLooped {
                restart_frame: music_outcome.loop_restart_frame,
            });
        }
        self.report_underrun(music_outcome.underrun_frames as u32);

        // クリッパの動作検知は開発ビルドのみイベント化する(初期構築仕様『§4.1』
        // 「動作した場合は開発ビルドでイベントとして観測できるようにする」)。
        // 既存の累計カウンタ(`clipper.rs::SoftClipper::engaged_count`)をこのコールバックの
        // 前後で比較するだけで足りる——1サンプルごとではなく、このコールバックで
        // 1回でも動作したかどうかを1件のイベントにまとめる(`ClipperEngaged` は
        // 数を持たないマーカーイベントなので、これで十分)。
        let clipper_engaged_before_frame_loop = self.clipper.engaged_count();

        // --- BGM ボイス(初期構築仕様『§2』M14, M4-3) ---
        // `music_voice` と違ってサンプル精度の予約発火(§4.5)を持たないため、
        // ここでは分割レンダリングは不要。`output` は既に楽曲の生 PCM 保持に
        // 使っているため二重利用できず、かつヒープ確保もできない(§5.3)ので、
        // 固定長のスタックチャンク(`BGM_CHUNK_FRAMES` フレームぶん)へ読み直しながら
        // 下の周波数ループの中で都度リフィルする([`BGM_CHUNK_FRAMES`] のドキュメント参照)。
        let mut bgm_chunk = [0.0f32; BGM_CHUNK_FRAMES * CHANNELS];

        // --- SE ボイス + 楽曲の合成 ---
        for frame_index in 0..frames {
            self.fire_due_se(
                frame_index,
                buffer_start_host_time_ns,
                buffer_end_ns,
                default_ramp,
            );

            // BGM チャンクの先頭フレームに来たら、次のチャンクぶんをレンダリングし直す。
            // `MusicVoice::render` は渡したスライスを毎回まるごと埋める(先頭で無音
            // 初期化してから書く実装、`music.rs` 参照)ため、末尾が
            // `BGM_CHUNK_FRAMES` に満たない最終チャンクでも古いチャンクの残骸が
            // 残る心配は無い。
            if frame_index % BGM_CHUNK_FRAMES == 0 {
                let this_chunk_frames = (frames - frame_index).min(BGM_CHUNK_FRAMES);
                if let Some(bgm_slice) = bgm_chunk.get_mut(..this_chunk_frames * CHANNELS) {
                    self.bgm_voice.render(bgm_slice, &mut self.bgm_source);
                }
            }

            let mut bus_volume = [0.0f32; BUS_COUNT];
            for id in ALL_BUSES {
                bus_volume[id.index()] = self.buses.get_mut(id).volume.advance();
            }

            let (se_l, se_r) = self.voices.mix_frame(&bus_volume);

            let base = frame_index * CHANNELS;
            // 2で書き込んだ楽曲の生 PCM をここで読み直し、Bgm バス音量を適用してから
            // SE 分に加算する(この読み出しの直後に同じ位置へ最終値を上書きする)。
            // BGM ボイスも**同じ Bgm バス**を共有する(初期構築仕様『§2』M14 の
            // 「同じバスを2人が触る」を意図的な設計として受け入れた結果——楽曲〔ライブ中〕と
            // BGM〔メタ画面〕は同時に鳴らない運用が前提だが、画面遷移をまたぐクロス
            // フェード〔片方をフェードアウトしつつもう片方をフェードインする〕は
            // 各ボイス自身のゲイン〔`MusicVoice::gain`〕が独立しているため、この
            // 素朴な加算だけで自然に成立する)。
            let bgm_gain = bus_volume[BusId::Bgm.index()];
            let raw_music_l = output.get(base).copied().unwrap_or(0.0);
            let raw_music_r = output.get(base + 1).copied().unwrap_or(0.0);
            let chunk_local = frame_index % BGM_CHUNK_FRAMES;
            let bgm_base = chunk_local * CHANNELS;
            let raw_bgm_l = bgm_chunk.get(bgm_base).copied().unwrap_or(0.0);
            let raw_bgm_r = bgm_chunk.get(bgm_base + 1).copied().unwrap_or(0.0);

            let mut l = se_l + (raw_music_l + raw_bgm_l) * bgm_gain;
            let mut r = se_r + (raw_music_r + raw_bgm_r) * bgm_gain;

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

        // 開発ビルドのみ発火(`cfg!` はリリースビルドでは定数 false に畳み込まれ、
        // 分岐ごと最適化で消える。実行時コストは事実上ゼロ)。
        let clipper_engaged_this_callback =
            self.clipper.engaged_count() != clipper_engaged_before_frame_loop;
        if cfg!(debug_assertions) && clipper_engaged_this_callback {
            self.events.push_realtime(Event::ClipperEngaged);
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
            self.music_voice.state(),
        );

        // --- BGM ボイスの状態を公開(初期構築仕様『§2』M14, M4-3) ---
        // クロックではなく状態1本だけ(`BgmStatePublisher`)。曲位置・世代カウンタは
        // 発行しない——BGM はそもそもそれらを持たない設計(M14)。
        self.bgm_state.write(self.bgm_voice.state());
    }

    /// アンダーランの集約(初期構築仕様『§4.6』)。
    ///
    /// `MusicVoice::render` は既にコールバック1回ぶんの合計フレーム数を
    /// `MusicRenderOutcome::underrun_frames` として返す(= 1コールバックにつき
    /// ここでの加算は高々1回)。それでも毎コールバック素直にイベント化すると、
    /// デコードスレッドの継続的な遅延など「アンダーランが何十〜何百コールバックも
    /// 連続する」状況で、固定容量64のイベントキューを数百ms〜数秒のうちに
    /// アンダーラン単体で溢れさせてしまう(他の重要なイベントを押し出してしまう)。
    ///
    /// そこで「アンダーランが止んだ(このコールバックは0フレームだった)」か
    /// 「累積が閾値([`Config::underrun_report_threshold_frames`])に達した」の
    /// **いずれか早い方**で1件のイベントにまとめて報告し、累積をリセットする。
    /// 後者が無いと、アンダーランが無限に続くケース(=最も報告してほしいケース)を
    /// 「収まるまで待つ」だけでは永遠に報告できない。
    fn report_underrun(&mut self, frames_this_callback: u32) {
        self.underrun_accumulator = self
            .underrun_accumulator
            .saturating_add(frames_this_callback);
        if self.underrun_accumulator == 0 {
            return;
        }
        let should_flush = frames_this_callback == 0
            || self.underrun_accumulator >= self.underrun_report_threshold_frames;
        if should_flush {
            self.events.push_realtime(Event::Underrun {
                frames: self.underrun_accumulator,
            });
            self.underrun_accumulator = 0;
        }
    }

    /// クリッパが動作(閾値超過)した累計回数(開発ビルドでの動作検知。§4.1)。
    pub fn clipper_engaged_count(&self) -> u64 {
        self.clipper.engaged_count()
    }

    /// 現在の楽曲ボイスの状態(初期構築仕様『§4.3』)。
    pub fn music_state(&self) -> MusicState {
        self.music_voice.state()
    }

    /// 現在の BGM ボイスの状態(初期構築仕様『§2』M14, M4-3)。
    pub fn bgm_state(&self) -> MusicState {
        self.bgm_voice.state()
    }

    /// 現在の BGM ボイスのループ区間(テスト・診断用)。
    #[cfg(test)]
    fn bgm_loop_region(&self) -> Option<(u64, u64)> {
        self.bgm_voice.loop_region()
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
        self.bgm_voice.set_sample_rate(sample_rate);
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
        let (mut mixer, _sender, _reclaim, _music_producer, _music_clock, _events, _bgm) =
            build(Config::default(), 48_000);
        let mut buf = vec![1.0; 32 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn play_command_produces_sound_on_the_very_next_render_call() {
        let (mut mixer, sender, _reclaim, _music_producer, _music_clock, _events, _bgm) =
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
        let (mut mixer, sender, _reclaim, _music_producer, _music_clock, _events, _bgm) =
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
        let (mut mixer, sender, _reclaim, _music_producer, _music_clock, _events, _bgm) =
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
        let (mut mixer, sender, mut reclaim, _music_producer, _music_clock, _events, _bgm) =
            build(config, 48_000);

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
        let (mut mixer, sender, _reclaim, _music_producer, _music_clock, _events, _bgm) =
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

    /// `ConstantDecoder` の総フレーム数ありバージョン(M2-6: 自然終了イベントの検証用)。
    struct FiniteDecoder {
        cursor: u64,
        total: u64,
    }

    impl MusicDecoder for FiniteDecoder {
        fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
            let want = out.len() / CHANNELS;
            let remaining = (self.total.saturating_sub(self.cursor)) as usize;
            let n = want.min(remaining);
            for i in 0..n {
                out[i * CHANNELS] = 1.0;
                out[i * CHANNELS + 1] = 1.0;
            }
            self.cursor += n as u64;
            Ok(n)
        }

        fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
            self.cursor = frame;
            Ok(())
        }

        fn total_frames(&self) -> Option<u64> {
            Some(self.total)
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
        let (mut mixer, sender, _reclaim, _music_producer, _music_clock, _events, _bgm) =
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
            let (mut mixer, sender, _reclaim, _mp, _mc, _events, _bgm) =
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
            let (mut mixer, sender, _reclaim, _mp, _mc, _events, _bgm) =
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
            let (mut mixer, sender, _reclaim, _mp, _mc, _events, _bgm) =
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
            let (mut mixer, sender, _reclaim, _mp, _mc, _events, _bgm) =
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
        let (mut mixer, sender, _reclaim, _mp, _mc, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
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
        let (mut mixer, sender, _reclaim, _mp, _mc, _events, _bgm) =
            build(config, TEST_SAMPLE_RATE);

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

    // =====================================================================
    // バグ修正: StopVoice / StopVoicesUsingSound による未発火予約 SE のキャンセル
    // (キャリブレーション画面キャンセル後もメトロノームが鳴り続ける退行の修正)
    // =====================================================================

    /// 依頼書のテスト要件1: 予約した SE を発火前に `StopVoice` すると鳴らない。
    #[test]
    fn stop_voice_cancels_an_unfired_scheduled_se() {
        let (mut mixer, sender, _reclaim, _mp, _mc, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: 5 * NS_PER_SAMPLE,
            entry: se_entry(1, 1.0),
        }));
        assert!(sender.send(Command::StopVoice { voice_serial: 1 }));

        let mut buf = vec![-1.0; 16 * CHANNELS];
        mixer.render(&mut buf, 0);

        assert!(
            buf.iter().all(|&s| s == 0.0),
            "a scheduled voice cancelled before it fired must never make sound"
        );
        assert_eq!(
            mixer.se_schedule_len(),
            0,
            "the cancelled entry must actually be removed from the queue, not just skipped"
        );
    }

    /// 依頼書のテスト要件2: 別の voice の予約は巻き添えで消えない。
    #[test]
    fn stop_voice_does_not_cancel_a_different_voices_unfired_schedule() {
        let (mut mixer, sender, _reclaim, _mp, _mc, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: 5 * NS_PER_SAMPLE,
            entry: se_entry(1, 0.4),
        }));
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: 5 * NS_PER_SAMPLE,
            entry: se_entry(2, 0.7),
        }));
        assert!(sender.send(Command::StopVoice { voice_serial: 1 }));

        let mut buf = vec![0.0; 16 * CHANNELS];
        mixer.render(&mut buf, 0);

        // voice 1 は消えているので、鳴っているのは voice 2 の音量(0.7)のみのはず。
        assert!(
            (buf[5 * CHANNELS] - 0.7).abs() < 1e-6,
            "the untouched voice's schedule must still fire at its exact offset"
        );
    }

    /// 依頼書のテスト要件3: 既に発火済みの voice への `StopVoice` が従来どおり効く
    /// (退行が無いこと——`se_schedule` 側の新しい削除ロジックが `voices.stop()` の
    /// 既存経路を壊していないことの直接確認)。
    #[test]
    fn stop_voice_on_an_already_fired_voice_still_stops_it_via_the_ramp() {
        let (mut mixer, sender, _reclaim, _mp, _mc, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        // 過去時刻を予約 → 最初の render の先頭フレームで即座に発火して `voices` へ移る。
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: 0,
            entry: se_entry(7, 1.0),
        }));
        let mut buf1 = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut buf1, 0);
        assert!((buf1[0] - 1.0).abs() < 1e-6, "must have already fired");
        assert_eq!(mixer.se_schedule_len(), 0, "queue must be empty once fired");

        // 発火済みの同じ voice_serial へ StopVoice を送る(従来どおりの経路)。
        assert!(sender.send(Command::StopVoice { voice_serial: 7 }));
        let ramp_len = ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as usize;
        let mut buf2 = vec![-1.0; ramp_len * CHANNELS];
        mixer.render(&mut buf2, 4 * NS_PER_SAMPLE);

        let last_l = buf2[(ramp_len - 1) * CHANNELS];
        assert!(
            last_l.abs() < 1e-6,
            "an already-active voice must still ramp down to silence on StopVoice, got {last_l}"
        );
    }

    /// 依頼書のテスト要件4: `StopVoicesUsingSound` で、その音源の未発火予約も消える
    /// (`mw_sound_release` が解放済み音源を後から鳴らしてしまう退行の修正)。
    #[test]
    fn stop_voices_using_sound_cancels_unfired_schedules_referencing_that_sound() {
        let (mut mixer, sender, _reclaim, _mp, _mc, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        let released_sound_id = 100u64;
        let other_sound_id = 200u64;
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: 5 * NS_PER_SAMPLE,
            entry: ScheduledSe {
                voice_serial: 1,
                sound_id: released_sound_id,
                sound: constant_sound(1_000, 0.9),
                bus: BusId::Se,
                volume: 1.0,
            },
        }));
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: 5 * NS_PER_SAMPLE,
            entry: ScheduledSe {
                voice_serial: 2,
                sound_id: other_sound_id,
                sound: constant_sound(1_000, 0.5),
                bus: BusId::Se,
                volume: 1.0,
            },
        }));
        assert!(sender.send(Command::StopVoicesUsingSound {
            sound_id: released_sound_id,
        }));

        let mut buf = vec![0.0; 16 * CHANNELS];
        mixer.render(&mut buf, 0);

        // released_sound_id の予約は消え、other_sound_id の予約(0.5)だけが鳴る。
        assert!(
            (buf[5 * CHANNELS] - 0.5).abs() < 1e-6,
            "only the schedule referencing the still-alive sound must fire"
        );
        assert_eq!(mixer.se_schedule_len(), 0);
    }

    /// 依頼書のテスト要件5: 削除後も昇順の不変条件が保たれ、`fire_due_se` の
    /// 「先頭が未来なら打ち切る」最適化が引き続き正しく動く(生き残った予約が
    /// 正しい順序・正しいオフセットで発火する)。
    #[test]
    fn removal_preserves_ascending_order_so_remaining_schedules_still_fire_correctly() {
        let (mut mixer, sender, _reclaim, _mp, _mc, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        // 1フレームだけ鳴る短い音にして、後続フレームへ音が持ち越されないようにする
        // (`se_entry` の既定 1000 フレームだと発火後ずっと鳴り続けてしまい、
        // 「中間だけ鳴らない」ことを1サンプル単位で検証できないため)。
        let short_entry = |voice_serial: u64, value: f32| ScheduledSe {
            voice_serial,
            sound_id: voice_serial,
            sound: constant_sound(1, value),
            bus: BusId::Se,
            volume: 1.0,
        };
        // 先頭・中間・末尾の3件を仕込み、中間だけ StopVoice でキャンセルする。
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: 3 * NS_PER_SAMPLE,
            entry: short_entry(1, 0.1),
        }));
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: 6 * NS_PER_SAMPLE,
            entry: short_entry(2, 0.2),
        }));
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: 9 * NS_PER_SAMPLE,
            entry: short_entry(3, 0.3),
        }));
        assert!(sender.send(Command::StopVoice { voice_serial: 2 }));

        let mut buf = vec![0.0; 16 * CHANNELS];
        mixer.render(&mut buf, 0);

        assert!(
            (buf[3 * CHANNELS] - 0.1).abs() < 1e-6,
            "the earlier survivor must still fire at its own offset"
        );
        assert_eq!(
            buf[6 * CHANNELS],
            0.0,
            "the cancelled middle entry must not fire"
        );
        assert!(
            (buf[9 * CHANNELS] - 0.3).abs() < 1e-6,
            "the later survivor must still fire at its own offset, proving ordering was not corrupted"
        );
        assert_eq!(
            mixer.se_schedule_len(),
            0,
            "all three must have been consumed (2 fired, 1 cancelled)"
        );
    }

    /// キャンセルされた予約の `Arc<SoundData>` は回収キュー経由でのみ解放される
    /// ことを確認する(音声コールバック内での Arc ドロップ禁止、§5.3)。
    #[test]
    fn stop_voice_routes_the_cancelled_arc_through_the_reclaim_queue_not_an_inline_drop() {
        let (mut mixer, sender, mut reclaim, _mp, _mc, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        assert!(sender.send(Command::SeSchedule {
            host_time_ns: 5 * NS_PER_SAMPLE,
            entry: se_entry(1, 1.0),
        }));
        assert!(sender.send(Command::StopVoice { voice_serial: 1 }));

        assert_eq!(reclaim.pending_len(), 0);
        let mut buf = vec![0.0; 8 * CHANNELS];
        mixer.render(&mut buf, 0);

        assert_eq!(
            reclaim.pending_len(),
            1,
            "the cancelled schedule's Arc<SoundData> must be handed to the reclaim queue"
        );
        reclaim.drain();
        assert_eq!(reclaim.pending_len(), 0);
    }

    // --- 楽曲の予約再生 ---

    #[test]
    fn music_play_scheduled_starts_exactly_at_sample_offset_with_fade_in() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, _events, _bgm) =
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
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, _events, _bgm) =
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
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, _events, _bgm) =
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
        let (mut mixer, sender, _reclaim, mut producer, music_clock, _events, _bgm) =
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
        let (mut mixer, sender, _reclaim, mut producer, music_clock, _events, _bgm) =
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

    // =====================================================================
    // M2-6: イベント通知(初期構築仕様『§4.6』)
    // =====================================================================
    //
    // 破棄数・溢れ・部分ポーリングといった `EventQueue` 自体の汎用的な振る舞いは
    // `event.rs` のユニットテストで検証済み。ここでは「発生源から正しく積まれること」
    // (依頼書のテスト要件1)——`Mixer::render` が既存の戻り値・カウンタから
    // 正しい種別・付随データでイベントを積んでいることに絞って検証する。

    fn drain_events(events: &EventQueue, max: usize) -> (Vec<Event>, u32) {
        let mut out = Vec::new();
        let (_, dropped) = events.drain(max, |e| out.push(e));
        (out, dropped)
    }

    #[test]
    fn music_ended_event_is_pushed_on_natural_end() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);

        let mut decoder = FiniteDecoder {
            cursor: 0,
            total: 5,
        };
        producer.pump(&mut decoder).expect("pump must succeed");
        let mut warmup = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut warmup, 0);
        assert_eq!(mixer.music_state(), MusicState::Ready);

        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        // 総フレーム数(5)より大きい要求 -> このコールバック内で自然終了するはず。
        let mut buf = vec![0.0; 20 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert_eq!(
            mixer.music_state(),
            MusicState::Ready,
            "must have returned to Ready via natural end"
        );

        let (out, dropped) = drain_events(&events, 10);
        assert_eq!(dropped, 0);
        assert!(
            out.contains(&Event::MusicEnded),
            "natural end must push a MusicEnded event, got {out:?}"
        );
    }

    #[test]
    fn music_looped_event_is_pushed_with_the_restart_frame() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);

        // ループ区間の設定を行う `Command` はまだ公開されていない(楽曲制御 API 全体の
        // 公開は M2-6 の範囲外の後続作業)。同一クレート内のテストとして `MusicVoice`
        // (既存の公開メソッド)を直接叩く——新たな検知ロジックを作るのではなく、
        // 既存機能への配線経路が無いだけなので、テストの都合上ここで直接呼ぶ。
        mixer.music_voice.set_loop(Some((0, 8)));

        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let mut buf = vec![0.0; 64 * CHANNELS];
        mixer.render(&mut buf, 0);

        let (out, dropped) = drain_events(&events, 10);
        assert_eq!(dropped, 0);
        let restart_frame = out.iter().find_map(|e| match e {
            Event::MusicLooped { restart_frame } => Some(*restart_frame),
            _ => None,
        });
        assert_eq!(
            restart_frame,
            Some(0),
            "must report the loop region's start frame, got {out:?}"
        );
    }

    /// アンダーランのまとめ方(`Mixer::report_underrun`)の検証:
    /// 閾値に達するまでは連続するコールバックをまたいでイベントを積まず、
    /// 達した時点でまとめて1件だけ報告する。
    #[test]
    fn underrun_is_coalesced_across_callbacks_not_pushed_every_time() {
        let config = Config {
            // preroll_ms(1.0)@1000Hz -> preroll_frames=1、リングバッファ容量はその4倍の
            // 4フレームぶんだけ(意図的に極小にして、数コールバックで枯渇させる)。
            preroll_ms: 1.0,
            underrun_report_threshold_frames: 10,
            ..Config::default()
        };
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, events, _bgm) =
            build(config, TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);
        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });

        // 1コールバック目: リングバッファに残っていた4フレームがちょうど発火直後に
        // 消費される(アンダーラン無し)。
        let mut buf = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert!(drain_events(&events, 10).0.is_empty());

        // 以後 pump しないためリングバッファは空のまま = 毎コールバック4フレームぶん
        // アンダーランする。閾値(10)に達するまではイベントを積まない
        // (4 -> 8 の2コールバックぶん)。
        for _ in 0..2 {
            let mut buf = vec![0.0; 4 * CHANNELS];
            mixer.render(&mut buf, 0);
            assert!(
                drain_events(&events, 10).0.is_empty(),
                "must not push an event before the coalescing threshold is reached"
            );
        }

        // 3コールバック目で累積 12(4+4+4)が閾値10を超え、まとめて1件だけ報告される。
        let mut buf = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut buf, 0);
        let (out, dropped) = drain_events(&events, 10);
        assert_eq!(dropped, 0);
        assert_eq!(
            out,
            vec![Event::Underrun { frames: 12 }],
            "consecutive underrunning callbacks must be coalesced into a single event"
        );
    }

    #[test]
    fn clipper_engaged_event_is_pushed_when_the_clipper_activates() {
        let (mut mixer, sender, _reclaim, _mp, _mc, events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        for i in 0..4u64 {
            sender.send(Command::PlaySe {
                voice_serial: i + 1,
                sound_id: i + 1,
                sound: constant_sound(10, 0.9),
                bus: BusId::Se,
                volume: 1.0,
            });
        }
        let mut buf = vec![0.0; CHANNELS];
        mixer.render(&mut buf, 0);
        assert!(
            mixer.clipper_engaged_count() > 0,
            "test setup must actually clip"
        );

        let (out, dropped) = drain_events(&events, 10);
        assert_eq!(dropped, 0);
        // `cargo test` はデバッグビルドで走る(既定。§4.6「開発ビルドのみ」に対応する
        // 判定は `cfg!(debug_assertions)`)。リリースビルド(`cargo test --release`)では
        // 意図的に発火しないため、このテスト自体もその場合はスキップ相当にする。
        if cfg!(debug_assertions) {
            assert!(out.contains(&Event::ClipperEngaged), "got {out:?}");
        } else {
            assert!(!out.contains(&Event::ClipperEngaged));
        }
    }

    // =====================================================================
    // M2-7: 楽曲制御コマンドの拡充(pause/resume_at/stop/set_loop)
    // =====================================================================
    //
    // `MusicVoice` 自体の状態機械(フェード・ループ折り返し・不連続フラグ)は
    // `music.rs` のユニットテストで既に検証済み。ここでは「`Command` 経由でコマンド
    // キューを通しても同じ結果になること」(= このファイルの配線が正しいこと)に絞る。

    #[test]
    fn music_pause_command_freezes_position_then_resume_at_command_restarts_from_given_frame() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);

        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let mut buf = vec![0.0; 50 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert_eq!(mixer.music_state(), MusicState::Playing);

        sender.send(Command::MusicPause);
        // 既定ランプが尽きるだけの余裕を与える(`music.rs::tests::pause_fades_out_...`
        // と同じ考え方)。
        let ramp_len = ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as usize;
        let mut settle = vec![0.0; (ramp_len + 10) * CHANNELS];
        mixer.render(&mut settle, 0);
        assert_eq!(mixer.music_state(), MusicState::Paused);

        let frozen = mixer.music_voice.position_frames();
        let mut idle = vec![0.0; 20 * CHANNELS];
        mixer.render(&mut idle, 0);
        assert_eq!(
            mixer.music_voice.position_frames(),
            frozen,
            "a paused voice must not advance while no command arrives"
        );

        sender.send(Command::MusicResumeAt { frames: 3 });
        let mut resumed = vec![0.0; CHANNELS];
        mixer.render(&mut resumed, 0);
        assert_eq!(mixer.music_state(), MusicState::Playing);
        assert_eq!(
            mixer.music_voice.position_frames(),
            3,
            "resume_at must reposition immediately, not after the fade-in settles"
        );
    }

    #[test]
    fn music_stop_command_returns_to_ready_and_resets_position_to_zero() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);

        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let mut buf = vec![0.0; 50 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert!(
            mixer.music_voice.position_frames() > 0,
            "test setup must actually have advanced before stopping"
        );

        sender.send(Command::MusicStop);
        // `Playing` からの stop はフェードアウト経由(`MusicVoice::stop` 参照)なので、
        // 既定ランプが尽きるだけの余裕を与える。
        let ramp_len = ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as usize;
        let mut settle = vec![0.0; (ramp_len + 10) * CHANNELS];
        mixer.render(&mut settle, 0);

        assert_eq!(mixer.music_state(), MusicState::Ready);
        assert_eq!(mixer.music_voice.position_frames(), 0);
    }

    #[test]
    fn music_set_loop_command_wraps_at_region_end_then_none_clears_it() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);

        // `MusicSetLoop` と `MusicPlayScheduled` は同じコマンドキュー上にあるので、
        // 両方を送ってから1回 render すれば同じコールバックでまとめて消化される
        // (`drain_commands` はコールバック先頭で溜まっている分を全部処理する)。
        sender.send(Command::MusicSetLoop {
            region: Some((0, 8)),
        });
        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let mut buf = vec![0.0; 64 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert_eq!(mixer.music_voice.loop_region(), Some((0, 8)));

        let (out, dropped) = drain_events(&events, 10);
        assert_eq!(dropped, 0);
        assert!(
            out.contains(&Event::MusicLooped { restart_frame: 0 }),
            "the loop region must actually wrap and report restart_frame=0, got {out:?}"
        );

        sender.send(Command::MusicSetLoop { region: None });
        let mut after = vec![0.0; CHANNELS];
        mixer.render(&mut after, 0);
        assert_eq!(
            mixer.music_voice.loop_region(),
            None,
            "MusicSetLoop{{ region: None }} must clear the loop"
        );
    }

    /// 依頼書のテスト要件4: `mw_music_state()` の実体になるクロックスナップショットが
    /// Loading → Ready → Playing → Paused の4状態すべてを正しく反映すること。
    #[test]
    fn music_clock_snapshot_reflects_all_four_states_through_the_playback_lifecycle() {
        let (mut mixer, sender, _reclaim, mut producer, music_clock, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);

        // Loading: まだ pump していない。
        let mut loading_buf = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut loading_buf, 0);
        let snap_loading = music_clock.snapshot();
        assert_eq!(snap_loading.state, MusicState::Loading);
        assert!(!snap_loading.is_playing);

        // Ready: プリロールを満たす。
        let mut decoder = ConstantDecoder {
            cursor: 0,
            value: 1.0,
        };
        producer.pump(&mut decoder).expect("pump must succeed");
        let mut warmup = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut warmup, 0);
        assert_eq!(mixer.music_state(), MusicState::Ready);
        let snap_ready = music_clock.snapshot();
        assert_eq!(snap_ready.state, MusicState::Ready);
        assert!(!snap_ready.is_playing);

        // Playing。
        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let mut playing_buf = vec![0.0; 20 * CHANNELS];
        mixer.render(&mut playing_buf, 0);
        assert_eq!(mixer.music_state(), MusicState::Playing);
        let snap_playing = music_clock.snapshot();
        assert_eq!(snap_playing.state, MusicState::Playing);
        assert!(snap_playing.is_playing);

        // Paused。
        sender.send(Command::MusicPause);
        let ramp_len = ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as usize;
        let mut settle = vec![0.0; (ramp_len + 10) * CHANNELS];
        mixer.render(&mut settle, 0);
        assert_eq!(mixer.music_state(), MusicState::Paused);
        let snap_paused = music_clock.snapshot();
        assert_eq!(snap_paused.state, MusicState::Paused);
        assert!(!snap_paused.is_playing);
    }

    /// 依頼書のテスト要件5: seek だけでなく resume_at でも世代カウンタが進むこと
    /// (`render_bumps_generation_exactly_once_on_seek_discontinuity` の resume_at 版)。
    #[test]
    fn render_bumps_generation_on_resume_at_discontinuity() {
        let (mut mixer, sender, _reclaim, mut producer, music_clock, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);

        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let mut buf = vec![0.0; 20 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert_eq!(music_clock.snapshot().generation, 0);

        sender.send(Command::MusicResumeAt { frames: 500 });
        let mut buf2 = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut buf2, 100_000 * NS_PER_SAMPLE);

        let snap = music_clock.snapshot();
        assert_eq!(
            snap.generation, 1,
            "resume_at must bump the generation exactly once"
        );
        assert_eq!(
            snap.song_frames, 500,
            "the post-resume position must be reflected immediately"
        );

        // 追加の render では世代がさらに進まない(不連続はそのバッファだけの出来事)。
        let mut buf3 = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut buf3, 200_000 * NS_PER_SAMPLE);
        assert_eq!(music_clock.snapshot().generation, 1);
    }

    // =====================================================================
    // M2-7: 楽曲ロード(`Command::MusicPrepare`, `mw-ffi::mw_music_set` の下ごしらえ)
    // =====================================================================

    #[test]
    fn music_prepare_command_resets_to_loading_and_clears_loop_region_even_while_playing() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);

        // ループ終端(1000)をこのレンダリング範囲よりずっと先に置き、途中で
        // 折り返さないようにする(このテストの主眼は「position が進んだこと」を
        // 素直に確認することであり、折り返しの往復で偶然 0 に戻る余地を無くす)。
        sender.send(Command::MusicSetLoop {
            region: Some((0, 1_000)),
        });
        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let mut buf = vec![0.0; 50 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert_eq!(mixer.music_state(), MusicState::Playing);
        assert!(mixer.music_voice.position_frames() > 0);

        sender.send(Command::MusicPrepare);
        let mut after = vec![0.0; CHANNELS];
        mixer.render(&mut after, 0);

        // `MusicVoice::prepare` は `MusicFrameSource` に一切触れない(モジュール doc
        // 参照)ため、リングバッファに前曲の PCM がまだ残っていれば `is_ready()` は
        // 真のままで、`state` はこの同じ render 呼び出しの中で `Loading` から
        // すぐ `Ready` へ戻ることがある(`MusicVoice::render` の自動遷移)。
        // ここで固定化したいのは「二度と `Playing` には戻らない(ゲインが 0 で
        // 固まったまま鳴り続ける事故が起きない)」ことと、位置・ループ区間が
        // 確実にリセットされることの2点——リングバッファ自体の掃除
        // (`is_ready()` を本当に偽へ戻す)は `MusicSeek` の役目であり、その
        // 組み合わせは `music_set_command_sequence_recovers_cleanly_from_a_song_still_playing`
        // で別途検証する。
        assert_ne!(
            mixer.music_state(),
            MusicState::Playing,
            "prepare must never leave the voice stuck in Playing with a frozen gain"
        );
        assert_eq!(mixer.music_voice.position_frames(), 0);
        assert_eq!(mixer.music_voice.loop_region(), None);
    }

    /// `MusicPrepare` → `MusicStop` → `MusicSeek { frames: 0 }` の並びを実際に踏んだ
    /// ときの結果を固定化する回帰テスト。
    ///
    /// **`mw-ffi::mw_music_set` は現在この3つのうち `MusicPrepare` と `MusicSeek` の
    /// 2つしか送らない**(`MusicStop` を挟む必要が無いことがこのテストで固定化されて
    /// いる不変条件そのものであるため、送信側から省かれた——`ffi.rs::mw_music_set` の
    /// ドキュメント参照)。このテストが `MusicStop` を明示的に間に挟んだままにしてある
    /// のは、「万一どこかが `MusicStop` を送っても壊れない」という不変条件そのものを
    /// 回帰させないため。
    ///
    /// この順序が肝心な理由: 前の曲が `Playing` 中に曲を切り替えると、`MusicPrepare`
    /// より先に `MusicStop` だけを送った場合は `pending_settle = Stop` を積んで
    /// ゲインを 0 へ向けてランプさせる。そこへ`MusicSeek` が
    /// `pending_settle = None`(位置の付け替えは保留中のフェードの前提を壊すため
    /// 破棄する、`MusicVoice::seek` 参照)を上書きしてしまうと、`state` は
    /// `Playing` のまま・ゲインは 0 へ向かったランプの target だけが残り、
    /// 誰も戻さないまま鳴らなくなる(新しい曲が無音のまま「再生中」になる事故)。
    /// `MusicPrepare` を先に送ることで `pending_settle`/`gain` を無条件で
    /// 初期化しておき、直後の `MusicStop` を完全な no-op にしてこの事故を防ぐ。
    #[test]
    fn music_set_command_sequence_recovers_cleanly_from_a_song_still_playing() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);

        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let mut buf = vec![0.0; 50 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert_eq!(mixer.music_state(), MusicState::Playing);

        // `MusicPrepare` の後に `MusicStop` を挟んでも安全であることを固定化する
        // (現在の `mw_music_set` はこの `MusicStop` を送らないが、送っても no-op に
        // なるという不変条件を守り続けるための回帰テスト。上のドキュメント参照)。
        sender.send(Command::MusicPrepare);
        sender.send(Command::MusicStop);
        sender.send(Command::MusicSeek { frames: 0 });
        let mut after = vec![0.0; CHANNELS];
        mixer.render(&mut after, 0);

        assert_eq!(mixer.music_state(), MusicState::Loading);
        assert_eq!(mixer.music_voice.position_frames(), 0);

        // 新しい曲としてプリロールを満たせば、ゲインが固まったりせず正しく
        // フェードインして聞こえる(固まっていたら振幅が 0 のまま伸びない)。
        let mut decoder2 = ConstantDecoder {
            cursor: 0,
            value: 1.0,
        };
        producer.pump(&mut decoder2).expect("pump must succeed");
        let mut warmup = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut warmup, 0);
        assert_eq!(mixer.music_state(), MusicState::Ready);

        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let ramp_len = ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as usize;
        let mut settle = vec![0.0; (ramp_len + 10) * CHANNELS];
        mixer.render(&mut settle, 0);
        assert_eq!(mixer.music_state(), MusicState::Playing);
        let last = settle[(ramp_len + 9) * CHANNELS];
        assert!(
            (last - 1.0).abs() < 1e-4,
            "must fade in to full volume, not stay stuck silent forever; got {last}"
        );
    }

    /// 選曲プレビュー(M4 終了条件)の実際の使われ方を模した回帰テスト:
    /// **ユーザーが曲を連打で切り替える** ―― A を再生中に B へ切り替え、B が
    /// まだプリロールを終える(=Ready になる)前にさらに C へ切り替える。
    /// `mw-ffi::mw_music_set` が実際に送る2コマンド列(`MusicPrepare` →
    /// `MusicSeek { frames: 0 }`)をそのまま踏む(このファイル内の
    /// `music_set_command_sequence_recovers_cleanly_from_a_song_still_playing` と
    /// 同じ理由でコマンド列を直接送る——`mw-ffi` 側はデコーダを差し替えてから同じ
    /// 2コマンドを送るだけで、デコーダの差し替え自体はこのテストでは「次にどの
    /// デコーダを `pump` するか」で表現している)。
    ///
    /// 固定化したいのは3点:
    /// 1. 切り替えの直後(まだ次の曲が Ready になっていない間)は無音のままで、
    ///    前の曲の PCM が漏れて聞こえたりしない(`prepare()` がゲインを即座に 0 へ
    ///    落とすため、ランプ経由の残響すら無いはず)。
    /// 2. 前の曲がまだ Ready になっていないうちにもう一度切り替えても `Loading` に
    ///    固まったまま(スタック)にならない――最終的に指定した曲(C)が Ready になる。
    /// 3. 最終的に鳴るのは最後に指定した曲(C)の PCM だけで、途中でキャンセルされた
    ///    A・B の値がどこにも紛れ込まない。
    #[test]
    fn preview_style_rapid_track_switch_never_leaks_previous_audio_and_settles_on_latest_track() {
        let (mut mixer, sender, _reclaim, mut producer, _music_clock, _events, _bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);

        // 曲 A: プリロール完了・再生中にする(選曲プレビューでまず1曲目を鳴らした状態)。
        make_music_ready(&mut mixer, &mut producer, 0.3);
        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let mut buf = vec![0.0; 20 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert_eq!(mixer.music_state(), MusicState::Playing);

        // ユーザーが即座に曲 B へ切り替える(`mw_music_set` 相当の2コマンド)。
        // B はまだ一切 pump していない ―― デコードスレッドがまだ1パケットも
        // 届けていない、実運用で最もありふれた「ロード中に切り替えた」状態。
        sender.send(Command::MusicPrepare);
        sender.send(Command::MusicSeek { frames: 0 });
        let mut after_switch_to_b = vec![-1.0; 8 * CHANNELS];
        mixer.render(&mut after_switch_to_b, 0);
        assert_eq!(mixer.music_state(), MusicState::Loading);
        assert!(
            after_switch_to_b.iter().all(|&s| s == 0.0),
            "switching must be silent immediately, not leak track A's PCM while B loads"
        );

        // B がまだ Ready にすらなっていないうちに、さらに曲 C へ切り替える(連打)。
        sender.send(Command::MusicPrepare);
        sender.send(Command::MusicSeek { frames: 0 });
        let mut after_switch_to_c = vec![-1.0; 8 * CHANNELS];
        mixer.render(&mut after_switch_to_c, 0);
        assert_eq!(
            mixer.music_state(),
            MusicState::Loading,
            "a second switch before the first one finished loading must not get stuck"
        );
        assert!(after_switch_to_c.iter().all(|&s| s == 0.0));

        // 曲 C のプリロールが満たされたら、素直に Ready まで到達する(スタックしない)。
        let mut decoder_c = ConstantDecoder {
            cursor: 0,
            value: 0.9,
        };
        producer.pump(&mut decoder_c).expect("pump must succeed");
        let mut warmup = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut warmup, 0);
        assert_eq!(
            mixer.music_state(),
            MusicState::Ready,
            "must reach Ready for the *last* requested track (C), not remain stuck in Loading"
        );

        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        let ramp_len = ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as usize;
        let mut settle = vec![0.0; (ramp_len + 20) * CHANNELS];
        mixer.render(&mut settle, 0);
        assert_eq!(mixer.music_state(), MusicState::Playing);
        let last = settle[(ramp_len + 19) * CHANNELS];
        assert!(
            (last - 0.9).abs() < 1e-4,
            "must fade in to track C's value (0.9), got {last}"
        );
        for &sample in settle.iter() {
            assert!(
                (sample - 0.3).abs() > 1e-3,
                "track A's PCM value (0.3) must never leak into the output after switching \
                 tracks twice; got a sample of {sample}"
            );
        }
    }

    // =====================================================================
    // M4-3: BGM(初期構築仕様『§2』M14「BGM 用の2本目の楽曲ボイス」)
    // =====================================================================
    //
    // `bgm_voice` は `music_voice` と同じ `MusicVoice` 型を転用しているため、
    // フェード・ループそのものの数値的な正しさ(ランプの傾き・境界値等)は
    // `music.rs`/このファイル上部の楽曲テストで既に固定化済み。ここで確認したいのは
    // **「BGM 専用の配線」**だけに絞ってある: BGM コマンドが `music_voice` ではなく
    // `bgm_voice` を動かすこと、`BgmStatePublisher` へ正しく状態が公開されること、
    // 音楽クロック(`music_clock`)に一切触れないこと、そして楽曲ボイスと同じ Bgm バスを
    // 通って**両方が加算される**こと(初期構築仕様『§2』M14 が意図する
    // 「画面遷移をまたぐクロスフェード」が成立するための前提)。

    /// `build` 直後の `Mixer` へ、BGM ボイスが `Ready` になるまで PCM を供給する
    /// (`make_music_ready` の BGM 版)。
    fn make_bgm_ready(mixer: &mut Mixer, producer: &mut MusicStreamProducer, value: f32) {
        let mut decoder = ConstantDecoder { cursor: 0, value };
        producer.pump(&mut decoder).expect("pump must succeed");
        let mut warmup = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut warmup, 0);
        assert_eq!(mixer.bgm_state(), MusicState::Ready);
    }

    #[test]
    fn bgm_ready_then_play_fades_in_and_is_scaled_by_the_bgm_bus() {
        let (mut mixer, sender, _reclaim, _mp, _mc, _events, mut bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_bgm_ready(&mut mixer, &mut bgm.stream_producer, 1.0);
        // `BgmStatePublisher` は `mixer.bgm_state()` と同じ値を公開しているはず
        // (同じ音声スレッド書き込みを別経路〔ロック無しの Arc〕から読むだけ)。
        assert_eq!(bgm.state.read(), MusicState::Ready);

        // Bgm バスを 0.5 に設定してから再生する(バス音量が正しく適用されることも
        // 一緒に確認する)。
        sender.send(Command::SetBusVolume {
            bus: BusId::Bgm,
            volume: 0.5,
        });
        assert!(sender.send(Command::BgmPlay));

        let ramp_len = ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as usize;
        let mut buf = vec![-1.0; (ramp_len + 20) * CHANNELS];
        mixer.render(&mut buf, 0);

        assert_eq!(mixer.bgm_state(), MusicState::Playing);
        assert_eq!(bgm.state.read(), MusicState::Playing);
        // ランプ収束後は「BGM のフェードイン(1.0)× Bgm バス(0.5)」= 0.5 に漸近する。
        let settled = buf[(ramp_len + 19) * CHANNELS];
        assert!(
            (settled - 0.5).abs() < 1e-3,
            "expected ~0.5 (full BGM gain x 0.5 bus volume), got {settled}"
        );
        // フェードインの最初のサンプルは無音より大きく、収束値未満(まだ途中)。
        assert!(buf[0] > 0.0 && buf[0] < settled);
    }

    #[test]
    fn bgm_stop_fades_out_then_returns_to_ready_and_resets_position() {
        let (mut mixer, sender, _reclaim, _mp, _mc, _events, mut bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_bgm_ready(&mut mixer, &mut bgm.stream_producer, 1.0);
        sender.send(Command::BgmPlay);

        let mut warmup = vec![0.0; 50 * CHANNELS];
        mixer.render(&mut warmup, 0);
        assert_eq!(mixer.bgm_state(), MusicState::Playing);
        assert!(mixer.bgm_voice.position_frames() > 0);

        assert!(sender.send(Command::BgmStop));
        let ramp_len = ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as usize;
        let mut settle = vec![0.0; (ramp_len + 10) * CHANNELS];
        mixer.render(&mut settle, 0);

        assert_eq!(mixer.bgm_state(), MusicState::Ready);
        assert_eq!(mixer.bgm_voice.position_frames(), 0);
        assert_eq!(bgm.state.read(), MusicState::Ready);
    }

    #[test]
    fn bgm_and_music_voices_sum_together_on_the_shared_bgm_bus() {
        // 初期構築仕様『§2』M14「画面遷移をまたぐクロスフェード」の前提: 楽曲ボイスと
        // BGM ボイスは同じ Bgm バスを共有し、両方が鳴っていれば単純に加算される
        // (「片方をフェードアウトしつつもう片方をフェードインする」がこの加算だけで
        // 自然に成立する。`Mixer::render` のコメント参照)。
        let (mut mixer, sender, _reclaim, mut producer, _mc, _events, mut bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 0.3);
        make_bgm_ready(&mut mixer, &mut bgm.stream_producer, 0.4);

        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        sender.send(Command::BgmPlay);

        let ramp_len = ms_to_samples(Config::DEFAULT_RAMP_MS, TEST_SAMPLE_RATE) as usize;
        let mut buf = vec![0.0; (ramp_len + 20) * CHANNELS];
        mixer.render(&mut buf, 0);

        assert_eq!(mixer.music_state(), MusicState::Playing);
        assert_eq!(mixer.bgm_state(), MusicState::Playing);
        // Bgm バスは既定値 1.0 のまま: 0.3(楽曲) + 0.4(BGM) = 0.7。
        let settled = buf[(ramp_len + 19) * CHANNELS];
        assert!(
            (settled - 0.7).abs() < 1e-3,
            "expected the two voices to sum to ~0.7 on the shared Bgm bus, got {settled}"
        );
    }

    #[test]
    fn bgm_commands_never_touch_the_music_clock_or_the_song_voice() {
        let (mut mixer, sender, _reclaim, _mp, music_clock, _events, mut bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_bgm_ready(&mut mixer, &mut bgm.stream_producer, 1.0);

        sender.send(Command::BgmPlay);
        let mut buf = vec![0.0; 100 * CHANNELS];
        mixer.render(&mut buf, 0);
        sender.send(Command::BgmSetLoop {
            region: Some((0, 40)),
        });
        sender.send(Command::BgmStop);
        mixer.render(&mut buf, 100 * NS_PER_SAMPLE);

        // 楽曲ボイスは一度も触っていないので `Loading` のまま。
        assert_eq!(mixer.music_state(), MusicState::Loading);
        // 音楽クロックの相関点・世代カウンタは BGM の再生・ループ・停止では一切動かない
        // (発行元は楽曲ボイスに固定、初期構築仕様『§2』M14)。
        let snapshot = music_clock.snapshot();
        assert_eq!(snapshot.song_frames, 0);
        assert_eq!(snapshot.generation, 0);
        assert!(!snapshot.is_playing);
    }

    #[test]
    fn bgm_prepare_and_seek_reset_the_bgm_voice_not_the_song_voice() {
        // `mw_bgm_set` が実際に送る2コマンド(デコーダ差し替え → Prepare → Seek{0})の
        // うちコマンド部分だけをここで再現する(`music_set_command_sequence_...` の
        // BGM 版)。対象が `bgm_voice` に限られ、`music_voice` は無傷であることを見る。
        let (mut mixer, sender, _reclaim, mut producer, _mc, _events, mut bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_music_ready(&mut mixer, &mut producer, 1.0);
        make_bgm_ready(&mut mixer, &mut bgm.stream_producer, 1.0);
        sender.send(Command::MusicPlayScheduled { host_time_ns: 0 });
        sender.send(Command::BgmPlay);

        let mut buf = vec![0.0; 50 * CHANNELS];
        mixer.render(&mut buf, 0);
        assert_eq!(mixer.music_state(), MusicState::Playing);
        assert_eq!(mixer.bgm_state(), MusicState::Playing);

        sender.send(Command::BgmPrepare);
        sender.send(Command::BgmSeek { frames: 0 });
        let mut after = vec![0.0; CHANNELS];
        mixer.render(&mut after, 0);

        assert_eq!(
            mixer.bgm_state(),
            MusicState::Loading,
            "BgmPrepare must reset only the BGM voice"
        );
        assert_eq!(mixer.bgm_voice.position_frames(), 0);
        assert_eq!(
            mixer.music_state(),
            MusicState::Playing,
            "the song voice must be completely unaffected by BGM commands"
        );
    }

    #[test]
    fn bgm_loop_region_wraps_repeatedly_like_the_song_preview_loop() {
        let (mut mixer, sender, _reclaim, _mp, _mc, _events, mut bgm) =
            build(Config::default(), TEST_SAMPLE_RATE);
        make_bgm_ready(&mut mixer, &mut bgm.stream_producer, 1.0);
        sender.send(Command::BgmPlay);
        sender.send(Command::BgmSetLoop {
            region: Some((0, 12)),
        });
        // コマンドは `render` の先頭(`drain_commands`)で消化されるまでキューに
        // 留まる(初期構築仕様『§5.2』)。ループ区間を1回 render を通してから確認する。
        let mut warmup = vec![0.0; CHANNELS];
        mixer.render(&mut warmup, 0);
        assert_eq!(mixer.bgm_loop_region(), Some((0, 12)));

        let mut loop_events_seen = 0;
        for _ in 0..20 {
            let mut buf = vec![0.0; 4 * CHANNELS];
            mixer.render(&mut buf, 0);
            if mixer.bgm_voice.position_frames() < 12 {
                loop_events_seen += 1;
            }
        }
        assert!(
            loop_events_seen > 0,
            "the BGM voice must keep wrapping within its loop region"
        );
        assert!(mixer.bgm_voice.position_frames() < 12);

        sender.send(Command::BgmSetLoop { region: None });
        let mut after_clear = vec![0.0; 4 * CHANNELS];
        mixer.render(&mut after_clear, 0);
        assert_eq!(mixer.bgm_loop_region(), None);
    }
}
