//! ゲームスレッド → 音声スレッドのコマンド(初期構築仕様 §5.2, §7.1)。
//!
//! FFI 呼び出しはこのコマンドをキューへ積むだけ(非ブロッキング)。
//! 音声スレッドはコールバック先頭でキューを消化する([`crate::mixer::Mixer::render`])。
//! SPSC ロックフリーキューの実体は `rtrb`(【仮】)。生成・Producer/Consumer の分配は
//! [`crate::mixer::build`] が行う。

use std::sync::Arc;

use crate::bus::BusId;
use crate::sound::SoundData;

/// 予約 SE 1件分のペイロード(初期構築仕様『§4.5 スケジュール発音』)。
///
/// `Command::PlaySe` と同じ形の発音パラメータに、キュー内で時刻順を保つための
/// `host_time_ns` を除いたものを持たせただけ(時刻自体は `Command::SeSchedule` /
/// `crate::schedule::ScheduleQueue` のキー側で保持する)。
#[derive(Debug)]
pub struct ScheduledSe {
    pub voice_serial: u64,
    /// `SoundStorage` が払い出した不透明 ID(`StopVoicesUsingSound` の照合キー)。
    pub sound_id: u64,
    pub sound: Arc<SoundData>,
    pub bus: BusId,
    pub volume: f32,
}

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
    /// SE をサンプル精度で予約発音する(初期構築仕様『§4.5 スケジュール発音』, M2-5)。
    /// 用途はメトロノームとキャリブレーション用クリック。`Mixer` はこれをソート済み
    /// キューへ挿入し、該当バッファのレンダリング時にバッファ内オフセットサンプル位置
    /// から発音する(バッファ境界への丸めはしない)。
    SeSchedule {
        host_time_ns: u64,
        entry: ScheduledSe,
    },
    /// 楽曲を予約再生する(初期構築仕様『§4.3 楽曲再生』, M2-5)。
    ///
    /// プリロール完了前(`MusicState::Loading`)に予約時刻が到来した場合はエラーにせず、
    /// 「準備完了後、可能な最速時刻」へ繰り下げる 【仮】(`mixer.rs::MusicSchedule` 参照)。
    MusicPlayScheduled { host_time_ns: u64 },
    /// 楽曲ボイスをシークする(内部配線専用。初期構築仕様『§4.3』の `mw_music_seek` に
    /// 相当するが、M2-5 時点では FFI には未公開——楽曲制御 API 全体の公開は後続作業。
    /// ここでは音楽クロックの世代カウンタ(§4.4)が不連続で正しく進むことをオフライン
    /// テストで検証するための最小の配線として用意した)。
    MusicSeek { frames: u64 },
}
