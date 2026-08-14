//! ゲームスレッド → 音声スレッドのコマンド(初期構築仕様 §5.2, §7.1)。
//!
//! FFI 呼び出しはこのコマンドをキューへ積むだけ(非ブロッキング)。
//! 音声スレッドはコールバック先頭でキューを消化する([`crate::mixer::Mixer::render`])。
//! SPSC ロックフリーキューの実体は `rtrb`(【仮】)。生成・Producer/Consumer の分配は
//! [`crate::mixer::build`] が行う。

use std::sync::Arc;

use crate::bus::BusId;
use crate::sound::SoundData;

/// 音声スレッドへ送るコマンド。
///
/// `Command` 自体および内部の `Arc<SoundData>` の複製(clone、参照カウント増加のみ)は
/// すべてゲームスレッド側(コマンド発行時)で完結する。音声スレッド側はこれを
/// 受け取って所有権を保持するだけで、複製もドロップも(通常経路では)行わない
/// (ドロップの回収経路は `crate::mixer::ReclaimReceiver` を参照)。
#[derive(Debug)]
pub enum Command {
    /// SE を即時発音する。次のオーディオコールバックで必ず発音される(§4.2)。
    PlaySe {
        voice_serial: u64,
        /// `SoundStorage` が払い出した不透明 ID(`StopVoicesUsingSound` の照合キー)。
        sound_id: u64,
        sound: Arc<SoundData>,
        bus: BusId,
        volume: f32,
    },
    /// 指定ボイスを停止する(既定ランプ経由。§4.2/M13)。
    StopVoice { voice_serial: u64 },
    /// 指定ボイスの音量を変更する(既定ランプ経由。M13)。
    SetVoiceVolume { voice_serial: u64, volume: f32 },
    /// 指定サウンド ID を再生中の全ボイスを停止する(`mw_sound_release` から発行される。
    /// 既定ランプ経由。停止後、当該ボイスが保持していた `Arc` は回収キュー経由で
    /// ゲームスレッドへ返却される)。
    ///
    /// 照合は `SoundStorage` の不透明 ID(単調増加、再利用されない)で行う。
    /// `Arc::as_ptr` のポインタ比較にしないのは、解放直後にアロケータがアドレスを
    /// 再利用した場合の ABA 問題を構造的に避けるため。
    StopVoicesUsingSound { sound_id: u64 },
    /// バス音量を変更する(既定ランプ経由。M13)。
    SetBusVolume { bus: BusId, volume: f32 },
    /// バスをフェードする(呼び出し側指定の時間、ms)。
    BusFade { bus: BusId, target: f32, ms: f32 },
}
