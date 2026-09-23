//! init/shutdown ハンドルの管理。
//!
//! 初期構築仕様 §4.8(確定): ハンドルは不透明な整数 ID(ポインタを C# に渡さない)。
//! 二重解放・使用後解放は無効 ID エラーとして検出する(クラッシュさせない)。
//! §6: Unity Editor のドメインリロードに備え、init/shutdown は冪等にする
//! (dylib は常駐前提。ドメインリロードのたびに新規プロセスが立つわけではない)。
//!
//! M0 時点ではインスタンスは同時に1つのみ(グローバルレジストリ)。
//! 複数インスタンスを許すかは未検討(現状の Unity 統合はプロセス内で1つのみ使う想定)。
//!
//! M1 で `Instance` にゲームスレッド側ハンドル(`CommandSender` / `ReclaimReceiver`)と
//! サウンドストレージ・ボイスシリアル採番器を追加した(§5.2「コマンド/イベントキュー」)。
//!
//! M2-7 で楽曲再生(初期構築仕様『§4.3』)の下ごしらえを追加した:
//! - デコードスレッド(`crate::decode_thread`)を `init()` で1本立て、`shutdown()` で
//!   確実に停止・join する。
//! - 楽曲(圧縮バイト列)のストレージ `music_bytes` と、その ID 採番・空間分離
//!   (`MUSIC_ID_FLAG`)。SE の ID(`mw_core::SoundStorage` が採番)とは別空間にする
//!   理由は [`MUSIC_ID_FLAG`] のドキュメント参照。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use mw_backend::{Backend, BackendError, CpalBackend};
use mw_core::{
    BUS_COUNT, BgmStatePublisher, BusId, Command, CommandSender, Config, EventQueue,
    MusicClockPublisher, MusicClockSnapshot, MusicDecoder, MusicState, ReclaimReceiver, Renderer,
    SoundStorage, StreamErrorReason, SymphoniaDecoder,
};

use crate::decode_thread::{self, DecoderSender};
use crate::reopen::ReopenPolicy;

/// 出力デバイスが実際にオープンされるまでの暫定サンプルレート(§4.7 推奨の 48kHz)。
/// `CpalBackend::open` がデバイスとネゴシエートした実レートで上書きする
/// (`Renderer::set_sample_rate`、コールバックが動き出す前)。
const PROVISIONAL_SAMPLE_RATE: u32 = 48_000;

/// 楽曲(Music モード)の ID に立てる目印ビット(オーケストレータの決定)。
///
/// SE の ID は `mw_core::SoundStorage` が採番し `Arc<SoundData>`(デコード済み PCM)を
/// 指す。楽曲は「圧縮のまま保持」(初期構築仕様『§5.5』)なので置き場所が違い
/// (`Instance::music_bytes`)、同じ ID 空間を共有すると `mw_sound_release` が
/// 誤って SE 側のエントリを消してしまう事故が起こりうる。そこで楽曲 ID は
/// 最上位ビットを立てて空間を分離する。ID は C# から見て不透明な整数
/// (初期構築仕様『§4.8』)なので、このビット演算による分離は呼び出し側に副作用を
/// 持たない。
pub const MUSIC_ID_FLAG: u64 = 1 << 63;

/// `id` が楽曲(Music モード)の ID かどうかを判定する([`MUSIC_ID_FLAG`] 参照)。
pub fn is_music_id(id: u64) -> bool {
    id & MUSIC_ID_FLAG != 0
}

/// 楽曲ボイス(`mw_music_*`)か BGM ボイス(`mw_bgm_*`)かを表す選択子(P1-7)。
///
/// `mw_music_set`/`mw_bgm_set`・`mw_music_state`/`mw_bgm_state`・`mw_music_stop`/
/// `mw_bgm_stop`・`mw_music_set_loop`/`mw_bgm_set_loop` の4組は実体がほぼ完全重複
/// していたため、この enum 1つで実装を共有する(`crate::ffi` 側の
/// `set_music_track`/`music_target_state`/`stop_music_track`/`set_music_track_loop`
/// が使う)。
///
/// **この4組以外は意図的に対象外**——`mw_music_pause`/`mw_music_resume_at`/
/// `mw_music_seek`/`mw_music_play_scheduled`/`mw_music_get_position`/`mw_bgm_play`
/// は BGM 側(または楽曲側)に対応する API 自体が無い(`mw_core::Command` に
/// `BgmPause`/`BgmResumeAt`/公開版の `BgmSeek`/`BgmPlayScheduled` が無い。理由は
/// `crates/mw-ffi/CLAUDE.md`「BGM との分担 —— 共有するもの・分けるもの」参照)ため、
/// 共有化すると存在しない対称性を捏造することになる。個別のまま残す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MusicTarget {
    Music,
    Bgm,
}

impl MusicTarget {
    /// ログ・エラーメッセージに使う FFI 関数名(`set_music_track` が使う。元の
    /// `mw_music_set`/`mw_bgm_set` それぞれのログ文言をそのまま保つため)。
    pub(crate) fn set_fn_name(self) -> &'static str {
        match self {
            MusicTarget::Music => "mw_music_set",
            MusicTarget::Bgm => "mw_bgm_set",
        }
    }

    /// `MusicPrepare`/`BgmPrepare` のうち対応する方。
    pub(crate) fn prepare_command(self) -> Command {
        match self {
            MusicTarget::Music => Command::MusicPrepare,
            MusicTarget::Bgm => Command::BgmPrepare,
        }
    }

    /// `MusicSeek`/`BgmSeek` のうち対応する方。
    pub(crate) fn seek_command(self, frames: u64) -> Command {
        match self {
            MusicTarget::Music => Command::MusicSeek { frames },
            MusicTarget::Bgm => Command::BgmSeek { frames },
        }
    }

    /// `MusicStop`/`BgmStop` のうち対応する方。
    pub(crate) fn stop_command(self) -> Command {
        match self {
            MusicTarget::Music => Command::MusicStop,
            MusicTarget::Bgm => Command::BgmStop,
        }
    }

    /// `MusicSetLoop`/`BgmSetLoop` のうち対応する方。
    pub(crate) fn set_loop_command(self, region: Option<(u64, u64)>) -> Command {
        match self {
            MusicTarget::Music => Command::MusicSetLoop { region },
            MusicTarget::Bgm => Command::BgmSetLoop { region },
        }
    }
}

pub struct Instance {
    handle: u64,
    /// `Box<dyn Backend + Send>` にしてテスト時だけ fake backend を差し込めるようにする。
    /// 音声コールバックは `Backend::open` へムーブ済みの `Renderer` から駆動され、
    /// `Instance` を経由しないため、動的ディスパッチが増えるのは元から FFI 越えの
    /// コストを払っているゲームスレッド側の呼び出しだけで、リアルタイム安全性への
    /// 影響はない。`Send` は再オープンの切り離し一式をワーカースレッドへムーブし、
    /// `Instance` を static なレジストリへ保持するために必要で、`Sync` は要らない。
    backend: Box<dyn Backend + Send>,
    pub command_sender: CommandSender,
    pub reclaim_receiver: Mutex<ReclaimReceiver>,
    pub sounds: Mutex<SoundStorage>,
    /// イベント通知(初期構築仕様『§4.6』)の読み書きハンドル。書き込み・読み出しの
    /// 両方を1つの型に集約してある(`mw_core::event` モジュール doc 参照)。
    /// `mw_poll_events`(ゲームスレッド)がここから `drain` し、`CpalBackend` が
    /// ストリームのエラー通知経路(非リアルタイムスレッド)から `push_side_channel` する。
    pub events: Arc<EventQueue>,
    next_voice_serial: AtomicU64,
    /// I/O バッファ長の実測ログを既に出したか(1インスタンスにつき1回だけ出す)。
    logged_buffer_info: AtomicBool,
    /// 音楽クロック(初期構築仕様『§4.4』)の読み手。`mw_music_get_position`/
    /// `mw_music_state` はここから seqlock 経由でロック無しに読む
    /// (`Renderer::build` が返す `Arc<MusicClockPublisher>` をそのまま保持する)。
    music_clock: Arc<MusicClockPublisher>,
    /// 楽曲(圧縮バイト列)のストレージ。`mw_core::SoundStorage` には持たせない
    /// ——mw-core は「オフラインレンダリングだけで完結する層」という位置づけで、
    /// 未デコードの圧縮バイト列(ファイル形式のパースすら済んでいないもの)は
    /// そこに属さない(`crates/mw-core/CLAUDE.md` 参照)。
    music_bytes: Mutex<HashMap<u64, Arc<Vec<u8>>>>,
    next_music_serial: AtomicU64,
    /// デコードスレッドへ新しいデコーダを渡すハンドル(`mw_music_set` から使う。
    /// `crate::decode_thread` モジュール doc 参照)。
    decoder_tx: DecoderSender,
    /// デコードスレッドの停止フラグ。`shutdown()` がこれを立ててから join する。
    decode_thread_stop: Arc<AtomicBool>,
    /// デコードスレッドの join ハンドル。`shutdown()` で必ず取り出して join する
    /// (`Option` なのは `shutdown()` が `&mut self` で `take` するため)。
    decode_thread: Option<JoinHandle<()>>,
    /// BGM ボイスの状態(初期構築仕様『§2』M14, M4-3)。`mw_bgm_state` はここから
    /// ロック無しで読む(`mw_core::mixer::BgmHandles::state` と同じ `Arc`)。
    /// 音楽クロック(`music_clock`)とは別物——BGM はクロックを持たない。
    bgm_state: Arc<BgmStatePublisher>,
    /// BGM 専用のデコードスレッドへ新しいデコーダを渡すハンドル(`mw_bgm_set` から使う)。
    /// 楽曲用の `decoder_tx` とは完全に独立した別スレッド(`decode_thread::spawn` を
    /// もう一度呼んで立てる。`crate::decode_thread` はどちらの用途にも汎用に使える
    /// 設計になっている——`MusicStreamProducer` を受け取って回すだけで、
    /// 「楽曲用」「BGM 用」という区別を一切知らない)。
    bgm_decoder_tx: DecoderSender,
    /// BGM 用デコードスレッドの停止フラグ(`decode_thread_stop` の BGM 版)。
    bgm_decode_thread_stop: Arc<AtomicBool>,
    /// BGM 用デコードスレッドの join ハンドル(`decode_thread` の BGM 版)。
    bgm_decode_thread: Option<JoinHandle<()>>,
    /// 予約 SE のキューが満杯で挿入できず**発音されなかった**累計件数
    /// (`mw_core::Mixer::se_schedule_overflow_count` の複製)。`mw_get_output_underrun_stats`
    /// がここから読む。
    ///
    /// 🔴 **`Renderer` を `Backend::open` へムーブする前に取ること** —— ムーブ後は
    /// `Mixer` へ触れない(`mw_core::renderer` モジュール doc「所有権の設計」)。
    /// ⚠️ 再オープン(`Instance::begin_reopen`/`run_reopen_worker`)は `Mixer` ごと
    /// 作り直すので、**このフィールドも一緒に差し替える**(差し替え忘れると、誰も
    /// 増やさない古いカウンタを読み続けて「ずっと 0」に見える)。
    se_schedule_overflow_count: Arc<AtomicU64>,

    // --- M3「Android(AAudio)切断復旧」案A(初期構築仕様『§6』確定) ------------------
    //
    // 内部再オープン(`Instance::begin_reopen` 〔段1〕→ `run_reopen_worker`
    // 〔段2〕→ `finalize_reopen_success`/`finalize_reopen_failure` 〔段3〕。
    // P3-11 で3段構成へ分割した)は `Renderer`/`Mixer` を丸ごと作り直す(=上の
    // ハンドル一式をすべて差し替える)ため、復元に要る情報は `Renderer` の外側
    // (=このキャッシュ)に持っておく必要がある。`sounds`/`music_bytes` は元々
    // ここにあり再オープンでも触らないため、「ロード済みの SoundId が無効に
    // ならない」は追加のキャッシュ無しで自動的に満たされる
    // (`Instance::begin_reopen` のドキュメント参照)。
    /// 直近の `mw_music_set` が指定した楽曲 ID(0 = 未設定)。再オープン後に同じ曲を
    /// 読み直すために使う。
    last_music_sound_id: AtomicU64,
    /// 直近の `mw_music_set_loop` が設定したループ区間(`mw_music_set` のたびに
    /// `None` へ仕切り直す——`MusicVoice::prepare` 自身がループ区間を無条件に
    /// 初期化する挙動と揃えるため。`Instance::note_music_set` 参照)。
    last_music_loop: Mutex<Option<(u64, u64)>>,
    /// `last_music_sound_id` の BGM 版。
    last_bgm_sound_id: AtomicU64,
    /// `last_music_loop` の BGM 版。
    last_bgm_loop: Mutex<Option<(u64, u64)>>,
    /// 4バスの直近の音量(`mw_bus_set_volume`/`mw_bus_fade` の目標値。`f32::to_bits`/
    /// `from_bits` で `AtomicU32` へ格納する——`f32` 自体は atomic 型が無いため)。
    /// 既定値は `mw_core::bus::Bus::new()` と同じ 1.0。
    bus_volumes: [AtomicU32; BUS_COUNT],
    /// 切断からの内部再オープンを試みるかどうかの判定(`crate::reopen::ReopenPolicy`
    /// のドキュメント参照)。
    reopen: ReopenPolicy,

