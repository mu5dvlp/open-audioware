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
//! # 調査結果: `cpal::StreamInstant` はどの時計か(cpal 0.18.1)
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
//! ビルド・オフラインテスト専用であり実デバイスとの相関には使わない)。
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

/// ホスト単調時刻をナノ秒で返す。
///
/// リアルタイム安全: アロケーション・ロックを一切行わない(単純なシステムコール
/// またはその薄いラッパ1回のみ)。`mw_se_schedule` / `mw_music_play_scheduled` の
/// `host_time_ns` 引数、および `cpal::OutputCallbackInfo` から求めるデバイス
/// タイムスタンプは、いずれもこの関数と同じ時計を指す(モジュール doc 参照)。
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
pub fn host_time_ns() -> u64 {
    // cpal 0.18.1 `src/host/coreaudio/mod.rs::host_time_to_stream_instant` と
    // 完全に同じ式(mach_absolute_time の raw tick 数 × timebase の numer/denom → ns)。
    // 同じ式で読む以上、`OutputCallbackInfo::timestamp()` が返す `StreamInstant` と
    // 単位・原点・レートのいずれもずれようがない。
    // SAFETY: どちらも Mach カーネルへの単純な問い合わせで、引数はスタック上のローカル
    // 変数への有効なポインタのみ。エラー時も未初期化メモリを読まない
    // (`mach_timebase_info` が失敗した場合は下の `KERN_SUCCESS` チェックで弾く)。
    unsafe {
        let ticks = mach2::mach_time::mach_absolute_time();
        let mut info = mach2::mach_time::mach_timebase_info::default();
        let status = mach2::mach_time::mach_timebase_info(&mut info);
        if status != mach2::kern_return::KERN_SUCCESS || info.denom == 0 {
            // 失敗しても致命傷にはしない(§4.8 の思想と同じ)。実機の CoreAudio では
            // まず失敗しない経路(cpal 自身もこれを事実上想定していない)だが、
            // 保険として raw tick をそのまま返す(単調性だけは保たれる)。
            return ticks;
        }
        (ticks as u128 * info.numer as u128 / info.denom as u128) as u64
    }
}

/// ホスト単調時刻をナノ秒で返す(Android / Linux)。
///
/// cpal 0.18.1 `src/host/aaudio/convert.rs::now_stream_instant` と同じ式
/// (`clock_gettime(CLOCK_MONOTONIC)`)。Android 実機ではこれが `OutputCallbackInfo` の
/// `StreamInstant` と同一時計になる(モジュール doc 参照)。Linux(CI 専用。
/// 実配布対象ではない)でも同じ syscall が使えるため、ここに含めて
/// `cargo test --workspace` を通す。
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
pub fn host_time_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` はスタック上の有効な `timespec`。`clock_gettime` はこれへ
    // 書き込むだけで、他の副作用は無い。
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
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
}
