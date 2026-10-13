//! ホスト単調時刻(初期構築仕様『§4.4 音楽クロック』, M2-5)。
//!
//! `mw-ffi` がここへ薄く委譲して `mw_host_time_ns()` として公開する。
//! C# 側は起動時にこの値と `Time.realtimeSinceStartupAsDouble` を1回ずつサンプリングし、
//! その差を定数オフセットとして保持することで両者を橋渡しする設計
//! (初期構築仕様『§4.4』)なので、**ここは素の単調時刻を返すだけでよい**。
//!
//! ただし唯一の要件は「デバイスの出力タイムスタンプ(`cpal::OutputCallbackInfo` の
//! `StreamInstant`)と直接比較できる時計であること」。
//!
//! # 調査結果: `cpal::StreamInstant` はどの時計か(cpal 0.18.2)
//!
//! `cpal::StreamInstant` のドキュメント(`timestamp.rs`)にホストごとの時刻源が
//! 明記されており、実装(`src/host/*`)でも確認した:
//!
//! | ホスト(対象プラットフォーム) | `StreamInstant` の実体 |
//! |---|---|
//! | CoreAudio(macOS / iOS / tvOS) | `mach_absolute_time()` を `mach_timebase_info` で ns 換算(`src/host/coreaudio/mod.rs::host_time_to_stream_instant`) |
//! | AAudio(Android) | `clock_gettime(CLOCK_MONOTONIC)`(`src/host/aaudio/convert.rs::now_stream_instant`) |
//!
//! (WASAPI/ALSA 等の他ホストは対象外——本プロジェクトの配布ターゲットは macOS Editor /
//! iOS / Android のみ、`README.md`。`cargo test --workspace` は CI で Linux 上でも
//! 走る〔`.github/workflows/ci.yml`〕ため Linux 向けの実装も用意するが、これは
//! ビルド・オフラインテスト専用であり実デバイスとの相関には使わない。Windows も同様に
//! `cargo check`(ビルド確認のみ)の対象として `QueryPerformanceCounter` /
//! `QueryPerformanceFrequency` による実装を用意する。自前の WASAPI バックエンド
//! 〔`native_backend::windows`〕は、この時計を「いま」として出力時刻を求める)。
//!
//! そのため [`host_time_ns`] は、**両ホストの式をそれぞれのプラットフォームで
//! 文字通り再現する**。「たまたま近い値になる」のではなく「同じ時計を同じ式で読む」
//! ことを保証することで、原理的な単位・原点・レートのずれを排除している。
//!
//! # C# 側は何を呼べば揃うか
//!
//! この関数(`mw_host_time_ns` として FFI 公開)が返す値と直接比較可能な既存の C# 時計は
//! 存在しない(`Time.realtimeSinceStartupAsDouble` は原点もレートも異なる別の時計)。
//! 初期構築仕様『§4.4』の設計どおり、**C# 側は起動時に `mw_host_time_ns()` と
//! `Time.realtimeSinceStartupAsDouble` の両方を1回ずつサンプリングし、その差を
//! 定数オフセットとして保持する**——以後はそのオフセットを介して変換する。

/// Mach の `mach_timebase_info`(C ABI 互換の自前宣言、ステップ5-2)。
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
#[repr(C)]
#[derive(Default)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

