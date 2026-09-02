//! リアルタイム安全性の検証(初期構築仕様 §5.3, §8)。
//!
//! 「コールバック相当経路のアロケーションゼロをテストで固定化」を、カウンティング
//! アロケータフックで実施する。`#[global_allocator]` はプロセス全体(=このテスト
//! バイナリ全体)に適用されるため、専用の統合テストファイルとして分離してある
//! (`mw-core` の他ユニットテストと同じバイナリに混ぜない)。
//!
//! 手法: **このスレッドの thread-local な `TRACKING` フラグ**が立っている間だけ
//! アロケーション/デアロケーション回数を数える。`Renderer::render`(= 音声コールバック
//! 相当の経路)呼び出しの前後だけ [`RenderTrackingGuard`] でフラグを立てることで、
//! セットアップ側(ボイスコマンドの用意、バッファ確保等)のアロケーションを除外し、
//! 音声スレッド経路そのものだけを検証する。
//!
//! # なぜ thread-local か(CI フレークの真因、2026-08-29 発生・実測で確定)
//!
//! 当初 `TRACKING` は **プロセス全体で共有される `AtomicBool`** だった。これだと
//! 計測ウィンドウ中に**別スレッドが行った確保も無差別に数えてしまう**——CI
//! (ubuntu-24.04 ランナー)でだけ不定期に `left: 4 right: 0` で落ちるフレークの
//! 原因はこれだった。ローカル(macOS / docker `rust:1.98` / docker `ubuntu:24.04` +
//! rustup 1.98.0)で一度も再現しなかったのは、CI ランナーの方がスレッド生成・
//! スケジューリングの背景ノイズ(テストランナー自体のスレッドプール等)が多く、
//! 計測ウィンドウとたまたま重なる確率が高かったためと考えられる。
//!
//! もう一つの仮説(遅延初期化された `Mutex` 等の未ウォーム経路が BGM 側に増えた)は、
//! 調査の結果**採らなかった**——音声コールバック経路(`Mixer::render` 以下)には
//! `event.rs::EventQueue::push_side_channel` 用の `Mutex` 以外の遅延初期化状態が無く、
//! かつこの `Mutex` は `push_realtime`(音声スレッド専用経路)からは触れられない
//! (`event.rs` モジュール doc 参照)。しかも BGM 側の初回 `render` 呼び出しは
//! このテストで元から計測対象外(`renderer.render(&mut warmup, 0)` によるウォームアップ、
//! 下記)になっており、M4-3 の時点で既にこの対策が入っていたにもかかわらず CI で
//! 発生したことも、原因が BGM 固有の未ウォーム経路ではないことの傍証になる。
//!
//! 真因を測定で確定させるため、[`render_tracking_is_isolated_from_concurrent_background_allocation`]
//! で「計測ウィンドウ中、バックグラウンドスレッドが確保し続けていても
//! カウントは0のまま」であることを回帰テストとして固定化した。このテストは
//! **旧方式(プロセス全体で共有する `AtomicBool`)に戻すと実際に落ちる**ことを
//! 手元で確認済み(`docs/history.md` 参照)。
//!
//! thread-local 化により、計測用のグローバル状態(`ALLOC_COUNT`/`DEALLOC_COUNT`)は
//! 依然としてプロセス全体で共有されるが、**加算するかどうかの判定が呼び出しスレッド
//! ごとに独立する**ため、複数の `#[test]` 関数が並行して自分自身の計測ウィンドウを
//! 持てるようになった(各テストは計測開始直前・直後のカウンタ値の差分〔delta〕を見る
//! ことで、他のテストが同時に計測していても正しく動く——お互いのウィンドウで
//! 起きているのは実際には「0 加算」なので、足し合わせても 0 のままのため)。

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use mw_core::{
    CHANNELS, Command, Config, DecodeError, Event, MusicDecoder, MusicState, Renderer, ScheduledSe,
    SoundData,
};

struct CountingAllocator;

thread_local! {
    /// このスレッドが現在「レンダー経路の計測ウィンドウ」の中にいるかどうか。
    /// プロセス全体で共有する `AtomicBool` だった旧方式が CI フレークの真因だった
    /// ため(上のモジュール doc 参照)、thread-local にしてある——他スレッドの
    /// 確保・解放を一切拾わない。
    static TRACKING: Cell<bool> = const { Cell::new(false) };
}
static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static DEALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

/// [`TRACKING`] を RAII で立て下げするガード。`new()` で自スレッドのフラグを立て、
/// `Drop` で必ず下ろす(途中の `assert!` 失敗等でパニックしてもフラグが立ちっぱなしに
/// ならない——`Drop` はパニック解放(unwind)の途中でも走る)。
struct RenderTrackingGuard;