    // --- P3-11(2026-09-23)「段2」の間だけ有効な補助フィールド ---------------------
    /// 直近に実際にネゴシエートできていた出力サンプルレート(`Backend::open` が
    /// 成功するたびに書き込む。`init()`/段3 `finalize_reopen_success` 参照)。
    ///
    /// 🔴 [`Instance::backend_sample_rate`] のフォールバック専用。P3-11 で再オープンの
    /// 「段2」(バックエンドを `Instance` から切り離してワーカースレッド上で
    /// close→open し直す間)を導入した結果、その間だけ `self.backend` が新品の
    /// 未オープンバックエンド(`sample_rate() == 0`)になる——`mw_sound_load` の
    /// リサンプル要否判定(初期構築仕様『§4.7』)がこの窓で 0 を掴むと壊れた wav
    /// として扱われかねないため、直近の実測値をここに保持してフォールバックする。
    /// 詳しいトレードオフは [`Instance::backend_sample_rate`] のドキュメント参照。
    last_known_sample_rate: AtomicU32,
}

impl Instance {
    /// 出力デバイスの実サンプルレート(`Backend::open` がネゴシエートした値)。
    ///
    /// `mw_sound_load`(SE ロード)が wav のリサンプル要否を判定するために使う
    /// (初期構築仕様『§4.7』: 「SE はロード時に全デコード + 必要ならロード時に
    /// リサンプルして出力レート化」)。
    ///
    /// 🔴 P3-11(2026-09-23): 内部再オープンの「段2」の間(`self.backend` がまだ
    /// 新品の未オープンバックエンドに差し替わっているだけの状態)は
    /// `self.backend.sample_rate()` が 0 になる——このときは代わりに
    /// 直近に実際にオープンできていたときの値([`Instance::last_known_sample_rate`])
    /// を返す。⚠️ **トレードオフを承知で受け入れる**: デバイス側のレートが再オープンを
    /// 挟んで実際に変わっていた場合、この窓の間にロードされた SE は「1つ前の」
    /// レートを前提にリサンプルされてしまう(初期構築仕様『§4.7』)。代替案(段2でも
    /// ロックを取ってブロックする)はこの P3-11 そのものが撤去しようとしている挙動へ
    /// 逆戻りするため採らない。窓は数百 ms、影響は「SE 1個がわずかに違うレートへ
    /// リサンプルされる」だけ(クラッシュしない・再ロードすれば直る)なので許容する。
    pub fn backend_sample_rate(&self) -> u32 {
        let live = self.backend.sample_rate();
        if live != 0 {
            return live;
        }
        self.last_known_sample_rate.load(Ordering::Relaxed)
    }

    /// 新規ボイスシリアル(不透明な voice id)を1つ払い出す。0 は「未割当」の予約値。
    pub fn next_voice_serial(&self) -> u64 {
        self.next_voice_serial
            .fetch_add(1, Ordering::Relaxed)
            .max(1)
    }

    /// オーディオコールバックが実際に受け取ったバッファ長を、1インスタンスにつき1回だけ
    /// ログへ出す(`docs/measurement-m1.md` §8.7 の裏取り)。
    ///
    /// iOS の `AVAudioSession` は希望した I/O バッファ長をそのまま採用したかのように申告する
    /// ことがあり、申告値だけでは遅延の見積もりを信用できない。ここで出すのは音声スレッドが
    /// 実際に受け取ったフレーム数なので、突き合わせれば申告値の真偽が分かる。
    ///
    /// ゲームスレッドから呼ぶこと(`mw_backend::mw_log!` はリアルタイム安全ではない)。
    /// コールバックがまだ1度も走っていなければ何もせず、次の機会に持ち越す。
    pub fn log_buffer_info_once(&self) {
        if self.logged_buffer_info.load(Ordering::Relaxed) {
            return;
        }
        let frames = self.backend.last_callback_frames();
        let sample_rate = self.backend.sample_rate();
        if frames == 0 || sample_rate == 0 {
            return;
        }
        self.logged_buffer_info.store(true, Ordering::Relaxed);
        let ms = frames as f64 * 1000.0 / sample_rate as f64;
        mw_backend::mw_log!(
            "[mw-ffi] audio callback buffer (measured): {frames} frames @ {sample_rate} Hz = {ms:.3} ms"
        );
    }

    /// 音声スレッドが手放した `Arc<SoundData>` をゲームスレッド上で回収する。
    /// FFI 呼び出しの合間に日和見的に呼ぶ(§5.4: 非ブロッキング。rtrb の pop は O(1))。
    pub fn drain_reclaimed(&self) {
        let mut guard = self
            .reclaim_receiver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.drain();
    }

    /// 音楽クロックのスナップショットを取得する(`mw_music_state`/
    /// `mw_music_get_position` から使う)。ロック無し(seqlock、`snapshot()` 自体の
    /// ドキュメント参照)。
    pub fn music_clock_snapshot(&self) -> MusicClockSnapshot {
        self.music_clock.snapshot()
    }

    /// 楽曲(圧縮バイト列)を登録し、SE とは ID 空間を分離した不透明 ID を発行する
    /// ([`MUSIC_ID_FLAG`] 参照)。
    pub fn insert_music_bytes(&self, bytes: Vec<u8>) -> u64 {
        // voice serial 採番(`next_voice_serial`)と同じ流儀: 0 を「未割当」として
        // 避けるため `.max(1)` する。`MUSIC_ID_FLAG` を OR するので実際にはこの
        // `.max(1)` が無くても id が 0 になることは無いが、採番ロジックの見た目を
        // 他の採番器と揃えておく。
        let serial = self
            .next_music_serial
            .fetch_add(1, Ordering::Relaxed)
            .max(1);
        let id = serial | MUSIC_ID_FLAG;
        let mut map = self
            .music_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.insert(id, Arc::new(bytes));
        id
    }

    /// `id` に対応する楽曲バイト列の `Arc` 複製を返す(`mw_music_set` が使う)。
    pub fn get_music_bytes(&self, id: u64) -> Option<Arc<Vec<u8>>> {
        let map = self
            .music_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.get(&id).cloned()
    }

    /// `id` をストレージから取り除く(`mw_sound_release` の楽曲 ID 経路)。
    ///
    /// 再生中の楽曲を release した場合の挙動: 拒否せず即座に解放を受け付ける。
    ///
    /// 🔴 **P3-13(2026-09-15)で理由が変わった。** それまでは `mw_music_set` が
    /// `SymphoniaDecoder::open` へバイト列の**複製**を渡していた(`Vec<u8>` を値で
    /// 要求するため)ので「そもそも別々のメモリ」だった。いまは
    /// `SymphoniaDecoder::open_shared` に `Arc<Vec<u8>>` をそのまま渡しており、
    /// **デコードスレッドとストレージは同じメモリを共有している。**
    ///
    /// それでも release が安全なのは `Arc` の参照カウントによる ——
    /// ここでマップから外れても、デコーダが保持している複製(`Arc` の複製であって
    /// バイト列の複製ではない)が生きている限りメモリは解放されない。
    /// ⚠️ したがって「release したのにメモリが減らない」ことが**ありうる**
    /// (再生中の曲を release した場合。曲を切り替えるかデコーダが落ちた時点で減る)。
    /// 数十MB の複製を毎回作るコストと引き換えに、これは受け入れる判断。
    pub fn remove_music_bytes(&self, id: u64) -> Option<Arc<Vec<u8>>> {
        let mut map = self
            .music_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.remove(&id)
    }

    /// デコードスレッドへ新しいデコーダを渡す(`mw_music_set` が使う)。
    ///
    /// 送信が失敗する(`false` を返す)のは、デコードスレッドの受信側が既に
    /// 終了している場合のみ——通常はデコードスレッドはパニックしても内部で
    /// `catch_unwind` して生き続ける(`crate::decode_thread` 参照)ため、
    /// `mw_shutdown` 済みのハンドルを使い回そうとした場合を除き実運用では
    /// 起こらない防御的分岐。
    pub fn send_decoder(&self, decoder: Box<dyn MusicDecoder + Send>) -> bool {
        self.decoder_tx.send(decoder).is_ok()
    }

    /// BGM ボイスの現在の状態(初期構築仕様『§2』M14, M4-3)。ロック無し
    /// (`BgmStatePublisher::read` そのもの)。`mw_bgm_state` が使う。
    pub fn bgm_state(&self) -> MusicState {
        self.bgm_state.read()
    }

    /// BGM 用デコードスレッドへ新しいデコーダを渡す(`mw_bgm_set` が使う。
    /// [`Instance::send_decoder`] の BGM 版)。
    pub fn send_bgm_decoder(&self, decoder: Box<dyn MusicDecoder + Send>) -> bool {
        self.bgm_decoder_tx.send(decoder).is_ok()
    }

    // --- P1-7: `MusicTarget` によるディスパッチ(`crate::ffi::set_music_track` 等が使う) ---
    //
    // 既存の個別メソッド([`Instance::send_decoder`]/[`Instance::send_bgm_decoder`]/
    // [`Instance::note_music_set`]/[`Instance::note_bgm_set`]/
    // [`Instance::note_music_loop`]/[`Instance::note_bgm_loop`]/[`Instance::bgm_state`]/
    // [`Instance::music_clock_snapshot`])は削除せず残してある——`begin_reopen` /
    // `restore_music` / `restore_bgm`(意図的に楽曲と BGM で挙動が異なる箇所。
    // `Instance::begin_reopen` のドキュメント「復元できる状態・できない状態」参照)は
    // 引き続きそれぞれを個別に呼ぶ。ここへ統合すると、その意図的な非対称性まで
    // 巻き込んで畳んでしまう事故になる。

    /// [`MusicTarget`] に応じたデコードスレッドへデコーダを渡す
    /// ([`Instance::send_decoder`]/[`Instance::send_bgm_decoder`] のディスパッチ版)。
    pub(crate) fn send_decoder_for(
        &self,
        target: MusicTarget,
        decoder: Box<dyn MusicDecoder + Send>,
    ) -> bool {
        match target {
            MusicTarget::Music => self.send_decoder(decoder),
            MusicTarget::Bgm => self.send_bgm_decoder(decoder),
        }
    }

    /// [`MusicTarget`] に応じた `mw_music_set`/`mw_bgm_set` 成功後の復元キャッシュ更新
    /// ([`Instance::note_music_set`]/[`Instance::note_bgm_set`] のディスパッチ版)。
    pub(crate) fn note_track_set(&self, target: MusicTarget, sound_id: u64) {
        match target {
            MusicTarget::Music => self.note_music_set(sound_id),
            MusicTarget::Bgm => self.note_bgm_set(sound_id),
        }
    }

    /// [`MusicTarget`] に応じた `mw_music_set_loop`/`mw_bgm_set_loop` 成功後の
    /// 復元キャッシュ更新([`Instance::note_music_loop`]/[`Instance::note_bgm_loop`]
    /// のディスパッチ版)。
    pub(crate) fn note_track_loop(&self, target: MusicTarget, region: Option<(u64, u64)>) {
        match target {
            MusicTarget::Music => self.note_music_loop(region),
            MusicTarget::Bgm => self.note_bgm_loop(region),
        }
    }

    /// [`MusicTarget`] に応じた再生状態(`mw_music_state`/`mw_bgm_state` のディスパッチ版)。
    ///
    /// **格納場所の非対称性は温存する**: 楽曲は音楽クロック
    /// (`music_clock_snapshot().state`、seqlock)、BGM は専用の
    /// `BgmStatePublisher::read`——BGM はクロック(位置・世代)を持たない
    /// (`crates/mw-ffi/CLAUDE.md`「共有するもの・分けるもの」参照)ため、ここを
    /// `bgm_state()` 相当の一本化されたストレージに統合することはできない
    /// (統合すると BGM が誤って音楽クロックの世代・位置を持つかのようになってしまう)。
    pub(crate) fn track_state(&self, target: MusicTarget) -> MusicState {
        match target {
            MusicTarget::Music => self.music_clock_snapshot().state,
            MusicTarget::Bgm => self.bgm_state(),
        }
    }

