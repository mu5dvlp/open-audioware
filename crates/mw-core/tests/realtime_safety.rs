//! リアルタイム安全性の検証(初期構築仕様 §5.3, §8)。
//!
//! 「コールバック相当経路のアロケーションゼロをテストで固定化」を、カウンティング
//! アロケータフックで実施する。`#[global_allocator]` はプロセス全体(=このテスト
//! バイナリ全体)に適用されるため、専用の統合テストファイルとして分離してある
//! (`mw-core` の他ユニットテストと同じバイナリに混ぜない)。
//!
//! 手法: `TRACKING` フラグが立っている間だけアロケーション/デアロケーション回数を数える。
//! `Renderer::render`(= 音声コールバック相当の経路)呼び出しの前後だけフラグを立てることで、
//! セットアップ側(ボイスコマンドの用意、バッファ確保等)のアロケーションを除外し、
//! 音声スレッド経路そのものだけを検証する。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use mw_core::{
    CHANNELS, Command, Config, DecodeError, Event, MusicDecoder, MusicState, Renderer, ScheduledSe,
    SoundData,
};

struct CountingAllocator;

static TRACKING: AtomicBool = AtomicBool::new(false);
static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static DEALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

// SAFETY: すべての呼び出しをそのまま `System` へ委譲するだけの薄いラッパ。
// 追加の状態はアトミックカウンタのみで、`GlobalAlloc` の安全性契約に影響しない。
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACKING.load(Ordering::SeqCst) {
            ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if TRACKING.load(Ordering::SeqCst) {
            DEALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if TRACKING.load(Ordering::SeqCst) {
            ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn constant_sound(frames: usize, value: f32) -> Arc<SoundData> {
    Arc::new(SoundData {
        sample_rate: 48_000,
        frames,
        interleaved: vec![value; frames * CHANNELS],
    })
}

/// M2-6: 総フレーム数無制限(常に供給し続けられる)のフェイクデコーダ。
/// リングバッファ容量をわざと小さくしたうえで一度だけ `pump` し、以後は
/// 二度と `pump` しないことで「デコードスレッドが停止した」状況を模し、
/// 継続的なアンダーラン(`Event::Underrun`)を誘発する
/// (`crates/mw-core/src/mixer.rs` の `underrun_is_coalesced_across_callbacks_...`
/// テストと同じ考え方)。
struct UnboundedMusicDecoder {
    cursor: u64,
}

impl MusicDecoder for UnboundedMusicDecoder {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
        let n = out.len() / CHANNELS;
        for i in 0..n {
            out[i * CHANNELS] = 0.2;
            out[i * CHANNELS + 1] = 0.2;
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

/// 音声コールバック相当経路(`Renderer::render` およびそこから呼ばれる全コード)は
/// ヒープアロケーション・デアロケーションを一切行わない(初期構築仕様 §5.3)。
///
/// ボイス満杯 → スティール、停止、バスフェード、クリッパ動作を全部含む
/// リアルなシナリオを事前に仕込んでおき、その後の `render` 呼び出しだけを計測する。
///
/// M2-6: イベント通知(初期構築仕様『§4.6』)の書き込み経路(`EventQueue::push_realtime`,
/// `mixer.rs::Mixer::report_underrun` 等)もこの経路から呼ばれるため、
/// 楽曲の自然終了・アンダーラン・クリッパ動作を実際に発生させ、イベントが積まれる
/// 経路そのものをアロケーション計測の対象に含める。
#[test]
fn render_path_never_allocates_or_deallocates() {
    // preroll を極小(1ms)にし、リングバッファ容量を1コールバック(512フレーム)より
    // ずっと小さくする(初期構築仕様『§4.7』の `preroll_ms` から
    // `capacity_frames = preroll_frames * 4` で決まる。`stream.rs::channel` 参照)。
    // これにより、最初の1回だけ `pump` してその後は二度と供給しないだけで、
    // 「デコードスレッドが停止し続けた」状況を再現できる。
    let config = Config {
        preroll_ms: 1.0,
        ..Config::default()
    };
    let max_voices = config.max_voices;
    let (mut renderer, sender, mut reclaim, mut music_producer, _music_clock, events) =
        Renderer::build(config, 48_000);

    let mut decoder = UnboundedMusicDecoder { cursor: 0 };
    music_producer
        .pump(&mut decoder)
        .expect("pump must succeed");
    let mut warmup = vec![0.0f32; 4 * CHANNELS];
    renderer.render(&mut warmup, 0); // Loading -> Ready(セットアップ、計測対象外)。
    assert_eq!(renderer.music_state(), MusicState::Ready);
    assert!(sender.send(Command::MusicPlayScheduled { host_time_ns: 0 }));

    // ボイスプールを満杯にし、さらに追加でスティールを誘発する(セットアップ段階。
    // ここでのアロケーションはトラッキング対象外)。
    for i in 0..(max_voices as u64 + 8) {
        let ok = sender.send(Command::PlaySe {
            voice_serial: i + 1,
            sound_id: i + 1,
            sound: constant_sound(4_096, 0.6),
            bus: mw_core::BusId::Se,
            volume: 1.0,
        });
        assert!(
            ok,
            "command queue must have room for this scripted scenario"
        );
    }
    assert!(sender.send(Command::StopVoice { voice_serial: 3 }));
    assert!(sender.send(Command::SetVoiceVolume {
        voice_serial: 5,
        volume: 0.2,
    }));
    assert!(sender.send(Command::SetBusVolume {
        bus: mw_core::BusId::Master,
        volume: 0.8,
    }));
    assert!(sender.send(Command::BusFade {
        bus: mw_core::BusId::Bgm,
        target: 0.3,
        ms: 50.0,
    }));

    let buffer_frames = 512u64;
    let sample_rate = 48_000u64;
    // 512フレーム@48kHz のバッファ長(ns)。M2-5: `Mixer::render` へ渡すホスト時刻を
    // コールバックごとに実バッファ長ぶん進め、実運用に近い形にする。
    let buffer_duration_ns = buffer_frames * 1_000_000_000 / sample_rate;

    // M2-5: 予約発音のソート済みキュー(`ScheduleQueue::try_insert`/`pop_front`)への
    // 挿入・取り出しも音声コールバック経路(`Mixer::apply_command`/`fire_due_se`)で
    // 起こるため、ここで検証対象に含める。過去(即時発音)・ループ序盤・ループ終盤の
    // 3パターンを混ぜ、挿入されたまま「未来」で残るものと実際に発火するものの両方の
    // 経路を通す。
    assert!(sender.send(Command::SeSchedule {
        host_time_ns: 0, // 既に過去 = 最初のコールバックの先頭で即時発音
        entry: ScheduledSe {
            voice_serial: 9_001,
            sound_id: 9_001,
            sound: constant_sound(64, 0.3),
            bus: mw_core::BusId::Se,
            volume: 1.0,
        },
    }));
    assert!(sender.send(Command::SeSchedule {
        host_time_ns: buffer_duration_ns * 50 + 3, // 途中のコールバックのバッファ内で発火
        entry: ScheduledSe {
            voice_serial: 9_002,
            sound_id: 9_002,
            sound: constant_sound(64, 0.3),
            bus: mw_core::BusId::Se,
            volume: 1.0,
        },
    }));
    assert!(sender.send(Command::SeSchedule {
        host_time_ns: buffer_duration_ns * 1_000, // ループが終わるまで発火せず残る
        entry: ScheduledSe {
            voice_serial: 9_003,
            sound_id: 9_003,
            sound: constant_sound(64, 0.3),
            bus: mw_core::BusId::Se,
            volume: 1.0,
        },
    }));

    // バグ修正: `StopVoice`/`StopVoicesUsingSound` による未発火予約のキャンセル
    // (`ScheduleQueue::remove_where`)もこの経路(`Mixer::apply_command`)から呼ばれる
    // ため、ここで検証対象に含める。取り除いた `Arc<SoundData>` を回収キュー経由へ
    // 転送するだけでその場では drop しない設計(§5.3)なので、これも 0 アロケーション/
    // デアロケーションのはず。コマンドは同じキューへ FIFO で積まれるため、最初の
    // 計測対象 `render` 呼び出しの中で「予約 → 即キャンセル」の両方が消化される。
    assert!(sender.send(Command::SeSchedule {
        host_time_ns: buffer_duration_ns * 80,
        entry: ScheduledSe {
            voice_serial: 9_004,
            sound_id: 9_004,
            sound: constant_sound(64, 0.3),
            bus: mw_core::BusId::Se,
            volume: 1.0,
        },
    }));
    assert!(sender.send(Command::StopVoice {
        voice_serial: 9_004,
    }));
    assert!(sender.send(Command::SeSchedule {
        host_time_ns: buffer_duration_ns * 90,
        entry: ScheduledSe {
            voice_serial: 9_005,
            sound_id: 9_005,
            sound: constant_sound(64, 0.3),
            bus: mw_core::BusId::Se,
            volume: 1.0,
        },
    }));
    assert!(sender.send(Command::StopVoicesUsingSound { sound_id: 9_005 }));

    let mut buffer = vec![0.0f32; buffer_frames as usize * CHANNELS];

    // `EventQueue::drain`/`push_side_channel` が内部で使う `Mutex` は、プラットフォーム
    // によっては**初回のロック時**に一度だけ OS 側の内部状態を確保することがある
    // (このテストと同じカウンティングアロケータで実測して判明した)。これは
    // `drain`/`push_side_channel` という**非リアルタイムスレッド専用の経路**のコストで
    // あり、音声スレッド側の `push_realtime`(§5.3 の対象、`event.rs` モジュール doc
    // 参照)はこれらの `Mutex` に一切触れないため実運用の制約には抵触しない。
    // このテストの計測対象は「音声コールバック経路 + その後の定常的なポーリング」
    // なので、初回だけの初期化コストは事前に(トラッキング外で)払っておく。
    events.drain(1, |_| {});

    // M2-6: `EventQueue::drain` 自体もロック・アロケーション無しで実装してある
    // (`event.rs` モジュール doc)。ここで毎コールバック後にポーリングを挟むことで
    // 「音声コールバック → 積む → ゲームスレッドがポーリングして取り出す」の
    // 一往復をまるごとトラッキング対象に含める(かつ、後から溢れて上書きされる前に
    // 都度取り出すことで、どの種別が実際に発生したかを確実に観測できる)。
    let mut saw_underrun = false;
    let mut saw_clipper_engaged = false;

    TRACKING.store(true, Ordering::SeqCst);
    let mut host_time_ns = 0u64;
    for _ in 0..200 {
        renderer.render(&mut buffer, host_time_ns);
        host_time_ns += buffer_duration_ns;

        events.drain(64, |event| match event {
            Event::Underrun { .. } => saw_underrun = true,
            Event::ClipperEngaged => saw_clipper_engaged = true,
            _ => {}
        });
    }
    TRACKING.store(false, Ordering::SeqCst);

    assert_eq!(
        ALLOC_COUNT.load(Ordering::SeqCst),
        0,
        "audio callback path must not allocate (§5.3)"
    );
    assert_eq!(
        DEALLOC_COUNT.load(Ordering::SeqCst),
        0,
        "audio callback path must not deallocate — Arc<SoundData> release must happen \
         on the game thread via the reclaim queue, not inside the callback"
    );

    // このシナリオが実際にイベント経路(`Mixer::report_underrun`/クリッパ動作検知)を
    // 通ったことを確認する(=上のゼロアロケーション判定が「何も起きなかったから
    // たまたま0だった」のではないことの裏取り)。
    assert!(
        saw_underrun,
        "the starved ring buffer must have produced at least one aggregated Underrun event"
    );
    if cfg!(debug_assertions) {
        assert!(
            saw_clipper_engaged,
            "the oversubscribed voice scenario must have engaged the clipper"
        );
    }

    // 後片付け(トラッキング外)。
    reclaim.drain();
}
