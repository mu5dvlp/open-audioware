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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use mw_backend::{Backend, CpalBackend};
use mw_core::{
    BgmStatePublisher, CommandSender, Config, EventQueue, MusicClockPublisher, MusicClockSnapshot,
    MusicDecoder, MusicState, ReclaimReceiver, Renderer, SoundStorage,
};

use crate::decode_thread::{self, DecoderSender};

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

pub struct Instance {
    handle: u64,
    backend: CpalBackend,
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
}

impl Instance {
    /// 出力デバイスの実サンプルレート(`CpalBackend::open` がネゴシエートした値)。
    ///
    /// `mw_sound_load`(SE ロード)が wav のリサンプル要否を判定するために使う
    /// (初期構築仕様『§4.7』: 「SE はロード時に全デコード + 必要ならロード時に
    /// リサンプルして出力レート化」)。
    pub fn backend_sample_rate(&self) -> u32 {
        self.backend.sample_rate()
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
    /// `mw_music_set` は `SymphoniaDecoder::open` にバイト列の**複製**を渡す
    /// (`SymphoniaDecoder::open` が `Vec<u8>` を値で要求するため)ので、デコード
    /// スレッドが実際に読んでいるメモリはここで管理する `Arc<Vec<u8>>` とは
    /// 最初から独立している。したがって release してもデコードスレッド側の
    /// 再生には一切影響しない(参照カウントが尽きるまで生かす、という `Arc` 由来の
    /// 間接的な挙動ではなく、そもそも別々のメモリになっている、という単純な話)。
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

    /// 出力レイテンシの実測値([`Backend::output_latency_ns`])を取得する
    /// (`mw_get_output_latency_ns` が使う)。
    pub fn output_latency_ns(&self) -> u64 {
        self.backend.output_latency_ns()
    }

    /// 出力レイテンシの実測値を1回だけログへ出す
    /// (`CpalBackend::log_output_latency_once` へ委譲)。`mw_get_output_latency_ns`
    /// (ゲームスレッド経路)から呼ぶこと。コールバック内から呼んではいけない
    /// (`mw_log!` はアロケーションとロックを伴う)。
    pub fn log_output_latency_once(&self) {
        self.backend.log_output_latency_once();
    }
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

    let mut backend = CpalBackend::new();
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

    // 見つかったインスタンスをレジストリから外す。`close` が失敗しても状態は握ったままに
    // しない(二重 shutdown は次回呼び出しで ErrInvalidHandle として検出されるため)。
    match guard.take() {
        Some(mut instance) => {
            // M2-7: デコードスレッドを確実に停止・join する(`mw_init` で1本だけ
            // 立てたぶん、`mw_shutdown` で確実に回収する。スレッドリーク防止)。
            // 最大で `decode_thread::POLL_INTERVAL` 分だけこの呼び出しがブロックし
            // うるが、`mw_shutdown` は毎フレーム呼ぶ関数ではない一度きりの終了処理
            // なので、初期構築仕様『§5.4』の「全関数非ブロッキング」が要求する
            // 粒度の対象外とみなす(既存の `backend.close()` も内部でストリームの
            // 停止を同期的に待つ設計になっている)。
            instance.decode_thread_stop.store(true, Ordering::Relaxed);
            if let Some(join_handle) = instance.decode_thread.take() {
                // デコードスレッド内部は catch_unwind で panic を握りつぶす設計
                // (`crate::decode_thread` 参照)なので、ここでの `Err`(パニック伝播)
                // は理論上起こらない。万一起きても shutdown 自体は続行する
                // (join 失敗を理由にハンドルを不定状態のまま残さない)。
                let _ = join_handle.join();
            }
            // M4-3: BGM 専用デコードスレッドも同様に停止・join する(楽曲用と
            // 対称。どちらか一方だけ回収し忘れるとスレッドリークになる)。
            instance
                .bgm_decode_thread_stop
                .store(true, Ordering::Relaxed);
            if let Some(join_handle) = instance.bgm_decode_thread.take() {
                let _ = join_handle.join();
            }
            match instance.backend.close() {
                Ok(()) => ShutdownOutcome::Closed,
                Err(_) => ShutdownOutcome::CloseFailed,
            }
        }
        // 直前の `is_valid` チェックで Some を確認済みのため到達しない防御的分岐。
        None => ShutdownOutcome::InvalidHandle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