    /// 出力レイテンシの実測値([`Backend::output_latency_ns`])を取得する
    /// (`mw_get_output_latency_ns` が使う)。
    pub fn output_latency_ns(&self) -> u64 {
        self.backend.output_latency_ns()
    }

    /// 出力レイテンシの実測値を1回だけログへ出す
    /// (`Backend::log_output_latency_once` へ委譲)。`mw_get_output_latency_ns`
    /// (ゲームスレッド経路)から呼ぶこと。コールバック内から呼んではいけない
    /// (`mw_log!` はアロケーションとロックを伴う)。
    pub fn log_output_latency_once(&self) {
        self.backend.log_output_latency_once();
    }

    /// 出力コールバックのアンダーラン(の疑い)統計([`Backend::output_underrun_count`]
    /// 等)を取得する(`mw_get_output_underrun_stats` が使う)。
    ///
    /// **`mw_core::Event::Underrun`(`mw_poll_events` 経由)とは別物。**
    /// `mw_backend::underrun` モジュール doc / `crate::types::MwOutputUnderrunStats`
    /// のドキュメント参照。
    /// ⚠️ 4つ目(`se_schedule_overflow_count`)だけは**バックエンド由来ではない** ——
    /// ミキサのカウンタで、相乗りさせている理由は `crate::types::MwOutputUnderrunStats`
    /// のドキュメント参照。
    pub fn output_underrun_stats(&self) -> (u64, u64, u32, u64) {
        (
            self.backend.output_underrun_count(),
            self.backend.last_output_underrun_host_time_ns(),
            self.backend.consecutive_output_underrun_count(),
            self.se_schedule_overflow_count.load(Ordering::Relaxed),
        )
    }

    /// 出力コールバックのアンダーラン(の疑い)を新たに検知していれば、その分だけ
    /// ログへ出す(`Backend::log_new_output_underruns` へ委譲)。
    /// `mw_get_output_underrun_stats`(ゲームスレッド経路)から呼ぶこと。
    /// コールバック内から呼んではいけない(`mw_log!` はアロケーションとロックを伴う)。
    pub fn log_new_output_underruns(&self) {
        self.backend.log_new_output_underruns();
    }

    // --- M3「Android(AAudio)切断復旧」案A ---------------------------------------

