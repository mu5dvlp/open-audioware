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

use mw_backend::{Backend, CpalBackend};
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

    // --- M3「Android(AAudio)切断復旧」案A(初期構築仕様『§6』確定) ------------------
    //
    // 内部再オープン(`Instance::attempt_reopen`)は `Renderer`/`Mixer` を丸ごと
    // 作り直す(=上のハンドル一式をすべて差し替える)ため、復元に要る情報は
    // `Renderer` の外側(=このキャッシュ)に持っておく必要がある。`sounds`/
    // `music_bytes` は元々ここにあり再オープンでも触らないため、「ロード済みの
    // SoundId が無効にならない」は追加のキャッシュ無しで自動的に満たされる
    // (`Instance::attempt_reopen` のドキュメント参照)。
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

    /// 出力コールバックのアンダーラン(の疑い)統計([`Backend::output_underrun_count`]
    /// 等)を取得する(`mw_get_output_underrun_stats` が使う)。
    ///
    /// **`mw_core::Event::Underrun`(`mw_poll_events` 経由)とは別物。**
    /// `mw_backend::underrun` モジュール doc / `crate::types::MwOutputUnderrunStats`
    /// のドキュメント参照。
    pub fn output_underrun_stats(&self) -> (u64, u64, u32) {
        (
            self.backend.output_underrun_count(),
            self.backend.last_output_underrun_host_time_ns(),
            self.backend.consecutive_output_underrun_count(),
        )
    }

    /// 出力コールバックのアンダーラン(の疑い)を新たに検知していれば、その分だけ
    /// ログへ出す(`CpalBackend::log_new_output_underruns` へ委譲)。
    /// `mw_get_output_underrun_stats`(ゲームスレッド経路)から呼ぶこと。
    /// コールバック内から呼んではいけない(`mw_log!` はアロケーションとロックを伴う)。
    pub fn log_new_output_underruns(&self) {
        self.backend.log_new_output_underruns();
    }

    // --- M3「Android(AAudio)切断復旧」案A ---------------------------------------

    /// `mw_music_set` が成功した後に呼ぶ復元キャッシュの更新
    /// (`Instance::attempt_reopen` が使う)。`MusicVoice::prepare` 自身がループ区間を
    /// 無条件に初期化する挙動(`music.rs::MusicVoice::prepare`)に揃え、ここでも
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

    /// 診断用: 再オープン復元キャッシュのバス音量を覗く。テスト専用
    /// (`bus_volumes` フィールドは private なので、外から確認するにはこの経路が要る)。
    #[cfg(test)]
    pub fn bus_volume_for_test(&self, bus: BusId) -> f32 {
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

    /// デコードスレッド(楽曲・BGM の両方)を停止・join する。`mw_shutdown` と
    /// `attempt_reopen` の両方から呼ばれる共通処理(元は `shutdown` 内に直接
    /// 書かれていたが、再オープンでも同じ手順が要るためここへ切り出した)。
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

    /// 案A(初期構築仕様『§6』確定): ミドルウェア内部でストリームを再オープンし、
    /// できる限り状態を復元する。
    ///
    /// **必ずゲームスレッドから呼ぶこと**(`handle_registry::maybe_reopen` 経由)。
    /// `Renderer::build_with_events`/デコードスレッドの `thread::spawn` はいずれも
    /// ヒープアロケーションを伴うため、初期構築仕様『§5.3』のリアルタイム安全性規約の
    /// 対象である音声コールバックから呼んではならない(依頼書「⚠️ 再オープンは
    /// ゲームスレッド側でやること」)。
    ///
    /// ## 復元できる状態・できない状態
    ///
    /// - **楽曲の再生位置**(いちばん重要): `music_clock` のスナップショットが持つ
    ///   `song_frames`/`state` を [`Instance::restore_music`] が
    ///   `MusicPrepare`→`MusicSeek`→(再生中だったときのみ)`MusicPlayScheduled` として
    ///   再送する。§4.3 で元から用意されているコマンド列(`mw_music_set` と同じ順序
    ///   厳守)を再利用しているだけで、専用の復元経路を新設してはいない。
    /// - **バス音量**: `bus_volumes` キャッシュから4本とも `SetBusVolume` を再送する
    ///   ([`Instance::restore_bus_volumes`])。
    /// - **ロード済みの `SoundId`**: `sounds`/`music_bytes` はこのメソッドが一切
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
    /// このメソッドは1回試すだけで、結果を [`ReopenPolicy::record_result`] に記録して
    /// 返す。**無限リトライはしない**——呼び出し元(`handle_registry::maybe_reopen`)は
    /// `ReopenPolicy` のバックオフに従い間隔を空けて再試行し、既定
    /// [`crate::reopen::REOPEN_BACKOFF_SCHEDULE_MS`] を使い切ったら自動での再試行を
    /// 諦める(`crate::reopen` モジュール doc「無限リトライを禁止する」参照)。
    /// 🔴 `default_output_device()` が切断直後に使える保証は無い(依頼書の指摘。
    /// 実機でしか確認できない)——失敗した場合も `Instance` は次の呼び出しでまた
    /// 試せる一貫した状態のままになる(`self.backend` は単に「開いていない」状態、
    /// 既存の `mw_get_output_latency_ns` 等は 0/既定値を返すだけでパニックしない)。
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
    fn attempt_reopen(&mut self, now_ns: u64) -> bool {
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

        // 2) 現在のセッションを畳む。AAudio の切断は API 契約上すでに終端的
        //    (`ndk::audio::AudioError::Disconnected` のドキュメント。
        //    `docs/history/03-2026-08-31.md` 参照)なので、明示的に close しなくても
        //    もう鳴っていない——ここでの close は状態を明示的に確定させる後始末に
        //    過ぎない。失敗してもベストエフォートで続行する。
        if let Err(err) = self.backend.close() {
            mw_backend::mw_log!("[mw-ffi] attempt_reopen: backend close failed (ignored): {err}");
        }
        self.stop_and_join_decode_threads();

        // 3) 新しい Renderer 一式を組み立てる。`events`(`Arc<EventQueue>`)の identity は
        //    変えない——`mw_poll_events` が読み出す先を差し替える必要が無いようにする
        //    (`mw_core::renderer::Renderer::build_with_events` のドキュメント参照)。
        let (renderer, command_sender, reclaim_receiver, music_producer, music_clock, _events, bgm) =
            Renderer::build_with_events(
                Config::default(),
                PROVISIONAL_SAMPLE_RATE,
                Arc::clone(&self.events),
            );

        // 4) 🔴 generation を必ず bump する。新しいクロックは generation=0 から
        //    始まるため、直前のクロックが最後に観測させた値と偶然一致しうる——
        //    `wrapping_add(1)` した値でシードすることで「直前に観測されたどの値とも
        //    異なる」ことを保証する
        //    (`MusicClockPublisher::seed_generation_after_reopen` のドキュメント参照)。
        music_clock.seed_generation_after_reopen(clock_before.generation.wrapping_add(1));

        // 5) 実際にバックエンドを開き直す。
        match self.backend.open(renderer, Arc::clone(&self.events)) {
            Ok(()) => {
                let (decoder_tx, decode_thread_stop, decode_thread_handle) =
                    decode_thread::spawn(music_producer, Arc::clone(&self.events));
                let (bgm_decoder_tx, bgm_decode_thread_stop, bgm_decode_thread_handle) =
                    decode_thread::spawn(bgm.stream_producer, Arc::clone(&self.events));

                self.command_sender = command_sender;
                self.reclaim_receiver = Mutex::new(reclaim_receiver);
                self.music_clock = music_clock;
                self.bgm_state = bgm.state;
                self.decoder_tx = decoder_tx;
                self.decode_thread_stop = decode_thread_stop;
                self.decode_thread = Some(decode_thread_handle);
                self.bgm_decoder_tx = bgm_decoder_tx;
                self.bgm_decode_thread_stop = bgm_decode_thread_stop;
                self.bgm_decode_thread = Some(bgm_decode_thread_handle);
                // 新しいセッション用に「1回だけ」ログのフラグも仕切り直す(そうしないと
                // 新しいストリームの実測値〔I/O バッファ長〕が二度とログされない)。
                self.logged_buffer_info.store(false, Ordering::Relaxed);

                // 6) 状態を復元する(このメソッドのドキュメント「復元できる状態」参照)。
                self.restore_bus_volumes(&saved_bus_volumes);
                self.restore_music(saved_music_sound_id, &clock_before, saved_music_loop);
                self.restore_bgm(saved_bgm_sound_id, saved_bgm_loop);

                mw_backend::mw_log!("[mw-ffi] internal stream reopen succeeded");
                self.reopen.record_result(true, now_ns);
                // 🔴 音が戻ったことを C# 側へ通知する(`Instance::notify_reopen_outcome`
                // のドキュメント参照)。
                self.notify_reopen_outcome(true);
                true
            }
            Err(err) => {
                mw_backend::mw_log!("[mw-ffi] internal stream reopen failed: {err}");
                self.reopen.record_result(false, now_ns);
                if self.reopen.is_exhausted() {
                    // 依頼書「無限リトライは禁止」: バックオフを使い切ったので、
                    // 新しい切断イベント(`note_stream_error`)が届くまで自動での
                    // 再試行を止める(`crate::reopen` モジュール doc 参照)。
                    mw_backend::mw_log!(
                        "[mw-ffi] internal stream reopen: giving up after {} consecutive \
                         failed attempts; will retry automatically only if a new disconnect \
                         is observed",
                        self.reopen.attempts()
                    );
                    // 🔴 諦めたことを C# 側へ通知する(作業①: 直前まで `mw_log!` にしか
                    // 出ていなかったため、C# 側に「音が永久に死んだ」ことを知る手段が
                    // 無かった。`Instance::notify_reopen_outcome` のドキュメント参照)。
                    self.notify_reopen_outcome(false);
                }
                false
            }
        }
    }

    /// 内部再オープン(案A)の最終結果を C# 側へ通知する
    /// ([`Instance::attempt_reopen`] のドキュメント「結果の C# への通知」参照)。
    ///
    /// **既存の `Event::AudioInterruptionEnded { recovered }` を転用する**(新しい
    /// イベント種別は追加していない——`mw_core::Event::AudioInterruptionEnded` の
    /// ドキュメント参照)。呼び出し元(`attempt_reopen`)は必ずゲームスレッドから
    /// 呼ばれる(初期構築仕様『§5.3』の対象外)ので、`cpal` のエラーコールバックが
    /// `StreamError` を積むのと同じ [`EventQueue::push_side_channel`] を使う
    /// (`push_realtime` は音声スレッド専用の単一書き手前提のため、ここでは使えない/
    /// 使う必要が無い)。
    fn notify_reopen_outcome(&self, recovered: bool) {
        self.events
            .push_side_channel(mw_core::Event::AudioInterruptionEnded { recovered });
    }

    /// 4本のバスの音量を再送する([`Instance::attempt_reopen`] 手順6)。
    fn restore_bus_volumes(&self, volumes: &[f32; BUS_COUNT]) {
        for bus in mw_core::ALL_BUSES {
            let volume = volumes[bus.index()];
            let _ = self
                .command_sender
                .send(Command::SetBusVolume { bus, volume });
        }
    }

    /// 楽曲ボイスの状態を再送する([`Instance::attempt_reopen`] 手順6)。
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
        let decoder = match SymphoniaDecoder::open((*bytes).clone(), output_sample_rate) {
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

    /// BGM ボイスの状態を再送する([`Instance::attempt_reopen`] 手順6)。
    ///
    /// **BGM の再生位置・再生中だったかどうかは復元しない**(このメソッドの呼び出し元
    /// `Instance::attempt_reopen` のドキュメント「復元できる状態・できない状態」参照)。
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
        let decoder = match SymphoniaDecoder::open((*bytes).clone(), output_sample_rate) {
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
        last_music_sound_id: AtomicU64::new(0),
        last_music_loop: Mutex::new(None),
        last_bgm_sound_id: AtomicU64::new(0),
        last_bgm_loop: Mutex::new(None),
        // `mw_core::bus::Bus::new()` の既定音量(1.0)と揃える。
        bus_volumes: std::array::from_fn(|_| AtomicU32::new(1.0_f32.to_bits())),
        reopen: ReopenPolicy::new(),
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
            // 立てたぶん、`mw_shutdown` で確実に回収する。スレッドリーク防止。
            // M4-3 で BGM 専用のもう1本も同じ手順で対称に扱うようになった)。
            // 最大で `decode_thread::POLL_INTERVAL` 分だけこの呼び出しがブロックし
            // うるが、`mw_shutdown` は毎フレーム呼ぶ関数ではない一度きりの終了処理
            // なので、初期構築仕様『§5.4』の「全関数非ブロッキング」が要求する
            // 粒度の対象外とみなす(既存の `backend.close()` も内部でストリームの
            // 停止を同期的に待つ設計になっている)。`Instance::attempt_reopen`
            // (M3, 案A)も同じ手順を必要としたため、共通処理として切り出してある
            // (`Instance::stop_and_join_decode_threads`)。
            instance.stop_and_join_decode_threads();
            match instance.backend.close() {
                Ok(()) => ShutdownOutcome::Closed,
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
/// `ReopenPolicy::is_due` が `false` を返す安価なチェックだけで即座に戻る
/// (`registry()` のロック取得+数回の atomic load のみ)。`instance.handle != handle`
/// (無効ハンドル)・インスタンス未初期化の場合も静かに何もしない
/// (`with_instance`/`shutdown` と同じ「クラッシュしない」方針)。
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
    instance.attempt_reopen(now_ns);
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
