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

use mw_core::{CHANNELS, Command, Config, Renderer, SoundData};

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

/// 音声コールバック相当経路(`Renderer::render` およびそこから呼ばれる全コード)は
/// ヒープアロケーション・デアロケーションを一切行わない(初期構築仕様 §5.3)。
///
/// ボイス満杯 → スティール、停止、バスフェード、クリッパ動作を全部含む
/// リアルなシナリオを事前に仕込んでおき、その後の `render` 呼び出しだけを計測する。
#[test]
fn render_path_never_allocates_or_deallocates() {
    let config = Config::default();
    let max_voices = config.max_voices;
    let (mut renderer, sender, mut reclaim) = Renderer::build(config, 48_000);

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

    let mut buffer = vec![0.0f32; 512 * CHANNELS];

    TRACKING.store(true, Ordering::SeqCst);
    for _ in 0..200 {
        renderer.render(&mut buffer);
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

    // 後片付け(トラッキング外)。
    reclaim.drain();
}
