//! 出力デバイス抽象(初期構築仕様 §5.1: 「将来の oboe / RemoteIO 直叩き実装もここに並べる」)。

use std::fmt;
use std::sync::Arc;

use mw_core::{EventQueue, Renderer};

/// `Backend` の操作で発生しうるエラー。
///
/// FFI 境界(mw-ffi)ではこれを `MwResult` の負のエラーコードへ writeする。
/// バックエンド内部でパニックさせず、必ずこの型で失敗を報告する(§4.8 の思想を
/// mw-backend 内でも先取りする)。
#[derive(Debug)]
pub enum BackendError {
    /// 既定の出力デバイスが見つからない(デバイス無し環境。CI・ヘッドレス環境等)。
    NoOutputDevice,
    /// M0 が対応する f32 ステレオの出力構成が見つからない。
    NoSupportedStreamConfig,
    /// cpal のストリーム構築に失敗した。
    BuildStreamFailed(String),
    /// cpal のストリーム開始(`play`)に失敗した。
    PlayStreamFailed(String),
    /// 既に開いているバックエンドへ再度 `open` した。
    AlreadyOpen,
    /// 開いていないバックエンドを `close` した。
    NotOpen,
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::NoOutputDevice => write!(f, "no default output device"),
            BackendError::NoSupportedStreamConfig => {
                write!(f, "no supported f32 stereo output stream config")
            }
            BackendError::BuildStreamFailed(msg) => write!(f, "failed to build stream: {msg}"),
            BackendError::PlayStreamFailed(msg) => write!(f, "failed to start stream: {msg}"),
            BackendError::AlreadyOpen => write!(f, "backend is already open"),
            BackendError::NotOpen => write!(f, "backend is not open"),
        }
    }
}

impl std::error::Error for BackendError {}

/// 出力デバイスを開閉し、内部でオーディオコールバックから `mw-core` のレンダラを駆動する抽象。
///
/// 実装はスレッドセーフであること(§5.4)。`open`/`close` はゲームスレッドから
/// 呼ばれる想定だが、コールバック自体は OS が生成する専用の音声スレッドで実行される。
///
/// `renderer` は値渡し(ムーブ)で受け取る。`Renderer` のミキサ状態(ボイスプール・バス・
/// コマンドキュー受信側)は音声コールバックスレッドの単一の書き手専用であり、
/// ゲームスレッドは別途 `mw_core::CommandSender` / `mw_core::ReclaimReceiver` 経由でのみ
/// やり取りする(`Arc` 共有や内部可変性を持ち込まない設計。`crates/mw-core/src/renderer.rs`
/// のドキュメント参照)。
pub trait Backend {
    /// 出力ストリームを開き、再生を開始する。
    ///
    /// 既に開いている場合は `Err(BackendError::AlreadyOpen)` を返す(パニックしない)。
    /// 冪等性(二重 init の扱い)は呼び出し元の mw-ffi が担う(§4.8)。
    ///
    /// `events` は初期構築仕様『§4.6 イベント通知』のイベントキュー。実装は
    /// ストリームのエラー通知経路(cpal の `err_fn` 等、**音声スレッドとは別の**
    /// 非リアルタイムスレッド)から `EventQueue::push_side_channel` で `StreamError`
    /// イベントを積む(`crate::cpal_backend` の実装を参照)。この経路は §5.3 の
    /// 対象外(音声スレッドではない)なのでロックを使ってよい。
    fn open(&mut self, renderer: Renderer, events: Arc<EventQueue>) -> Result<(), BackendError>;

    /// ストリームを停止して閉じる。
    ///
    /// 開いていない場合は `Err(BackendError::NotOpen)` を返す(パニックしない)。
    fn close(&mut self) -> Result<(), BackendError>;

    /// 現在ストリームが開いているか。
    fn is_open(&self) -> bool;

    /// 直近のオーディオコールバックが実際に受け取ったフレーム数。まだ1度も呼ばれていなければ 0。
    ///
    /// I/O バッファ長の**実測値**。iOS の `AVAudioSession` は希望値をそのまま採用したように
    /// 申告することがあるため、申告値だけでは遅延の見積もりを裏取りできない
    /// (`docs/measurement-m1.md` §8.7)。初期構築仕様 M3「出力レイテンシ問い合わせ」の
    /// 最小の第一歩でもある。
    ///
    /// 音声スレッドがアトミックに書き、ゲームスレッドが読む(コールバック側の追加コストは
    /// アトミックストア1回。リアルタイム安全性規約に抵触しない)。
    fn last_callback_frames(&self) -> u32;

