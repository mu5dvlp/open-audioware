//! 音楽クロックの土台。
//!
//! 初期構築仕様 §4.4(M5, 確定): 音声スレッドが「楽曲ボイスをこれまで何フレーム
//! レンダリングしたか」を数え、これと出力デバイスのタイムスタンプとの相関から
//! 曲位置とホスト単調時刻の対応(`SongTimeAt`)を導く。
//!
//! M0 時点ではデバイスタイムスタンプとの相関・世代カウンタ(generation)・
//! seqlock スナップショットは未実装(M2/M4 で拡張する)。ここでは
//! 「音声コールバックが送出したフレーム数」を単調カウントする土台のみを置く。
//!
//! この値は音声スレッド(書き込み)とゲームスレッド(読み取り)の双方から
//! ロック無しでアクセスできる必要があるため、最初から `AtomicU64` で持つ。

use std::sync::atomic::{AtomicU64, Ordering};

/// 音声コールバックがこれまでにレンダリングしたフレーム数を数えるカウンタ。
///
/// - `add` は音声スレッドから呼ぶ(リアルタイム安全: ロック・アロケーション無し)。
/// - `get` はどのスレッドからでも呼べる(将来のクロック相関 API・FFI スナップショットの
///   読み出し元になる)。
#[derive(Debug, Default)]
pub struct RenderedFrameCounter {
    frames: AtomicU64,
}

impl RenderedFrameCounter {
    pub const fn new() -> Self {
        Self {
            frames: AtomicU64::new(0),
        }
    }

    /// レンダリング済みフレーム数を加算する。音声スレッドから呼ぶ想定。
    ///
    /// リアルタイム安全: アロケーションもロックも行わない。
    pub fn add(&self, frames: u64) {
        // Release: この加算より前に行われたバッファ書き込みが、
        // 他スレッドから `get` (Acquire) を経由して観測されたときに見えるようにする。
        self.frames.fetch_add(frames, Ordering::Release);
    }

    /// 現在のレンダリング済みフレーム数を取得する。
    pub fn get(&self) -> u64 {
        self.frames.load(Ordering::Acquire)
    }

    /// カウンタをリセットする(将来: 再オープン・テスト用途)。
    pub fn reset(&self) {
        self.frames.store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_zero() {
        let counter = RenderedFrameCounter::new();
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn add_accumulates() {
        let counter = RenderedFrameCounter::new();
        counter.add(128);
        counter.add(256);
        assert_eq!(counter.get(), 384);
    }

    #[test]
    fn reset_returns_to_zero() {
        let counter = RenderedFrameCounter::new();
        counter.add(1_000);
        counter.reset();
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn is_shareable_across_threads() {
        use std::sync::Arc;
        let counter = Arc::new(RenderedFrameCounter::new());
        let writer = {
            let counter = Arc::clone(&counter);
            std::thread::spawn(move || {
                for _ in 0..1_000 {
                    counter.add(1);
                }
            })
        };
        writer.join().unwrap();
        assert_eq!(counter.get(), 1_000);
    }
}