impl RenderTrackingGuard {
    fn new() -> Self {
        TRACKING.with(|t| t.set(true));
        RenderTrackingGuard
    }
}

impl Drop for RenderTrackingGuard {
    fn drop(&mut self) {
        TRACKING.with(|t| t.set(false));
    }
}

// SAFETY: すべての呼び出しをそのまま `System` へ委譲するだけの薄いラッパ。
// 追加の状態はアトミックカウンタと thread-local フラグのみで、`GlobalAlloc` の
// 安全性契約に影響しない。
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACKING.with(Cell::get) {
            ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if TRACKING.with(Cell::get) {
            DEALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if TRACKING.with(Cell::get) {
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
    let (mut renderer, sender, mut reclaim, mut music_producer, _music_clock, events, mut bgm) =
        Renderer::build(config, 48_000);

    let mut decoder = UnboundedMusicDecoder { cursor: 0 };
    music_producer
        .pump(&mut decoder)
        .expect("pump must succeed");
    let mut warmup = vec![0.0f32; 4 * CHANNELS];
    renderer.render(&mut warmup, 0); // Loading -> Ready(セットアップ、計測対象外)。
    assert_eq!(renderer.music_state(), MusicState::Ready);
    assert!(sender.send(Command::MusicPlayScheduled { host_time_ns: 0 }));

    // M4-3: BGM ボイス(初期構築仕様『§2』M14)もこのシナリオへ含める。
    // `music_producer` と全く同じ理由・同じ手法で「デコードスレッドが停止した」
    // 状況(継続的なアンダーラン)を再現し、BGM 側のチャンクレンダリング経路
    // (`mixer.rs::Mixer::render` の `BGM_CHUNK_FRAMES` 固定長スタック配列)も
    // ゼロアロケーションであることを実際の render 呼び出しの中で検証する。
    let mut bgm_decoder = UnboundedMusicDecoder { cursor: 0 };
    bgm.stream_producer
        .pump(&mut bgm_decoder)
        .expect("pump must succeed");
    renderer.render(&mut warmup, 0); // Loading -> Ready(セットアップ、計測対象外)。
    assert_eq!(renderer.bgm_state(), MusicState::Ready);
    assert!(sender.send(Command::BgmPlay));
    assert!(sender.send(Command::BgmSetLoop {
        region: Some((0, 4_000)),
    }));

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
    // SE ボイスのループ再生(ホールド保持音向け)。折り返し(`voice::Voice::next_frame`
    // の位置巻き戻し)がこの200コールバックぶんのレンダリング中に何度も発生する
    // 短いループ区間にして、ゼロアロケーション経路の対象へ含める。
    assert!(sender.send(Command::SetVoiceLoop {
        voice_serial: 6,
        region: Some((10, 50)),
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

    // delta(計測開始直前/直後の差分)で判定する: `ALLOC_COUNT`/`DEALLOC_COUNT` は
    // プロセス全体で共有するグローバルカウンタなので、cargo test がこのファイル内の
    // 他の `#[test]`(下記の回帰テスト)を並行実行していても、絶対値ではなく差分を
    // 見ることで正しく動く(お互いの計測ウィンドウは「自スレッドの加算だけ」を
    // 見ており、期待値は両方とも0なので足し合わせても0のまま)。
    let alloc_before = ALLOC_COUNT.load(Ordering::SeqCst);
    let dealloc_before = DEALLOC_COUNT.load(Ordering::SeqCst);
    {
        let _tracking = RenderTrackingGuard::new();
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
    }

    assert_eq!(
        ALLOC_COUNT.load(Ordering::SeqCst) - alloc_before,
        0,
        "audio callback path must not allocate (§5.3)"
    );
    assert_eq!(
        DEALLOC_COUNT.load(Ordering::SeqCst) - dealloc_before,
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

/// 回帰テスト: CI(ubuntu-24.04)で不定期に落ちていたフレークの真因を固定化する。
///
/// 計測ウィンドウ(=このスレッドの [`RenderTrackingGuard`] が生きている区間)の間、
/// **別スレッドが確保・解放をひたすら回し続けていても**、レンダー経路の計測は
/// 0 のままであることを確認する。
///
/// **旧方式(`TRACKING` がプロセス全体で共有する `AtomicBool` だった実装)へ
/// 一時的に戻すと、このテストは実測で毎回落ちる**
/// (`ALLOC_COUNT`/`DEALLOC_COUNT` が背景スレッドの確保分だけ非0になる)。
/// これは実際の CI フレーク(`render_path_never_allocates_or_deallocates` が
/// 「left: 4 right: 0」のような小さな非ゼロ値で落ちる)と同じ形の失敗であり、
/// 「プロセス全体で共有するカウンタが他スレッドの確保を拾ってしまう」ことが
/// 真因だったことの実測による裏取りになっている(詳細は `docs/history.md`)。
#[test]
fn render_tracking_is_isolated_from_concurrent_background_allocation() {
    let config = Config {
        preroll_ms: 1.0,
        ..Config::default()
    };
    let (mut renderer, sender, mut reclaim, mut music_producer, _music_clock, _events, _bgm) =
        Renderer::build(config, 48_000);

    let mut decoder = UnboundedMusicDecoder { cursor: 0 };
    music_producer
        .pump(&mut decoder)
        .expect("pump must succeed");
    let mut warmup = vec![0.0f32; 4 * CHANNELS];
    renderer.render(&mut warmup, 0); // Loading -> Ready(セットアップ、計測対象外)。
    assert_eq!(renderer.music_state(), MusicState::Ready);
    assert!(sender.send(Command::MusicPlayScheduled { host_time_ns: 0 }));

    // バックグラウンドスレッド: このテストのレンダー計測とは無関係の、確保し続けるだけの
    // スレッド。旧方式(プロセス全体で共有する `AtomicBool`)ならこのスレッドの確保も
    // カウントへ混入してしまう。
    let stop = Arc::new(AtomicBool::new(false));
    let bg_iterations = Arc::new(AtomicUsize::new(0));
    let bg_stop = Arc::clone(&stop);
    let bg_counter = Arc::clone(&bg_iterations);
    let handle = thread::spawn(move || {
        while !bg_stop.load(Ordering::Relaxed) {
            let v: Vec<u8> = Vec::with_capacity(64);
            std::hint::black_box(&v);
            drop(v);
            bg_counter.fetch_add(1, Ordering::Relaxed);
        }
    });

    // sleep には頼らない: バックグラウンドスレッドが実際に確保を回し始めた
    // (=一定回数ループした)ことを、観測されたイテレーション数そのもので確認してから
    // 計測ウィンドウへ入る。
    while bg_iterations.load(Ordering::Relaxed) < 10_000 {
        std::hint::spin_loop();
    }

    // `buffer` は計測ウィンドウの**外**で確保する(このテスト自身のセットアップであり、
    // 計測対象の「レンダー経路」そのものではない)。guard の内側に置くと `Vec` 自身の
    // 確保/解放を計測してしまい「レンダー経路がアロケーションした」という誤検出になる
    // (作成時に実際にこの取り違えで `left: 1` の誤検出を踏んだため、明示的に警告として残す)。
    let mut buffer = vec![0.0f32; 512 * CHANNELS];
    let buffer_duration_ns = 512u64 * 1_000_000_000 / 48_000;

    let alloc_before = ALLOC_COUNT.load(Ordering::SeqCst);
    let dealloc_before = DEALLOC_COUNT.load(Ordering::SeqCst);
    let bg_iterations_before = bg_iterations.load(Ordering::Relaxed);
    {
        let _tracking = RenderTrackingGuard::new();
        let mut host_time_ns = 0u64;
        for _ in 0..200 {
            renderer.render(&mut buffer, host_time_ns);
            host_time_ns += buffer_duration_ns;
        }
    }
    let alloc_after = ALLOC_COUNT.load(Ordering::SeqCst);
    let dealloc_after = DEALLOC_COUNT.load(Ordering::SeqCst);
    let bg_iterations_after = bg_iterations.load(Ordering::Relaxed);

    stop.store(true, Ordering::Relaxed);
    handle
        .join()
        .expect("background allocator thread must not panic");

    // このテスト自体が無意味(背景スレッドがたまたま計測ウィンドウ中に止まっていた)
    // ではないことを確認する: ウィンドウの間も背景スレッドは確保を回し続けていたはず。
    assert!(
        bg_iterations_after > bg_iterations_before,
        "background thread must have kept allocating throughout the measured window"
    );
    assert_eq!(
        alloc_after - alloc_before,
        0,
        "render path allocation tracking must stay isolated to this thread and must not \
         pick up a concurrently running background thread's allocations"
    );
    assert_eq!(
        dealloc_after - dealloc_before,
        0,
        "render path deallocation tracking must stay isolated to this thread and must not \
         pick up a concurrently running background thread's deallocations"
    );

    reclaim.drain();
}
