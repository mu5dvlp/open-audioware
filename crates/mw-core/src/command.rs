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
    /// 新しい楽曲を準備する(初期構築仕様『§4.3』の「準備(プリロール込み)」/
    /// `mw_music_set` に相当。M2-7)。
    ///
    /// `MusicVoice::prepare` は状態を `Loading` へ戻し、位置・ループ区間・保留中の
    /// フェード遷移をすべて捨てて仕切り直す(目標音量だけは曲をまたいで保持する。
    /// `MusicVoice::prepare` 自身のドキュメント参照)。以降の `render` で
    /// `source.is_ready()` が true になった時点で自動的に `Ready` へ遷移する
    /// ——`mw_music_state()` が最終的に `Ready` を返すまでのポーリング契約は
    /// この遷移に乗っている。
    ///
    /// **リングバッファへ残った前曲の PCM を掃除する役目はこのコマンドには無い**
    /// (`MusicVoice::prepare` は `MusicFrameSource` に一切触れない)。曲切り替え時の
    /// 掃除は `mw-ffi::mw_music_set` が直後に送る `MusicSeek { frames: 0 }` が
    /// `source.request_seek` 経由で行う(`stream.rs` モジュール doc「シークの調停」
    /// 参照)。`MusicPrepare` を先に送っておくことで、直後の `MusicStop`(§4.3 の
    /// 手順どおり送るが、`Loading` から見ると no-op になる)がまだ `Playing` だった
    /// 前曲のフェードアウト中に割り込む余地を作らない——`prepare` が
    /// `pending_settle`/`gain` を無条件に初期化してしまうため。
    MusicPrepare,
    /// 楽曲ボイスをシークする(初期構築仕様『§4.3』の `mw_music_seek` に相当)。
    ///
    /// `MusicVoice::seek` はランプを経由しない不連続そのものであり、`MusicRenderOutcome`
    /// の `discontinuity` 経由で音楽クロックの世代カウンタ(§4.4)を進める
    /// (`mixer.rs::Mixer::render` 参照)。
    MusicSeek { frames: u64 },
    /// 楽曲を一時停止する(初期構築仕様『§4.3』の `mw_music_pause` に相当)。
    ///
    /// `MusicVoice::pause` は既定ランプでフェードアウトしてから `Paused` へ収束する
    /// (M13: 音量変化・停止はランプを経由する)。`Playing` 以外からの呼び出しは
    /// `MusicVoice` 側で no-op として無視される。
    MusicPause,
    /// 巻き戻し付きで再開する(初期構築仕様『§4.3』の `mw_music_resume_at` に相当。
    /// テンプレート仕様「中断対応」の「数秒巻き戻し + カウントダウン再開」の受け皿)。
    ///
    /// `MusicVoice::resume_at` は指定フレームへ再位置決めしたうえで既定ランプで
    /// フェードインする。位置の付け替え自体は不連続としてクロックの世代カウンタを進める。
    MusicResumeAt { frames: u64 },
    /// 楽曲を停止する(初期構築仕様『§4.3』の `mw_music_stop` に相当)。
    ///
    /// `Playing` 中は既定ランプでフェードアウトしてから位置を 0 に戻し `Ready` へ、
    /// `Paused` 中はランプ無しでその場で `Ready` へ戻る(`MusicVoice::stop` 参照)。
    MusicStop,
    /// 楽曲のループ区間を設定・解除する(初期構築仕様『§4.3』の `mw_music_set_loop` に
    /// 相当。選曲プレビュー用、フェードイン/アウト付き)。
    ///
    /// `None` はループ解除。`Some((start, end))` で `start >= end` の不正な区間は
    /// `MusicVoice::set_loop` 側でループ無しとして扱われる(リアルタイム安全性のため
    /// パニックしない)。
    MusicSetLoop { region: Option<(u64, u64)> },
}
