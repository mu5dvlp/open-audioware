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

use mw_backend::{Backend, BackendError};
use mw_core::{EventQueue, Renderer};

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

/// 次に `crate::handle::make_backend()` が呼ばれたときに払い出す fake を、
/// **先頭から順に**予約する。使い切ったあとは「素直に開く既定の fake」を新規に作って返す。
/// 戻り値のガードが drop されるとファクトリは外れる(本番経路へ戻る)。
pub(crate) fn install_scripted_factory(scripted: Vec<Arc<FakeBackendShared>>) -> FactoryGuard {
    let mut factory = FACTORY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *factory = Some(scripted.into_iter().collect());
    FactoryGuard
}

pub(crate) fn take_from_factory() -> Option<Box<dyn Backend + Send>> {
    let mut factory = FACTORY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let scripted = factory.as_mut()?;
    let shared = scripted
        .pop_front()
        .unwrap_or_else(|| FakeBackendShared::new(48_000));
    Some(Box::new(FakeBackend::from_shared(shared)))
}

static FACTORY: Mutex<Option<VecDeque<Arc<FakeBackendShared>>>> = Mutex::new(None);

pub(crate) struct FactoryGuard;

impl Drop for FactoryGuard {
    fn drop(&mut self) {
        *FACTORY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}