/// `<mach/kern_return.h>` の `KERN_SUCCESS`。
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
const KERN_SUCCESS: i32 = 0;

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
unsafe extern "C" {
    /// `std` が既にリンク済みの libSystem(Mach カーネル API)が提供するシンボルを
    /// 直接呼ぶ。**直接依存の `mach2` クレートだけを外す**ための自前宣言
    /// (ステップ5-2)——`std` 自体のリンクは変わらないので、新たな動的リンクは増えない。
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

/// `mach_timebase_info` の numer/denom を使って raw tick 数を ns へ変換する純関数部分
/// (`denom == 0` は呼び出し側〔`mach_ticks_to_ns`〕が失敗として raw tick を返す規約で、
/// ここでは防御的に同じ扱いにしておく)。
///
/// FFI(`mach_timebase_info` の実際のシステムコール)から分離してあるので、
/// 実機・ハードウェア無しでも計算式だけを固定できる(下の `tests` 参照)。
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
fn ticks_to_ns_with_timebase(ticks: u64, numer: u32, denom: u32) -> u64 {
    if denom == 0 {
        return ticks;
    }
    // `u128` で計算した時点では桁あふれしないが、結果を `u64` へ戻す際に
    // `u64::MAX` を超えていることがある(例: numer > denom で ticks が極端に大きい)。
    // `as u64` はラップアラウンドするため、ここは必ず saturating にする。
    (ticks as u128 * numer as u128 / denom as u128).min(u64::MAX as u128) as u64
}

/// Mach の tick 値(`mach_absolute_time()` の戻り値、または `AudioTimeStamp::mHostTime`
/// のような同じ時計源の値)を ns へ変換する。
///
/// [`host_time_ns`] 自身もこれに委譲する。`native_backend::apple` のレンダーコールバックが
/// `AudioTimeStamp.mHostTime` を同じ式で ns 化するためにも使う(cpal 0.18.2 の
/// `host_time_to_stream_instant` と同じ式。モジュール doc 参照)——`OutputCallbackInfo` の
/// デバイスタイムスタンプと直接比較可能な ns 値を得るには、どちらも同じ変換式を通す必要がある。
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
pub(crate) fn mach_ticks_to_ns(ticks: u64) -> u64 {
    // SAFETY: スタック上のローカル変数への有効なポインタを渡すだけの単純な問い合わせ。
    // エラー時も未初期化メモリを読まない(`mach_timebase_info` が失敗した場合は
    // `ticks_to_ns_with_timebase` 側の `denom == 0` 相当のチェックで弾く)。
    unsafe {
        let mut info = MachTimebaseInfo::default();
        let status = mach_timebase_info(&mut info);
        if status != KERN_SUCCESS {
            // 失敗しても致命傷にはしない(§4.8 の思想と同じ)。実機の CoreAudio では
            // まず失敗しない経路(cpal 自身もこれを事実上想定していない)だが、
            // 保険として raw tick をそのまま返す(単調性だけは保たれる)。
            return ticks;
        }
        ticks_to_ns_with_timebase(ticks, info.numer, info.denom)
    }
}

/// ホスト単調時刻をナノ秒で返す。
///
/// リアルタイム安全: アロケーション・ロックを一切行わない(単純なシステムコール
/// またはその薄いラッパ1回のみ)。`mw_se_schedule` / `mw_music_play_scheduled` の
/// `host_time_ns` 引数、および `cpal::OutputCallbackInfo` から求めるデバイス
/// タイムスタンプは、いずれもこの関数と同じ時計を指す(モジュール doc 参照)。
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
pub fn host_time_ns() -> u64 {
    // cpal 0.18.2 `src/host/coreaudio/mod.rs::host_time_to_stream_instant` と
    // 完全に同じ式(mach_absolute_time の raw tick 数 × timebase の numer/denom → ns)。
    // 同じ式で読む以上、`OutputCallbackInfo::timestamp()` が返す `StreamInstant` と
    // 単位・原点・レートのいずれもずれようがない。
    // SAFETY: 引数を取らない単純なシステムコール。
    unsafe { mach_ticks_to_ns(mach_absolute_time()) }
}

/// POSIX `clock_gettime` が書き込む `timespec`(C ABI 互換の自前宣言、ステップ5-1)。
///
/// `tv_sec`/`tv_nsec` の実体は C の `long`(`time_t`/`suseconds_t` 相当)——bionic
/// (Android)は 32bit ターゲット(armv7/i686)でも `long` が 32bit のままであり、
/// time64 化していない glibc も同様。`i64` 固定だと ABI 上のフィールド幅が実際の
/// `long` とずれ、32bit ターゲットでは呼び出し自体が壊れた値を返す。`core::ffi::c_long`
/// を使えば、ターゲットごとの実際の `long` 幅(64bit ターゲットは 64bit、32bit
/// ターゲットは 32bit)にそのまま追従する。
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "windows"
)))]
#[repr(C)]
struct Timespec {
    tv_sec: core::ffi::c_long,
    tv_nsec: core::ffi::c_long,
}

/// `<time.h>` の `CLOCK_MONOTONIC`(Linux / Android どちらも値は 1)。
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "windows"
)))]
const CLOCK_MONOTONIC: i32 = 1;

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "windows"
)))]
unsafe extern "C" {
    /// `std` が既にリンク済みの libc(Linux は glibc、Android は bionic)が提供する
    /// シンボルを直接呼ぶ。**直接依存の `libc` クレートだけを外す**ための自前宣言
    /// (ステップ5-1)——`std` 自体のリンクは変わらないので、新たな動的リンクは増えない。
    fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32;
}

