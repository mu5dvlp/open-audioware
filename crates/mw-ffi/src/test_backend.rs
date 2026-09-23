//! 実デバイスなしで内部再オープンの段2・段3を通すためのテスト用バックエンド。
//!
//! `FakeBackend` は `Backend::open` が成功したことだけを記録し、`Renderer` と
//! `EventQueue` を保持する。これにより、本物の `CpalBackend` と同じく、open 後も
//! コマンドキューの受信側が生きている状態で再オープンのスレッド経路を検証できる。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use mw_backend::{Backend, BackendError, host_time_ns};
use mw_core::{CHANNELS, EventQueue, Renderer};

const VIRTUAL_SAMPLE_RATE: u32 = 48_000;

/// 仮想デバイスの I/O バッファ長(フレーム)。**実デバイスと同じく固定長にしてある。**
///
/// 🔴 「経過時間ぶんのフレーム数をそのまま render する」形にしないこと —— 起動直後は
/// 経過が 0.1ms しか無いので 5 フレーム等の極端に短いバッファで呼ぶことになり、
/// `Instance::log_buffer_info_once` が `5 frames @ 48000 Hz = 0.104 ms` のような
/// **実機では起こりえない値を「実測の I/O バッファ長」として**ログへ出す。
/// 診断ログの値は信用できる状態に保つ(このリポジトリはこのログで iOS の申告値を
/// 裏取りしている。`crates/mw-backend/src/backend.rs` の `last_callback_frames` doc)。
///
/// 480 フレーム = 10ms @48kHz。実機の実測値(macOS 512 / Android 192〜886。
/// `docs/measurement-m1.md`)と同じ桁に収まる。
const VIRTUAL_FRAMES_PER_BUFFER: u64 = 480;

/// 1回のウェイクで連続して出してよいバッファ数の上限。
///
/// スレッドが飢えて実時間から遅れたぶんは**まとめて取り戻す**(実デバイスも OS 側が
/// 詰まればバッファを続けて要求する)。ただし1回のウェイクを有界にしておかないと、
/// 長く止まった後に際限なく回り続けることになる。
const MAX_BUFFERS_PER_WAKE: u64 = 10;

const VIRTUAL_OUTPUT_LATENCY_NS: u64 = 10_000_000;

/// グローバルレジストリ(`crate::handle` の `registry()`)と
/// `REOPEN_IN_PROGRESS` を触るテストを直列化する。
///
/// 🔴 `cargo test` は既定で同一プロセス内の複数テストを並行実行するため、
/// これが無いと (1) テストダブルのファクトリが、実デバイス越しの統合テスト
/// (`crate::ffi::tests::init_se_lifecycle_...`)の `mw_init` に紛れ込んで
/// そのテストの意味を壊す、(2) 進行中フラグを見るテストが互いに干渉する。
static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn registry_lock() -> MutexGuard<'static, ()> {
    REGISTRY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `cond` が true になるまで短い sleep を挟んで待つ。タイムアウトしたら false。
