//! サウンドストレージ(初期構築仕様 §1「サウンドストレージ」/ §5.2)。
//!
//! ロード済み PCM(f32 ステレオ)を ID 管理する。SE はロード時に全デコードして
//! メモリ常駐させる(§4.2)。PCM データの所有権はゲームスレッド側(このストレージ)が持ち、
//! 音声スレッドへは `Arc` の複製をコマンド経由で渡す(所有権は複製されるだけで、
//! 実データがコピーされるわけではない)。
//!
//! `SoundStorage` 自体はゲームスレッド専用(音声コールバックからは触れない)。
//! ヒープアロケーションを行うが、それはリアルタイム安全性規約(§5.3)の対象外
//! (対象は `Renderer::render` の経路のみ)。

use std::collections::HashMap;
use std::sync::Arc;

use crate::format::CHANNELS;

/// ロード済み PCM データ(f32 ステレオ・インターリーブ)。
///
/// `interleaved.len() == frames * CHANNELS` を不変条件とする。
#[derive(Debug)]
pub struct SoundData {
    pub sample_rate: u32,
    pub frames: usize,
    pub interleaved: Vec<f32>,
}

impl SoundData {
    /// フレーム `frame_index`(0始まり)の (L, R) サンプルを返す。
    /// 範囲外は `None`(添字パニックを避ける。§5.3 の規約は本番の音声スレッド経路に
    /// 適用されるが、ここでも一貫して `get` 系を使う)。
    pub fn frame(&self, frame_index: usize) -> Option<(f32, f32)> {
        if frame_index >= self.frames {
            return None;
        }
        let base = frame_index * CHANNELS;
        let l = *self.interleaved.get(base)?;
        let r = *self.interleaved.get(base + 1)?;
        Some((l, r))
    }
}

/// 不透明なサウンド ID。0 は「未割当」を意味する予約値として使わない
/// (`mw-ffi::handle` のハンドル採番方式(初期構築仕様 §4.8)と揃える)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SoundId(pub u64);

/// ロード済みサウンドを ID で管理するストレージ。ゲームスレッド専用。
#[derive(Debug, Default)]
pub struct SoundStorage {
    next_id: u64,
    sounds: HashMap<u64, Arc<SoundData>>,
}

impl SoundStorage {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            sounds: HashMap::new(),
        }
    }

    /// PCM データを登録し、新規 ID を発行する。
    pub fn insert(&mut self, data: SoundData) -> SoundId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.sounds.insert(id, Arc::new(data));
        SoundId(id)
    }

    /// `id` に対応する PCM データの `Arc` 複製を返す(音声スレッドへコマンドで渡す用)。
    pub fn get(&self, id: SoundId) -> Option<Arc<SoundData>> {
        self.sounds.get(&id.0).cloned()
    }

    /// `id` をストレージから取り除き、保持していた `Arc` を返す。
    ///
    /// 呼び出し元(mw-ffi)は、再生中ボイスがあれば別途 `StopVoicesUsingSound` 相当の
    /// コマンドで停止させる(このストレージ自体はボイスの生死を知らない)。
    /// ここで `Arc` の参照を1つ手放しても、音声スレッド側が複製を保持していれば
    /// 実データの解放はその複製が尽きるまで起きない。
    pub fn remove(&mut self, id: SoundId) -> Option<Arc<SoundData>> {
        self.sounds.remove(&id.0)
    }

    pub fn contains(&self, id: SoundId) -> bool {
        self.sounds.contains_key(&id.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silent_sound(frames: usize) -> SoundData {
        SoundData {
            sample_rate: 48_000,
            frames,
            interleaved: vec![0.0; frames * CHANNELS],
        }
    }

    #[test]
    fn ids_start_at_one_and_increment() {
        let mut storage = SoundStorage::new();
        let a = storage.insert(silent_sound(1));
        let b = storage.insert(silent_sound(1));
        assert_eq!(a.0, 1);
        assert_eq!(b.0, 2);
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let storage = SoundStorage::new();
        assert!(storage.get(SoundId(999)).is_none());
    }

    #[test]
    fn remove_then_get_returns_none_but_existing_arc_clone_stays_alive() {
        let mut storage = SoundStorage::new();
        let id = storage.insert(silent_sound(4));
        let kept_alive = storage.get(id).unwrap();

        let removed = storage.remove(id);
        assert!(removed.is_some());
        assert!(storage.get(id).is_none());
        // 音声スレッド側が持っているつもりの複製に相当する `kept_alive` はまだ生きている。
        assert_eq!(kept_alive.frames, 4);
    }

    #[test]
    fn frame_returns_none_out_of_range() {
        let sound = silent_sound(2);
        assert!(sound.frame(2).is_none());
        assert!(sound.frame(0).is_some());
    }
}