    /// `mw_music_set` が成功した後に呼ぶ復元キャッシュの更新
    /// (段1 `Instance::begin_reopen` がここへ読みに来る)。`MusicVoice::prepare` 自身が
    /// ループ区間を無条件に初期化する挙動(`music.rs::MusicVoice::prepare`)に揃え、ここでも
    /// ループキャッシュを一緒に仕切り直す——そうしないと、曲を切り替えた後の
    /// 再オープンで前の曲のループ区間を新しい曲へ誤って復元してしまう。
    pub fn note_music_set(&self, sound_id: u64) {
        self.last_music_sound_id.store(sound_id, Ordering::Relaxed);
        *self
            .last_music_loop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// `mw_music_set_loop` が成功した後に呼ぶ復元キャッシュの更新。
    pub fn note_music_loop(&self, region: Option<(u64, u64)>) {
        *self
            .last_music_loop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = region;
    }

    /// [`Instance::note_music_set`] の BGM 版。
    pub fn note_bgm_set(&self, sound_id: u64) {
        self.last_bgm_sound_id.store(sound_id, Ordering::Relaxed);
        *self
            .last_bgm_loop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// [`Instance::note_music_loop`] の BGM 版。
    pub fn note_bgm_loop(&self, region: Option<(u64, u64)>) {
        *self
            .last_bgm_loop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = region;
    }

    /// `mw_bus_set_volume`/`mw_bus_fade` が成功した後に呼ぶ復元キャッシュの更新
    /// (フェードは最終的な目標値〔`target`〕をキャッシュする——再オープン後は
    /// フェードの途中経過ではなく収束後の値へ即座に復元する設計、
    /// `Instance::restore_bus_volumes` 参照)。
    pub fn note_bus_volume(&self, bus: BusId, volume: f32) {
        self.bus_volumes[bus.index()].store(volume.to_bits(), Ordering::Relaxed);
    }

    /// `mw_poll_events` の drain コールバックから、読み出した各イベントについて呼ぶ。
    /// `reason == DeviceUnavailable` のときだけ内部再オープン(案A)の対象として記録する
    /// (`crate::reopen::ReopenPolicy::mark_pending`)。**それ以外の reason
    /// (`Reconfigured`/`PermissionDenied`/`Backend`)はここでは何もしない**——
    /// `Reconfigured`(iOS のルート変化)は `mw-backend::ios_interruption` が既に
    /// 独自の経路(`pause()`→`play()`)で扱っており、ここで反応すると二重処理になる
    /// (依頼書「🔴 iOS を壊さないこと」)。
    pub fn note_stream_error(&self, reason: StreamErrorReason) {
        if reason == StreamErrorReason::DeviceUnavailable {
            self.reopen.mark_pending(mw_backend::host_time_ns());
        }
    }

    /// 診断用: 内部再オープンの判定状態を覗く(`(保留中か, 連続失敗回数, 諦めたか)`)。
    /// テスト専用——FFI には公開しない([`crate::reopen::ReopenPolicy`] のフィールドは
    /// すべて private なので、外から状態を確認するにはこの経路が要る)。
    #[cfg(test)]
    pub fn reopen_diagnostics(&self) -> (bool, u32, bool) {
        (
            self.reopen.is_pending(),
            self.reopen.attempts(),
            self.reopen.is_exhausted(),
        )
    }

    /// 直近の `mw_bus_set_volume`/`mw_bus_fade` が設定した目標音量を読む
    /// (`mw_bus_get_volume` の実体。R38 調査用に追加、2026-08-31)。
    ///
    /// <b>ここで返るのは「最後に指定された目標値」であり、音声スレッドのランプが
    /// 実際に収束済みの瞬間値ではない</b>(`note_bus_volume` のドキュメント参照。
    /// フェードの途中経過ではなく収束後の値をキャッシュする設計はもともと M3 の
    /// 再オープン復元キャッシュ用に存在していた)。診断用途(「SE/BGM/マスターの
    /// どれかが意図せず 0 になっていないか」の確認)にはこれで十分——何かが
    /// `mw_bus_set_volume(.., 0.0)`/`mw_bus_fade(.., 0.0, ..)` を呼んでいれば、
    /// 呼ばれた直後にこの値も 0 になる。`bus_volumes` フィールドは private なので、
    /// 外から読むにはこの経路が要る。
    pub fn bus_volume(&self, bus: BusId) -> f32 {
        f32::from_bits(self.bus_volumes[bus.index()].load(Ordering::Relaxed))
    }

    /// 診断用: 再オープン復元キャッシュの楽曲ループ区間を覗く。テスト専用。
    #[cfg(test)]
    pub fn music_loop_for_test(&self) -> Option<(u64, u64)> {
        *self
            .last_music_loop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// デコードスレッド(楽曲・BGM の両方)を停止・join する。`mw_shutdown` が使う
    /// (元は `shutdown` 内に直接書かれていたが、下記 `Instance::begin_reopen` の
    /// 「段1」相当の処理が同じ手順を必要としていたためここへ切り出した経緯がある)。
    ///
    /// 🔴 P3-11(2026-09-23)以降、再オープン(`begin_reopen`)自身はこのメソッドを
    /// 呼ばない——停止・join はロックを手放した「段2」(`crate::handle::
    /// run_reopen_worker`)がワーカースレッド上で直接行う(レジストリの `Mutex` を
    /// 握ったまま `join`(待ち時間に上限が無い)を行わないため)。`begin_reopen` は
    /// `decode_thread`/`bgm_decode_thread` の `JoinHandle` を `take()` するだけに
    /// とどめ、実際の `store(true)`+`join()` は段2側で行う。
    fn stop_and_join_decode_threads(&mut self) {
        self.decode_thread_stop.store(true, Ordering::Relaxed);
        if let Some(join_handle) = self.decode_thread.take() {
            // デコードスレッド内部は catch_unwind で panic を握りつぶす設計
            // (`crate::decode_thread` 参照)なので、ここでの `Err`(パニック伝播)は
            // 理論上起こらない。万一起きても続行する。
            let _ = join_handle.join();
        }
        self.bgm_decode_thread_stop.store(true, Ordering::Relaxed);
        if let Some(join_handle) = self.bgm_decode_thread.take() {
            let _ = join_handle.join();
        }
    }

    /// 案A(初期構築仕様『§6』確定): ミドルウェア内部でストリームを再オープンする、
    /// その「段1」(`crate::handle::maybe_reopen` がレジストリの `Mutex` を握ったまま
    /// 呼ぶ)。
    ///
    /// 🔴 **P3-11(2026-09-23)で3段構成へ分割した設計の入口。** 元の `attempt_reopen`
    /// は「close → デコードスレッド2本 join(待ち時間に上限無し)→
    /// `Renderer::build_with_events` → `backend.open`(cpal のデバイスオープン。
    /// 数百 ms かかりうる)→ `SymphoniaDecoder::open_shared`(デコード probe)」を
    /// **レジストリの `Mutex` を握ったまま**通しで実行しており、その間は他のあらゆる
    /// FFI 呼び出し(`with_instance` 経由)がゲームスレッドで足止めされていた
    /// (初期構築仕様『§5.4』「全関数非ブロッキング」への最悪の違反、
    /// `docs/plans/REFACTOR-PLAN.md` P3-11 参照)。設計はオーケストレータが決定済み
    /// (`docs/handoff/operations.md` 決定表): **再オープン中の API 呼び出しは
    /// 「成功を返して捨てる」**(専用エラーは作らない・ブロックしない)。
    ///
    /// ```text
    /// 段1(このメソッド。ロック保持中)
    ///   → 復元に要るスナップショットを読み取る
    ///   → 重い部品(`backend`/デコードスレッドの JoinHandle)を Instance から
    ///     切り離す(`self.backend` は新品の未オープンバックエンドに、
    ///     `self.decode_thread`/`self.bgm_decode_thread` は None になる)
    ///   → [`ReopenDetached`] を返す(呼び出し元がロックを手放してから
    ///     `crate::handle::run_reopen_worker` へ渡す)
    /// 段2(ワーカースレッド上。ロック無し。`run_reopen_worker` 参照)
    ///   → 切り離した旧バックエンド・旧デコードスレッドを畳む
    ///   → 新しい Renderer 一式を組み立て、新バックエンドを開く
    /// 段3(`finalize_reopen_success`/`finalize_reopen_failure`。再度ロック保持中)
    ///   → できた一式を Instance へ差し込み、状態を復元する
    ///   → ただし **Instance がまだ同じもの(`handle` が一致する)場合に限る**
    ///     (段2 の間に `mw_shutdown` が来ていれば、差し込まずに自分で畳む)
    /// ```
    ///
    /// **この間、`Instance` はレジストリに存在し続ける。** これが「25箇所の FFI 呼び
    /// 出しサイトを1つも変えずに済む」ための鍵——`with_instance` は段2の間も同じ
    /// `Instance` を見つけ、`f(instance)` を呼べる:
    /// - getter 系は `self.backend`(段2の間は新品の未オープンバックエンド)を読む
    ///   だけなので、[`Backend`] トレイトが元々持つ「未オープンなら 0」の契約
    ///   (`crates/mw-backend/src/backend.rs` の各メソッド doc)にそのまま乗る——
    ///   新しいエラーコードもパニックも要らない。
    /// - コマンド系は `self.command_sender`(段2の間は畳まれつつある旧 `Renderer` の
    ///   受信側を指す、いずれ受信側ごと破棄される)へ積むだけなので、rtrb の
    ///   `Producer::push` は相手が消えていても即座にはエラーにならず(容量に余裕が
    ///   ある限り成功を返す)、`Ok` を返しつつ実際には誰にも読まれず捨てられる——
    ///   まさに「成功を返して捨てる」がそのまま実現される。
    ///
    /// ⚠️ **例外1箇所**: [`Instance::backend_sample_rate`] は「未オープンなら 0」を
    /// そのまま返すと `mw_sound_load` のリサンプル判定(初期構築仕様『§4.7』)を壊す
    /// ため、直近の実測値へフォールバックする専用の対処を入れてある(同メソッドの
    /// ドキュメント参照)。
    /// ⚠️ **例外2箇所目(軽微、実装時に判明)**: `mw_music_set`/`mw_bgm_set` が使う
    /// `decoder_tx`/`bgm_decoder_tx`(`mpsc::Sender`)は rtrb と異なり、受信側
    /// (旧デコードスレッド)が段2で実際に `join()` され切った後は `send` が
    /// `Err`(切断)を返す——`send_decoder_for` はこれを `false` として伝播し、
    /// `mw_music_set`/`mw_bgm_set` は `MwResult::ErrCommandQueueFull` を返す。
    /// **この挙動は P3-11 が新たに持ち込んだものではない**——同じことは今回の分割
    /// 以前から、再オープンが**失敗**した場合の「次の自動再試行までの待ち時間」の
    /// 間にも既に起きていた(失敗時は `self.decoder_tx` を新しいものへ差し替えない
    /// ため、旧デコードスレッドが止まった後は送信が恒久的に失敗し続ける——本 diff
    /// 以前の `attempt_reopen` の失敗アームを参照)。今回変わったのは「その隙間が
    /// 段2 の間(数百 ms)にも新たに生じる」ことだけで、性質(ブロックしない・
    /// 新しいエラーコードを増やさない・SE 欠け同様に許容範囲内の一時的な取りこぼし)
    /// は変わらないと判断し、依頼された設計をそのまま実装した。専用の対処
    /// (例: 段2 の間だけ「決して読まれないが解放もされない」ダミー受信先へ差し替える
    /// 等)も検討したが、対症療法が増えるだけで却って複雑になるため見送った——
    /// 詳細はこの作業の報告に記載。
    ///
    /// ## 復元できる状態・できない状態(段3 `finalize_reopen_success` が行う)
    ///
    /// - **楽曲の再生位置**(いちばん重要): [`ReopenSnapshot`] が持つ `song_frames`/
    ///   `state` を [`Instance::restore_music`] が
    ///   `MusicPrepare`→`MusicSeek`→(再生中だったときのみ)`MusicPlayScheduled` として
    ///   再送する。§4.3 で元から用意されているコマンド列(`mw_music_set` と同じ順序
    ///   厳守)を再利用しているだけで、専用の復元経路を新設してはいない。
    /// - **バス音量**: `bus_volumes` キャッシュから4本とも `SetBusVolume` を再送する
    ///   ([`Instance::restore_bus_volumes`])。
    /// - **ロード済みの `SoundId`**: `sounds`/`music_bytes` はこの一連の処理が一切
    ///   触れないストレージ(`Instance` 直下に元々ある。`Renderer`/`Mixer` の外側)
    ///   なので、無条件に有効なまま残る——追加の復元処理は不要。
    /// - **ループ設定**: `last_music_loop`/`last_bgm_loop` キャッシュから
    ///   `MusicSetLoop`/`BgmSetLoop` を再送する。
    /// - **効果音の発音**: 復元しない(依頼書「捨ててよい」)。`VoicePool` は
    ///   `Renderer::build_with_events` によって丸ごと新規生成されるため、鳴っていた
    ///   SE ボイスは失われる。理由: 発音は短命(初期構築仕様『§4.2』)で、かつ
    ///   「何が鳴っていたか」を再現するには発音時刻・残り再生位置まで追跡する
    ///   専用の状態が要り、投資に見合わないと判断した。
    /// - **BGM の再生位置**: 復元しない(既知の制約)。`mw_core::BgmStatePublisher`
    ///   は状態(`MusicState`)だけを公開する設計で、そもそもフレーム位置を読み出す
    ///   経路が無い(M14「BGM はクロックを持たない」——位置を公開する仕組み自体が
    ///   無い)。ロード済みトラック・ループ区間は復元するが、**再生中だった場合も
    ///   自動では再生を再開しない**(Ready で止める)。理由: BGM の再生開始
    ///   (`Command::BgmPlay`)は `MusicVoice::play()` が `Ready` 状態でのみ有効という
    ///   即時 API で、楽曲側の `MusicPlayScheduled`(プリロール未完了なら自動的に
    ///   繰り下げる仕組み、§4.3)に相当する「準備完了を待ってから発火する」経路が
    ///   BGM には無い。ここでゲームスレッドをブロックして `Ready` になるまで
    ///   スピン待機する実装も検討したが、再オープンという1回きりの処理のために
    ///   新しい待ち合わせパターンを持ち込むほどの価値は無いと判断し、見送った——
    ///   BGM はメタ画面のループ BGM 用途(初期構築仕様『§2』M14)であり、無音に
    ///   戻ってもゲーム進行(判定)には影響しない。クライアント側が `mw_bgm_state`
    ///   をポーリングして `Ready` を検知したら `mw_bgm_play` を呼び直せば復帰できる。
    ///
    /// ## 再オープンに失敗したとき
    ///
    /// 1回試すだけで、結果を [`ReopenPolicy::record_result`]([`finalize_reopen_failure`]
    /// 内)に記録する。**無限リトライはしない**——呼び出し元(`handle_registry::
    /// maybe_reopen`)は `ReopenPolicy` のバックオフに従い間隔を空けて再試行し、既定
    /// [`crate::reopen::REOPEN_BACKOFF_SCHEDULE_MS`] を使い切ったら自動での再試行を
    /// 諦める(`crate::reopen` モジュール doc「無限リトライを禁止する」参照)。
    /// 🔴 `default_output_device()` が切断直後に使える保証は無い(実機でしか確認
    /// できない)——失敗した場合も `Instance` は次の呼び出しでまた試せる一貫した
    /// 状態のままになる(`self.backend` は単に「開いていない」状態、既存の
    /// `mw_get_output_latency_ns` 等は 0/既定値を返すだけでパニックしない)。
    ///
    /// ## 結果の C# への通知(`docs/history/04-2026-08-31.md`「再オープンを諦めたことを
    /// C# 側へ通知できるようにする」)
    ///
    /// 成功時・「諦めた」時のどちらも [`Instance::notify_reopen_outcome`] が
    /// `Event::AudioInterruptionEnded { recovered }`(既存のイベント種別を転用。
    /// 新種別は追加していない——理由は `mw_core::Event::AudioInterruptionEnded` の
    /// ドキュメント参照)を積む。**中間の失敗(まだバックオフの途中)では積まない**——
    /// 通知するのは「これ以上自動では回復しない」ことが確定した瞬間(バックオフを
    /// 使い切った瞬間)と、実際に音が戻った瞬間だけに絞ってある(そうしないと
    /// バックオフ中の一時的な失敗のたびにイベントが積まれ、C# 側から見て
    /// 「まだ試行中なのか、もう諦めたのか」が読み取りにくくなる)。
    ///
    /// 🔴 **受け入れ確認は実機が要る**(Android の AAudio 切断→復帰)。
    /// デスクトップの `cargo test` では「(段2 の間)固まらないこと」も
    /// 「実機で実際に音が復帰すること」も測れない——CI にはデバイスが無く
    /// `backend.open()` が即座に失敗して終わるため、いま直したい待ち時間そのものが
    /// 発生しない(`crate::reopen` モジュール doc、`docs/plans/REFACTOR-PLAN.md`
    /// 「P3-11」参照)。
    fn begin_reopen(&mut self, now_ns: u64) -> ReopenDetached {
        // 1) セッションを壊す前に、復元に要る情報をすべて読み取っておく。
        let clock_before = self.music_clock.snapshot();
        let bgm_state_before = self.bgm_state.read();
        let saved_music_sound_id = {
            let id = self.last_music_sound_id.load(Ordering::Relaxed);
            (id != 0).then_some(id)
        };
        let saved_music_loop = *self
            .last_music_loop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved_bgm_sound_id = {
            let id = self.last_bgm_sound_id.load(Ordering::Relaxed);
            (id != 0).then_some(id)
        };
        let saved_bgm_loop = *self
            .last_bgm_loop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved_bus_volumes: [f32; BUS_COUNT] =
            std::array::from_fn(|i| f32::from_bits(self.bus_volumes[i].load(Ordering::Relaxed)));

        mw_backend::mw_log!(
            "[mw-ffi] attempting internal stream reopen (attempt={}, music_state={:?}, \
             music_frames={}, bgm_state={:?})",
            self.reopen.attempts() + 1,
            clock_before.state,
            clock_before.song_frames,
            bgm_state_before,
        );

        // 2) 重い部品を切り離す(ロックはまだ握ったまま——ここは安い代入・take だけ)。
        //    `self.backend` は新品の未オープンバックエンドに、`decode_thread`/
        //    `bgm_decode_thread` は `None` になる。close・stop・join は一切ここでは
        //    やらない(それをやるのは段2、`run_reopen_worker`)——このメソッドの
        //    ドキュメント「例外」節、および `Backend` トレイトの「未オープンなら 0」
        //    契約により、他の FFI 呼び出しはこの間もクラッシュせず動く。
        let old_backend = std::mem::replace(&mut self.backend, make_backend());
        let old_decode_thread = self.decode_thread.take();
        let old_bgm_decode_thread = self.bgm_decode_thread.take();
        // 停止フラグ自体は複製(Arc::clone)するだけで、`self` 側からは奪わない——
        // 実際に `store(true)` するのは段2 の役目であり、その間に `mw_shutdown` が
        // 割り込んでも(`self` 側の Arc は生きているので)安全に触れる
        // (`stop_and_join_decode_threads` のドキュメント参照)。
        let old_decode_thread_stop = Arc::clone(&self.decode_thread_stop);
        let old_bgm_decode_thread_stop = Arc::clone(&self.bgm_decode_thread_stop);

        ReopenDetached {
            handle: self.handle,
            old_backend,
            old_decode_thread,
            old_decode_thread_stop,
            old_bgm_decode_thread,
            old_bgm_decode_thread_stop,
            events: Arc::clone(&self.events),
            now_ns,
            snapshot: ReopenSnapshot {
                clock_before,
                saved_music_sound_id,
                saved_music_loop,
                saved_bgm_sound_id,
                saved_bgm_loop,
                saved_bus_volumes,
            },
        }
    }

    /// 内部再オープン(案A)の最終結果を C# 側へ通知する
    /// ([`Instance::begin_reopen`] のドキュメント「結果の C# への通知」参照)。
    ///
    /// **既存の `Event::AudioInterruptionEnded { recovered }` を転用する**(新しい
    /// イベント種別は追加していない——`mw_core::Event::AudioInterruptionEnded` の
    /// ドキュメント参照)。呼び出し元(段3、`finalize_reopen_success`/
    /// `finalize_reopen_failure`)は必ずゲームスレッドから呼ばれる(初期構築仕様
    /// 『§5.3』の対象外)ので、`cpal` のエラーコールバックが `StreamError` を積むのと
    /// 同じ [`EventQueue::push_side_channel`] を使う(`push_realtime` は音声スレッド
    /// 専用の単一書き手前提のため、ここでは使えない/使う必要が無い)。
    fn notify_reopen_outcome(&self, recovered: bool) {
        self.events
            .push_side_channel(mw_core::Event::AudioInterruptionEnded { recovered });
    }

    /// 4本のバスの音量を再送する(段3 `finalize_reopen_success` が呼ぶ)。
    fn restore_bus_volumes(&self, volumes: &[f32; BUS_COUNT]) {
        for bus in mw_core::ALL_BUSES {
            let volume = volumes[bus.index()];
            let _ = self
                .command_sender
                .send(Command::SetBusVolume { bus, volume });
        }
    }

    /// 楽曲ボイスの状態を再送する(段3 `finalize_reopen_success` が呼ぶ)。
    fn restore_music(
        &self,
        sound_id: Option<u64>,
        clock_before: &MusicClockSnapshot,
        loop_region: Option<(u64, u64)>,
    ) {
        let Some(sound_id) = sound_id else {
            return; // 一度も mw_music_set が呼ばれていない。復元するものが無い。
        };
        let Some(bytes) = self.get_music_bytes(sound_id) else {
            // 再オープンの間に mw_sound_release されていた(呼び出し側が明示的に
            // 手放した)。復元を諦める——存在しない音を捏造しない。
            mw_backend::mw_log!(
                "[mw-ffi] attempt_reopen: cannot restore music, sound_id {sound_id} is no \
                 longer loaded"
            );
            return;
        };
        let output_sample_rate = self.backend_sample_rate();
        // 🔴 複製しない(P3-13)。`get_music_bytes` が返す `Arc` をそのまま渡す。
        let decoder = match SymphoniaDecoder::open_shared(bytes, output_sample_rate) {
            Ok(decoder) => decoder,
            Err(err) => {
                mw_backend::mw_log!("[mw-ffi] attempt_reopen: re-decoding music failed: {err}");
                return;
            }
        };
        if !self.send_decoder(Box::new(decoder)) {
            return;
        }
        // `mw_music_set` と同じ順序厳守(デコーダ差し替え→Prepare→Seek。
        // `ffi.rs::mw_music_set` のドキュメント「曲の切り替えで前曲の PCM が漏れる
        // 問題への対処」参照)。
        let _ = self.command_sender.send(Command::MusicPrepare);
        let _ = self.command_sender.send(Command::MusicSeek {
            frames: clock_before.song_frames,
        });
        if let Some(region) = loop_region {
            let _ = self.command_sender.send(Command::MusicSetLoop {
                region: Some(region),
            });
        }
        if clock_before.state == MusicState::Playing {
            // 準備完了(Ready)を待たずに予約すれば、既存の「プリロール未完了時は
            // 繰り下げる」仕組み(`mw_core::mixer::MusicSchedule`, 初期構築仕様
            // 『§4.3』)が、Ready になった時点で自動的に発音してくれる——専用の
            // 待ち合わせは不要。
            let _ = self.command_sender.send(Command::MusicPlayScheduled {
                host_time_ns: mw_backend::host_time_ns(),
            });
        }
        // `Paused` だった場合は位置だけ復元し、明示的な再生は行わない(`Ready` の
        // まま止まる)。ネイティブ側だけで `Paused` 状態そのものを再現する経路は
        // 用意していない——`mw_music_resume_at`/`mw_music_pause` を呼べる状態
        // (`Ready`)まで戻すところまでが復元の範囲、という割り切り。
    }

    /// BGM ボイスの状態を再送する(段3 `finalize_reopen_success` が呼ぶ)。
    ///
    /// **BGM の再生位置・再生中だったかどうかは復元しない**(`Instance::begin_reopen`
    /// のドキュメント「復元できる状態・できない状態」参照)。
    /// ロード済みトラックとループ区間だけを復元し、`Ready` で止める。
    fn restore_bgm(&self, sound_id: Option<u64>, loop_region: Option<(u64, u64)>) {
        let Some(sound_id) = sound_id else {
            return;
        };
        let Some(bytes) = self.get_music_bytes(sound_id) else {
            mw_backend::mw_log!(
                "[mw-ffi] attempt_reopen: cannot restore bgm, sound_id {sound_id} is no longer \
                 loaded"
            );
            return;
        };
        let output_sample_rate = self.backend_sample_rate();
        // 🔴 複製しない(P3-13)。`get_music_bytes` が返す `Arc` をそのまま渡す。
        let decoder = match SymphoniaDecoder::open_shared(bytes, output_sample_rate) {
            Ok(decoder) => decoder,
            Err(err) => {
                mw_backend::mw_log!("[mw-ffi] attempt_reopen: re-decoding bgm failed: {err}");
                return;
            }
        };
        if !self.send_bgm_decoder(Box::new(decoder)) {
            return;
        }
        let _ = self.command_sender.send(Command::BgmPrepare);
        let _ = self.command_sender.send(Command::BgmSeek { frames: 0 });
        if let Some(region) = loop_region {
            let _ = self.command_sender.send(Command::BgmSetLoop {
                region: Some(region),
            });
        }
    }
}

// --- P3-11(2026-09-23): 3段構成の再オープン ------------------------------------
//
// `Instance::begin_reopen`(段1)/`run_reopen_worker`(段2)/`finalize_reopen_success`・
// `finalize_reopen_failure`(段3)の3つに分割した経緯・全体設計は
// `Instance::begin_reopen` のドキュメント参照。ここでは「段の間で受け渡す入れ物」
// (`ReopenSnapshot`/`ReopenDetached`)と、段2 の重複起動を防ぐ進行中フラグを定義する。

/// 段1(ロック保持中)で読み取り、段3(再度ロック保持中)で実際に使う、復元用の
/// スナップショット。`Instance::begin_reopen`/`Instance::restore_music` 参照。
struct ReopenSnapshot {
    /// 再オープン前の音楽クロック(位置・世代・状態)。[`Instance::restore_music`] が
    /// `song_frames`/`state` を、[`Instance::begin_reopen`] のログが `state`/
    /// `song_frames` を、それぞれ使う。
    clock_before: MusicClockSnapshot,
    /// 直近の `mw_music_set` が指定した楽曲 ID(`Instance::last_music_sound_id` の
    /// 段1 時点の複製。`None` = 一度も呼ばれていない)。
    saved_music_sound_id: Option<u64>,
    /// 直近の `mw_music_set_loop` が設定したループ区間の複製。
    saved_music_loop: Option<(u64, u64)>,
    /// `saved_music_sound_id` の BGM 版。
    saved_bgm_sound_id: Option<u64>,
    /// `saved_music_loop` の BGM 版。
    saved_bgm_loop: Option<(u64, u64)>,
    /// 4バスの直近の目標音量の複製。
    saved_bus_volumes: [f32; BUS_COUNT],
}

/// 段1 が `Instance` から切り離し、段2(ワーカースレッド)へ渡す一式。
/// [`Instance::begin_reopen`] のドキュメント参照。
struct ReopenDetached {
    /// 段3 が「この再オープンの対象だった `Instance` がまだ同じものか」を判定する
    /// ためのキー(`guard.as_ref().map(|i| i.handle)` と比較する)。
    handle: u64,
    /// 切り離した旧バックエンド(段2 が `close()` する)。
    old_backend: Box<dyn Backend + Send>,
    /// 切り離した旧デコードスレッドの `JoinHandle`(段2 が停止フラグを立てたうえで
    /// join する)。`None` はここには来ない想定だが、`Instance::stop_and_join_decode_threads`
    /// と同じ「念のための `Option`」の流儀に合わせておく。
    old_decode_thread: Option<JoinHandle<()>>,
    /// 旧デコードスレッドの停止フラグ(`Instance::decode_thread_stop` の複製。
    /// `Instance` 側にも同じ `Arc` が残っているため、段2 と `mw_shutdown` の両方が
    /// 同時に `store(true)` しても問題ない——冪等な操作のため)。
    old_decode_thread_stop: Arc<AtomicBool>,
    /// `old_decode_thread` の BGM 版。
    old_bgm_decode_thread: Option<JoinHandle<()>>,
    /// `old_decode_thread_stop` の BGM 版。
    old_bgm_decode_thread_stop: Arc<AtomicBool>,
    /// `mw_poll_events` が読み出す先のイベントキュー。identity を変えないまま
    /// 新しい `Renderer`/`CpalBackend` へも同じ `Arc` を渡す
    /// (`Renderer::build_with_events` のドキュメント参照)。
    events: Arc<EventQueue>,
    /// 段1(`maybe_reopen`)が読んだホスト時刻。段3の `ReopenPolicy::record_result` へ
    /// そのまま渡す(バックオフの起点をブレさせないため、段2の所要時間ぶんズレる
    /// ことを許容しつつも、少なくとも「いつ試行を始めたか」を基準にする)。
    now_ns: u64,
    /// 復元用スナップショット。
    snapshot: ReopenSnapshot,
}

/// 内部再オープン(P3-11)の「段2」(`run_reopen_worker`)が現在進行中かどうか。
///
/// **レジストリの `Mutex` を取らずに読めることが要る**——`maybe_reopen` はこのフラグを
/// 安価に確認してから初めてレジストリのロックを取りに行く(`Instance` のフィールドに
/// しなかった理由も同じ: レジストリの `Mutex` を取らないと `Instance` へは触れないため、
/// フィールドにすると「ロックを取らずに読む」という目的そのものが果たせない)。
///
/// `Release`(下ろす)/`Acquire`(読む)で十分——このフラグ自体は他のどのメモリも
/// 保護していない(実データは引き続きレジストリの `Mutex` が保護する)。役割は
/// 「段2 を二重に起動しない」という一回性の判定のみで、`SeqCst` ほど強い順序保証は
/// 要らない。`maybe_reopen` がフラグを立てる操作はレジストリの `Mutex` を握った
/// ままなので、その1箇所は普通の `store` で足りる(競合しない)。
static REOPEN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// 診断・テスト用: 内部再オープンの段2が現在進行中かどうかを覗く。
pub fn is_reopen_in_progress() -> bool {
    REOPEN_IN_PROGRESS.load(Ordering::Acquire)
}

/// [`REOPEN_IN_PROGRESS`] を確実に解除する RAII ガード。段2 のワーカースレッド上で
/// スコープの先頭から保持する。
///
/// **フラグを「立てる」側はこのガードの役目ではない**——`maybe_reopen` が既に
/// レジストリの `Mutex` を握ったまま `REOPEN_IN_PROGRESS.store(true, ..)` した後で
/// ワーカースレッドへ移る設計のため、ここでは「下ろす」ことだけに専念させる。
///
/// 🔴 このガードが要る理由: このワークスペースは `panic = "unwind"` のまま
/// (`Cargo.toml` のコメント参照)なので、段2の内部(`run_reopen_worker`)で万一
/// パニックが起きても、スタック巻き戻しの過程でこの `Drop` は必ず走り、フラグは
/// 解除される。`mw_poll_events` を包む `catch_unwind` は**ゲームスレッド側**の
/// 保護であり、この独立したワーカースレッド内のパニックまでは捕まえられないため、
/// この保証はそれとは別に必要——ここが無いと、フラグが `true` のまま永久に残り、
/// `maybe_reopen` が以後ずっと「進行中」と誤認して二度と再オープンを試みなくなる
/// (`ReopenPolicy` 自体はバックオフを使い切っていなくても、この誤認だけで
/// 実質的に「詰む」)。
///
/// 📌 対象の `AtomicBool` を `&'static` 参照として持つ(グローバルな
/// [`REOPEN_IN_PROGRESS`] を直接名指ししない)——`cargo test` は既定で複数テストを
/// 同一プロセス内で並行実行するため、この Drop 挙動そのものを単体テストしたい場合に
/// 実際のグローバルフラグを弄ると、たまたま並走している本物の再オープン
/// (`crate::ffi` の実機越し統合テスト)を誤って巻き込みかねない
/// (`tests::reopen_in_progress_guard_clears_the_flag_even_when_the_scope_panics`
/// がテスト専用の `static` を使ってこの依存を断ち切っている理由)。
struct ReopenInProgressGuard(&'static AtomicBool);

impl ReopenInProgressGuard {
    /// 本番用のコンストラクタ: グローバルフラグ [`REOPEN_IN_PROGRESS`] を対象にする。
    fn new() -> Self {
        Self(&REOPEN_IN_PROGRESS)
    }
}

impl Drop for ReopenInProgressGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// 段2: ロックを持たないワーカースレッド上で、実際に重い処理
/// (close→join→`Renderer::build_with_events`→`open`→デコードスレッド起動)を行う。
///
/// `Instance::begin_reopen`(段1)が呼び出し元(`maybe_reopen`)から渡された
/// [`ReopenDetached`] を消費する。成功・失敗いずれの結果も、レジストリのロックを
/// 再度取る段3([`finalize_reopen_success`]/[`finalize_reopen_failure`])へ引き継ぐ。
fn run_reopen_worker(detached: ReopenDetached) {
    let ReopenDetached {
        handle,
        mut old_backend,
        old_decode_thread,
        old_decode_thread_stop,
        old_bgm_decode_thread,
        old_bgm_decode_thread_stop,
        events,
        now_ns,
        snapshot,
    } = detached;

    // 1) 現在のセッションを畳む。AAudio の切断は API 契約上すでに終端的
    //    (`ndk::audio::AudioError::Disconnected` のドキュメント。
    //    `docs/history/03-2026-08-31.md` 参照)なので、明示的に close しなくても
    //    もう鳴っていない——ここでの close は状態を明示的に確定させる後始末に
    //    過ぎない。失敗してもベストエフォートで続行する。
    if let Err(err) = old_backend.close() {
        mw_backend::mw_log!("[mw-ffi] attempt_reopen: backend close failed (ignored): {err}");
    }
    // 🔴 ここが今回の分割の核心: この `join()`(待ち時間に上限が無い)はもう
    // レジストリの `Mutex` を握っていない状態で行われる——他の FFI 呼び出しを
    // 巻き込まない(初期構築仕様『§5.4』違反の解消)。
    old_decode_thread_stop.store(true, Ordering::Relaxed);
    if let Some(join_handle) = old_decode_thread {
        let _ = join_handle.join();
    }
    old_bgm_decode_thread_stop.store(true, Ordering::Relaxed);
    if let Some(join_handle) = old_bgm_decode_thread {
        let _ = join_handle.join();
    }

    // 2) 新しい Renderer 一式を組み立てる。`events`(`Arc<EventQueue>`)の identity は
    //    変えない——`mw_poll_events` が読み出す先を差し替える必要が無いようにする
    //    (`mw_core::renderer::Renderer::build_with_events` のドキュメント参照)。
    let (renderer, command_sender, reclaim_receiver, music_producer, music_clock, _events, bgm) =
        Renderer::build_with_events(
            Config::default(),
            PROVISIONAL_SAMPLE_RATE,
            Arc::clone(&events),
        );

    // 3) 🔴 generation を必ず bump する。新しいクロックは generation=0 から始まる
    //    ため、直前のクロックが最後に観測させた値と偶然一致しうる——
    //    `wrapping_add(1)` した値でシードすることで「直前に観測されたどの値とも
    //    異なる」ことを保証する
    //    (`MusicClockPublisher::seed_generation_after_reopen` のドキュメント参照)。
    music_clock.seed_generation_after_reopen(snapshot.clock_before.generation.wrapping_add(1));

    // 🔴 `renderer` を `Backend::open` へムーブする**前に**、予約 SE オーバーフローの
    //    カウンタを取っておく(ムーブ後は `Mixer` へ触れない)。
    //    ⚠️ ここで取り忘れると、再オープン後は**誰も増やさない古いカウンタ**を
    //    読み続けることになり、診断値が「ずっと 0」に見える。
    let se_schedule_overflow_count = renderer.se_schedule_overflow_counter();

    // 4) 実際にバックエンドを開き直す(cpal のデバイスオープン。数百 ms かかりうる。
    //    ここもロック無しで行われる——今回の分割の目的そのもの)。
    let mut new_backend = make_backend();
    match new_backend.open(renderer, Arc::clone(&events)) {
        Ok(()) => {
            let (decoder_tx, decode_thread_stop, decode_thread_handle) =
                decode_thread::spawn(music_producer, Arc::clone(&events));
            let (bgm_decoder_tx, bgm_decode_thread_stop, bgm_decode_thread_handle) =
                decode_thread::spawn(bgm.stream_producer, Arc::clone(&events));

            finalize_reopen_success(
                handle,
                new_backend,
                command_sender,
                reclaim_receiver,
                music_clock,
                bgm.state,
                decoder_tx,
                decode_thread_stop,
                decode_thread_handle,
                bgm_decoder_tx,
                bgm_decode_thread_stop,
                bgm_decode_thread_handle,
                se_schedule_overflow_count,
                snapshot,
                now_ns,
            );
        }
        Err(err) => {
            mw_backend::mw_log!("[mw-ffi] internal stream reopen failed: {err}");
            finalize_reopen_failure(handle, now_ns);
        }
    }
}

/// 段3(成功): レジストリのロックを再度取り、段2 が組み立てた一式を差し込む。
///
/// 🔴 **`Instance` が段2 の間に消えている/差し替わっている可能性がある**
/// (`mw_shutdown` が割り込んだ、または shutdown→init で全く別のインスタンスに
/// なった)。`handle` の一致で判定し、一致しなければレジストリには一切触れず、
/// ここで組み立てた一式を自分で畳んで捨てる(スレッドリーク・二重使用を防ぐ)。
#[allow(clippy::too_many_arguments)]
fn finalize_reopen_success(
    handle: u64,
    new_backend: Box<dyn Backend + Send>,
    command_sender: CommandSender,
    reclaim_receiver: ReclaimReceiver,
    music_clock: Arc<MusicClockPublisher>,
    bgm_state: Arc<BgmStatePublisher>,
    decoder_tx: DecoderSender,
    decode_thread_stop: Arc<AtomicBool>,
    decode_thread_handle: JoinHandle<()>,
    bgm_decoder_tx: DecoderSender,
    bgm_decode_thread_stop: Arc<AtomicBool>,
    bgm_decode_thread_handle: JoinHandle<()>,
    se_schedule_overflow_count: Arc<AtomicU64>,
    snapshot: ReopenSnapshot,
    now_ns: u64,
) {
    let mut guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let still_ours = guard
        .as_ref()
        .is_some_and(|instance| instance.handle == handle);
    if !still_ours {
        drop(guard);
        mw_backend::mw_log!(
            "[mw-ffi] internal stream reopen: instance is gone or was replaced during the \
             reopen; discarding the newly (re)opened backend and decode threads without \
             touching the registry"
        );
        teardown_orphaned_reopen(
            new_backend,
            decode_thread_stop,
            decode_thread_handle,
            bgm_decode_thread_stop,
            bgm_decode_thread_handle,
        );
        return;
    }

    // `expect`: 直前の `still_ours` チェックで `Some` かつ `handle` 一致を確認済み。
    let instance = guard.as_mut().expect("checked Some above");
    instance.backend = new_backend;
    instance.command_sender = command_sender;
    instance.reclaim_receiver = Mutex::new(reclaim_receiver);
    instance.music_clock = music_clock;
    instance.bgm_state = bgm_state;
    instance.decoder_tx = decoder_tx;
    instance.decode_thread_stop = decode_thread_stop;
    instance.decode_thread = Some(decode_thread_handle);
    instance.bgm_decoder_tx = bgm_decoder_tx;
    instance.bgm_decode_thread_stop = bgm_decode_thread_stop;
    instance.bgm_decode_thread = Some(bgm_decode_thread_handle);
    instance.se_schedule_overflow_count = se_schedule_overflow_count;
    // 新しいセッション用に「1回だけ」ログのフラグも仕切り直す(そうしないと
    // 新しいストリームの実測値〔I/O バッファ長〕が二度とログされない)。
    instance.logged_buffer_info.store(false, Ordering::Relaxed);
    // `backend_sample_rate()` のフォールバック用キャッシュを更新する
    // (`Instance::backend_sample_rate` のドキュメント参照)。
    instance
        .last_known_sample_rate
        .store(instance.backend.sample_rate(), Ordering::Relaxed);

    // 状態を復元する(`Instance::begin_reopen` のドキュメント「復元できる状態」参照)。
    instance.restore_bus_volumes(&snapshot.saved_bus_volumes);
    instance.restore_music(
        snapshot.saved_music_sound_id,
        &snapshot.clock_before,
        snapshot.saved_music_loop,
    );
    instance.restore_bgm(snapshot.saved_bgm_sound_id, snapshot.saved_bgm_loop);

    mw_backend::mw_log!("[mw-ffi] internal stream reopen succeeded");
    instance.reopen.record_result(true, now_ns);
    // 🔴 音が戻ったことを C# 側へ通知する(`Instance::notify_reopen_outcome`
    // のドキュメント参照)。
    instance.notify_reopen_outcome(true);
}

/// 段3(失敗): レジストリのロックを再度取り、失敗をバックオフ判定へ記録する。
///
/// 段2 で `backend.open()` が失敗した場合。`Instance` が段2の間に消えている/
/// 差し替わっている可能性は成功時と同じ——その場合は記録すべき `Instance` 自体が
/// 無いので、ログだけ残して何もしない。
fn finalize_reopen_failure(handle: u64, now_ns: u64) {
    let mut guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(instance) = guard.as_mut() else {
        mw_backend::mw_log!(
            "[mw-ffi] internal stream reopen failed and the instance is gone; nothing to record"
        );
        return;
    };
    if instance.handle != handle {
        mw_backend::mw_log!(
            "[mw-ffi] internal stream reopen failed and the instance was replaced during the \
             reopen; nothing to record"
        );
        return;
    }

    instance.reopen.record_result(false, now_ns);
    if instance.reopen.is_exhausted() {
        // 依頼書「無限リトライは禁止」: バックオフを使い切ったので、新しい切断
        // イベント(`note_stream_error`)が届くまで自動での再試行を止める
        // (`crate::reopen` モジュール doc 参照)。
        mw_backend::mw_log!(
            "[mw-ffi] internal stream reopen: giving up after {} consecutive failed attempts; \
             will retry automatically only if a new disconnect is observed",
            instance.reopen.attempts()
        );
        // 🔴 諦めたことを C# 側へ通知する(`Instance::notify_reopen_outcome` の
        // ドキュメント参照)。
        instance.notify_reopen_outcome(false);
    }
}

/// 段3 が `Instance` の消失/差し替えを検知したときに、段2 が組み立てた一式
/// (新バックエンド・新デコードスレッド2本)を自分で畳む。レジストリには一切触れない
/// (呼び出し元 [`finalize_reopen_success`] が既にロックを手放した後に呼ぶこと)。
fn teardown_orphaned_reopen(
    mut new_backend: Box<dyn Backend + Send>,
    decode_thread_stop: Arc<AtomicBool>,
    decode_thread_handle: JoinHandle<()>,
    bgm_decode_thread_stop: Arc<AtomicBool>,
    bgm_decode_thread_handle: JoinHandle<()>,
) {
    if let Err(err) = new_backend.close() {
        mw_backend::mw_log!("[mw-ffi] reopen teardown: backend close failed (ignored): {err}");
    }
    decode_thread_stop.store(true, Ordering::Relaxed);
    let _ = decode_thread_handle.join();
    bgm_decode_thread_stop.store(true, Ordering::Relaxed);
    let _ = bgm_decode_thread_handle.join();
}

/// ハンドルは 1 から始まる単調増加の不透明 ID。0 は「未割当」を意味する予約値として使わない
/// (呼び出し側が初期化し忘れた `out_handle` を誤ってハンドルだと解釈しにくくするため)。
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

fn registry() -> &'static Mutex<Option<Instance>> {
    static REGISTRY: OnceLock<Mutex<Option<Instance>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(None))
}

/// 現在初期化済みのインスタンスに対して `f` を実行する。無効ハンドルは `None` を返す。
///
/// レジストリの `Mutex` はゲームスレッド側のコードでのみ取得される(§5.3 が禁止するのは
/// 音声スレッド側でのロック取得のみ。ここは FFI 呼び出し = ゲームスレッド経路)。
pub fn with_instance<T>(handle: u64, f: impl FnOnce(&Instance) -> T) -> Option<T> {
    let guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match guard.as_ref() {
        Some(instance) if instance.handle == handle => Some(f(instance)),
        _ => None,
    }
}

/// バックエンドを1つ作る唯一の口。本番は常に [`CpalBackend`]。
///
/// 🔴 テストビルドでのみ、テストダブルを差し込めるようにしてある —— CI にもこの環境にも
/// 実デバイスが無く、`CpalBackend::open()` が必ず失敗するため、再オープンの段2・段3
/// (`run_reopen_worker` / `finalize_reopen_success` / `teardown_orphaned_reopen`)へ
/// 実際に到達させる手段が他に無い(`crates/mw-ffi/src/test_backend.rs` 参照)。
#[cfg(not(test))]
fn make_backend() -> Box<dyn Backend + Send> {
    Box::new(CpalBackend::new())
}

#[cfg(test)]
fn make_backend() -> Box<dyn Backend + Send> {
    crate::test_backend::take_from_factory().unwrap_or_else(|| Box::new(CpalBackend::new()))
}

pub enum InitOutcome {
    /// 新規に出力ストリームを開いた。
    Opened(u64),
    /// 既に開いていたので同一ハンドルを返す(冪等)。
    AlreadyOpen(u64),
    /// バックエンドのオープンに失敗した(デバイス無し環境等)。
    Failed,
}

/// ミドルウェアを初期化する。既に初期化済みなら同一ハンドルを返す(冪等)。
pub fn init() -> InitOutcome {
    let mut guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(instance) = guard.as_ref() {
        return InitOutcome::AlreadyOpen(instance.handle);
    }

    // `events`(イベントキュー)は M2-6 からここで使う(`mw_poll_events` と
    // `CpalBackend::open` の両方へ同じ `Arc` を配る)。`music_stream_producer`
    // (楽曲PCM供給の生産側)と `music_clock`(音楽クロックの読み手)は M2-7 で
    // 実際に使い道ができた——前者はデコードスレッドへムーブし、後者は `Instance`
    // へ保持して `mw_music_get_position`/`mw_music_state` から読む。`bgm`(BGM 専用
    // ハンドル一式)は M4-3 で追加した——`stream_producer` は楽曲とは別のもう1本の
    // デコードスレッドへムーブし、`state` は `Instance` へ保持して `mw_bgm_state` から読む。
    let (
        renderer,
        command_sender,
        reclaim_receiver,
        music_stream_producer,
        music_clock,
        events,
        bgm,
    ) = Renderer::build(Config::default(), PROVISIONAL_SAMPLE_RATE);

    // 🔴 `renderer` を `Backend::open` へムーブする**前に**取る(`run_reopen_worker` と
    // 同じ理由)。
    let se_schedule_overflow_count = renderer.se_schedule_overflow_counter();

    let mut backend = make_backend();
    if let Err(err) = backend.open(renderer, Arc::clone(&events)) {
        // 実機(特に iOS)では失敗理由が分からないと原因を特定できないため、
        // 具体的な BackendError を必ず残す(docs/measurement-m1.md §7.6-1)。
        // MwResult は粒度が粗い(ErrBackendOpenFailed 一種)ので、詳細はこのログが唯一の手がかりになる。
        mw_backend::mw_log!("[mw-ffi] mw_init: backend open failed: {err}");
        // `music_stream_producer` はここで(誰にも渡されないまま)ドロップされる。
        // デコードスレッドはまだ立てていない(下の spawn より前でここへ抜けるため)
        // ので、スレッドリークの心配は無い。
        return InitOutcome::Failed;
    }

    // M2-7: デコードスレッドを1本立てる(`mw_music_set` のたびに立て直さない設計の
    // 理由・待ち方・ポーリング間隔の根拠は `crate::decode_thread` モジュール doc
    // 参照)。**バックエンドのオープンに成功した後でのみ**立てる——先に立ててしまうと、
    // このあと何らかの理由で `Instance` を作らずに抜けた場合(現状はここでしか
    // 早期リターンしないが)、スレッドを止め・join する手段(`decode_thread_stop`/
    // `JoinHandle`)がどこにも保持されないまま孤立してしまう。
    let (decoder_tx, decode_thread_stop, decode_thread_handle) =
        decode_thread::spawn(music_stream_producer, Arc::clone(&events));
    // M4-3: BGM 専用のもう1本のデコードスレッド。`decode_thread::spawn` は
    // 「楽曲用」「BGM 用」を一切区別しない汎用実装なので、独立した
    // `MusicStreamProducer`(`bgm.stream_producer`)を渡すだけでそのまま使い回せる
    // (`crate::decode_thread` モジュール doc 参照)。
    let (bgm_decoder_tx, bgm_decode_thread_stop, bgm_decode_thread_handle) =
        decode_thread::spawn(bgm.stream_producer, Arc::clone(&events));

    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    // P3-11: `backend_sample_rate()` のフォールバック用キャッシュの初期値
    // (`Instance::backend_sample_rate` のドキュメント参照)。`backend.open` は
    // 成功時に必ず `sample_rate` を書き込む(`CpalBackend::open` 実装参照)ため、
    // ここで 0 になることは無い。
    let initial_sample_rate = backend.sample_rate();
    *guard = Some(Instance {
        handle,
        backend,
        command_sender,
        reclaim_receiver: Mutex::new(reclaim_receiver),
        sounds: Mutex::new(SoundStorage::new()),
        events,
        next_voice_serial: AtomicU64::new(1),
        logged_buffer_info: AtomicBool::new(false),
        music_clock,
        music_bytes: Mutex::new(HashMap::new()),
        next_music_serial: AtomicU64::new(1),
        decoder_tx,
        decode_thread_stop,
        decode_thread: Some(decode_thread_handle),
        bgm_state: bgm.state,
        bgm_decoder_tx,
        bgm_decode_thread_stop,
        bgm_decode_thread: Some(bgm_decode_thread_handle),
        se_schedule_overflow_count,
        last_music_sound_id: AtomicU64::new(0),
        last_music_loop: Mutex::new(None),
        last_bgm_sound_id: AtomicU64::new(0),
        last_bgm_loop: Mutex::new(None),
        // `mw_core::bus::Bus::new()` の既定音量(1.0)と揃える。
        bus_volumes: std::array::from_fn(|_| AtomicU32::new(1.0_f32.to_bits())),
        reopen: ReopenPolicy::new(),
        last_known_sample_rate: AtomicU32::new(initial_sample_rate),
    });
    InitOutcome::Opened(handle)
}

pub enum ShutdownOutcome {
    /// 正常に停止・解放した。
    Closed,
    /// バックエンドのクローズ自体に失敗した(レジストリからは外れるため二重解放にはならない)。
    CloseFailed,
    /// ハンドルが無効(未初期化 / 二重 shutdown / 他インスタンスのハンドル)。
    InvalidHandle,
}

/// ミドルウェアを終了する。無効なハンドルはエラーとして検出し、クラッシュしない。
pub fn shutdown(handle: u64) -> ShutdownOutcome {
    let mut guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let is_valid = guard
        .as_ref()
        .is_some_and(|instance| instance.handle == handle);
    if !is_valid {
        return ShutdownOutcome::InvalidHandle;
    }

    // P3-11: 段2(`run_reopen_worker`)がまだ走っている最中に呼ばれた場合、
    // `instance.backend` は既に段1が切り離した新品の未オープンバックエンド
    // (「旧バックエンドは段2のワーカースレッドが握っている」状態)——これは
    // `close()` が失敗する(`BackendError::NotOpen`)が、「本当に閉じ損なった」の
    // ではなく「段2の途中でたまたま検出した」だけなので、`CloseFailed` は誤解を招く。
    // ロックを外す前に判定しておく(`guard.take()` の後では `Instance` 自体が
    // 手元に残るので、順序自体はどちらでもよいが、判断材料は `guard.take()` より
    // 前に確定させておいた方が読みやすい)。
    let reopen_was_in_progress = is_reopen_in_progress();

    // 見つかったインスタンスをレジストリから外す。`close` が失敗しても状態は握ったままに
    // しない(二重 shutdown は次回呼び出しで ErrInvalidHandle として検出されるため)。
    match guard.take() {
        Some(mut instance) => {
            // M2-7: デコードスレッドを確実に停止・join する(`mw_init` で1本だけ
            // 立てたぶん、`mw_shutdown` で確実に回収する。スレッドリーク防止。
            // M4-3 で BGM 専用のもう1本も同じ手順で対称に扱うようになった)。
            // 最大で `decode_thread::POLL_INTERVAL` 分だけこの呼び出しがブロックし
            // うるが、`mw_shutdown` は毎フレーム呼ぶ関数ではない一度きりの終了処理
            // なので、初期構築仕様『§5.4』の「全関数非ブロッキング」が要求する
            // 粒度の対象外とみなす(既存の `backend.close()` も内部でストリームの
            // 停止を同期的に待つ設計になっている)。
            //
            // 🔴 P3-11(2026-09-23): 段2 の間にここへ来た場合、`decode_thread`/
            // `bgm_decode_thread` は既に段1(`Instance::begin_reopen`)が `take()`
            // 済みで `None` なので、この呼び出し自体は(`Option::take` が no-op に
            // なるだけで)安全——旧デコードスレッドの実際の停止・join は段2
            // (`run_reopen_worker`)が独立して行う。ここで二重に join しには行かない。
            instance.stop_and_join_decode_threads();
            match instance.backend.close() {
                Ok(()) => ShutdownOutcome::Closed,
                // 🔴 段2 が進行中だったなら、`self.backend` が「本物の未クローズの
                // バックエンド」ではなく段1が置いた新品の未オープンバックエンド
                // であることが分かっている——旧バックエンドは段2側で既に(または
                // まもなく)`close()` される。ここでの `NotOpen` は「本当に閉じ損なった」
                // ことを意味しないため `Closed` として報告する(段2側は段3で
                // `Instance` が消えたことを検出し、自分が組み立てた新バックエンドを
                // 自分で畳む——`finalize_reopen_success` の「Instance が消えている」
                // 分岐参照)。段2が進行中でないのに `NotOpen` が返る場合(例:
                // 再オープンが**失敗**した直後、次の自動再試行までの待機中)は、
                // これまで通り正直に `CloseFailed` を返す——`self.backend` が
                // 本当に「開けなかった」状態のままだからで、この場合の挙動は
                // P3-11 以前から変えていない(`Instance::begin_reopen` のドキュメント
                // 「再オープンに失敗したとき」参照)。
                // 📌 この「失敗後は `CloseFailed`」は
                // `tests::reopen_failure_is_recorded_and_eventually_gives_up` が
                // 固定化している(2026-09-23。それまで未検証だった)。
                Err(BackendError::NotOpen) if reopen_was_in_progress => ShutdownOutcome::Closed,
                Err(_) => ShutdownOutcome::CloseFailed,
            }
        }
        // 直前の `is_valid` チェックで Some を確認済みのため到達しない防御的分岐。
        None => ShutdownOutcome::InvalidHandle,
    }
}

/// 切断からの内部再オープン(初期構築仕様『§6』案A)を、必要であれば試みる。
///
/// **`mw_poll_events`(ゲームスレッド)のすべての呼び出しから呼ぶ想定。** 通常は
/// [`is_reopen_in_progress`]/`ReopenPolicy::is_due` が `false` を返す安価なチェック
/// だけで即座に戻る(`registry()` のロック取得+数回の atomic load のみ)。
/// `instance.handle != handle`(無効ハンドル)・インスタンス未初期化の場合も静かに
/// 何もしない(`with_instance`/`shutdown` と同じ「クラッシュしない」方針)。
///
/// 🔴 **P3-11(2026-09-23): ここでレジストリの `Mutex` を握るのは「段1」の間だけ**
/// (`Instance::begin_reopen` のドキュメント参照)。実際に重い処理(close→join→
/// `Renderer::build_with_events`→`open`)を行う「段2」はロックを手放した後、
/// 専用のワーカースレッド(`run_reopen_worker`)上で行う——この関数自身は段2の
/// 完了を待たずに戻る(段1が終わり次第すぐ返る、非ブロッキング)。段2・段3の結果は
/// 次回以降の `mw_poll_events`(`reopen_diagnostics`/`mw_poll_events` が積む
/// `Event::AudioInterruptionEnded`)からしか観測できない。
///
/// [`REOPEN_IN_PROGRESS`] は「段2 を二重に起動しない」ための番人——`ReopenPolicy`
/// だけでは足りない: 1回の試行が「保留中(pending)」のまま結果を記録する
/// (`ReopenPolicy::record_result`)のは段3(=段2が終わった後)であり、段2が実行中の
/// 間は `next_earliest_ns` がまだ前回のバックオフのままなので、`is_due` は
/// 依然として `true` を返しうる。このフラグが無いと、次のフレームの
/// `mw_poll_events` が同じ `Instance` に対してもう1本ワーカースレッドを起動し、
/// 2つの段2が同時にバックエンドの close/open を競合させてしまう。
///
/// 🔴 **iOS/tvOS ではこの関数への呼び出し自体が存在しない**——呼び出し元
/// (`crate::ffi::mw_poll_events`)側が `#[cfg(not(any(target_os = "ios", target_os =
/// "tvos")))]` で丸ごと除去している。iOS には実機で確認済みの既存の復帰経路
/// (`mw-backend::ios_interruption`、割り込み・ルート変化からの `pause()`→`play()`)が
/// 既にあり、両者が同じ `Event::StreamError { reason: DeviceUnavailable }` に対して
/// 競合しないようにするため。「どちらの経路を通るか」はこの `cfg` 1箇所だけで
/// コンパイル時に決まる(実行時のヒューリスティックには一切頼らない——依頼書
/// 「⚠️『どちらの経路を通るか』の判定を曖昧にしないこと」への回答)。
pub fn maybe_reopen(handle: u64) {
    // 安価な事前チェック(ロック不要)。既に段2が進行中なら、レジストリの
    // ロックすら取りに行かずに戻る(定常状態のコストを増やさないため)。
    if is_reopen_in_progress() {
        return;
    }

    let detached = {
        let mut guard = registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(instance) = guard.as_mut() else {
            return;
        };
        if instance.handle != handle {
            return;
        }
        let now_ns = mw_backend::host_time_ns();
        if !instance.reopen.is_due(now_ns) {
            return;
        }
        // ロックを握ったまま進行中フラグを確定させる——直前の事前チェックと
        // ここまでの間に他スレッドが割り込む余地は無い(レジストリの `Mutex` を
        // 排他的に握っているため)。これで以後の `maybe_reopen` 呼び出しは
        // 冒頭の事前チェックで弾かれるようになる。
        REOPEN_IN_PROGRESS.store(true, Ordering::Release);
        instance.begin_reopen(now_ns)
    };
    // ここでレジストリのロックは既に手放されている(上のブロックのスコープを
    // 抜けた時点で `guard` が drop 済み)。段2 は専用のワーカースレッド上で行う——
    // `mw_poll_events`(呼び出し元)がこの関数の戻りを待つ間もブロックしない。
    std::thread::spawn(move || {
        // `Drop` がどの終了経路(成功・失敗・パニック)でも進行中フラグを確実に
        // 下ろす([`ReopenInProgressGuard`] のドキュメント参照)。
        let _guard = ReopenInProgressGuard::new();
        run_reopen_worker(detached);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    // Note: これらのテストはグローバルレジストリを共有するため、cpal のデバイス有無に
    // 依存する部分(実際に `Opened` になるかどうか)は環境依存。ここでは
    // 「無効ハンドルの検出」「二重 shutdown の安全性」という契約のみを固定する。

    #[test]
    fn shutdown_with_never_issued_handle_is_invalid() {
        // 十分大きい値を使い、他のテストが払い出した可能性のあるハンドルと衝突しないようにする。
        match shutdown(u64::MAX) {
            ShutdownOutcome::InvalidHandle => {}
            ShutdownOutcome::Closed | ShutdownOutcome::CloseFailed => {
                panic!("unissued handle must not match an existing instance")
            }
        }
    }

    #[test]
    fn with_instance_returns_none_for_unknown_handle() {
        assert!(with_instance(u64::MAX, |_| ()).is_none());
    }

    // --- P3-11(2026-09-23): 内部再オープンの非同期化 -------------------------------
    //
    // ⚠️ ここに無いもの(実機が要る/この環境では検証できない):
    // - 段2(`run_reopen_worker`)が実際に走っている間、他の FFI 呼び出しが
    //   ブロックされないこと自体の実測(「固まらない」ことを測るテストは書けない
    //   ——`crate::reopen` モジュール doc、`docs/plans/REFACTOR-PLAN.md` 参照)。
    // - Android(AAudio)の実機切断→復帰での動作確認。
    // - `mw_music_set`/`mw_bgm_set` が段2の窓でどう振る舞うか(このモジュールの
    //   `Instance::begin_reopen` ドキュメント「例外2箇所目」参照。実際に踏むには
    //   タイミングを狙ったレースが要り、この環境では確定的に再現できない)。
    // 実ハンドル越しの通し(切断イベント注入→再オープン成功)は既存の
    // `crate::ffi::tests::run_reopen_lifecycle`(実 `CpalBackend` 越し)が引き続き担う
    // ——このテストモジュールが増やしたのは、それとは独立に固定化できる
    // 「ロック不要の進行中フラグ」自体の契約だけ。

    #[test]
    fn is_reopen_in_progress_is_false_at_rest() {
        // レジストリと進行中フラグを共有するテストを直列化することで、他のテストの
        // 再オープンが一瞬だけ true を作る理論上のレースも起こらないことを固定する。
        let _lock = crate::test_backend::registry_lock();
        assert!(!is_reopen_in_progress());
    }

    #[test]
    fn maybe_reopen_with_unknown_handle_is_a_noop_and_does_not_touch_the_flag() {
        // グローバルなレジストリ・進行中フラグを共有するテストを直列化し、未知ハンドル
        // の no-op 以外の変化が混ざらないことを固定する。
        let _lock = crate::test_backend::registry_lock();
        let before = is_reopen_in_progress();
        maybe_reopen(u64::MAX);
        assert_eq!(
            is_reopen_in_progress(),
            before,
            "an unknown handle must be a complete no-op, including the in-progress flag"
        );
    }

    #[test]
    fn reopen_in_progress_guard_clears_the_flag_even_when_the_scope_panics() {
        // 本物の `REOPEN_IN_PROGRESS`(グローバル)ではなく、このテスト専用の
        // `static` を対象にする——並走する他テスト(実機越しの再オープン統合
        // テストを含む)を巻き込まずに `Drop` の契約だけを固定化するため
        // (`ReopenInProgressGuard` のドキュメント「📌」参照)。
        static TEST_FLAG: AtomicBool = AtomicBool::new(true);

        let result = std::panic::catch_unwind(|| {
            let _guard = ReopenInProgressGuard(&TEST_FLAG);
            assert!(
                TEST_FLAG.load(Ordering::Acquire),
                "the guard's caller is responsible for setting the flag before constructing it"
            );
            panic!("intentional panic to exercise the Drop guard's unwind path");
        });

        assert!(
            result.is_err(),
            "the inner closure must have actually panicked"
        );
        assert!(
            !TEST_FLAG.load(Ordering::Acquire),
            "Drop must clear the flag even when the guarded scope unwinds via panic"
        );
    }

    #[test]
    fn shutdown_during_stage2_tears_down_the_orphaned_reopen() {
        use crate::test_backend::{FakeBackendShared, Gate, install_scripted_factory, wait_until};

        // 段2の open を門の内側で止め、shutdown が差し替え用 backend を閉じた後に
        // ワーカーが自分の新 backend とデコードスレッド2本を畳む経路を固定する。
        let _lock = crate::test_backend::registry_lock();
        let first = FakeBackendShared::new(48_000);
        let replacement = FakeBackendShared::new(0);
        let third = FakeBackendShared::new(44_100);
        let gate = Gate::closed();
        third.set_open_gate(Some(Arc::clone(&gate)));
        let _factory = install_scripted_factory(vec![
            Arc::clone(&first),
            Arc::clone(&replacement),
            Arc::clone(&third),
        ]);

        let handle = match init() {
            InitOutcome::Opened(handle) => handle,
            InitOutcome::AlreadyOpen(_) => panic!("test registry must start empty"),
            InitOutcome::Failed => panic!("the first fake backend must open"),
        };
        assert_eq!(
            with_instance(handle, |instance| instance.backend_sample_rate()),
            Some(48_000)
        );
        with_instance(handle, |instance| {
            instance.note_stream_error(StreamErrorReason::DeviceUnavailable);
        });
        maybe_reopen(handle);

        assert!(third.wait_entered_open(Duration::from_secs(5)));
        // 段2の門の中だけで、未オープンの差し替え用 backend と直近値フォールバックを
        // 同時に観測できることを固定する。
        assert!(is_reopen_in_progress());
        assert_eq!(
            with_instance(handle, |instance| instance.backend_sample_rate()),
            Some(48_000)
        );
        assert!(matches!(shutdown(handle), ShutdownOutcome::Closed));

        gate.release();
        assert!(wait_until(
            || !is_reopen_in_progress(),
            Duration::from_secs(5)
        ));
        assert_eq!(third.open_calls(), 1);
        assert_eq!(third.close_calls(), 1);
        assert!(!third.is_open());
        assert!(with_instance(handle, |_| ()).is_none());
        assert!(matches!(shutdown(handle), ShutdownOutcome::InvalidHandle));

        let handle2 = match init() {
            InitOutcome::Opened(handle) => handle,
            InitOutcome::AlreadyOpen(_) => panic!("orphan teardown must clear the registry"),
            InitOutcome::Failed => panic!("the default fake backend must open"),
        };
        assert_ne!(handle2, handle);
        assert!(matches!(shutdown(handle2), ShutdownOutcome::Closed));
    }

    #[test]
    fn teardown_orphaned_reopen_closes_the_backend_and_joins_both_decode_threads() {
        use crate::test_backend::{FakeBackend, FakeBackendShared};

        // 孤児化した段3の後始末が backend の close だけでなく、デコードスレッド2本の
        // 停止要求と join まで完了させることを固定する。
        let (
            renderer,
            _command_sender,
            _reclaim_receiver,
            music_producer,
            _music_clock,
            events,
            bgm,
        ) = Renderer::build(Config::default(), 48_000);
        let (_music_tx, stop_a, join_a) = decode_thread::spawn(music_producer, Arc::clone(&events));
        let (_bgm_tx, stop_b, join_b) =
            decode_thread::spawn(bgm.stream_producer, Arc::clone(&events));
        let observed_a = Arc::clone(&stop_a);
        let observed_b = Arc::clone(&stop_b);

        let shared = FakeBackendShared::new(48_000);
        let mut fake = FakeBackend::from_shared(Arc::clone(&shared));
        fake.open(renderer, events).expect("fake backend must open");
        teardown_orphaned_reopen(Box::new(fake), stop_a, join_a, stop_b, join_b);

        assert_eq!(shared.close_calls(), 1);
        assert!(!shared.is_open());
        assert!(observed_a.load(Ordering::Relaxed));
        assert!(observed_b.load(Ordering::Relaxed));
        // スレッドは自分用の Arc クローンを握るため、join 後は観測用の1本だけが残る。
        // ただし、スレッドがたまたま先に終了していれば join 無しでも1になりうる限界はある。
        assert_eq!(Arc::strong_count(&observed_a), 1);
        assert_eq!(Arc::strong_count(&observed_b), 1);
    }

    #[test]
    fn reopen_succeeds_and_swaps_in_the_new_backend() {
        use crate::test_backend::{FakeBackendShared, install_scripted_factory, wait_until};

        // 段1→段2→段3の成功を通し、旧 backend の close、新 backend の open、状態と通知の
        // 差し替えが一つの再オープンとして完了することを固定する。
        let _lock = crate::test_backend::registry_lock();
        let first = FakeBackendShared::new(48_000);
        let replacement = FakeBackendShared::new(0);
        let third = FakeBackendShared::new(44_100);
        let _factory = install_scripted_factory(vec![
            Arc::clone(&first),
            Arc::clone(&replacement),
            Arc::clone(&third),
        ]);
        let handle = match init() {
            InitOutcome::Opened(handle) => handle,
            InitOutcome::AlreadyOpen(_) => panic!("test registry must start empty"),
            InitOutcome::Failed => panic!("the first fake backend must open"),
        };
        with_instance(handle, |instance| {
            instance.events.drain(64, |_| {});
            instance.note_stream_error(StreamErrorReason::DeviceUnavailable);
        });
        maybe_reopen(handle);
        assert!(wait_until(
            || !is_reopen_in_progress(),
            Duration::from_secs(5)
        ));

        assert_eq!(first.close_calls(), 1);
        assert_eq!(third.open_calls(), 1);
        assert!(third.is_open());
        assert_eq!(
            with_instance(handle, |instance| instance.backend_sample_rate()),
            Some(44_100)
        );
        assert_eq!(
            with_instance(handle, |instance| instance.reopen_diagnostics()),
            Some((false, 0, false))
        );
        let events = with_instance(handle, |instance| {
            let mut events = Vec::new();
            instance.events.drain(64, |event| events.push(event));
            events
        })
        .expect("the instance must still be registered");
        assert!(events.contains(&mw_core::Event::AudioInterruptionEnded { recovered: true }));
        assert!(matches!(shutdown(handle), ShutdownOutcome::Closed));
    }

    #[test]
    fn reopen_failure_is_recorded_and_eventually_gives_up() {
        use crate::test_backend::{FakeBackendShared, install_scripted_factory, wait_until};

        // 段2の open 失敗を、1回目の記録からバックオフ全消化による諦め・通知まで進め、
        // 失敗中もサンプルレートのフォールバックが保たれることを固定する。
        let _lock = crate::test_backend::registry_lock();
        let first = FakeBackendShared::new(48_000);
        let replacement = FakeBackendShared::new(0);
        let third = FakeBackendShared::new(44_100);
        third.set_open_should_fail(true);
        let _factory = install_scripted_factory(vec![
            Arc::clone(&first),
            Arc::clone(&replacement),
            Arc::clone(&third),
        ]);
        let handle = match init() {
            InitOutcome::Opened(handle) => handle,
            InitOutcome::AlreadyOpen(_) => panic!("test registry must start empty"),
            InitOutcome::Failed => panic!("the first fake backend must open"),
        };
        with_instance(handle, |instance| {
            instance.events.drain(64, |_| {});
            instance.note_stream_error(StreamErrorReason::DeviceUnavailable);
        });
        maybe_reopen(handle);
        assert!(wait_until(
            || !is_reopen_in_progress(),
            Duration::from_secs(5)
        ));

        assert_eq!(third.open_calls(), 1);
        assert!(!third.is_open());
        assert_eq!(
            with_instance(handle, |instance| instance.reopen_diagnostics()),
            Some((true, 1, false))
        );
        assert_eq!(
            with_instance(handle, |instance| instance.backend_sample_rate()),
            Some(48_000)
        );
        let events = with_instance(handle, |instance| {
            let mut events = Vec::new();
            instance.events.drain(64, |event| events.push(event));
            events
        })
        .expect("the instance must still be registered");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, mw_core::Event::AudioInterruptionEnded { .. }))
        );

        // バックオフの実時間を待たず、private な段3を直接呼んで通算6回目の失敗まで進める。
        let now_ns = mw_backend::host_time_ns();
        for step in 0..5 {
            finalize_reopen_failure(handle, now_ns + (step + 1) * 10_000_000_000);
        }
        let diagnostics = with_instance(handle, |instance| instance.reopen_diagnostics())
            .expect("the instance must still be registered");
        assert!(diagnostics.2);
        let events = with_instance(handle, |instance| {
            let mut events = Vec::new();
            instance.events.drain(64, |event| events.push(event));
            events
        })
        .expect("the instance must still be registered");
        assert!(events.contains(&mw_core::Event::AudioInterruptionEnded { recovered: false }));

        // 🔴 段2が進行中で**ない**まま未オープンのバックエンドを閉じにいく経路は、
        // `CloseFailed` を正直に返すのが `shutdown` の約束(`self.backend` が本当に
        // 「開けなかった」状態のままだから)。段2 の割り込みだけを `Closed` へ
        // 丸める分岐(`reopen_was_in_progress`)と取り違えないよう、ここで固定する。
        assert!(matches!(shutdown(handle), ShutdownOutcome::CloseFailed));
    }
}