pub(crate) fn wait_until(cond: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

/// `open()` を解放するまで待たせるための最小の門。
pub(crate) struct Gate {
    released: Mutex<bool>,
    condvar: Condvar,
}

impl Gate {
    pub(crate) fn closed() -> Arc<Self> {
        Arc::new(Self {
            released: Mutex::new(false),
            condvar: Condvar::new(),
        })
    }

    pub(crate) fn release(&self) {
        let mut released = self
            .released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *released = true;
        self.condvar.notify_all();
    }

    fn wait(&self) {
        let mut released = self
            .released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*released {
            released = self
                .condvar
                .wait(released)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// テスト用の「開けたことにするだけ」のバックエンド。
pub(crate) struct FakeBackend {
    shared: Arc<FakeBackendShared>,
}

/// `FakeBackend` の観測点。テスト側は `Arc` でこれを握り、
/// 呼ばれ方(open/close の回数・開いているか)を後から確認する。
pub(crate) struct FakeBackendShared {
    open_calls: AtomicU32,
    close_calls: AtomicU32,
    is_open: AtomicBool,
    sample_rate_when_open: u32,
    open_should_fail: AtomicBool,
    open_gate: Mutex<Option<Arc<Gate>>>,
    entered_open: Arc<(Mutex<bool>, Condvar)>,
    renderer: Mutex<Option<Renderer>>,
    events: Mutex<Option<Arc<EventQueue>>>,
}

impl FakeBackendShared {
    pub(crate) fn new(sample_rate_when_open: u32) -> Arc<Self> {
        Arc::new(Self {
            open_calls: AtomicU32::new(0),
            close_calls: AtomicU32::new(0),
            is_open: AtomicBool::new(false),
            sample_rate_when_open,
            open_should_fail: AtomicBool::new(false),
            open_gate: Mutex::new(None),
            entered_open: Arc::new((Mutex::new(false), Condvar::new())),
            renderer: Mutex::new(None),
            events: Mutex::new(None),
        })
    }

    pub(crate) fn set_open_should_fail(&self, should_fail: bool) {
        self.open_should_fail.store(should_fail, Ordering::Relaxed);
    }

    pub(crate) fn set_open_gate(&self, gate: Option<Arc<Gate>>) {
        *self
            .open_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = gate;
    }

    pub(crate) fn open_calls(&self) -> u32 {
        self.open_calls.load(Ordering::Relaxed)
    }

    pub(crate) fn close_calls(&self) -> u32 {
        self.close_calls.load(Ordering::Relaxed)
    }

    pub(crate) fn is_open(&self) -> bool {
        self.is_open.load(Ordering::Relaxed)
    }

    pub(crate) fn wait_entered_open(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (entered, condvar) = &*self.entered_open;
        let mut entered = entered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*entered {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, result) = condvar
                .wait_timeout(entered, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            entered = next;
            if result.timed_out() && !*entered {
                return false;
            }
        }
        true
    }
}

impl FakeBackend {
    pub(crate) fn from_shared(shared: Arc<FakeBackendShared>) -> Self {
        Self { shared }
    }
}

impl Backend for FakeBackend {
    fn open(&mut self, renderer: Renderer, events: Arc<EventQueue>) -> Result<(), BackendError> {
        self.shared.open_calls.fetch_add(1, Ordering::Relaxed);
        *self
            .shared
            .renderer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(renderer);
        *self
            .shared
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(events);

        let (entered, condvar) = &*self.shared.entered_open;
        *entered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        condvar.notify_all();

        let gate = self
            .shared
            .open_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(gate) = gate {
            gate.wait();
        }

        if self.shared.open_should_fail.load(Ordering::Relaxed) {
            return Err(BackendError::NoOutputDevice);
        }
        if self.shared.is_open.swap(true, Ordering::Relaxed) {
            return Err(BackendError::AlreadyOpen);
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), BackendError> {
        if !self.shared.is_open.swap(false, Ordering::Relaxed) {
            return Err(BackendError::NotOpen);
        }
        self.shared.close_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.shared.is_open()
    }

    fn last_callback_frames(&self) -> u32 {
        0
    }

    fn sample_rate(&self) -> u32 {
        if self.is_open() {
            self.shared.sample_rate_when_open
        } else {
            0
        }
    }

    fn output_latency_ns(&self) -> u64 {
        0
    }

    fn log_output_latency_once(&self) {}

    fn log_new_output_underruns(&self) {}

    fn output_underrun_count(&self) -> u64 {
        0
    }

    fn last_output_underrun_host_time_ns(&self) -> u64 {
        0
    }

    fn consecutive_output_underrun_count(&self) -> u32 {
        0
    }
}

/// 実デバイスの代わりに「音声スレッド相当」を自前で回すテスト用バックエンド。
///
/// `open()` でレンダリング用のスレッドを1本立て、`Renderer::render` を実ホスト時刻つき
/// で、ほぼ実時間の歩調で周期的に呼ぶ。`close()` と `Drop` はどちらからでも安全に
/// スレッドを停止して join する。
pub(crate) struct VirtualBackend {
    shared: Arc<VirtualBackendShared>,
    stop: Option<Arc<AtomicBool>>,
    render_thread: Option<thread::JoinHandle<()>>,
    is_open: bool,
    sample_rate: u32,
}

/// `VirtualBackend` の観測点。払い出された仮想デバイスはファクトリがすべて記録し、
/// テスト側はこの共有値を通じて open/close・render・ログ呼び出しを観測できる。
pub(crate) struct VirtualBackendShared {
    open_calls: AtomicU32,
    close_calls: AtomicU32,
    is_open: AtomicBool,
    last_callback_frames: AtomicU32,
    log_output_latency_calls: AtomicU32,
    log_new_output_underruns_calls: AtomicU32,
}

impl VirtualBackendShared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            open_calls: AtomicU32::new(0),
            close_calls: AtomicU32::new(0),
            is_open: AtomicBool::new(false),
            last_callback_frames: AtomicU32::new(0),
            log_output_latency_calls: AtomicU32::new(0),
            log_new_output_underruns_calls: AtomicU32::new(0),
        })
    }

    pub(crate) fn open_calls(&self) -> u32 {
        self.open_calls.load(Ordering::Relaxed)
    }

    pub(crate) fn close_calls(&self) -> u32 {
        self.close_calls.load(Ordering::Relaxed)
    }

    pub(crate) fn is_open(&self) -> bool {
        self.is_open.load(Ordering::Relaxed)
    }

    pub(crate) fn last_callback_frames(&self) -> u32 {
        self.last_callback_frames.load(Ordering::Relaxed)
    }

    pub(crate) fn log_output_latency_calls(&self) -> u32 {
        self.log_output_latency_calls.load(Ordering::Relaxed)
    }

    pub(crate) fn log_new_output_underruns_calls(&self) -> u32 {
        self.log_new_output_underruns_calls.load(Ordering::Relaxed)
    }
}

impl VirtualBackend {
    pub(crate) fn from_shared(shared: Arc<VirtualBackendShared>) -> Self {
        Self {
            shared,
            stop: None,
            render_thread: None,
            is_open: false,
            sample_rate: 0,
        }
    }

    /// 保持している音声スレッドを停止して join する。`close()` と `Drop` の共通経路。
    fn stop_and_join(&mut self) {
        self.is_open = false;
        self.sample_rate = 0;
        self.shared.is_open.store(false, Ordering::Relaxed);
        self.shared.last_callback_frames.store(0, Ordering::Relaxed);

        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(render_thread) = self.render_thread.take() {
            let _ = render_thread.join();
        }
    }
}

impl Backend for VirtualBackend {
    fn open(
        &mut self,
        mut renderer: Renderer,
        _events: Arc<EventQueue>,
    ) -> Result<(), BackendError> {
        if self.is_open {
            return Err(BackendError::AlreadyOpen);
        }

        // 実物の CpalBackend と同じく、ストリーム相当のスレッドを動かす前に
        // Renderer 側のサンプルレートを確定させる。
        renderer.set_sample_rate(VIRTUAL_SAMPLE_RATE);

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let shared = Arc::clone(&self.shared);
        let stream_start_ns = host_time_ns();
        let render_thread = thread::Builder::new()
            .name("mw-virtual-audio".to_owned())
            .spawn(move || {
                let mut renderer = renderer;
                // 出力バッファはスレッドごとに1本だけ確保して、各 render で使い回す
                // (実デバイスと同じ固定長。`mixer::render` は毎回バッファを書き潰す)。
                let mut output = vec![0.0_f32; VIRTUAL_FRAMES_PER_BUFFER as usize * CHANNELS];
                let mut frames_rendered_so_far = 0_u64;

                while !thread_stop.load(Ordering::Relaxed) {
                    // 実ホスト時刻へ追従させる: 経過時間ぶんに満たない端数は出さず、
                    // **満たしたバッファの数だけ**続けて出す(固定長を崩さないため)。
                    // 固定 sleep で固定フレーム数を出す形にすると、CI の runner が
                    // 混んだときにフレームクロックが実時間から遅れていき、
                    // 「位置が進むのを待つ」系のテストがタイムアウトする。
                    let now_ns = host_time_ns();
                    let elapsed_ns = now_ns.saturating_sub(stream_start_ns);
                    let elapsed_frames = ((elapsed_ns as u128 * VIRTUAL_SAMPLE_RATE as u128)
                        / 1_000_000_000)
                        .min(u64::MAX as u128) as u64;
                    let behind = elapsed_frames.saturating_sub(frames_rendered_so_far);
                    let buffers = (behind / VIRTUAL_FRAMES_PER_BUFFER).min(MAX_BUFFERS_PER_WAKE);

                    if buffers == 0 {
                        // まだ1バッファぶん溜まっていない。次のウェイクまで待つ。
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    for _ in 0..buffers {
                        renderer.render(&mut output, host_time_ns());
                        frames_rendered_so_far =
                            frames_rendered_so_far.saturating_add(VIRTUAL_FRAMES_PER_BUFFER);
                        shared
                            .last_callback_frames
                            .store(VIRTUAL_FRAMES_PER_BUFFER as u32, Ordering::Relaxed);
                    }
                }
            })
            .map_err(|error| BackendError::BuildStreamFailed(error.to_string()))?;

        self.stop = Some(stop);
        self.render_thread = Some(render_thread);
        self.is_open = true;
        self.sample_rate = VIRTUAL_SAMPLE_RATE;
        self.shared.open_calls.fetch_add(1, Ordering::Relaxed);
        self.shared.is_open.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn close(&mut self) -> Result<(), BackendError> {
        if !self.is_open {
            return Err(BackendError::NotOpen);
        }
        self.shared.close_calls.fetch_add(1, Ordering::Relaxed);
        self.stop_and_join();
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.is_open
    }

    fn last_callback_frames(&self) -> u32 {
        self.shared.last_callback_frames()
    }

    fn sample_rate(&self) -> u32 {
        if self.is_open { self.sample_rate } else { 0 }
    }

    fn output_latency_ns(&self) -> u64 {
        if self.is_open {
            VIRTUAL_OUTPUT_LATENCY_NS
        } else {
            0
        }
    }

    fn log_output_latency_once(&self) {
        self.shared
            .log_output_latency_calls
            .fetch_add(1, Ordering::Relaxed);
    }

    fn log_new_output_underruns(&self) {
        self.shared
            .log_new_output_underruns_calls
            .fetch_add(1, Ordering::Relaxed);
    }

    fn output_underrun_count(&self) -> u64 {
        0
    }

    fn last_output_underrun_host_time_ns(&self) -> u64 {
        0
    }

    fn consecutive_output_underrun_count(&self) -> u32 {
        0
    }
}

impl Drop for VirtualBackend {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// 次に `crate::handle::make_backend()` が呼ばれたときに払い出す fake を、
/// **先頭から順に**予約する。使い切ったあとは「素直に開く既定の fake」を新規に作って返す。
/// 戻り値のガードが drop されるとファクトリは外れる(本番経路へ戻る)。
pub(crate) fn install_scripted_factory(scripted: Vec<Arc<FakeBackendShared>>) -> FactoryGuard {
    let mut factory = FACTORY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *factory = Some(FactoryMode::Scripted(scripted.into_iter().collect()));
    FactoryGuard
}

/// `make_backend()` が呼ばれるたびに新しい仮想デバイスを払い出すファクトリを差し込む。
/// 払い出した全デバイスは `devices()` から観測できる。
pub(crate) fn install_virtual_device_factory() -> VirtualDeviceFactoryGuard {
    let state = Arc::new(VirtualDeviceFactoryState {
        devices: Mutex::new(Vec::new()),
    });
    let mut factory = FACTORY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *factory = Some(FactoryMode::Virtual(Arc::clone(&state)));
    VirtualDeviceFactoryGuard { state }
}

struct VirtualDeviceFactoryState {
    devices: Mutex<Vec<Arc<VirtualBackendShared>>>,
}

pub(crate) struct VirtualDeviceFactoryGuard {
    state: Arc<VirtualDeviceFactoryState>,
}

impl VirtualDeviceFactoryGuard {
    pub(crate) fn devices(&self) -> Vec<Arc<VirtualBackendShared>> {
        self.state
            .devices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

pub(crate) fn take_from_factory() -> Option<Box<dyn Backend + Send>> {
    let mut factory = FACTORY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match factory.as_mut()? {
        FactoryMode::Scripted(scripted) => {
            let shared = scripted
                .pop_front()
                .unwrap_or_else(|| FakeBackendShared::new(48_000));
            Some(Box::new(FakeBackend::from_shared(shared)))
        }
        FactoryMode::Virtual(state) => {
            let shared = VirtualBackendShared::new();
            state
                .devices
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(Arc::clone(&shared));
            Some(Box::new(VirtualBackend::from_shared(shared)))
        }
    }
}

enum FactoryMode {
    Scripted(VecDeque<Arc<FakeBackendShared>>),
    Virtual(Arc<VirtualDeviceFactoryState>),
}

static FACTORY: Mutex<Option<FactoryMode>> = Mutex::new(None);

pub(crate) struct FactoryGuard;

impl Drop for FactoryGuard {
    fn drop(&mut self) {
        *FACTORY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

impl Drop for VirtualDeviceFactoryGuard {
    fn drop(&mut self) {
        *FACTORY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}