    /// オープン時にデバイスとネゴシエートしたサンプルレート。未オープンなら 0。
    /// [`Backend::last_callback_frames`] をミリ秒へ換算するのに要る。
    fn sample_rate(&self) -> u32;

    /// 直近のオーディオコールバックにおける出力レイテンシ(ns)。まだ1度もコールバックが
    /// 走っていなければ 0(「まだ不明」を意味する。実測上 0ns ちょうどになることは
    /// まず無いため、この特殊値と「たまたま 0ns だった」を取り違える実害は無い)。
    ///
    /// cpal の `OutputCallbackInfo::timestamp()` が返す `OutputStreamTimestamp` の
    /// `playback`(このバッファが実際にスピーカー/DAC へ出力されると cpal が予測する
    /// 時刻)と `callback`(このコールバックが呼ばれた時刻)の差、`playback - callback`。
    /// 両フィールドの意味は cpal 0.18.1 のソース(`timestamp.rs` の
    /// `OutputStreamTimestamp` doc)で確認済み。時計源はホストごとに異なる
    /// (macOS/iOS の CoreAudio は `mach_absolute_time()`、Android の AAudio は
    /// `AAudioStream_getTimestamp(CLOCK_MONOTONIC)` 等)が、`StreamInstant` のdocに
    /// 「同一ストリーム内では全インスタントが同じ時計を共有する」とある通り、
    /// 同一コールバック呼び出し内の `playback`/`callback` 同士の引き算は意味を持つ。
    ///
    /// **これが表すのは cpal/OS が申告する「バッファ→スピーカー」間の遅延だけ**であり、
    /// `docs/measurement-m1.md` で実測している「タップ→音」のエンドツーエンド遅延
    /// (画面の入力検出・タッチイベントのディスパッチ・OS ミキサ等も含む)とは別物。
    /// 初期構築仕様 §5.5 `mw_get_output_latency_ns` の値の供給元だが、これ単体を
    /// タップ→音の遅延と混同しないこと。
    ///
    /// **iOS では同じ落とし穴がある可能性がある。** [`Backend::last_callback_frames`]
    /// のdocの通り、iOS の `AVAudioSession` は希望した I/O バッファ長をそのまま
    /// 採用したかのように申告することがある(`crates/mw-ffi/src/handle.rs::Instance::
    /// log_buffer_info_once` / `docs/measurement-m1.md` §8.7)。cpal が `playback`
    /// タイムスタンプを算出する際に OS 側の「希望値」を額面通り使っていれば、この
    /// 差分も実測ではなく申告上の値になりうる。実機では
    /// [`Backend::last_callback_frames`] の実測値と突き合わせて裏取りすること。
    ///
    /// 音声スレッドがアトミックに書き、ゲームスレッドが読む(コールバック側の追加コストは
    /// 減算1回+アトミックストア1回。リアルタイム安全性規約に抵触しない)。
    fn output_latency_ns(&self) -> u64;

    /// 出力コールバックの間隔異常(「アンダーラン(の疑い)」)を検知した累計回数。
    ///
    /// **`mw_core::Event::Underrun` とは別物。** あちらは楽曲/BGM のデコード
    /// リングバッファがデータ供給に追いつかず無音で埋めたことの検知(M2)。
    /// こちらは音声コールバック自体が想定より遅く/間隔が開いて呼ばれたことの検知
    /// (M3「アンダーラン検知・テレメトリ」)——OS 側の出力バッファが実際に
    /// 枯渇した(音が途切れた)ことの直接的な兆候、またはその一歩手前の状態を示す。
    /// 検知の定義・cpal 自身のアンダーラン通知(`ErrorKind::Xrun`)を採用しなかった
    /// 理由は `crate::underrun` のモジュール doc を参照。
    ///
    /// 音声スレッドがアトミックに書き、ゲームスレッドが読む(コールバック側の追加
    /// コストは整数演算+アトミック操作のみ。リアルタイム安全性規約に抵触しない)。
    fn output_underrun_count(&self) -> u64;

    /// 直近に[出力コールバックのアンダーラン](Backend::output_underrun_count)を
    /// 検知したコールバックのホスト単調時刻(ns)。まだ1度も検知していなければ 0。
    fn last_output_underrun_host_time_ns(&self) -> u64;

    /// 直近まで連続して[出力コールバックのアンダーラン](Backend::output_underrun_count)
    /// を検知した回数。検知されなかったコールバックが1回でも挟まると 0 に戻る
    /// (「単発の乱れ」か「持続的な悪化」かをゲームスレッド側が区別できるようにする
    /// ための補助値)。
    fn consecutive_output_underrun_count(&self) -> u32;
}