/// ホスト単調時刻をナノ秒で返す(Android / Linux)。
///
/// cpal 0.18.2 `src/host/aaudio/convert.rs::now_stream_instant` と同じ式
/// (`clock_gettime(CLOCK_MONOTONIC)`)。Android 実機ではこれが `OutputCallbackInfo` の
/// `StreamInstant` と同一時計になる(モジュール doc 参照)。Linux(CI 専用。
/// 実配布対象ではない)でも同じ syscall が使えるため、ここに含めて
/// `cargo test --workspace` を通す。
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "windows"
)))]
pub fn host_time_ns() -> u64 {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` はスタック上の有効な `Timespec`。`clock_gettime` はこれへ
    // 書き込むだけで、他の副作用は無い。
    unsafe {
        clock_gettime(CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// QPC(`QueryPerformanceCounter`)の raw カウンタ値を freq(Hz)を使って ns へ
/// 変換する純関数部分(`denom == 0` 相当として `freq == 0` は呼び出し側の失敗時に
/// raw カウンタをそのまま返す規約——[`ticks_to_ns_with_timebase`] と揃えてある)。
///
/// `counter * 1_000_000_000` を先に計算すると桁あふれしうるため、`counter / freq`
/// を先に割って得た秒数を ns 化し、余り(`counter % freq`、常に `freq` 未満)だけを
/// ns スケールへ掛ける——余りの項は原理的に `1_000_000_000` を超えない
/// (`remainder < freq` なので `remainder * 1_000_000_000 / freq < 1_000_000_000`)。
///
/// FFI(実際の `QueryPerformanceCounter`/`QueryPerformanceFrequency` 呼び出し)から
/// 分離してあるので、Windows ターゲット無しでも計算式だけを固定できる
/// (`cfg(test)` にしているため macOS 上の `cargo test` でも検証できる。下の `tests` 参照)。
#[cfg(any(target_os = "windows", test))]
fn qpc_ticks_to_ns_with_frequency(counter: u64, freq: u64) -> u64 {
    if freq == 0 {
        return counter;
    }
    let secs = counter / freq;
    let remainder = counter % freq;
    let ns_from_secs = secs.saturating_mul(1_000_000_000);
    let ns_from_remainder = (remainder as u128 * 1_000_000_000 / freq as u128) as u64;
    ns_from_secs.saturating_add(ns_from_remainder)
}

// `<profileapi.h>` の `QueryPerformanceCounter` / `QueryPerformanceFrequency`
// (C ABI 互換の自前宣言——新しいクレート(`windows-sys` 等)を足さないための方針、
// ステップ5 と同じ)。kernel32.dll は Windows の `std` が既にリンク済みのため、
// 新たな動的リンクは増えない。
#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(lpPerformanceCount: *mut i64) -> i32;
    fn QueryPerformanceFrequency(lpFrequency: *mut i64) -> i32;
}

/// QPC の周波数(Hz)のキャッシュ。0 は「まだ取得していない」を表す
/// (`QueryPerformanceFrequency` は Windows XP 以降失敗しない契約だが、仮に失敗しても
/// 0 のままなら [`qpc_ticks_to_ns_with_frequency`] の `freq == 0` 規約で raw カウンタに
/// フォールバックする)。
///
/// プロセス内で一度取得すれば変わらない値(Windows Vista 以降はブート中変化しないことが
/// 保証されている)なので1回だけ取ってキャッシュする。音声コールバックスレッド(RT)から
/// 呼ばれる想定のため `Mutex`/`OnceLock` ではなく `AtomicU64` + `Ordering::Relaxed` の
/// 読み書きのみにする(ロックを取らない——リアルタイム安全性規約、
/// `crates/mw-backend/COMMON.md` §5.3 / `underrun.rs` モジュール doc と同じ方針)。
/// 複数スレッドが同時に初回呼び出しをしても同じ値を書き込むだけなので競合しても安全。
#[cfg(target_os = "windows")]
static QPC_FREQUENCY_HZ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(target_os = "windows")]
fn qpc_frequency_hz() -> u64 {
    use std::sync::atomic::Ordering;
    let cached = QPC_FREQUENCY_HZ.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let mut freq: i64 = 0;
    // SAFETY: `freq` はスタック上の有効な `i64` へのポインタ。
    // `QueryPerformanceFrequency` はこれへ書き込むだけで他の副作用は無い。
    unsafe {
        QueryPerformanceFrequency(&mut freq);
    }
    let freq = freq.max(0) as u64;
    QPC_FREQUENCY_HZ.store(freq, Ordering::Relaxed);
    freq
}

/// ホスト単調時刻をナノ秒で返す(Windows)。
///
/// `QueryPerformanceCounter` の raw カウンタ値を [`qpc_frequency_hz`] で取得した
/// 周波数を使って ns へ変換する。自前の WASAPI バックエンド
/// (`native_backend::windows`)が出力時刻の基準にする時計。
///
/// リアルタイム安全: アロケーション・ロックを一切行わない(他の枝と同じ方針)。
#[cfg(target_os = "windows")]
pub fn host_time_ns() -> u64 {
    let mut counter: i64 = 0;
    // SAFETY: `counter` はスタック上の有効な `i64` へのポインタ。
    // `QueryPerformanceCounter` はこれへ書き込むだけで他の副作用は無い。
    unsafe {
        QueryPerformanceCounter(&mut counter);
    }
    qpc_ticks_to_ns_with_frequency(counter.max(0) as u64, qpc_frequency_hz())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_time_ns_is_monotonically_non_decreasing() {
        let a = host_time_ns();
        let b = host_time_ns();
        assert!(b >= a, "a={a}, b={b}");
    }

    #[test]
    fn host_time_ns_advances_over_a_short_sleep() {
        let a = host_time_ns();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = host_time_ns();
        assert!(b > a, "a={a}, b={b}");
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
    #[test]
    fn ticks_to_ns_with_timebase_scales_by_numer_over_denom() {
        // Apple Silicon の実機値(numer=denom=1、tick がそのまま ns)。
        assert_eq!(ticks_to_ns_with_timebase(1_000, 1, 1), 1_000);
        // numer/denom = 1/2(tick がそのまま ns の半分)。
        assert_eq!(ticks_to_ns_with_timebase(1_000, 1, 2), 500);
        // 典型的な Intel Mac の値に近い比(numer/denom = 125/3)。
        assert_eq!(ticks_to_ns_with_timebase(24, 125, 3), 1_000);
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
    #[test]
    fn ticks_to_ns_with_timebase_falls_back_to_raw_ticks_when_denom_is_zero() {
        assert_eq!(ticks_to_ns_with_timebase(12_345, 7, 0), 12_345);
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
    #[test]
    fn ticks_to_ns_with_timebase_saturates_instead_of_overflowing() {
        // numer/denom = 2/1 で ticks=u64::MAX を変換すると、素の `u128` 計算では
        // u64 の範囲を超える(u64::MAX の2倍)。`as u64` で素直に切り詰めると
        // ラップアラウンドして小さい値になってしまうため、saturating が要る。
        assert_eq!(ticks_to_ns_with_timebase(u64::MAX, 2, 1), u64::MAX);
    }

    #[test]
    fn qpc_ticks_to_ns_with_frequency_converts_whole_seconds() {
        // 典型的な QPC 周波数(10MHz)で 1 秒ぶんのカウンタ値。
        assert_eq!(
            qpc_ticks_to_ns_with_frequency(10_000_000, 10_000_000),
            1_000_000_000
        );
        assert_eq!(qpc_ticks_to_ns_with_frequency(0, 10_000_000), 0);
    }

    #[test]
    fn qpc_ticks_to_ns_with_frequency_handles_non_exact_division() {
        // freq=3 は 1_000_000_000 を割り切れない(余りの扱いを確かめる)。
        assert_eq!(qpc_ticks_to_ns_with_frequency(1, 3), 333_333_333);
        assert_eq!(qpc_ticks_to_ns_with_frequency(4, 3), 1_333_333_333);
    }

    #[test]
    fn qpc_ticks_to_ns_with_frequency_falls_back_to_raw_counter_when_freq_is_zero() {
        assert_eq!(qpc_ticks_to_ns_with_frequency(12_345, 0), 12_345);
    }

    #[test]
    fn qpc_ticks_to_ns_with_frequency_saturates_instead_of_overflowing() {
        // freq=1 で counter=u64::MAX を変換すると素の計算は u64 の範囲を超える
        // (u64::MAX 秒分を ns 化すると u64::MAX の約10億倍)。saturating で止める。
        assert_eq!(qpc_ticks_to_ns_with_frequency(u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn qpc_ticks_to_ns_with_frequency_is_monotonically_non_decreasing_as_counter_grows() {
        let freq = 10_000_000;
        let mut previous = qpc_ticks_to_ns_with_frequency(0, freq);
        for counter in (0..10_000_000_000u64).step_by(123_456_789) {
            let current = qpc_ticks_to_ns_with_frequency(counter, freq);
            assert!(
                current >= previous,
                "counter={counter}, current={current}, previous={previous}"
            );
            previous = current;
        }
    }
}
