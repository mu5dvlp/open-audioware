//! macOS / iOS / tvOS 用の自前 AudioUnit バックエンド(AUDIOWARE-DEPS-PLAN.md
//! ステップ4-1〔macOS〕・4-2〔iOS/tvOS〕)。
//!
//! **新しい外部クレートは追加していない。** AudioToolbox / CoreAudio フレームワークの
//! C API を自前の `extern "C"` 宣言(下の「AudioToolbox の FFI 宣言」節)で直接呼ぶ——
//! `objc2-audio-toolbox` 等は使わない(計画書 §4 の決定どおり。それらは `objc2`/
//! `objc2-foundation` を引き込むため、依存排除の到達点〔表記ゼロ〕に反する)。
//! iOS/tvOS のルート変化監視だけは、`ios_interruption.rs` が既に使っている
//! `objc2-foundation`/`block2`(NSNotificationCenter)と `objc2-avf-audio`
//! (AVAudioSession)をそのまま再利用する——いずれも既存の iOS/tvOS 向け依存で、
//! 新規追加はない。
//!
//! 手順は計画書の記述そのまま: `AudioComponentFindNext` → `AudioComponentInstanceNew` →
//! `AudioUnitSetProperty`(StreamFormat / SetRenderCallback)→ `AudioUnitInitialize` →
//! `AudioOutputUnitStart`。使うコンポーネントは macOS では `kAudioUnitSubType_DefaultOutput`
//! (既定の出力デバイスへ自動的に追従する)、iOS/tvOS では `kAudioUnitSubType_RemoteIO`
//! (`AUDIO_UNIT_SUBTYPE`)——差分はサブタイプと、`ios_session::configure`(セッション設定。
//! [`Backend::open`] の冒頭で呼ぶ)・補正項の更新(`IosOutputLatencyWatcher`)だけに
//! 絞れた(4-1 の見込みどおり)。
//!
//! # iOS/tvOS の割り込み・バックグラウンドからの復帰
//!
//! cpal 版と**同じ監視**(`ios_interruption::Watcher`。割り込みの開始/終了・背面/前面・
//! ルート変化・出力停止のウォッチドッグ)をそのまま使い、復帰の操作だけをこのファイルの
//! [`UnitControl`](`ios_interruption::RecoverableOutput` の実装)で差し替える:
//!
//! - 止め直し: `AudioOutputUnitStop`。
//! - 動かし直し: `AudioOutputUnitStart`(失敗したら `AudioUnitUninitialize` →
//!   `AudioUnitInitialize` し直してから再度 Start)。直前に補正項(`outputLatency`)を
//!   読み直す(背面にいる間にルートが変わっていることがある)。
//! - 作り直し(メディアサービスのリセット後、または止め直しを繰り返しても進まないときの
//!   最後の手段): **AudioUnit だけ**を新しく作り、レンダーコールバックの context
//!   (`CallbackContext` —— `Renderer` を持つ)は**同じポインタのまま**付け替える。
//!   古いユニットを止めてから新しいユニットを開始するまでの間はコールバックが走らないので、
//!   context の排他所有(単一の書き手)は崩れない。サンプルレートは開いたときの値のまま
//!   (ハードウェアと違えば AudioUnit が変換する)——`Renderer` のサンプルレートを
//!   変えずに済ませるため。動かし直しと同じく、直前に補正項(`outputLatency`)を
//!   読み直す。
//!
//! 動かし直し・作り直しのたびにホスト時刻の相関点が飛ぶので、音声スレッドが次の
//! コールバックで音楽クロックの世代を進める(補正項の変化と同じ経路。
//! `CallbackContext::last_restart_epoch`)。
//!
//! AudioUnit は `UnitControl` の `Mutex` の中に置く。通知ハンドラ・復帰確認のワーカーと
//! [`Backend::close`] が同じロックを通るので、閉じた後のユニットへ Start を撃つことはない
//! (閉じた後の操作は何もせずエラーを返す)。音声スレッドはこのロックに一切触れない。
//!
//! # cpal 版との既知の差分
//!
//! macOS のデバイスの切断・既定出力デバイスの変更の監視は実装していない(4-1 は
//! 「macOS Editor 専用の開発機バックエンド」という前提の範囲で許容する判断とした)。
//! `StreamError` イベントは発行しない(iOS/tvOS の割り込み・ルート変化のイベントは
//! cpal 版と同じく `ios_interruption` が発行する)。出力コールバック自体の
//! 間隔異常(「アンダーラン(の疑い)」)は cpal 版と同じ
//! [`crate::underrun::OutputUnderrunTracker`] で検知する(macOS/iOS/tvOS 共通)。
//!
//! # タイムスタンプの扱い——相関点と補正項を分けて持つ(HANDOFF 0f の統合)
//!
//! `AURenderCallback` が渡す `AudioTimeStamp.mHostTime` は `mach_absolute_time()` と
//! 同じ時計源(`host_time.rs` のモジュール doc の調査結果どおり)なので、
//! [`crate::host_time::mach_ticks_to_ns`](同じ関数を cpal 版の時計整合性でも使っている)
//! で ns 化する——これが**相関点**(このコールバックが呼ばれたホスト時刻そのもの。
//! 毎コールバック読み直すだけで、どこにも保存しない)。そこへ**補正項**
//! (`CallbackContext::device_extra_latency_ns`。出力レイテンシの推定値)と
//! **バッファ長の項**(`buffer_duration_ns`。下記「バッファ長の項の出し方」参照)を
//! 足して `Renderer::render` に渡す `buffer_start_host_time_ns`(予測される DAC 出力時刻)を
//! 得る。式は [`compute_timestamps`] のドキュメント参照——この関数自体は相関点・補正項・
//! バッファ長の3値を受け取って合算するだけの純関数で、どちらも呼び出し側
//! (`render_proc`)が OS ごとに値を用意する。
//!
//! 補正項の**出し方**だけが OS で違う:
//!
//! - **macOS**: `open()` の中で `query_device_extra_latency_frames` を1度だけ呼び、
//!   以後は固定(デバイス切断の監視が無いのと同じ理由で、4-1 の時点では更新しない)。
//! - **iOS/tvOS**: `open()` の時点で `query_ios_output_latency_ns`
//!   (`AVAudioSession.outputLatency()`)を読んで初期化し、以後は
//!   `IosOutputLatencyWatcher` が `AVAudioSessionRouteChangeNotification` を監視して
//!   ルート(スピーカー/有線/Bluetooth)が変わるたびに読み直す——これが HANDOFF 0f の
//!   「補正項をルート変化で更新する」。それ以外で読み直すのは次の3箇所だけに絞っている:
//!   `Backend::refresh_output_latency`(曲の再生予約の直前。`mw-ffi` の
//!   `mw_music_play_scheduled` から呼ばれる)、`RecoverableOutput::play` の
//!   動かし直し、`RecoverableOutput::rebuild` の作り直し。いずれも音楽クロックの
//!   世代(`restart_epoch`)が進むタイミングと一致させてある——**曲の再生中(世代を
//!   またがない途中)に補正項だけを変えるとクロックが跳ぶため**、意図的にこの3箇所+
//!   ルート変化以外では読み直さない。セッションが落ち着く前に `open()` が読んだ値が
//!   プロセス生存中ずっと残ってしまう(Control Center 操作・背面復帰後に小さな
//!   常時のズレが残り続ける)問題への対処。
//!
//! バッファ長の項の**出し方**も OS で違う(いずれも [`compute_timestamps`] の3番目の
//! 引数 `buffer_duration_ns` として渡す):
//!
//! - **macOS**: このコールバックの実測フレーム数(`in_number_frames`)を
//!   `extra_latency_frames_to_ns` でそのつど ns 化する(変更無し。デバイス切断の
//!   監視が無い前提の範囲なので、実測フレーム数が安定していることを前提にしてよい)。
//! - **iOS/tvOS**: `AVAudioSession.IOBufferDuration()` を補正項
//!   (`device_extra_latency_ns`)と**全く同じ読み直しタイミング**(`open()`・
//!   `refresh_ios_output_latency` の4箇所)で読み、`CallbackContext::
//!   io_buffer_duration_ns` に保持する——実測フレーム数(`in_number_frames`)は
//!   使わない。RemoteIO は Control Center 表示中の出力停止からウォッチドッグ
//!   (`ios_interruption` の `OutputStalled` → `pause()`→`play()`)で復帰した後、
//!   `in_number_frames` が希望値(通常 240 フレーム=5ms)より大きい値(観測上
//!   2048 前後)のまま戻らないことがある——実際の出力はそのぶん遅れていないため、
//!   実測フレーム数をそのままバッファ長の項に使うと予測出力時刻が実測より恒久的に
//!   遅れる(実機症状: 曲に合わせて叩くと判定が「速い」側〔GREAT FAST〕に倒れる)。
//!   `IOBufferDuration` は OS に設定させた希望値であり、RemoteIO が実際に渡す
//!   フレーム数の揺れに引きずられない——cpal 0.18.1 の iOS 実装(モジュール doc
//!   「cpal 0.18.1 が iOS で返している値の意味」参照)が起動時に1度だけ
//!   `IOBufferDuration` をキャッシュしていたのと同じ考え方だが、こちらはルート変化等で
//!   読み直す分、より新鮮な値を使う。値が変わったら(macOS で補正項が変わるのと同じ
//!   扱いで)`MusicClockPublisher::bump_generation` を呼ぶ(`render_proc` 参照)。
//!
//! **cpal 0.18.1(固定中)が iOS で返している値の意味、および 0.18.2 でそれが壊れた理由**
//! (ソースを確認済み。`~/.cargo/registry/.../cpal-0.18.{1,2}/src/host/coreaudio/ios/mod.rs`):
//! 0.18.1 の `OutputStreamTimestamp::playback` は `callback + (AVAudioSession.
//! IOBufferDuration() を起動時に1度だけ frame 数へ換算した値)`——**`outputLatency()`
//! は一切読んでいない**。0.18.2 はそこへ `AVAudioSession.outputLatency()` を追加で
//! 足すようになり、それを相関点として使っている音楽クロックが実機で約65ms 遅れた
//! (`9fdf287` のコミットメッセージ、Cargo.toml のコメント参照)。
//!
//! **この実装は 0.18.1 の意味を保たず、意図的に変える。** `mw-core::mixer::Mixer::render`
//! の doc と client 側 `NativeMusicClockCore.cs` の doc(「相関点は"今スピーカーから
//! 聞こえている"曲位置を指す——出力レイテンシが織り込まれている」)が最初から要求している
//! のは **DAC 出力時刻の予測**であって、0.18.1 の iOS 実装はそれを部分的にしか満たして
//! いなかった(`IOBufferDuration` だけで `outputLatency` を欠く)。0.18.2 が壊したのは
//! 「正しさ」ではなく「依存の patch 更新という不透明な経路で、校正済みの値が無断で
//! 変わったこと」——そのため、ここでは**自分で定義した意味を持つ新バックエンドとして**
//! `outputLatency()` を正しく織り込む。安全策は2つ: (1) 既定は `backend-cpal` のまま
//! (この変更は `backend-native` を明示選択しない限り本番に影響しない)。(2) 実際に
//! iOS を `backend-native` へ切り替える段では、`AudioOffsetSeconds` 等の実機校正を
//! **出力レイテンシが変わった前提で**取り直す必要がある——これは client 側
//! `NativeMusicClockCore.cs` 自身のクラス doc が最初から明記している帰結であり、
//! この変更が新たに生む落とし穴ではない。二重補正になり得る箇所(client 側に
//! `outputLatency` 由来の手動オフセットが別途存在しないか)は確認済み
//! (`AudioOffsetSeconds` はプレイヤー入力遅延の校正専用で、出力レイテンシ由来の値は
//! 一切含まない)。
//!
//! # 診断用プローブ(io probe)
//!
//! Control Center を開く等で `ios_interruption::OutputStallDetector` のウォッチドッグが
//! 止め直し→動かし直しを行ったあと、レンダーコールバックが
//! `in_number_frames`(希望値、通常 240 フレーム=5ms)より大きい値(観測上 2048 前後)の
//! まま戻らない状態に入ることがある(「バッファ長の項の出し方」節の既知の症状)。
//! この状態では `AudioTimeStamp.mHostTime` が実際に聞こえる時刻より遅れる側にずれる
//! らしく、校正済みの音楽クロックが実機でずれる(判定が FAST 側へ偏る)——との仮説を
//! 実機ログで裏取りするための最小限の診断値を、音声スレッドはアトミックへ書くだけ、
//! ゲームスレッドが日和見的に読んで1行出す構成で持つ:
//!
//! - `in_number_frames`([`AppleBackend::callback_frames`]。既存)。
//! - **lead**: このコールバックの相関点(`AudioTimeStamp.mHostTime` を ns 化した値)から、
//!   コールバックへ入った直後に読んだ [`crate::host_time::host_time_ns`] を引いた差
//!   (ns、符号付き)。`mHostTime` が「今」からどれだけ離れているかを示す
//!   ([`CallbackContext::io_probe_lead_ns`])。
//! - **mSampleTime の連続性**: 今回の `AudioTimeStamp.sample_time` から「前回の
//!   `sample_time` + 前回のフレーム数」を引いた差(フレーム数、符号付き)。0 なら
//!   コールバックが途切れなく続いている([`CallbackContext::io_probe_sample_time_gap_frames`])。
//! - `AudioTimeStamp.flags`(生のビットマスク、[`CallbackContext::io_probe_flags`])。
//!
//! ログの口は新設せず、既存の[`Backend::log_new_output_underruns`]
//! (ゲームスレッドから日和見的に呼ばれる既存の口。`client` 側は
//! `mw_get_output_underrun_stats` を1秒に1回呼ぶことでこれを駆動する)に相乗りする。
//! 1行だけ出す条件は「`restart_epoch` が変わった直後」「`in_number_frames` が
//! 変わったとき」「前回のログから5秒以上経過したとき」のいずれか
//! ([`should_log_io_probe`]、純関数)。文言の頭は
//! `[mw-backend] (native/apple) io probe:` に固定してある(実機ログを `grep` する前提)。
//!
//! # オーバーサイズのコールバックからの直し(iOS/tvOS)
//!
//! 上の診断で捉えようとしている状態そのものへの対処。`in_number_frames` が
//! `AVAudioSession.IOBufferDuration()` から換算した期待フレーム数の2倍を超える状態が
//! 連続して続いていれば([`is_oversized_callback`]/[`should_rebuild_for_oversized_callbacks`]、
//! いずれも純関数)、**安全な時点でだけ** [`UnitControl::rebuild`] で出力を作り直して
//! 普段の状態へ戻す([`UnitControl::rebuild_if_oversized_callbacks`])。安全な時点は2箇所に
//! 絞ってある(曲の再生中〔世代をまたがない途中〕には作り直さない——音楽クロックが跳ぶため):
//!
//! 1. 曲の再生予約の直前([`Backend::refresh_output_latency`] が呼ばれる口。
//!    すでにゲームスレッド)。
//! 2. `OutputStalled` 等の復帰(止め直し・動かし直し、または最後の手段としての作り直し)が
//!    コールバックの前進で確認できた直後(`ios_interruption::attempt_recovery` の復帰確認
//!    ワーカースレッド)で、確認した時点でアプリが背面(`Backgrounded`)にいなかったとき。
//!
//! macOS は `io_buffer_duration_ns` が常に0のため期待フレーム数も常に0になり、
//! この仕組みは target_os の分岐無しで実質的に no-op になる(`is_oversized_callback` の
//! ドキュメント参照)。

use std::ffi::c_void;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mw_core::{CHANNELS, EventQueue, MusicClockPublisher, Renderer};

use crate::backend::{Backend, BackendError};
use crate::host_time::mach_ticks_to_ns;
use crate::ios_interruption::{self, RecoverableOutput};
use crate::underrun::OutputUnderrunTracker;

// ============================================================================
// AudioToolbox の FFI 宣言(自前。新しいクレートは追加しない)
// ============================================================================

/// `AudioComponent`(`struct ComponentRecord *` の不透明ポインタ)。
type AudioComponent = *mut c_void;
/// `AudioComponentInstance` / `AudioUnit`(どちらも `struct
/// OpaqueAudioComponentInstance *` の不透明ポインタ。CoreAudio ヘッダでは
/// `typedef AudioComponentInstance AudioUnit;`)。
type AudioUnit = *mut c_void;

/// `AudioComponentDescription`(`AudioComponent.h`)。
#[repr(C)]
#[allow(dead_code)] // レイアウトを合わせるためだけに持つフィールドがある(常に 0 を書くだけ)
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

/// `AudioStreamBasicDescription`(`CoreAudioBaseTypes.h`)。f32 インターリーブ・
/// ステレオの固定フォーマットを表すためだけに使う(`stereo_f32_asbd` 参照)。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

/// `SMPTETime`(`CoreAudioBaseTypes.h`)。`AudioTimeStamp` のレイアウトを合わせるためだけに
/// 持つ——内容は一切読まない(レンダーコールバックは `mHostTime` だけを使う)。
#[repr(C)]
#[allow(dead_code)]
struct SmpteTime {
    subframes: i16,
    subframe_divisor: i16,
    counter: u32,
    time_type: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

/// `AudioTimeStamp`(`CoreAudioBaseTypes.h`)。読むのは `host_time` のみ
/// (他のフィールドはレイアウトを合わせるためだけに持つ)。
#[repr(C)]
#[allow(dead_code)]
struct AudioTimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte_time: SmpteTime,
    flags: u32,
    reserved: u32,
}

/// `AudioBuffer`(`CoreAudioBaseTypes.h`)。読むのは `data`/`data_byte_size` のみ
/// (`number_channels` はインターリーブ形式では使わない——`AudioBufferList` 1個に
/// チャンネル分すべてが入っている)。
#[repr(C)]
#[allow(dead_code)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

/// `AudioBufferList`(`CoreAudioBaseTypes.h`)。本来は可変長配列だが、インターリーブ形式
/// (`kAudioFormatFlagIsNonInterleaved` を立てない)では常に `number_buffers == 1` になる
/// (`render_proc` が念のため確認する)。
#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

/// `AudioObjectPropertyAddress`(`AudioHardwareBase.h`、CoreAudio フレームワーク)。
/// [`AudioObjectGetPropertyData`] でデバイス側のプロパティ(出力遅延の見積もり、
/// `query_device_extra_latency_frames` 参照)を読むためだけに使う。
///
/// **macOS のみ。** iOS/tvOS には `AudioObject`(複数デバイスを管理する HAL)の概念が無く、
/// 出力レイテンシは `AVAudioSession.outputLatency()` で読む(`query_ios_output_latency_ns`)。
#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

/// `AURenderCallback`(`AUComponent.h`)。戻り値は `OSStatus`。
type AURenderCallback = unsafe extern "C" fn(
    *mut c_void,
    *mut u32,
    *const AudioTimeStamp,
    u32,
    u32,
    *mut AudioBufferList,
) -> i32;

/// `AURenderCallbackStruct`(`AudioUnitProperties.h`)。
#[repr(C)]
struct AURenderCallbackStruct {
    input_proc: AURenderCallback,
    input_proc_ref_con: *mut c_void,
}

const AUDIO_UNIT_TYPE_OUTPUT: u32 = 0x6175_6f75; // 'auou'

/// 使うコンポーネントのサブタイプ。macOS は既定の出力デバイスへ自動追従する
/// `kAudioUnitSubType_DefaultOutput`、iOS/tvOS はハードウェア I/O への唯一の経路である
/// `kAudioUnitSubType_RemoteIO`(モジュール doc 参照。4-1→4-2 の差分はこの値だけ)。
#[cfg(target_os = "macos")]
const AUDIO_UNIT_SUBTYPE: u32 = 0x6465_6620; // 'def '
#[cfg(any(target_os = "ios", target_os = "tvos"))]
const AUDIO_UNIT_SUBTYPE: u32 = 0x7269_6f63; // 'rioc'

const AUDIO_UNIT_MANUFACTURER_APPLE: u32 = 0x6170_706c; // 'appl'

/// `kAudioUnitScope_Global`。**macOS のみ**(`query_current_device_id` の
/// `kAudioOutputUnitProperty_CurrentDevice` 読み出しにしか使わない)。
#[cfg(target_os = "macos")]
const AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;
const AUDIO_UNIT_SCOPE_INPUT: u32 = 1;
const AUDIO_UNIT_SCOPE_OUTPUT: u32 = 2;

const AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
const AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
/// `kAudioOutputUnitProperty_CurrentDevice`(`AudioUnitProperties.h`)。この AUHAL が
/// 実際に使っている `AudioDeviceID`(スコープ Global、値型 `AudioObjectID`)を読む——
/// `query_device_extra_latency_frames` がデバイス側のプロパティを読むために使う。
/// **macOS のみ**(iOS/tvOS の RemoteIO には「現在のデバイス」という概念が無い)。
#[cfg(target_os = "macos")]
const AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE: u32 = 2000;

const AUDIO_FORMAT_LINEAR_PCM: u32 = 0x6c70_636d; // 'lpcm'
const AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;

/// `<MacTypes.h>` の `noErr`。
const NO_ERR: i32 = 0;

// `AudioObjectGetPropertyData` で読むデバイス側のプロパティ(`AudioHardwareBase.h`、
// CoreAudio フレームワーク)。cpal 0.18.1 の `get_device_extra_latency_frames`
// (`cpal::host::coreaudio::macos::device`)が読んでいるのと同じ2つ——
// `query_device_extra_latency_frames` のドキュメント参照。**macOS のみ。**
#[cfg(target_os = "macos")]
const AUDIO_DEVICE_PROPERTY_LATENCY: u32 = 0x6c74_6e63; // 'ltnc'
#[cfg(target_os = "macos")]
const AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET: u32 = 0x7361_6674; // 'saft'
#[cfg(target_os = "macos")]
const AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT: u32 = 0x6f75_7470; // 'outp'
#[cfg(target_os = "macos")]
const AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;

#[allow(non_snake_case)] // シンボル名は AudioToolbox の実際の C API 名そのまま
#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(
        in_component: AudioComponent,
        in_desc: *const AudioComponentDescription,
    ) -> AudioComponent;

    fn AudioComponentInstanceNew(in_component: AudioComponent, out_instance: *mut AudioUnit)
    -> i32;

    fn AudioComponentInstanceDispose(in_instance: AudioUnit) -> i32;

    fn AudioUnitInitialize(in_unit: AudioUnit) -> i32;
    fn AudioUnitUninitialize(in_unit: AudioUnit) -> i32;

    fn AudioUnitGetProperty(
        in_unit: AudioUnit,
        in_id: u32,
        in_scope: u32,
        in_element: u32,
        out_data: *mut c_void,
        io_data_size: *mut u32,
    ) -> i32;

    fn AudioUnitSetProperty(
        in_unit: AudioUnit,
        in_id: u32,
        in_scope: u32,
        in_element: u32,
        in_data: *const c_void,
        in_data_size: u32,
    ) -> i32;

    fn AudioOutputUnitStart(ci: AudioUnit) -> i32;
    fn AudioOutputUnitStop(ci: AudioUnit) -> i32;
}

// デバイス(`AudioObject`)側のプロパティを読むためだけの別フレームワーク
// (`AudioObjectGetPropertyData` は CoreAudio.framework が export する——AudioToolbox
// ではない。`nm` で確認済み)。**macOS のみ**(iOS/tvOS には `AudioObject` の HAL が無い)。
#[cfg(target_os = "macos")]
#[allow(non_snake_case)]
#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        in_object_id: u32,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        io_data_size: *mut u32,
        out_data: *mut c_void,
    ) -> i32;
}

// ============================================================================
// 純関数部分(ハードウェア無しで単体テストできる)
// ============================================================================

/// デバイスが見つからない等でハードウェアのレートを読めなかったときのフォールバック。
const FALLBACK_SAMPLE_RATE_HZ: f64 = 48_000.0;

/// f32 インターリーブ・ステレオの `AudioStreamBasicDescription` を組み立てる。
///
/// ネイティブエンディアン(Apple Silicon / Intel のいずれも little-endian)を前提に
/// `kAudioFormatFlagIsBigEndian` は立てない。`kAudioFormatFlagIsNonInterleaved` も
/// 立てないため、レンダーコールバックに渡る `AudioBufferList` は常に
/// `number_buffers == 1`(全チャンネルが1本のバッファにインターリーブされる)。
fn stereo_f32_asbd(sample_rate: f64) -> AudioStreamBasicDescription {
    let bytes_per_frame = (CHANNELS * std::mem::size_of::<f32>()) as u32;
    AudioStreamBasicDescription {
        sample_rate,
        format_id: AUDIO_FORMAT_LINEAR_PCM,
        format_flags: AUDIO_FORMAT_FLAG_IS_FLOAT | AUDIO_FORMAT_FLAG_IS_PACKED,
        // リニア PCM は1フレーム = 1パケット。
        bytes_per_packet: bytes_per_frame,
        frames_per_packet: 1,
        bytes_per_frame,
        channels_per_frame: CHANNELS as u32,
        bits_per_channel: 32,
        reserved: 0,
    }
}

/// レンダーコールバックが受け取った `AudioBuffer` へ安全に書き込める要素数(f32 の個数)を
/// 返す。null ポインタ・サイズ不足・0 フレーム・非アラインポインタはいずれも `None`
/// (呼び出し側は書き込みをスキップし、音声スレッドを絶対にパニックさせない。§4.8 の思想)。
///
/// `data` はポインタそのものを受け取る(値は読まない——アドレス値の null/アラインメント
/// チェックのみに使う。安全性は呼び出し側〔`render_proc`〕が呼ぶまで保たれる)。
///
/// 0 フレームは `data` の値を一切見ずに早期 `None` にする——書き込むものが無い以上、
/// 後続の `slice::from_raw_parts_mut`(ゼロ長でも非 null・アラインメント済みを要求する)
/// へその条件を満たしているか確認する必要すら無い。非 0 フレームでは、CoreAudio が通常
/// 16byte 境界に揃えて渡す前提はあるものの、契約違反(ダングリング・非アラインポインタ)を
/// 機械的にも弾いておく。
fn validated_sample_count(frames: u32, data: *mut c_void, data_byte_size: u32) -> Option<usize> {
    if frames == 0 {
        return None;
    }
    if data.is_null() {
        return None;
    }
    if !(data as usize).is_multiple_of(std::mem::align_of::<f32>()) {
        return None;
    }
    let samples = (frames as usize).checked_mul(CHANNELS)?;
    let needed_bytes = samples.checked_mul(std::mem::size_of::<f32>())?;
    if (data_byte_size as usize) < needed_bytes {
        return None;
    }
    Some(samples)
}

/// `callback_host_time_ns`(このレンダーコールバックの `AudioTimeStamp.mHostTime` を
/// ns 化した値)から、`Renderer::render` に渡す `buffer_start_host_time_ns`
/// (予測される出力時刻)と診断用の `output_latency_ns` を導く。
///
/// cpal 版(`cpal_backend::build_output_stream`)は `OutputStreamTimestamp` の
/// `playback`(予測出力時刻)と `callback`(コールバック自体の時刻)の差を、
/// デバイスの実バッファ長 + 追加レイテンシから見積もっている。AUHAL の
/// `AURenderCallback` は `mHostTime` をコールバック起動の基準時刻として渡してくるだけで
/// 予測出力時刻そのものは返さないため、ここでは **(1) このバッファの再生に要る時間
/// (`buffer_duration_ns`)+ (2) デバイス側が申告する追加レイテンシ
/// (`device_extra_latency_ns`)** を足したものを予測出力時刻として扱う。
///
/// **この関数自身は両方の値がどう算出されたかを問わない**(ただの加算)——
/// 呼び出し側(`render_proc`)が OS ごとに異なる出し方で2つの値を用意する
/// (モジュール doc「タイムスタンプの扱い」参照: `device_extra_latency_ns` は macOS が
/// `open()` 時の1回だけ・iOS/tvOS がルート変化等のたびに読み直す補正項、
/// `buffer_duration_ns` は macOS がこのコールバックの実測フレーム数から・iOS/tvOS が
/// `AVAudioSession.IOBufferDuration()` から求める値)。この分離により、2つの入力を
/// 直接指定するだけで target を問わず単体テストできる。
fn compute_timestamps(
    callback_host_time_ns: u64,
    device_extra_latency_ns: u64,
    buffer_duration_ns: u64,
) -> (u64, u64) {
    let output_latency_ns = device_extra_latency_ns.saturating_add(buffer_duration_ns);
    let buffer_start_host_time_ns = callback_host_time_ns.saturating_add(output_latency_ns);
    (buffer_start_host_time_ns, output_latency_ns)
}

/// frame 数を ns へ変換する(cpal 0.18.1 `host::frames_to_duration` と同じ式——
/// 分母が立たない場合〔`sample_rate == 0`〕は 0 を返すのも含めて一致させてある)。
/// 純関数——ハードウェア不要で単体テストできる(macOS でのみコンパイルされる。
/// 用途は2つ: `query_device_extra_latency_frames` が返す frame 数の ns 化〔macOS の
/// 補正項〕と、`render_proc` が毎コールバック求める macOS のバッファ長の項
/// (`in_number_frames` をそのつど ns 化する。[`compute_timestamps`] の
/// `buffer_duration_ns` 引数)——iOS/tvOS はどちらも `AVAudioSession` の秒の値を
/// [`seconds_to_ns`] で直接 ns 化するため、こちらは呼ばない)。
#[cfg(target_os = "macos")]
fn extra_latency_frames_to_ns(frames: u32, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    (frames as u64 * 1_000_000_000) / sample_rate as u64
}

/// 秒(`f64`)を ns(`u64`)へ変換する。iOS/tvOS の `query_ios_output_latency_ns` が
/// `AVAudioSession.outputLatency()`(秒)を ns 化するために使う——`AVAudioSession` の
/// ゲッタは NSError を返さないが、`f64` の異常値(負・NaN・無限大)は音声スレッドに渡る
/// 前にここで弾く(§4.8 の思想。マイナスの出力レイテンシは意味を持たないので 0 にする)。
/// 純関数——ハードウェア不要で単体テストできる(OS を問わずコンパイルされる。
/// iOS/tvOS の実機呼び出し元〔`query_ios_output_latency_ns`〕だけが target_os 限定)。
///
/// `pub` にしてある理由は実装上の都合: macOS ビルドでは実際の呼び出し元
/// (`query_ios_output_latency_ns`)が `#[cfg]` で存在しないため、非公開のままだと
/// `dead_code` が立つ(`ios_interruption::confirm_recovery_progress` 等が同じ理由で
/// `pub` にしてあるのと同じ事情)。
pub fn seconds_to_ns(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    (seconds * 1_000_000_000.0) as u64
}

/// ns の時間幅を、サンプルレートで割ってフレーム数へ変換する。モジュール doc「オーバーサイズの
/// コールバックからの直し」の期待フレーム数(`io_buffer_duration_ns` から求める)を導くために
/// 使う。`sample_rate == 0`(未確定)なら 0(致命傷にしない。§4.8 の思想)。`u32::MAX` を
/// 超える結果は `u32::MAX` に飽和させる(診断用の値であり、ここで異常終了させる必要は無い)。
/// 純関数——ハードウェア不要で単体テストできる。
fn ns_to_frames(duration_ns: u64, sample_rate: u32) -> u32 {
    if sample_rate == 0 {
        return 0;
    }
    let frames = duration_ns.saturating_mul(sample_rate as u64) / 1_000_000_000;
    frames.min(u32::MAX as u64) as u32
}

/// このコールバックの `in_number_frames` が、バッファ長から期待されるフレーム数
/// (`expected_frames`、[`ns_to_frames`] が `io_buffer_duration_ns` から求める)の2倍を
/// 超えているか。`expected_frames == 0`(macOS。モジュール doc「オーバーサイズの
/// コールバックからの直し」参照、または iOS/tvOS でまだ一度も `IOBufferDuration` を
/// 読んでいない)では判定しない(意味のある基準が無いため)。純関数——ハードウェア不要で
/// 単体テストできる。
fn is_oversized_callback(in_number_frames: u32, expected_frames: u32) -> bool {
    expected_frames > 0 && in_number_frames > expected_frames.saturating_mul(2)
}

/// [`is_oversized_callback`] が連続して何回 `true` を返したか
/// (`CallbackContext::consecutive_oversized_callbacks`)が、出力を作り直すべき閾値に
/// 達したか。
///
/// 【仮】閾値: iOS/tvOS の既定 `IOBufferDuration`(5ms)に対し通常のコールバック間隔
/// (`crate::underrun::OutputUnderrunTracker` と同様、実測ジッタは1ms未満)を踏まえ、
/// 一時的な単発の揺れでは誤発火しない程度に大きく、かつ実機症状(押しっぱなしだと
/// 判定が FAST に倒れ続ける)を長時間放置しない程度に小さい値として選んだ——根拠は
/// 実機ログで裏取りが取れたら見直す前提の値。純関数——ハードウェア不要で単体テストできる。
const OVERSIZED_CALLBACK_REBUILD_THRESHOLD: u32 = 20;

fn should_rebuild_for_oversized_callbacks(consecutive_oversized_callbacks: u32) -> bool {
    consecutive_oversized_callbacks >= OVERSIZED_CALLBACK_REBUILD_THRESHOLD
}

/// モジュール doc「診断用プローブ(io probe)」のログを今回出すべきか。
/// 「`restart_epoch` が変わった直後」「`in_number_frames` が変わったとき」「前回の
/// ログから [`IO_PROBE_LOG_INTERVAL_NS`] 以上経過したとき」のいずれかに該当すれば
/// `true`(複数該当しても1行だけ出す——呼び出し側がこの戻り値で1回だけ判断するため)。
/// 純関数——ハードウェア不要で単体テストできる。
fn should_log_io_probe(restart_epoch_changed: bool, frames_changed: bool, elapsed_ns: u64) -> bool {
    restart_epoch_changed || frames_changed || elapsed_ns >= IO_PROBE_LOG_INTERVAL_NS
}

/// [`should_log_io_probe`] の時間トリガ。頻度を抑えつつ、通常ライブ1曲(数分)の間に
/// 複数回は必ず出る程度の間隔として5秒を選んだ(【仮】)。
const IO_PROBE_LOG_INTERVAL_NS: u64 = 5_000_000_000;

// ============================================================================
// オープン済みユニットへの問い合わせ(実デバイス/実セッション必須。単体テスト対象外)
// ============================================================================

/// 既定出力デバイスの実際の(ハードウェア側)サンプルレートを読む。取得できなければ
/// [`FALLBACK_SAMPLE_RATE_HZ`] にフォールバックする(致命傷にしない。§4.8 の思想)。
///
/// `kAudioUnitScope_Output`(ハードウェア側)の `StreamFormat` を読むことで、アプリ側
/// (`kAudioUnitScope_Input` 側)に設定するフォーマットのサンプルレートをハードウェアへ
/// 合わせる——不一致のまま進めると AUHAL が暗黙にサンプルレート変換を挟むことになり、
/// 不要な遅延・音質劣化の余地を生む。
fn query_output_sample_rate(unit: AudioUnit) -> f64 {
    let mut asbd = stereo_f32_asbd(0.0);
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    // SAFETY: `unit` は呼び出し元が生成した有効なインスタンス。`asbd`/`size` は
    // スタック上の有効な書き込み先で、`size` は `asbd` の実サイズと一致する。
    let status = unsafe {
        AudioUnitGetProperty(
            unit,
            AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
            AUDIO_UNIT_SCOPE_OUTPUT,
            0,
            &mut asbd as *mut _ as *mut c_void,
            &mut size,
        )
    };
    if status == NO_ERR && asbd.sample_rate.is_finite() && asbd.sample_rate > 0.0 {
        asbd.sample_rate
    } else {
        FALLBACK_SAMPLE_RATE_HZ
    }
}

/// AUHAL が実際に使っている `AudioDeviceID`(`kAudioOutputUnitProperty_CurrentDevice`、
/// スコープ Global)を読む。取得できなければ `None`
/// (`query_device_extra_latency_frames` はこれを「申告なし」として extra latency を
/// 0 にする)。**macOS のみ**(iOS/tvOS は `query_ios_output_latency_ns` を使う)。
#[cfg(target_os = "macos")]
fn query_current_device_id(unit: AudioUnit) -> Option<u32> {
    let mut device_id: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `unit` は呼び出し元が生成した有効なインスタンス。`device_id`/`size` は
    // スタック上の有効な書き込み先で、`size` は `device_id` の実サイズと一致する。
    let status = unsafe {
        AudioUnitGetProperty(
            unit,
            AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE,
            AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            &mut device_id as *mut _ as *mut c_void,
            &mut size,
        )
    };
    if status == NO_ERR {
        Some(device_id)
    } else {
        None
    }
}

/// `device_id` の device-level プロパティ(frame 数)を `AudioObjectGetPropertyData` で
/// 読む。スコープは常に出力側(`AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT`)。取得できなければ
/// 0(cpal 0.18.1 の `get_device_extra_latency_frames` が `.unwrap_or(0)` にしているのと
/// 同じ扱い)。**macOS のみ。**
#[cfg(target_os = "macos")]
fn query_device_property_frames(device_id: u32, selector: u32) -> u32 {
    let address = AudioObjectPropertyAddress {
        selector,
        scope: AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
        element: AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `address`/`value`/`size` はスタック上の有効な値。`AudioObjectGetPropertyData`
    // はこれらを読み書きするだけで、所有権の移動は無い。`size` は `value` の実サイズと
    // 一致する。
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            &mut value as *mut _ as *mut c_void,
        )
    };
    if status == NO_ERR { value } else { 0 }
}

/// cpal 0.18.1 の `get_device_extra_latency_frames`
/// (`cpal::host::coreaudio::macos::device`、`~/.cargo/registry/src/*/cpal-0.18.1/
/// src/host/coreaudio/macos/device.rs`)と**同じ2プロパティの合計**
/// (`kAudioDevicePropertyLatency` + `kAudioDevicePropertySafetyOffset`、いずれも
/// 出力側 = `kAudioObjectPropertyScopeOutput`)を返す。cpal はこれを AUHAL の
/// `AudioUnitGetProperty` へ device-level のプロパティ ID のまま渡している(AUHAL が
/// アドレス変換して実デバイスへ転送する挙動に乗っている)が、ここでは
/// `kAudioOutputUnitProperty_CurrentDevice` で実デバイスの `AudioObjectID` を取り、
/// それへ直接 `AudioObjectGetPropertyData` を呼ぶ——読む対象のプロパティは同じなので
/// 値は一致するはずだが、AUHAL の転送という間接層を経由しない分、より素直に対応づけられる。
///
/// **`AudioUnitInitialize` の後**に呼ぶこと(`kAudioOutputUnitProperty_CurrentDevice` は
/// 初期化前は未確定の場合がある)。取得できなければ 0(致命傷にしない。§4.8 の思想)。
/// **macOS のみ。**
#[cfg(target_os = "macos")]
fn query_device_extra_latency_frames(unit: AudioUnit) -> u32 {
    let Some(device_id) = query_current_device_id(unit) else {
        return 0;
    };
    let device_latency_frames =
        query_device_property_frames(device_id, AUDIO_DEVICE_PROPERTY_LATENCY);
    let safety_offset_frames =
        query_device_property_frames(device_id, AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET);
    device_latency_frames.saturating_add(safety_offset_frames)
}

/// `AVAudioSession.outputLatency()`(秒)を ns 化して返す——iOS/tvOS の「補正項」の
/// 出し方そのもの(モジュール doc「タイムスタンプの扱い」参照)。[`AppleBackend::open`]
/// が初期化時に1度読み、[`refresh_ios_output_latency`] がそれ以外の読み直し箇所
/// (ルート変化・曲の再生予約・動かし直し・作り直し)から同じ関数を呼ぶ。
///
/// **バッファ長の項(`IOBufferDuration`)はここでは読まない**——そちらは
/// [`query_ios_io_buffer_duration_ns`] が別に持つ(モジュール doc「バッファ長の項の
/// 出し方」参照。実測フレーム数〔`in_number_frames`〕は使わない)。
///
/// `AVAudioSession` の呼び出しは NSError を返さない単純なゲッタ(`ios_session.rs` が
/// 同じ前提で呼んでいる)なので、失敗しうる分岐は無い——異常値のケア(負・NaN・無限大)は
/// [`seconds_to_ns`] が担う。実セッションが要るため単体テスト対象外。
#[cfg(any(target_os = "ios", target_os = "tvos"))]
fn query_ios_output_latency_ns() -> u64 {
    // SAFETY: `AVAudioSession::sharedInstance()` はプロセス唯一の共有インスタンスを返す
    // だけの呼び出しで、`outputLatency()` は NSError を返さない単純なゲッタ。
    let seconds = unsafe { objc2_avf_audio::AVAudioSession::sharedInstance().outputLatency() };
    seconds_to_ns(seconds)
}

/// `AVAudioSession.IOBufferDuration()`(秒)を ns 化して返す——iOS/tvOS の
/// 「バッファ長の項」の出し方そのもの(モジュール doc「バッファ長の項の出し方」参照)。
/// [`query_ios_output_latency_ns`] と**全く同じ呼び出し元・同じタイミング**
/// (`AppleBackend::open`・[`refresh_ios_output_latency`] の4箇所)で読む——
/// 片方だけ読み直して世代の整合が崩れることを避けるため、常にこの2関数を
/// セットで呼ぶ([`refresh_ios_output_latency`] 参照)。
///
/// `AVAudioSession` の呼び出しは NSError を返さない単純なゲッタなので、失敗しうる
/// 分岐は無い——異常値のケアは [`seconds_to_ns`] が担う。実セッションが要るため
/// 単体テスト対象外。
#[cfg(any(target_os = "ios", target_os = "tvos"))]
fn query_ios_io_buffer_duration_ns() -> u64 {
    // SAFETY: `AVAudioSession::sharedInstance()` はプロセス唯一の共有インスタンスを返す
    // だけの呼び出しで、`IOBufferDuration()` は NSError を返さない単純なゲッタ。
    let seconds = unsafe { objc2_avf_audio::AVAudioSession::sharedInstance().IOBufferDuration() };
    seconds_to_ns(seconds)
}

/// 補正項(`device_extra_latency_ns`)を [`query_ios_output_latency_ns`] で、
/// バッファ長の項(`io_buffer_duration_ns`)を [`query_ios_io_buffer_duration_ns`] で
/// それぞれ読み直して書き込み、両方の変化をログへ出す——ルート変化
/// (`ios_impl::Watcher`)・曲の再生予約(`AppleBackend::refresh_output_latency`)・
/// 動かし直し(`UnitControl::play`)・作り直し(`UnitControl::rebuild`)の4箇所が
/// 共有する実体(モジュール doc「タイムスタンプの扱い」参照)。常に両方を同じタイミングで
/// 読み直す——片方だけ読み直すと、音声スレッドの世代判定(`render_proc` の
/// `last_device_extra_latency_ns`/`last_io_buffer_duration_ns`)がどちらの値を
/// 基準に世代を進めたか追いづらくなるため。
///
/// `reason` はどの経路からの読み直しかをログに残すための短い文言(例:
/// `"music schedule"`)。
///
/// **呼び出し元はゲームスレッド・通知ハンドラ・復帰ワーカーといった非リアルタイムスレッド
/// であること**(`mw_log!` はアロケーションとロックを伴う。音声スレッドから呼んでは
/// ならない)。
#[cfg(any(target_os = "ios", target_os = "tvos"))]
fn refresh_ios_output_latency(
    device_extra_latency_ns: &AtomicU64,
    io_buffer_duration_ns: &AtomicU64,
    sample_rate: u32,
    reason: &str,
) {
    let previous_latency_ns = device_extra_latency_ns.load(Ordering::Relaxed);
    let new_latency_ns = query_ios_output_latency_ns();
    device_extra_latency_ns.store(new_latency_ns, Ordering::Relaxed);

    let previous_buffer_ns = io_buffer_duration_ns.load(Ordering::Relaxed);
    let new_buffer_ns = query_ios_io_buffer_duration_ns();
    io_buffer_duration_ns.store(new_buffer_ns, Ordering::Relaxed);

    crate::mw_log!(
        "[mw-backend] (native/apple ios) output latency correction refreshed ({reason}): \
         output_latency {:.3} ms -> {:.3} ms, io_buffer_duration (used as the buffer-length \
         term for the predicted output time, not in_number_frames) {:.3} ms -> {:.3} ms \
         (sample_rate={sample_rate} Hz)",
        previous_latency_ns as f64 / 1_000_000.0,
        new_latency_ns as f64 / 1_000_000.0,
        previous_buffer_ns as f64 / 1_000_000.0,
        new_buffer_ns as f64 / 1_000_000.0,
    );
}

// ============================================================================
// レンダーコールバック(音声スレッド)
// ============================================================================

/// レンダーコールバック(音声スレッド)が排他的に触る状態。[`AppleBackend::open`] が
/// `Box::into_raw` でリーク相当にしたポインタを `AURenderCallbackStruct::
/// input_proc_ref_con` として渡し、[`AppleBackend::close`] が `Box::from_raw` で回収する
/// (cpal 版の `renderer` ムーブと同じ「単一の書き手」設計。§5.3)。
struct CallbackContext {
    renderer: Renderer,
    /// オープン時に確定したサンプルレート(オープン中は変わらない)。
    sample_rate: u32,
    /// 補正項(出力レイテンシ、ns)。[`AppleBackend::device_extra_latency_ns`] と同じ
    /// `Arc` を指す——macOS は `open()` が `AudioUnitInitialize` の直後に1度だけ
    /// `query_device_extra_latency_frames` で書き、以後固定(4-1 の既知の差分)。
    /// iOS/tvOS は `open()` が `query_ios_output_latency_ns` で初期化し、
    /// `IosOutputLatencyWatcher` がルート変化のたびに書き直す(4-2、HANDOFF 0f)。
    /// [`compute_timestamps`] の「相関点」(このコールバックの `AudioTimeStamp.
    /// mHostTime`)とは別に持つ——音声コールバックは毎回 `Relaxed` で読むだけ。
    device_extra_latency_ns: Arc<AtomicU64>,
    /// `render_proc` がこのコールバックの直前までに観測していた
    /// `device_extra_latency_ns` の値。**音声スレッド専用**(他スレッドからは一切
    /// 触らない単純なフィールドで、atomic にしていない——単一の書き手である
    /// `render_proc` 自身が毎コールバック読み書きするだけなので § 5.3 の対象にはならない)。
    ///
    /// レビュー【高】の修正: 補正項(`device_extra_latency_ns`)は iOS/tvOS では
    /// `IosOutputLatencyWatcher` がルート変化のたびに別スレッドから書き換える
    /// (`AppleBackend::open` のドキュメント参照)。`render_proc` はこの値を毎コールバック
    /// 読んで [`compute_timestamps`] の「補正項」として使うが、変化を検知しないまま
    /// 使うと `host_time_ns` が同じ世代のまま飛んでしまい、`clock.rs::
    /// MusicClockSnapshot` の契約(「世代を跨いだ外挿をしてはならない」)に反する。
    /// この直前値と毎コールバック比較し、変化していれば [`MusicClockPublisher::
    /// bump_generation`] を呼んでから新しい補正項で `buffer_start_host_time_ns` を
    /// 計算する(`render_proc` 本体参照)。macOS はここが実質的に no-op——
    /// `device_extra_latency_ns` は `open()` 時に1度書いたあと固定なので、この値と
    /// 毎回一致し続け、世代は一度も進まない(4-1 の既知の差分は変えない)。
    last_device_extra_latency_ns: u64,
    /// バッファ長の項(ns)。[`AppleBackend::io_buffer_duration_ns`] と同じ `Arc` を
    /// 指す——macOS では誰も書かず常に 0 のまま(macOS は `render_proc` がこの値を
    /// 読まず、このコールバックの実測フレーム数から毎回計算する。モジュール doc
    /// 「バッファ長の項の出し方」参照)。iOS/tvOS は `open()` が
    /// `query_ios_io_buffer_duration_ns` で初期化し、[`refresh_ios_output_latency`]
    /// (ルート変化・曲の再生予約・動かし直し・作り直しの4箇所、
    /// `device_extra_latency_ns` と全く同じタイミング)が書き直す。
    io_buffer_duration_ns: Arc<AtomicU64>,
    /// `render_proc` が直前までに観測していた `io_buffer_duration_ns` の値。
    /// **音声スレッド専用**(`last_device_extra_latency_ns` と同じ理由・同じ扱い)。
    /// 変化を検知したら同様に世代を進める——macOS はこの値が常に 0 のまま変わらないので
    /// 実質的に no-op(`last_device_extra_latency_ns` の doc と同じ事情)。
    last_io_buffer_duration_ns: u64,
    /// 音楽クロックの発行ハンドル(`Mixer::music_clock_handle` と同じ `Arc` を指す)。
    /// `render_proc` が補正項の変化を検知したときに [`MusicClockPublisher::
    /// bump_generation`] を呼ぶためだけに使う——`renderer.render()` 自身が内部で
    /// 行う `bump_generation` 呼び出し(シーク等)と同じ音声スレッドから呼ぶので、
    /// seqlock の単一書き手前提は崩れない([`Mixer::music_clock_handle`] のドキュメント
    /// 参照)。
    music_clock: Arc<MusicClockPublisher>,
    /// 出力を動かし直した・作り直した回数([`UnitControl`] が書く)。
    /// [`AppleBackend::restart_epoch`] と同じ `Arc`。
    restart_epoch: Arc<AtomicU64>,
    /// `render_proc` が直前までに観測していた `restart_epoch`。**音声スレッド専用**
    /// (`last_device_extra_latency_ns` と同じ扱い)。変わっていたら、止まっていた間の
    /// ぶんホスト時刻の相関点が飛んでいるので世代を進める——補正項の変化と同じく、
    /// 同じ世代のまま跨いで外挿すると `MusicClockSnapshot` の契約に反する。
    last_restart_epoch: u64,
    /// モジュール doc「診断用プローブ(io probe)」の **lead**。[`AppleBackend::
    /// io_probe_lead_ns`] と同じ `Arc`。音声スレッドは毎コールバック上書きするだけ
    /// (ロック・アロケーション無し)。
    io_probe_lead_ns: Arc<AtomicI64>,
    /// 診断用プローブの **mSampleTime の連続性**(フレーム数、符号付き、四捨五入)。
    /// [`AppleBackend::io_probe_sample_time_gap_frames`] と同じ `Arc`。
    io_probe_sample_time_gap_frames: Arc<AtomicI64>,
    /// 診断用プローブの **flags**(`AudioTimeStamp.flags` の生のビットマスク)。
    /// [`AppleBackend::io_probe_flags`] と同じ `Arc`。
    io_probe_flags: Arc<AtomicU32>,
    /// `io_probe_sample_time_gap_frames` を求めるための、直前コールバックの
    /// `AudioTimeStamp.sample_time`。**音声スレッド専用**(`last_device_extra_latency_ns`
    /// と同じ理由——単一の書き手である `render_proc` 自身が毎コールバック読み書きするだけ)。
    last_probe_sample_time: f64,
    /// `io_probe_sample_time_gap_frames` を求めるための、直前コールバックのフレーム数。
    /// **音声スレッド専用**(`last_probe_sample_time` と同じ扱い)。
    last_probe_frames: u32,
    /// モジュール doc「オーバーサイズのコールバックからの直し」: バッファ長から期待される
    /// フレーム数の2倍を超えるコールバック([`is_oversized_callback`])が連続している回数。
    /// [`AppleBackend::consecutive_oversized_callbacks`]/[`UnitControl::
    /// consecutive_oversized_callbacks`] と同じ `Arc`——[`UnitControl::
    /// rebuild_if_oversized_callbacks`](ゲームスレッド・復帰確認ワーカーから安全な時点でだけ
    /// 呼ばれる)がこれを読んで作り直すかどうかを判定する。音声スレッドは毎コールバック
    /// `fetch_add`/`store` で更新するだけ(§5.3 に抵触しない)。
    consecutive_oversized_callbacks: Arc<AtomicU32>,
    underrun_tracker: OutputUnderrunTracker,
    /// [`AppleBackend::callback_frames`] へ渡す `Arc`。
    callback_frames: Arc<AtomicU32>,
    /// [`AppleBackend::output_latency_ns`] へ渡す `Arc`。
    output_latency_ns: Arc<AtomicU64>,
    /// [`AppleBackend::close`] がストリーム停止後に `Acquire` で読むための同期点
    /// (`cpal_backend::CpalBackend::render_completions` と同じ理由)。
    render_completions: Arc<AtomicU64>,
}

/// CoreAudio の音声スレッドから直接呼ばれる `AURenderCallback`。
///
/// # SAFETY(呼び出し元が守る契約)
///
/// - `in_ref_con` は [`AppleBackend::open`] が `Box::into_raw::<CallbackContext>` で渡した
///   ポインタのまま、[`AppleBackend::close`] が回収するまで有効——CoreAudio は
///   `AudioOutputUnitStop` が返った後にはこのコールバックを呼ばないことが前提
///   (`AppleBackend::close` のドキュメント参照)。
/// - `in_time_stamp`/`io_data` はこの呼び出しの間だけ有効な、CoreAudio 所有のポインタ。
///
/// パニックは FFI 境界(CoreAudio の C フレーム)の外へ絶対に漏らさない
/// (`mw-ffi/COMMON.md` の「不変条件」と同じ方針をここでも適用する)。`Renderer::render`
/// 自体はパニックしない契約(§5.3)だが、ここでは契約が破られた場合の最後の防波堤として
/// `catch_unwind` で包む——巻き戻りが実際に起きない限りアロケーション・ロックを
/// 行わないため、リアルタイム安全性規約には抵触しない。
unsafe extern "C" fn render_proc(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut u32,
    in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    let caught = panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: 呼び出し元の契約(関数 doc)により有効なポインタ。
        let context = unsafe { &mut *(in_ref_con as *mut CallbackContext) };

        // SAFETY: `io_data` は CoreAudio が渡す有効なポインタのはずだが、
        // defensively `as_mut` で確認してから使う(§4.8: 絶対にパニックしない)。
        let Some(io_data) = (unsafe { io_data.as_mut() }) else {
            return;
        };
        // インターリーブ形式(`kAudioFormatFlagIsNonInterleaved` を立てない、
        // `stereo_f32_asbd` 参照)なので `number_buffers` は常に1のはず。1以外は
        // (0 を含め)前提が崩れているとみなし、`buffers[0]` には一切触れずに書き込みを
        // スキップする。
        if io_data.number_buffers != 1 {
            return;
        }
        let buffer_data = io_data.buffers[0].data;
        let buffer_data_byte_size = io_data.buffers[0].data_byte_size;
        let Some(sample_count) =
            validated_sample_count(in_number_frames, buffer_data, buffer_data_byte_size)
        else {
            return;
        };
        // SAFETY: `validated_sample_count` が `sample_count * size_of::<f32>() <=
        // buffer_data_byte_size` を確認済み。`buffer_data` は CoreAudio が確保した
        // 16byte 境界に揃えられたバッファ(`AURenderCallback` のヘッダ doc)で、
        // インターリーブ形式(`kAudioFormatFlagIsNonInterleaved` 無し)なので
        // `number_buffers == 1` の単一バッファに全チャンネルが収まっている
        // (`stereo_f32_asbd` のドキュメント参照)。このコールバックの実行中のみ有効。
        let output =
            unsafe { std::slice::from_raw_parts_mut(buffer_data as *mut f32, sample_count) };

        context
            .callback_frames
            .store(in_number_frames, Ordering::Relaxed);

        // SAFETY: CoreAudio が渡す有効なポインタ(関数 doc の契約)。`host_time`/
        // `sample_time`/`flags` のいずれも、ここで読んだ参照からだけ導出する
        // (ポインタ自体の有効性はこの呼び出しの間だけ——関数 doc 参照)。
        let timestamp_ref = unsafe { in_time_stamp.as_ref() };
        let callback_host_time_ns = timestamp_ref
            .map(|ts| mach_ticks_to_ns(ts.host_time))
            .unwrap_or(0);

        // モジュール doc「診断用プローブ(io probe)」。`host_time_ns()` は
        // mach_absolute_time ベースで、ロック・アロケーション・システムコールを伴わない
        // (§5.3 に抵触しない)。コールバックへ入ってできるだけ早いタイミングで読むことで、
        // 以降の処理時間がノイズに乗らないようにしている。
        let probe_now_ns = crate::host_time::host_time_ns();
        context.io_probe_lead_ns.store(
            callback_host_time_ns as i64 - probe_now_ns as i64,
            Ordering::Relaxed,
        );
        let sample_time = timestamp_ref.map(|ts| ts.sample_time).unwrap_or(0.0);
        let timestamp_flags = timestamp_ref.map(|ts| ts.flags).unwrap_or(0);
        context
            .io_probe_flags
            .store(timestamp_flags, Ordering::Relaxed);
        let expected_sample_time =
            context.last_probe_sample_time + context.last_probe_frames as f64;
        context.io_probe_sample_time_gap_frames.store(
            (sample_time - expected_sample_time).round() as i64,
            Ordering::Relaxed,
        );
        context.last_probe_sample_time = sample_time;
        context.last_probe_frames = in_number_frames;

        let device_extra_latency_ns = context.device_extra_latency_ns.load(Ordering::Relaxed);
        let io_buffer_duration_ns = context.io_buffer_duration_ns.load(Ordering::Relaxed);
        let restart_epoch = context.restart_epoch.load(Ordering::Relaxed);
        if device_extra_latency_ns != context.last_device_extra_latency_ns
            || io_buffer_duration_ns != context.last_io_buffer_duration_ns
            || restart_epoch != context.last_restart_epoch
        {
            // 補正項・バッファ長の項が変わった(iOS/tvOS: `IosOutputLatencyWatcher`・
            // `refresh_ios_output_latency` が別スレッドで読み直し、書き換えた)。
            // 新しい相関点(このすぐ下の `compute_timestamps`)が確定する前に世代を
            // 進める——`MusicClockPublisher::bump_generation` のドキュメント「新しい
            // 相関点が確定する前に世代を進める」と同じ順序(`Mixer::render` 内部が
            // discontinuity を処理する順序とも揃える)。
            // 閾値は設けていない: どちらの値も `IosOutputLatencyWatcher` /
            // `refresh_ios_output_latency` が実際に読み直した結果だけが書き込む
            // (毎コールバック揺れる値ではない)ため、変化がどれだけ小さくても、それを
            // 同じ世代のまま跨いで外挿すると `MusicClockSnapshot` の契約違反になる
            // (`CallbackContext::last_device_extra_latency_ns`/
            // `last_io_buffer_duration_ns` のドキュメント参照)。
            // 出力を動かし直した・作り直した(`restart_epoch`)ときも同じ扱い——止まって
            // いた間のぶん相関点が飛ぶ(`CallbackContext::last_restart_epoch`)。3つが同時に
            // 変わっていても世代は1回だけ進める。
            context.music_clock.bump_generation();
            context.last_device_extra_latency_ns = device_extra_latency_ns;
            context.last_io_buffer_duration_ns = io_buffer_duration_ns;
            context.last_restart_epoch = restart_epoch;
        }

        // モジュール doc「オーバーサイズのコールバックからの直し」: バッファ長から期待される
        // フレーム数の2倍を超えるコールバックが連続した回数を数える(判定そのものは
        // ホストでテストできる純関数 `is_oversized_callback` に切り出してある)。macOS は
        // `io_buffer_duration_ns` が常に0なので `expected_io_buffer_frames` も常に0になり、
        // この仕組みは target_os の分岐無しで実質的に no-op になる。
        let expected_io_buffer_frames = ns_to_frames(io_buffer_duration_ns, context.sample_rate);
        if is_oversized_callback(in_number_frames, expected_io_buffer_frames) {
            context
                .consecutive_oversized_callbacks
                .fetch_add(1, Ordering::Relaxed);
        } else {
            context
                .consecutive_oversized_callbacks
                .store(0, Ordering::Relaxed);
        }

        // バッファ長の項: macOS はこのコールバックの実測フレーム数から毎回求める
        // (変更無し)。iOS/tvOS は直前に読んだ `io_buffer_duration_ns`(`AVAudioSession.
        // IOBufferDuration()` 由来)を使う——`in_number_frames` は使わない(モジュール doc
        // 「バッファ長の項の出し方」参照。Control Center 表示中の出力停止からの復帰後、
        // `in_number_frames` が希望値より大きい値のまま戻らないことがあり、実測値を
        // 使うと予測出力時刻が実際の出力より恒久的に遅れるため)。
        #[cfg(target_os = "macos")]
        let buffer_duration_ns = extra_latency_frames_to_ns(in_number_frames, context.sample_rate);
        #[cfg(any(target_os = "ios", target_os = "tvos"))]
        let buffer_duration_ns = io_buffer_duration_ns;

        let (buffer_start_host_time_ns, output_latency_ns) = compute_timestamps(
            callback_host_time_ns,
            device_extra_latency_ns,
            buffer_duration_ns,
        );
        context
            .output_latency_ns
            .store(output_latency_ns, Ordering::Relaxed);

        context.underrun_tracker.observe(
            callback_host_time_ns,
            in_number_frames,
            context.sample_rate,
        );

        // 音声スレッド上で呼ぶ mw-core 側の経路は `Renderer::render` と、直前の
        // `MusicClockPublisher::bump_generation`(呼ぶのはこのコールバック自身、つまり
        // 音声スレッドからのみ)に限る(`mw-backend/COMMON.md` の設計意図)。
        // いずれもロック・アロケーションを伴わない atomic ストアのみで、
        // リアルタイム安全性規約(§5.3)に抵触しない。
        context.renderer.render(output, buffer_start_host_time_ns);

        context.render_completions.fetch_add(1, Ordering::Release);
    }));

    match caught {
        Ok(()) => NO_ERR,
        Err(_) => {
            // 契約が破られた場合の最後の防波堤。バッファへは書き込まない(無音ではなく
            // 直前の内容が残る可能性があるが、クラッシュさせないことを優先する
            // ——§4.8 の思想と同じ)。
            NO_ERR
        }
    }
}

// ============================================================================
// AudioUnit の組み立て(`open` と作り直しで共用)
// ============================================================================

/// 出力用の AudioUnit のインスタンスを作る(まだ何も設定していない)。
fn new_output_unit() -> Result<AudioUnit, BackendError> {
    let description = AudioComponentDescription {
        component_type: AUDIO_UNIT_TYPE_OUTPUT,
        component_sub_type: AUDIO_UNIT_SUBTYPE,
        component_manufacturer: AUDIO_UNIT_MANUFACTURER_APPLE,
        component_flags: 0,
        component_flags_mask: 0,
    };

    // SAFETY: `description` はスタック上の有効な値。`AudioComponentFindNext` は
    // システムのコンポーネント登録簿を検索するだけで、所有権の移動は無い
    // (見つからなければ null を返す。コンポーネント自体は dispose 不要)。
    let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &description) };
    if component.is_null() {
        return Err(BackendError::NoOutputDevice);
    }

    let mut unit: AudioUnit = std::ptr::null_mut();
    // SAFETY: `component` は直前に取得した有効なハンドル。`&mut unit` はスタック上の
    // 有効な出力先。
    let status = unsafe { AudioComponentInstanceNew(component, &mut unit) };
    if status != NO_ERR || unit.is_null() {
        return Err(BackendError::BuildStreamFailed(format!(
            "AudioComponentInstanceNew failed: OSStatus {status}"
        )));
    }
    Ok(unit)
}

/// アプリ側(`kAudioUnitScope_Input`)のフォーマットを f32 インターリーブ・ステレオに
/// 設定する。ハードウェア側(`kAudioUnitScope_Output`)との差は AudioUnit が変換する。
/// 戻り値は `OSStatus`。
fn set_stream_format(unit: AudioUnit, sample_rate: f64) -> i32 {
    let asbd = stereo_f32_asbd(sample_rate);
    // SAFETY: `unit` は呼び出し元が生成した有効なインスタンス。`asbd` はスタック上の値で、
    // サイズを正しく渡している。
    unsafe {
        AudioUnitSetProperty(
            unit,
            AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
            AUDIO_UNIT_SCOPE_INPUT,
            0,
            &asbd as *const _ as *const c_void,
            std::mem::size_of::<AudioStreamBasicDescription>() as u32,
        )
    }
}

/// レンダーコールバック(`render_proc` + `context_ptr`)を設定する。戻り値は `OSStatus`。
/// コールバックが実際に呼ばれ始めるのは `AudioOutputUnitStart` の後。
fn set_render_callback(unit: AudioUnit, context_ptr: *mut CallbackContext) -> i32 {
    let callback_struct = AURenderCallbackStruct {
        input_proc: render_proc,
        input_proc_ref_con: context_ptr as *mut c_void,
    };
    // SAFETY: `unit` は呼び出し元が生成した有効なインスタンス。`callback_struct` は
    // スタック上の有効な値。
    unsafe {
        AudioUnitSetProperty(
            unit,
            AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
            AUDIO_UNIT_SCOPE_INPUT,
            0,
            &callback_struct as *const _ as *const c_void,
            std::mem::size_of::<AURenderCallbackStruct>() as u32,
        )
    }
}

/// 既にある context(`Renderer` を持つ)へつなぎ直す新しい AudioUnit を、初期化まで
/// 済ませて返す(開始はしない)。[`UnitControl::rebuild`] 専用。失敗したら作りかけの
/// ユニットは破棄して文言を返す。
fn build_unit_for_existing_context(
    sample_rate: f64,
    context_ptr: *mut CallbackContext,
) -> Result<AudioUnit, String> {
    let unit = new_output_unit().map_err(|e| e.to_string())?;
    let fail = |what: &str, status: i32| {
        // SAFETY: `unit` は直前に作ったばかりで、まだ開始していない(コールバックは
        // 一度も呼ばれていない)。
        unsafe {
            AudioComponentInstanceDispose(unit);
        }
        Err(format!("{what} failed: OSStatus {status}"))
    };
    let status = set_stream_format(unit, sample_rate);
    if status != NO_ERR {
        return fail("AudioUnitSetProperty(StreamFormat)", status);
    }
    let status = set_render_callback(unit, context_ptr);
    if status != NO_ERR {
        return fail("AudioUnitSetProperty(SetRenderCallback)", status);
    }
    // SAFETY: `unit` は構成済みの有効なインスタンス。
    let status = unsafe { AudioUnitInitialize(unit) };
    if status != NO_ERR {
        return fail("AudioUnitInitialize", status);
    }
    Ok(unit)
}

// ============================================================================
// 開いている AudioUnit の制御口(閉じる・止め直す・動かし直す・作り直す)
// ============================================================================

/// 開いている AudioUnit と、そのレンダーコールバックの context。
struct OpenUnit {
    unit: AudioUnit,
    context_ptr: *mut CallbackContext,
}

/// [`OpenUnit`] を `Mutex` の中に持ち、[`AppleBackend::close`] と iOS/tvOS の復帰
/// (`ios_interruption::Watcher` の通知ハンドラ・復帰確認のワーカー・ウォッチドッグ)の
/// 両方へ同じ口を渡す(モジュール doc「iOS/tvOS の割り込み・バックグラウンドからの復帰」)。
///
/// 🔴 **音声スレッドはこのロックに触れない**(`render_proc` は context だけを見る)。
/// ロックを取るのはゲームスレッド・通知ハンドラ・ワーカーといった非リアルタイムスレッドだけ。
///
/// `close` が [`OpenUnit`] を取り出した後は、すべての操作が何もせずエラーを返す——
/// 復帰確認のワーカーは `close` と同期しない(cpal 版でもストリームの `Arc` を握ったまま
/// 走り切る)ので、閉じたユニットへ Start を撃たないためにこの形にしてある。
struct UnitControl {
    slot: Mutex<Option<OpenUnit>>,
    /// 開いたときのサンプルレート。作り直しでもこの値のまま
    /// (`Renderer` 側のサンプルレートを変えずに済ませるため)。
    sample_rate: f64,
    /// [`AppleBackend::restart_epoch`] と同じ `Arc`。
    restart_epoch: Arc<AtomicU64>,
    /// 補正項。iOS/tvOS では動かし直し・作り直しの直前に読み直す
    /// (`refresh_ios_output_latency`)。
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    device_extra_latency_ns: Arc<AtomicU64>,
    /// バッファ長の項。`device_extra_latency_ns` と同じタイミングで
    /// `refresh_ios_output_latency` が読み直す(iOS/tvOS のみ)。
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    io_buffer_duration_ns: Arc<AtomicU64>,
    /// 古いユニットを捨ててから context を新しいユニットへ渡すときの同期点
    /// (`AppleBackend::close` と同じ理由)。
    render_completions: Arc<AtomicU64>,
    /// [`CallbackContext::consecutive_oversized_callbacks`] と同じ `Arc`。
    /// [`UnitControl::rebuild_if_oversized_callbacks`] がこれを読んで作り直すかどうかを
    /// 判定する(モジュール doc「オーバーサイズのコールバックからの直し」参照)。
    consecutive_oversized_callbacks: Arc<AtomicU32>,
}

// SAFETY: `OpenUnit` の生ポインタ(AudioUnit・context)へは必ず `slot` の `Mutex` を
// 通してしか触れない。AudioUnit の C API はどのスレッドから呼んでもよい。context の中身
// (`Renderer`)へはここからは一切触れず、ポインタ値をコールバックへ渡し直すだけ。
unsafe impl Send for UnitControl {}
unsafe impl Sync for UnitControl {}

const UNIT_CLOSED: &str = "the output unit is already closed";

impl UnitControl {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<OpenUnit>> {
        self.slot.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl RecoverableOutput for UnitControl {
    fn pause(&self) -> Result<(), String> {
        let guard = self.lock();
        let Some(open) = guard.as_ref() else {
            return Err(UNIT_CLOSED.to_owned());
        };
        // SAFETY: `open.unit` はロックの内側にある、まだ破棄していないインスタンス。
        let status = unsafe { AudioOutputUnitStop(open.unit) };
        if status == NO_ERR {
            Ok(())
        } else {
            Err(format!("AudioOutputUnitStop failed: OSStatus {status}"))
        }
    }

    fn play(&self) -> Result<(), String> {
        let guard = self.lock();
        let Some(open) = guard.as_ref() else {
            return Err(UNIT_CLOSED.to_owned());
        };

        // 背面にいる間・割り込みの間にルートが変わっていることがあるので、補正項を
        // 読み直してから動かす(最初のコールバックから新しい値を使わせる)。
        #[cfg(any(target_os = "ios", target_os = "tvos"))]
        refresh_ios_output_latency(
            &self.device_extra_latency_ns,
            &self.io_buffer_duration_ns,
            self.sample_rate as u32,
            "resume play()",
        );
        // 止まっていた間のぶんホスト時刻の相関点が飛ぶので、次のコールバックで世代を
        // 進めさせる(`CallbackContext::last_restart_epoch`)。
        self.restart_epoch.fetch_add(1, Ordering::Relaxed);

        // SAFETY: `open.unit` はロックの内側にある、初期化済みのインスタンス。
        let status = unsafe { AudioOutputUnitStart(open.unit) };
        if status == NO_ERR {
            return Ok(());
        }

        // Start が通らない(セッションの再アクティブ化の後などで初期化状態が崩れている)
        // ときは、初期化し直してからもう一度だけ Start する。
        // SAFETY: Start に失敗した = コールバックは動いていない。プロパティ(フォーマット・
        // コールバック)は Uninitialize を跨いで保たれる。
        let reinit = unsafe {
            AudioUnitUninitialize(open.unit);
            AudioUnitInitialize(open.unit)
        };
        if reinit != NO_ERR {
            return Err(format!(
                "AudioOutputUnitStart failed (OSStatus {status}) and AudioUnitInitialize \
                 failed (OSStatus {reinit})"
            ));
        }
        // SAFETY: 同上(初期化し直したインスタンス)。
        let retry = unsafe { AudioOutputUnitStart(open.unit) };
        if retry == NO_ERR {
            Ok(())
        } else {
            Err(format!(
                "AudioOutputUnitStart failed (OSStatus {status}), and again after \
                 re-initializing (OSStatus {retry})"
            ))
        }
    }

    fn supports_rebuild(&self) -> bool {
        true
    }

    fn rebuild(&self) -> Result<(), String> {
        let mut guard = self.lock();
        let Some(open) = guard.as_mut() else {
            return Err(UNIT_CLOSED.to_owned());
        };

        // 動かし直し(`play`)と同じ理由——作り直している間にルートが変わっている
        // ことがあるので、新しいユニットが最初のコールバックから新しい値を使えるよう
        // 先に補正項を読み直す。
        #[cfg(any(target_os = "ios", target_os = "tvos"))]
        refresh_ios_output_latency(
            &self.device_extra_latency_ns,
            &self.io_buffer_duration_ns,
            self.sample_rate as u32,
            "rebuild",
        );

        // 古いユニットを先に止める——ここから新しいユニットの Start までコールバックは
        // 走らないので、同じ context を新しいユニットへ渡しても書き手は1人のまま。
        // SAFETY: `open.unit` はロックの内側にある、まだ破棄していないインスタンス
        // (メディアサービスのリセット後は失敗を返しうるが、それは無視してよい)。
        unsafe {
            AudioOutputUnitStop(open.unit);
        }

        // 新しいユニットが作れなかったら古いユニットを残す(少なくとも有効なハンドルの
        // まま。続く動かし直し・次の作り直しの対象にする)。
        let new_unit = build_unit_for_existing_context(self.sample_rate, open.context_ptr)?;

        // SAFETY: 古いユニットは止めてあり、以後誰も使わない(`open.unit` をこの直後に
        // 差し替える)。
        unsafe {
            AudioUnitUninitialize(open.unit);
            AudioComponentInstanceDispose(open.unit);
        }
        // 古いユニットの最後のコールバックの書き込みを、新しいユニットのコールバックが
        // 読む前に観測しておく(`AppleBackend::close` の同じ load と同じ理由)。
        let _ = self.render_completions.load(Ordering::Acquire);
        open.unit = new_unit;
        self.restart_epoch.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn rebuild_if_oversized_callbacks(&self) {
        let streak = self.consecutive_oversized_callbacks.load(Ordering::Relaxed);
        if !should_rebuild_for_oversized_callbacks(streak) {
            return;
        }

        crate::mw_log!(
            "[mw-backend] (native/apple ios) oversized callbacks detected ({streak} consecutive \
             callbacks over twice the expected buffer length); rebuilding the output unit"
        );

        // 試みは1回だけ——失敗しても同じ streak のまま毎回呼び直されるのを避ける
        // (`rebuild_if_oversized_callbacks` は曲の予約・復帰確認のたびに呼ばれるため、
        // 失敗が続く場合に無意味な再試行を積み重ねないようにする)。
        self.consecutive_oversized_callbacks
            .store(0, Ordering::Relaxed);

        // 作り直す前にセッションを整え直す(`attempt_recovery` の最初の一手と同じ)。
        crate::ios_session::configure();

        match self.rebuild() {
            Ok(()) => {
                if let Err(err) = self.play() {
                    crate::mw_log!(
                        "[mw-backend] (native/apple ios) play() after rebuilding for oversized \
                         callbacks failed: {err}"
                    );
                }
            }
            Err(err) => crate::mw_log!(
                "[mw-backend] (native/apple ios) rebuild after detecting oversized callbacks \
                 failed: {err}"
            ),
        }
    }
}

// ============================================================================
// Backend 実装
// ============================================================================

/// macOS(AUHAL、`kAudioUnitSubType_DefaultOutput`)/ iOS・tvOS(RemoteIO、
/// `kAudioUnitSubType_RemoteIO`)の既定出力へ AudioUnit で直接出力する `Backend` 実装
/// (AUDIOWARE-DEPS-PLAN.md ステップ4-1〔macOS〕・4-2〔iOS/tvOS〕)。
pub struct AppleBackend {
    /// 開いている AudioUnit の制御口(`close` と iOS/tvOS の復帰で共有する)。
    /// `None` なら閉じている。
    control: Option<Arc<UnitControl>>,
    callback_frames: Arc<AtomicU32>,
    /// [`Backend::log_new_output_underruns`] が直近にログへ出した時点の
    /// [`callback_frames`](Self::callback_frames) の値。変化を検知するためだけの
    /// ゲームスレッド専用の状態(`logged_output_underrun_count` と同じ配線パターン)。
    logged_callback_frames: AtomicU32,
    output_latency_ns: Arc<AtomicU64>,
    render_completions: Arc<AtomicU64>,
    /// [`Backend::log_output_latency_once`] が直近にログへ出した時点の
    /// [`restart_epoch`](Self::restart_epoch) の値。`u64::MAX` は「まだ1度も
    /// ログしていない」を表す初期値(通常の `restart_epoch` がこの値に達することは
    /// 実運用上無いため、特殊値として安全に使える)。動かし直し・作り直しで
    /// `restart_epoch` が進むたびにこの値と食い違うようになるので、`measured output
    /// latency` のログを世代ごとに1回だけ再び出せる(開いた直後の1回限りではなく)。
    logged_output_latency_epoch: AtomicU64,
    sample_rate: u32,
    output_underrun_count: Arc<AtomicU64>,
    last_output_underrun_host_time_ns: Arc<AtomicU64>,
    consecutive_output_underrun_count: Arc<AtomicU32>,
    logged_output_underrun_count: AtomicU64,
    /// 補正項(出力レイテンシ、ns)。[`CallbackContext::device_extra_latency_ns`] と
    /// 同じ `Arc` ——音声コールバックはこちら経由ではなく `CallbackContext` 側の
    /// クローンを読むが、書き込み(macOS は `open()` で1度だけ、iOS/tvOS は `open()` +
    /// ルート変化のたびに`IosOutputLatencyWatcher`)はこちらを通す。他の `Arc<Atomic*>`
    /// 群と同じく `AppleBackend::new()` で1度だけ生成し、`open()`/`close()` を跨いで
    /// 再利用する。
    device_extra_latency_ns: Arc<AtomicU64>,
    /// バッファ長の項(ns)。[`CallbackContext::io_buffer_duration_ns`] と同じ `Arc`。
    /// macOS では誰も書かず常に 0(`render_proc` がこの値を読まないため、モジュール doc
    /// 「バッファ長の項の出し方」参照)。iOS/tvOS は `open()` で初期化し、
    /// `device_extra_latency_ns` と全く同じ読み直しタイミングで
    /// `refresh_ios_output_latency` が書き直す。
    io_buffer_duration_ns: Arc<AtomicU64>,
    /// iOS/tvOS: ルート変化のたびに `device_extra_latency_ns` を更新するウォッチャ。
    /// macOS では常に `Some` だが中身は no-op(構築・破棄のコストも無い)。
    /// `open()`/`close()` と1対1(`cpal_backend::CpalBackend::ios_interruption` と同じ配線)。
    ios_output_latency_watcher: Option<IosOutputLatencyWatcher>,
    /// 出力を動かし直した・作り直した回数。[`CallbackContext::restart_epoch`] /
    /// [`UnitControl::restart_epoch`] と同じ `Arc`(`open()`/`close()` を跨いで再利用する)。
    restart_epoch: Arc<AtomicU64>,
    /// iOS/tvOS の割り込み・背面遷移・ルート変化・出力停止からの復帰
    /// (cpal 版と同じ `ios_interruption::Watcher`。macOS では no-op)。
    /// `open()`/`close()` と1対1。
    ios_interruption: Option<ios_interruption::Watcher>,
    /// モジュール doc「診断用プローブ(io probe)」の **lead**。[`CallbackContext::
    /// io_probe_lead_ns`] と同じ `Arc`。
    io_probe_lead_ns: Arc<AtomicI64>,
    /// 診断用プローブの **mSampleTime の連続性**。[`CallbackContext::
    /// io_probe_sample_time_gap_frames`] と同じ `Arc`。
    io_probe_sample_time_gap_frames: Arc<AtomicI64>,
    /// 診断用プローブの **flags**。[`CallbackContext::io_probe_flags`] と同じ `Arc`。
    io_probe_flags: Arc<AtomicU32>,
    /// [`Backend::log_new_output_underruns`] が直近に io probe のログを出した時点の
    /// [`restart_epoch`](Self::restart_epoch) の値。`u64::MAX` は「まだ1度もログしていない」
    /// を表す初期値([`AppleBackend::logged_output_latency_epoch`] と同じ idiom)。
    logged_io_probe_restart_epoch: AtomicU64,
    /// [`Backend::log_new_output_underruns`] が直近に io probe のログを出した
    /// ホスト単調時刻(ns)。0 は「まだ1度もログしていない」(`host_time_ns()` が実際に
    /// 0ns を返すことはまず無いため、特殊値として安全に使える)。
    logged_io_probe_host_time_ns: AtomicU64,
    /// モジュール doc「オーバーサイズのコールバックからの直し」: バッファ長から期待される
    /// フレーム数の2倍を超えるコールバックが連続している回数。[`CallbackContext::
    /// consecutive_oversized_callbacks`]/[`UnitControl::consecutive_oversized_callbacks`] と
    /// 同じ `Arc`。
    consecutive_oversized_callbacks: Arc<AtomicU32>,
}

// SAFETY: `control` の中に生ポインタ(AudioUnit・context)を持つが、それらは
// `UnitControl` の `Mutex` を通してしか触れない。そのうえで、`mw-ffi::handle::Instance` がグローバル
// レジストリの `Mutex` 経由で単一所有を保証するため、`AppleBackend` の `&mut self`
// メソッドが複数スレッドから同時に呼ばれることは無い(`cpal_backend::CpalBackend` と同じ
// 理由。`mw-ffi/COMMON.md` の「バックエンドをトレイトオブジェクトで持つ理由」参照)。
// `Sync` は要らない。
unsafe impl Send for AppleBackend {}

impl Default for AppleBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleBackend {
    pub fn new() -> Self {
        Self {
            control: None,
            callback_frames: Arc::new(AtomicU32::new(0)),
            logged_callback_frames: AtomicU32::new(0),
            output_latency_ns: Arc::new(AtomicU64::new(0)),
            render_completions: Arc::new(AtomicU64::new(0)),
            logged_output_latency_epoch: AtomicU64::new(u64::MAX),
            sample_rate: 0,
            output_underrun_count: Arc::new(AtomicU64::new(0)),
            last_output_underrun_host_time_ns: Arc::new(AtomicU64::new(0)),
            consecutive_output_underrun_count: Arc::new(AtomicU32::new(0)),
            logged_output_underrun_count: AtomicU64::new(0),
            device_extra_latency_ns: Arc::new(AtomicU64::new(0)),
            io_buffer_duration_ns: Arc::new(AtomicU64::new(0)),
            ios_output_latency_watcher: None,
            restart_epoch: Arc::new(AtomicU64::new(0)),
            ios_interruption: None,
            io_probe_lead_ns: Arc::new(AtomicI64::new(0)),
            io_probe_sample_time_gap_frames: Arc::new(AtomicI64::new(0)),
            io_probe_flags: Arc::new(AtomicU32::new(0)),
            logged_io_probe_restart_epoch: AtomicU64::new(u64::MAX),
            logged_io_probe_host_time_ns: AtomicU64::new(0),
            consecutive_oversized_callbacks: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl Backend for AppleBackend {
    fn open(&mut self, renderer: Renderer, events: Arc<EventQueue>) -> Result<(), BackendError> {
        if self.control.is_some() {
            return Err(BackendError::AlreadyOpen);
        }

        // iOS/tvOS では RemoteIO がハードウェアとネゴシエートする前にセッション
        // (カテゴリ・希望サンプルレート・希望 I/O バッファ長・setActive)を設定しておく
        // 必要がある(`cpal_backend::CpalBackend::open` と同じ順序の理由)。macOS では
        // no-op(`ios_session::configure` のドキュメント参照)。
        crate::ios_session::configure();

        let unit = new_output_unit()?;

        let sample_rate = query_output_sample_rate(unit);

        // `kAudioUnitScope_Input` 側(アプリがデータを渡す側)への設定であり、
        // ハードウェア側(`kAudioUnitScope_Output`)のフォーマットは AUHAL が自動的に
        // 変換する(関数先頭のモジュール doc 参照)。
        let status = set_stream_format(unit, sample_rate);
        if status != NO_ERR {
            // SAFETY: `unit` は `AudioComponentInstanceNew` が返した有効なインスタンスで、
            // まだ誰にも共有していない(エラーで抜ける前にここで確実に破棄する)。
            unsafe {
                AudioComponentInstanceDispose(unit);
            }
            return Err(BackendError::NoSupportedStreamConfig);
        }

        // 音楽クロック(フレーム数→秒)とランプの換算が実際の出力レートを使うよう、
        // コールバックが動き出す前に確定させる(仮置きの 48kHz のままだと、出力が
        // 44.1kHz で開いたときに曲の時刻が実際の音より遅れて進む)。
        let mut renderer = renderer;
        renderer.set_sample_rate(sample_rate as u32);

        // `renderer` をムーブする**前に**音楽クロックの発行ハンドルを取る
        // (`Mixer::music_clock_handle` のドキュメント「ムーブする前に取ること」どおり。
        // `se_schedule_overflow_counter` を mw-ffi 側が同じ理由で先取りしているのと同じ
        // パターン)。
        let music_clock = renderer.music_clock_handle();

        let context = Box::new(CallbackContext {
            renderer,
            sample_rate: sample_rate as u32,
            // 初期値は 0(「まだ不明」)。初期化が成功した直後に確定させる(下記)——
            // `self.device_extra_latency_ns` と同じ `Arc` なので、以後の書き込みは
            // この `context_ptr` を経由しない(`self` 側の `Arc` へ直接 `store` する)。
            device_extra_latency_ns: Arc::clone(&self.device_extra_latency_ns),
            // 実際の初期値は `device_extra_latency_ns` と同じタイミングでは確定できない
            // (後述の算出は `AudioUnitInitialize` の後にしか行えない。macOS の
            // `query_device_extra_latency_frames` のドキュメント参照)——いったん 0 で置き、
            // 確定した直後に `context_ptr` 経由で書き直す(下記)。
            last_device_extra_latency_ns: 0,
            // バッファ長の項も同じ事情——いったん 0 で置き、`device_extra_latency_ns` と
            // 同じタイミングで確定させて `context_ptr` 経由で書き直す(下記)。
            io_buffer_duration_ns: Arc::clone(&self.io_buffer_duration_ns),
            last_io_buffer_duration_ns: 0,
            music_clock,
            restart_epoch: Arc::clone(&self.restart_epoch),
            // 開いた時点の値を基準にする(これと違う値を見たら世代を進める)。
            last_restart_epoch: self.restart_epoch.load(Ordering::Relaxed),
            io_probe_lead_ns: Arc::clone(&self.io_probe_lead_ns),
            io_probe_sample_time_gap_frames: Arc::clone(&self.io_probe_sample_time_gap_frames),
            io_probe_flags: Arc::clone(&self.io_probe_flags),
            last_probe_sample_time: 0.0,
            last_probe_frames: 0,
            consecutive_oversized_callbacks: Arc::clone(&self.consecutive_oversized_callbacks),
            underrun_tracker: OutputUnderrunTracker::new(
                Arc::clone(&self.output_underrun_count),
                Arc::clone(&self.last_output_underrun_host_time_ns),
                Arc::clone(&self.consecutive_output_underrun_count),
            ),
            callback_frames: Arc::clone(&self.callback_frames),
            output_latency_ns: Arc::clone(&self.output_latency_ns),
            render_completions: Arc::clone(&self.render_completions),
        });
        let context_ptr = Box::into_raw(context);

        // `context_ptr` は直前に `Box::into_raw` したばかりで、CoreAudio はまだこれを
        // 知らない(`AudioOutputUnitStart` を呼ぶまでレンダーコールバックは起動しない)。
        let status = set_render_callback(unit, context_ptr);
        if status != NO_ERR {
            // SAFETY: `context_ptr` は直前にこの関数が `Box::into_raw` したばかりで、
            // CoreAudio を含め他のどこからも参照されていない(上のコメント参照)。
            drop(unsafe { Box::from_raw(context_ptr) });
            unsafe {
                AudioComponentInstanceDispose(unit);
            }
            return Err(BackendError::BuildStreamFailed(format!(
                "AudioUnitSetProperty(SetRenderCallback) failed: OSStatus {status}"
            )));
        }

        // SAFETY: `unit` はここまで有効なまま構成済み。
        let status = unsafe { AudioUnitInitialize(unit) };
        if status != NO_ERR {
            // SAFETY: `context_ptr` はまだ誰も(CoreAudio も)参照していない
            // (`AudioOutputUnitStart` を呼ぶ前)。
            drop(unsafe { Box::from_raw(context_ptr) });
            unsafe {
                AudioComponentInstanceDispose(unit);
            }
            return Err(BackendError::BuildStreamFailed(format!(
                "AudioUnitInitialize failed: OSStatus {status}"
            )));
        }

        // 補正項(出力レイテンシ)の出し方は OS で異なる(モジュール doc「タイムスタンプの
        // 扱い」参照)。`self.device_extra_latency_ns`(`CallbackContext` 側のクローンと
        // 同じ `Arc`)へ直接 `store` するだけなので、`context_ptr` の生ポインタ越しに
        // 書く必要が無い——レンダースレッドがまだ走っていない今この時点でも、
        // 走り出した後(ルート変化での更新)でも、同じ1行で安全に書ける。
        #[cfg(target_os = "macos")]
        let device_extra_latency_ns = {
            let device_extra_latency_frames = query_device_extra_latency_frames(unit);
            extra_latency_frames_to_ns(device_extra_latency_frames, sample_rate as u32)
        };
        #[cfg(any(target_os = "ios", target_os = "tvos"))]
        let device_extra_latency_ns = query_ios_output_latency_ns();
        self.device_extra_latency_ns
            .store(device_extra_latency_ns, Ordering::Relaxed);
        // SAFETY: `AudioOutputUnitStart` を呼ぶ前なので、レンダーコールバックは一度も
        // 起動していない(直後の SAFETY コメントと同じ前提)——`context_ptr` はまだ
        // この関数が排他的に所有している。ここで書くのは `render_proc` が最初の
        // コールバックで比較する基準値(`CallbackContext::last_device_extra_latency_ns`
        // のドキュメント参照)。0 のまま残すと、ルート変化が一度も起きていなくても
        // 最初のコールバックで「補正項が変わった」と誤検知し、無意味な世代の繰り上げが
        // 1回起きてしまう。
        unsafe {
            (*context_ptr).last_device_extra_latency_ns = device_extra_latency_ns;
        }

        // バッファ長の項も同じ手順(モジュール doc「バッファ長の項の出し方」参照)。
        // macOS は常に 0(`render_proc` がこの値を読まないので、どちらでも実害は無いが
        // 「macOS は今のまま」を明示するため 0 固定にしてある)。
        #[cfg(target_os = "macos")]
        let io_buffer_duration_ns: u64 = 0;
        #[cfg(any(target_os = "ios", target_os = "tvos"))]
        let io_buffer_duration_ns = query_ios_io_buffer_duration_ns();
        self.io_buffer_duration_ns
            .store(io_buffer_duration_ns, Ordering::Relaxed);
        // SAFETY: 同上(`last_device_extra_latency_ns` の直前の SAFETY コメントと同じ前提)。
        unsafe {
            (*context_ptr).last_io_buffer_duration_ns = io_buffer_duration_ns;
        }

        // レビュー【低】の修正: ルート変化監視(iOS/tvOS)は `AudioOutputUnitStart` より
        // **前**に組み立てる——レンダーコールバックが実際に動き出す前に監視を始めることで、
        // 「Start した直後、監視がまだ無い間にルート変化が来て取り逃す」窓を塞ぐ
        // (macOS では `IosOutputLatencyWatcher::new` 自体が no-op なのでコストは無い)。
        // `close()` は依然これを一番最初に止める(逆順で対称)。
        self.ios_output_latency_watcher = Some(IosOutputLatencyWatcher::new(
            Arc::clone(&self.device_extra_latency_ns),
            Arc::clone(&self.io_buffer_duration_ns),
            sample_rate as u32,
        ));

        // SAFETY: `unit` は初期化済み。
        let status = unsafe { AudioOutputUnitStart(unit) };
        if status != NO_ERR {
            // 直前に組み立てた監視を手放す——`open()` 自体が失敗して返るため、
            // `self.control` は `None` のままで `is_open()` も
            // `false` のままになる(= 開いていない状態の一部として監視も無い状態に
            // 揃える)。
            self.ios_output_latency_watcher = None;
            // SAFETY: `AudioUnitInitialize` は成功したが `AudioOutputUnitStart` が
            // 失敗したため、レンダーコールバックは一度も呼ばれていない
            // (`context_ptr` はまだ排他的にこの関数が所有している)。
            unsafe {
                AudioUnitUninitialize(unit);
            }
            drop(unsafe { Box::from_raw(context_ptr) });
            unsafe {
                AudioComponentInstanceDispose(unit);
            }
            return Err(BackendError::PlayStreamFailed(format!(
                "AudioOutputUnitStart failed: OSStatus {status}"
            )));
        }

        crate::mw_log!(
            "[mw-backend] (native/apple) output stream started: sample_rate={} Hz, channels={}, \
             device_extra_latency={:.3} ms",
            sample_rate,
            CHANNELS,
            device_extra_latency_ns as f64 / 1_000_000.0,
        );

        let control = Arc::new(UnitControl {
            slot: Mutex::new(Some(OpenUnit { unit, context_ptr })),
            sample_rate,
            restart_epoch: Arc::clone(&self.restart_epoch),
            device_extra_latency_ns: Arc::clone(&self.device_extra_latency_ns),
            io_buffer_duration_ns: Arc::clone(&self.io_buffer_duration_ns),
            render_completions: Arc::clone(&self.render_completions),
            consecutive_oversized_callbacks: Arc::clone(&self.consecutive_oversized_callbacks),
        });

        // 割り込み・背面遷移・ルート変化・出力停止からの復帰(cpal 版と同じ監視。
        // iOS/tvOS 以外では no-op)。cpal 版と同じく、出力を開始した直後に組み立てる。
        // 「コールバックが進んだか」の実測には `render_completions`(毎コールバック1増える)
        // を使う。
        self.ios_interruption = Some(ios_interruption::Watcher::new(
            Arc::clone(&control) as Arc<dyn RecoverableOutput>,
            events,
            Arc::clone(&self.render_completions),
        ));

        self.control = Some(control);
        self.sample_rate = sample_rate as u32;
        Ok(())
    }

    fn close(&mut self) -> Result<(), BackendError> {
        // ルート変化の監視を先に止める(iOS/tvOS。macOS では no-op)——ストリームが
        // 止まりかけの状態で補正項を書き直されないようにする(`cpal_backend::
        // CpalBackend::close` が `ios_interruption` を先に止めるのと同じ理由)。
        self.ios_output_latency_watcher = None;
        // 復帰の監視も先に止める(ウォッチドッグはここで join される)。まだ走っている
        // 復帰確認のワーカーがあっても、下で `OpenUnit` を取り出した後の操作は
        // `UnitControl` が空振りさせる。
        self.ios_interruption = None;

        let open = self
            .control
            .take()
            .and_then(|control| control.lock().take());
        match open {
            Some(OpenUnit { unit, context_ptr }) => {
                // Apple が推奨する解体順序(Stop → Uninitialize → Dispose)をそのまま守る。
                // SAFETY: `unit` はこの `AppleBackend` だけが所有しており(`take()` で
                // 既に自分からも外した)、`open()` が成功させたまま一度も dispose
                // していない有効なインスタンス。
                unsafe {
                    AudioOutputUnitStop(unit);
                    AudioUnitUninitialize(unit);
                    AudioComponentInstanceDispose(unit);
                }

                // `AudioOutputUnitStop` が実際に同期的にレンダースレッドの完了を待つかは
                // ヘッダのドキュメントに明記が無いため、`cpal_backend::CpalBackend::close`
                // と同じ保険を入れる——直近コールバックが `Release` で残した書き込みを
                // この `Acquire` で明示的に観測してから、直後の `Box::from_raw` の drop が
                // それを読む前に happens-before の辺を明示しておく。
                // SAFETY: `context_ptr` が指すメモリは `open()` 以来有効であり、上の
                // Stop/Uninitialize/Dispose 後もまだ `Box::from_raw` で回収していない。
                let _ = unsafe { (*context_ptr).render_completions.load(Ordering::Acquire) };
                // SAFETY: `open()` で `Box::into_raw` したポインタをそのまま回収する。
                // レンダースレッドは上の `AudioOutputUnitStop`/`AudioComponentInstanceDispose`
                // より後には呼ばれない。
                drop(unsafe { Box::from_raw(context_ptr) });

                self.sample_rate = 0;
                self.callback_frames.store(0, Ordering::Relaxed);
                self.logged_callback_frames.store(0, Ordering::Relaxed);
                self.render_completions.store(0, Ordering::Relaxed);
                self.output_latency_ns.store(0, Ordering::Relaxed);
                self.logged_output_latency_epoch
                    .store(u64::MAX, Ordering::Relaxed);
                self.output_underrun_count.store(0, Ordering::Relaxed);
                self.last_output_underrun_host_time_ns
                    .store(0, Ordering::Relaxed);
                self.consecutive_output_underrun_count
                    .store(0, Ordering::Relaxed);
                self.logged_output_underrun_count
                    .store(0, Ordering::Relaxed);
                self.device_extra_latency_ns.store(0, Ordering::Relaxed);
                self.io_buffer_duration_ns.store(0, Ordering::Relaxed);
                self.io_probe_lead_ns.store(0, Ordering::Relaxed);
                self.io_probe_sample_time_gap_frames
                    .store(0, Ordering::Relaxed);
                self.io_probe_flags.store(0, Ordering::Relaxed);
                self.logged_io_probe_restart_epoch
                    .store(u64::MAX, Ordering::Relaxed);
                self.logged_io_probe_host_time_ns
                    .store(0, Ordering::Relaxed);
                self.consecutive_oversized_callbacks
                    .store(0, Ordering::Relaxed);
                Ok(())
            }
            _ => Err(BackendError::NotOpen),
        }
    }

    fn is_open(&self) -> bool {
        self.control.is_some()
    }

    fn last_callback_frames(&self) -> u32 {
        self.callback_frames.load(Ordering::Relaxed)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn refresh_output_latency(&self) {
        // macOS は未オープンのときも含めて何もしない——補正項は `open()` で1度
        // 読んだあと固定の既知の差分(モジュール doc「cpal 版との既知の差分」参照)。
        // iOS/tvOS だけが曲の再生予約の直前にここを通る
        // (`crates/mw-ffi/src/ffi.rs::mw_music_play_scheduled` から)。
        #[cfg(any(target_os = "ios", target_os = "tvos"))]
        if let Some(control) = self.control.as_ref() {
            refresh_ios_output_latency(
                &self.device_extra_latency_ns,
                &self.io_buffer_duration_ns,
                self.sample_rate,
                "music schedule or resume",
            );
            // モジュール doc「オーバーサイズのコールバックからの直し」の安全な時点の
            // 一つ: 曲の再生予約の直前(世代をまたがない途中には作り直さない)。
            control.rebuild_if_oversized_callbacks();
        }
    }

    fn output_latency_ns(&self) -> u64 {
        self.output_latency_ns.load(Ordering::Relaxed)
    }

    fn log_output_latency_once(&self) {
        let latency_ns = self.output_latency_ns();
        if latency_ns == 0 {
            return;
        }
        // 世代(`restart_epoch`)ごとに1回だけログを出す——`u64::MAX`(初期値)は
        // 「まだ1度も出していない」を表すので、開いた直後の最初の呼び出しは必ず通る。
        // 動かし直し・作り直し(`UnitControl::play`/`rebuild`)が `restart_epoch` を
        // 進めると、その後の呼び出しでまた1回だけ出せるようになる
        // ([`AppleBackend::logged_output_latency_epoch`] のドキュメント参照)——
        // Control Center 表示中の出力停止からの復帰のように、補正項・バッファ長の項が
        // 入れ替わった後の実測値を確かめ直せるようにするため。
        let current_epoch = self.restart_epoch.load(Ordering::Relaxed);
        let last_logged_epoch = self.logged_output_latency_epoch.load(Ordering::Relaxed);
        if last_logged_epoch == current_epoch {
            return;
        }
        self.logged_output_latency_epoch
            .store(current_epoch, Ordering::Relaxed);
        let latency_ms = latency_ns as f64 / 1_000_000.0;
        crate::mw_log!(
            "[mw-backend] (native/apple) output latency (measured, buffer duration + device \
             latency + safety offset): {latency_ns} ns = {latency_ms:.3} ms (restart_epoch={current_epoch})"
        );
    }

    fn log_new_output_underruns(&self) {
        let current = self.output_underrun_count.load(Ordering::Relaxed);
        let last_logged = self.logged_output_underrun_count.load(Ordering::Relaxed);
        if current > last_logged {
            self.logged_output_underrun_count
                .store(current, Ordering::Relaxed);
            let new_count = current - last_logged;
            let consecutive = self
                .consecutive_output_underrun_count
                .load(Ordering::Relaxed);
            crate::mw_log!(
                "[mw-backend] (native/apple) output underrun suspected: +{new_count} since last \
                 check (cumulative={current}, consecutive={consecutive})"
            );
        }

        // 音声スレッドが書いている実測フレーム数(`in_number_frames`)が前回ログした
        // ときから変わっていたら、ここ(ゲームスレッド、日和見的な定期呼び出し)から
        // 1行出す——Control Center 表示中の出力停止からの復帰後、iOS の RemoteIO が
        // 希望値(通常 240 フレーム)より大きい値のまま戻らないことがあるかどうかを
        // 実機ログで確かめるための診断用(モジュール doc「バッファ長の項の出し方」参照。
        // iOS/tvOS はこの値をもう `buffer_start_host_time_ns` の計算には使わないが、
        // 実測フレーム数そのものの変化は引き続き観測する価値がある)。毎フレームでは
        // なく、変わったときだけ出す。
        let current_frames = self.callback_frames.load(Ordering::Relaxed);
        let last_logged_frames = self
            .logged_callback_frames
            .swap(current_frames, Ordering::Relaxed);
        let frames_changed = current_frames != last_logged_frames;
        if frames_changed {
            crate::mw_log!(
                "[mw-backend] (native/apple) callback frame count (in_number_frames) changed: \
                 {last_logged_frames} -> {current_frames} frames"
            );
        }

        // モジュール doc「診断用プローブ(io probe)」。既存のこの口(ゲームスレッド、
        // 日和見的な定期呼び出し)に相乗りする——新しい FFI 口は増やさない。
        let current_epoch = self.restart_epoch.load(Ordering::Relaxed);
        let last_logged_epoch = self
            .logged_io_probe_restart_epoch
            .swap(current_epoch, Ordering::Relaxed);
        let restart_epoch_changed = last_logged_epoch != current_epoch;

        let now_ns = crate::host_time::host_time_ns();
        let last_logged_host_time_ns = self.logged_io_probe_host_time_ns.load(Ordering::Relaxed);
        let elapsed_ns = if last_logged_host_time_ns == 0 {
            u64::MAX
        } else {
            now_ns.saturating_sub(last_logged_host_time_ns)
        };

        if should_log_io_probe(restart_epoch_changed, frames_changed, elapsed_ns) {
            self.logged_io_probe_host_time_ns
                .store(now_ns, Ordering::Relaxed);
            let lead_ns = self.io_probe_lead_ns.load(Ordering::Relaxed);
            let sample_time_gap_frames =
                self.io_probe_sample_time_gap_frames.load(Ordering::Relaxed);
            let flags = self.io_probe_flags.load(Ordering::Relaxed);
            crate::mw_log!(
                "[mw-backend] (native/apple) io probe: in_number_frames={current_frames}, \
                 lead={lead_ms:.3} ms, sample_time_gap={sample_time_gap_frames} frames, \
                 flags={flags:#x}, restart_epoch={current_epoch}",
                lead_ms = lead_ns as f64 / 1_000_000.0,
            );
        }
    }

    fn output_underrun_count(&self) -> u64 {
        self.output_underrun_count.load(Ordering::Relaxed)
    }

    fn last_output_underrun_host_time_ns(&self) -> u64 {
        self.last_output_underrun_host_time_ns
            .load(Ordering::Relaxed)
    }

    fn consecutive_output_underrun_count(&self) -> u32 {
        self.consecutive_output_underrun_count
            .load(Ordering::Relaxed)
    }
}

// ============================================================================
// 補正項(出力レイテンシ)のルート変化追従(iOS/tvOS のみ。AUDIOWARE-DEPS-PLAN.md
// ステップ4-2、HANDOFF 0f の統合)
// ============================================================================

/// `AVAudioSessionRouteChangeNotification` を監視し、ルート(スピーカー/有線/
/// Bluetooth)が変わるたびに `device_extra_latency_ns`(補正項)を
/// `query_ios_output_latency_ns` で読み直す。macOS では no-op。
///
/// **`ios_interruption::Watcher` とは独立**——あちらは割り込み・背面遷移・ルート変化からの
/// 復帰(`UnitControl` 越しの止め直し・動かし直し)を受け持ち、こちらは「補正項を最新に
/// 保つ」ことだけを受け持つ(AudioUnit 自体には一切触れない)。同じルート変化の通知を
/// 両方が受け取るが、触る状態が重ならないので二重処理にはならない。
pub(super) struct IosOutputLatencyWatcher {
    #[cfg(any(target_os = "ios", target_os = "tvos"))]
    #[allow(dead_code)] // Drop 経由で `removeObserver` させるためだけに保持する
    inner: ios_impl::Watcher,
}

impl IosOutputLatencyWatcher {
    /// `device_extra_latency_ns`/`io_buffer_duration_ns` はルート変化のたびに書き直す先
    /// (`AppleBackend`/`CallbackContext` と同じ `Arc` を指す)。`sample_rate` はログ出力用
    /// (`refresh_ios_output_latency` へそのまま渡す)。
    #[cfg(any(target_os = "ios", target_os = "tvos"))]
    fn new(
        device_extra_latency_ns: Arc<AtomicU64>,
        io_buffer_duration_ns: Arc<AtomicU64>,
        sample_rate: u32,
    ) -> Self {
        Self {
            inner: ios_impl::Watcher::new(
                device_extra_latency_ns,
                io_buffer_duration_ns,
                sample_rate,
            ),
        }
    }

    /// macOS では何もしない(常に `Some(IosOutputLatencyWatcher::new(..))` を構築しても
    /// コストが無いようにするための no-op。`ios_interruption::Watcher` と同じ設計)。
    #[cfg(not(any(target_os = "ios", target_os = "tvos")))]
    fn new(
        _device_extra_latency_ns: Arc<AtomicU64>,
        _io_buffer_duration_ns: Arc<AtomicU64>,
        _sample_rate: u32,
    ) -> Self {
        Self {}
    }
}

#[cfg(any(target_os = "ios", target_os = "tvos"))]
mod ios_impl {
    use std::panic::{self, AssertUnwindSafe};
    use std::ptr::NonNull;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::NSObjectProtocol;
    use objc2::runtime::ProtocolObject;
    use objc2_avf_audio::AVAudioSessionRouteChangeNotification;
    use objc2_foundation::{NSNotification, NSNotificationCenter};

    use super::refresh_ios_output_latency;

    pub(super) struct Watcher {
        /// 通知を受け取らなくなったら即座に `removeObserver` できるよう保持する
        /// (`AVAudioSessionRouteChangeNotification` が取得できなかった場合は `None`
        /// ——その場合は補正項が初期値〔`open()` 時の1回だけ〕のまま固定される)。
        observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    }

    // SAFETY: `NSNotificationCenter` はスレッドセーフ。保持する `observer` は `Drop` で
    // `removeObserver` するためだけの不透明なトークンで、他スレッドと共有する可変状態は
    // 持たない(`ios_interruption::imp::Watcher` と同じ理由づけ)。
    unsafe impl Send for Watcher {}
    unsafe impl Sync for Watcher {}

    impl Watcher {
        pub(super) fn new(
            device_extra_latency_ns: Arc<AtomicU64>,
            io_buffer_duration_ns: Arc<AtomicU64>,
            sample_rate: u32,
        ) -> Self {
            let nc = NSNotificationCenter::defaultCenter();
            let block = RcBlock::new(move |_: NonNull<NSNotification>| {
                // パニックは Objective-C ランタイムの外へ絶対に漏らさない
                // (`ios_interruption.rs` と同じ方針)。
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                    refresh_ios_output_latency(
                        &device_extra_latency_ns,
                        &io_buffer_duration_ns,
                        sample_rate,
                        "route change",
                    );
                }));
                if outcome.is_err() {
                    crate::mw_log!(
                        "[mw-backend] (native/apple ios) panic while handling \
                         AVAudioSessionRouteChangeNotification (caught at the boundary)"
                    );
                }
            });
            // SAFETY: 通知名はプロセス生存中変化しない静的値。
            // `addObserverForName_object_queue_usingBlock` は block を Objective-C
            // ランタイムが retain する契約どおりに呼ぶ(`ios_interruption.rs` と同じ)。
            let observer = unsafe { AVAudioSessionRouteChangeNotification }.map(|name| unsafe {
                nc.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
            });
            if observer.is_none() {
                crate::mw_log!(
                    "[mw-backend] (native/apple ios) AVAudioSessionRouteChangeNotification is \
                     unavailable; the output latency correction will not track route changes"
                );
            }
            Self { observer }
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            if let Some(observer) = self.observer.take() {
                let nc = NSNotificationCenter::defaultCenter();
                // SAFETY: `observer` はこの `Watcher` が `addObserverForName_...` から
                // 受け取ったトークンそのもの。
                unsafe { nc.removeObserver(observer.as_ref()) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_f32_asbd_describes_interleaved_f32_stereo_pcm() {
        let asbd = stereo_f32_asbd(48_000.0);
        assert_eq!(asbd.sample_rate, 48_000.0);
        assert_eq!(asbd.format_id, AUDIO_FORMAT_LINEAR_PCM);
        assert_eq!(
            asbd.format_flags,
            AUDIO_FORMAT_FLAG_IS_FLOAT | AUDIO_FORMAT_FLAG_IS_PACKED
        );
        assert_eq!(asbd.frames_per_packet, 1, "PCM は1フレーム=1パケット");
        assert_eq!(asbd.channels_per_frame, 2);
        assert_eq!(asbd.bits_per_channel, 32);
        assert_eq!(asbd.bytes_per_frame, 8, "2ch * 4bytes");
        assert_eq!(asbd.bytes_per_packet, 8);
        assert_eq!(asbd.reserved, 0);
    }

    /// 実際に書き込んでよい、4byte アラインされた有効なバッファ(テスト用)。
    fn aligned_test_buffer() -> [f32; 4096] {
        [0.0; 4096]
    }

    #[test]
    fn validated_sample_count_accepts_an_exactly_sized_buffer() {
        let mut buf = aligned_test_buffer();
        assert_eq!(
            validated_sample_count(512, buf.as_mut_ptr().cast(), 512 * 2 * 4),
            Some(512 * 2)
        );
    }

    #[test]
    fn validated_sample_count_rejects_a_null_buffer() {
        assert_eq!(
            validated_sample_count(512, std::ptr::null_mut(), 512 * 2 * 4),
            None
        );
    }

    #[test]
    fn validated_sample_count_rejects_an_undersized_buffer() {
        let mut buf = aligned_test_buffer();
        assert_eq!(
            validated_sample_count(512, buf.as_mut_ptr().cast(), 512 * 2 * 4 - 1),
            None
        );
    }

    #[test]
    fn validated_sample_count_rejects_zero_frames_even_with_a_valid_buffer() {
        let mut buf = aligned_test_buffer();
        assert_eq!(validated_sample_count(0, buf.as_mut_ptr().cast(), 0), None);
    }

    /// 再現テスト(レビュー指摘2): 0 フレームのとき `data` の値を一切見ずに弾くはず
    /// だが、現状はアラインメントを見ないまま `Some(0)` を返してしまう。呼び出し側は
    /// それを基に(0 要素とはいえ)`slice::from_raw_parts_mut` を呼ぶため、ダングリング・
    /// 非アラインポインタでも前提違反になる。このポインタは一切 dereference しない
    /// (アドレス値の検査だけ)ので、作るだけなら未定義動作ではない。
    #[test]
    fn validated_sample_count_rejects_zero_frames_with_a_dangling_unaligned_pointer() {
        let dangling_unaligned = std::ptr::dangling_mut::<c_void>();
        assert_eq!(validated_sample_count(0, dangling_unaligned, 0), None);
    }

    /// 再現テスト(レビュー指摘2、非 0 フレーム側): アラインメント検査が無いため、
    /// 1byte しかずれていない non-null ポインタでもサイズ条件さえ満たせば
    /// 通ってしまう。
    #[test]
    fn validated_sample_count_rejects_a_misaligned_nonzero_buffer() {
        let mut buf = aligned_test_buffer();
        // 1byte ずらす(f32 のアラインメントは4byte なので非アライン化できる)。
        let misaligned = unsafe { buf.as_mut_ptr().cast::<u8>().add(1) }.cast::<c_void>();
        assert_eq!(validated_sample_count(4, misaligned, 4 * 2 * 4), None);
    }

    #[test]
    fn compute_timestamps_adds_device_latency_and_buffer_duration() {
        let (buffer_start, latency) = compute_timestamps(1_000_000_000, 2_000_000, 10_666_666);
        assert_eq!(latency, 2_000_000 + 10_666_666);
        assert_eq!(buffer_start, 1_000_000_000 + latency);
    }

    /// 平常時(iOS/tvOS の既定 `IOBufferDuration` = 5ms。macOS でも 240 frames @
    /// 48kHz のときの実測フレーム数由来のバッファ長の項と同じ値)の校正値を固定化する——
    /// この関数のリファクタ(`frames`/`sample_rate` を受け取らず、呼び出し側が
    /// 用意した `buffer_duration_ns` を直接受け取る形へ変更)の前後で、平常時の予測
    /// 出力時刻が変わっていないことの根拠。
    #[test]
    fn compute_timestamps_matches_the_steady_state_calibration_of_5ms_buffer_and_device_latency() {
        const STEADY_STATE_BUFFER_DURATION_NS: u64 = 5_000_000; // 240 frames @ 48kHz
        const DEVICE_LATENCY_NS: u64 = 17_900_000; // 基準端末 iPhone 14 の実測出力レイテンシ

        let (buffer_start, latency) = compute_timestamps(
            1_000_000_000,
            DEVICE_LATENCY_NS,
            STEADY_STATE_BUFFER_DURATION_NS,
        );

        assert_eq!(latency, DEVICE_LATENCY_NS + STEADY_STATE_BUFFER_DURATION_NS);
        assert_eq!(buffer_start, 1_000_000_000 + latency);
    }

    #[test]
    fn compute_timestamps_is_zero_latency_when_both_terms_are_zero() {
        let (buffer_start, latency) = compute_timestamps(1_000_000_000, 0, 0);
        assert_eq!(latency, 0);
        assert_eq!(buffer_start, 1_000_000_000);
    }

    #[test]
    fn compute_timestamps_saturates_instead_of_overflowing() {
        let (buffer_start, latency) = compute_timestamps(u64::MAX, u64::MAX, 512);
        assert_eq!(buffer_start, u64::MAX);
        assert!(latency > 0);
    }

    /// AUDIOWARE-DEPS-PLAN.md ステップ4-2(HANDOFF 0f の統合)の核心を固定化する:
    /// **相関点(`callback_host_time_ns`)と補正項(`device_extra_latency_ns`)は
    /// 独立な状態として持たれ**、補正項だけが書き換わっても(iOS/tvOS の
    /// `IosOutputLatencyWatcher` がルート変化のたびに行う更新を模している)、
    /// 相関点の読み方(`AudioTimeStamp.mHostTime` を ns 化するだけ)は一切変えずに
    /// `buffer_start_host_time_ns` へ差分がそのまま反映されることを確認する。
    #[test]
    fn compute_timestamps_reflects_a_route_change_driven_correction_update_independently_of_the_correlation_point()
     {
        let correction_ns = Arc::new(AtomicU64::new(2_000_000)); // 2ms(例: 有線ルート)
        let callback_host_time_ns = 1_000_000_000u64; // 相関点——以下では一切変えない
        let buffer_duration_ns = 5_000_000u64; // バッファ長の項——以下では一切変えない

        let (start_before, latency_before) = compute_timestamps(
            callback_host_time_ns,
            correction_ns.load(Ordering::Relaxed),
            buffer_duration_ns,
        );

        // ルート変化(例: Bluetooth A2DP へ切替)を模倣: `IosOutputLatencyWatcher` が
        // 行うのはこの1本のアトミックへの `store` だけで、相関点には触れない。
        correction_ns.store(80_000_000, Ordering::Relaxed); // 80ms(例: A2DP)

        let (start_after, latency_after) = compute_timestamps(
            callback_host_time_ns, // 相関点は不変
            correction_ns.load(Ordering::Relaxed),
            buffer_duration_ns, // バッファ長の項も不変
        );

        assert!(
            latency_after > latency_before,
            "補正項の増加がそのまま反映されるはず"
        );
        assert_eq!(
            start_after - start_before,
            latency_after - latency_before,
            "相関点は変えず、補正項の差分だけが buffer_start_host_time_ns に反映されるはず"
        );
    }

    /// 上と対になる固定化: iOS/tvOS の `IOBufferDuration` 由来のバッファ長の項が
    /// (補正項とは無関係に)書き換わっても、差分がそのまま `buffer_start_host_time_ns`
    /// へ反映される——`render_proc` がこの値を `in_number_frames` の代わりに使う
    /// ようにした変更の核心(モジュール doc「バッファ長の項の出し方」参照)。
    #[test]
    fn compute_timestamps_reflects_an_io_buffer_duration_change_independently_of_the_correlation_point()
     {
        let callback_host_time_ns = 1_000_000_000u64;
        let device_extra_latency_ns = 17_900_000u64; // 補正項——以下では一切変えない

        let (start_before, latency_before) =
            compute_timestamps(callback_host_time_ns, device_extra_latency_ns, 5_000_000);

        // `in_number_frames` が希望値より大きい値のまま戻らなかった状況を模倣:
        // `IOBufferDuration` ベースのバッファ長の項だけが更新される。
        let (start_after, latency_after) =
            compute_timestamps(callback_host_time_ns, device_extra_latency_ns, 42_666_666);

        assert!(
            latency_after > latency_before,
            "バッファ長の項の増加がそのまま反映されるはず"
        );
        assert_eq!(
            start_after - start_before,
            latency_after - latency_before,
            "相関点・補正項は変えず、バッファ長の項の差分だけが buffer_start_host_time_ns に \
             反映されるはず"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extra_latency_frames_to_ns_converts_using_the_sample_rate() {
        // 48 frames @ 48kHz = 1ms。
        assert_eq!(extra_latency_frames_to_ns(48, 48_000), 1_000_000);
    }

    /// macOS のバッファ長の項(`render_proc` がこの関数で毎コールバック求める)の
    /// 平常時の値を固定化する——240 frames @ 48kHz は iOS/tvOS の既定
    /// `IOBufferDuration`(5ms)と同じ値になる(`compute_timestamps_matches_the_steady_
    /// state_calibration_of_5ms_buffer_and_device_latency` と対になる macOS 側の根拠)。
    #[cfg(target_os = "macos")]
    #[test]
    fn extra_latency_frames_to_ns_matches_the_steady_state_240_frames_at_48khz() {
        assert_eq!(extra_latency_frames_to_ns(240, 48_000), 5_000_000);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extra_latency_frames_to_ns_is_zero_when_sample_rate_is_unknown() {
        assert_eq!(extra_latency_frames_to_ns(48, 0), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extra_latency_frames_to_ns_is_zero_for_zero_frames() {
        assert_eq!(extra_latency_frames_to_ns(0, 48_000), 0);
    }

    /// iOS/tvOS の `query_ios_output_latency_ns`
    /// (`AVAudioSession.outputLatency()` の ns 化)が使う純関数部分。
    #[test]
    fn seconds_to_ns_converts_a_typical_output_latency_value() {
        // 17.9ms(基準端末 iPhone 14 の実測出力レイテンシ、初期構築仕様『§1』)。
        assert_eq!(seconds_to_ns(0.0179), 17_900_000);
    }

    #[test]
    fn seconds_to_ns_floors_a_negative_value_to_zero() {
        assert_eq!(seconds_to_ns(-0.005), 0);
    }

    #[test]
    fn seconds_to_ns_is_zero_for_exactly_zero() {
        assert_eq!(seconds_to_ns(0.0), 0);
    }

    #[test]
    fn seconds_to_ns_is_zero_for_nan() {
        assert_eq!(seconds_to_ns(f64::NAN), 0);
    }

    #[test]
    fn seconds_to_ns_is_zero_for_infinity() {
        assert_eq!(seconds_to_ns(f64::INFINITY), 0);
        assert_eq!(seconds_to_ns(f64::NEG_INFINITY), 0);
    }

    /// iOS/tvOS の既定 `IOBufferDuration`(5ms)を 48kHz で読んだときの期待フレーム数
    /// (240 フレーム。モジュール doc「オーバーサイズのコールバックからの直し」参照)。
    #[test]
    fn ns_to_frames_converts_the_steady_state_io_buffer_duration() {
        assert_eq!(ns_to_frames(5_000_000, 48_000), 240);
    }

    #[test]
    fn ns_to_frames_is_zero_when_sample_rate_is_unknown() {
        assert_eq!(ns_to_frames(5_000_000, 0), 0);
    }

    #[test]
    fn ns_to_frames_is_zero_for_zero_duration() {
        assert_eq!(ns_to_frames(0, 48_000), 0);
    }

    #[test]
    fn ns_to_frames_saturates_instead_of_overflowing() {
        assert_eq!(ns_to_frames(u64::MAX, u32::MAX), u32::MAX);
    }

    /// 平常時(240 フレーム=期待フレーム数そのもの)は「オーバーサイズ」ではない。
    #[test]
    fn is_oversized_callback_is_false_for_the_expected_frame_count() {
        assert!(!is_oversized_callback(240, 240));
    }

    /// ちょうど2倍は「超える」の境界に含めない(`>`、`>=` ではない)。
    #[test]
    fn is_oversized_callback_is_false_at_exactly_double() {
        assert!(!is_oversized_callback(480, 240));
    }

    #[test]
    fn is_oversized_callback_is_true_just_over_double() {
        assert!(is_oversized_callback(481, 240));
    }

    /// 実機で観測されている値(240 希望 → 2048 実測)はオーバーサイズと判定されるはず。
    #[test]
    fn is_oversized_callback_is_true_for_the_observed_degraded_frame_count() {
        assert!(is_oversized_callback(2048, 240));
    }

    /// 期待フレーム数が0(macOS。または iOS/tvOS でまだ `IOBufferDuration` を読んでいない)
    /// では、フレーム数がどれだけ大きくても判定しない。
    #[test]
    fn is_oversized_callback_is_false_when_expected_frames_is_zero() {
        assert!(!is_oversized_callback(2048, 0));
    }

    #[test]
    fn should_rebuild_for_oversized_callbacks_is_false_below_the_threshold() {
        assert!(!should_rebuild_for_oversized_callbacks(
            OVERSIZED_CALLBACK_REBUILD_THRESHOLD - 1
        ));
    }

    #[test]
    fn should_rebuild_for_oversized_callbacks_is_true_at_the_threshold() {
        assert!(should_rebuild_for_oversized_callbacks(
            OVERSIZED_CALLBACK_REBUILD_THRESHOLD
        ));
    }

    #[test]
    fn should_rebuild_for_oversized_callbacks_is_true_above_the_threshold() {
        assert!(should_rebuild_for_oversized_callbacks(
            OVERSIZED_CALLBACK_REBUILD_THRESHOLD + 1
        ));
    }

    #[test]
    fn should_log_io_probe_is_false_when_nothing_changed_and_the_interval_has_not_elapsed() {
        assert!(!should_log_io_probe(
            false,
            false,
            IO_PROBE_LOG_INTERVAL_NS - 1
        ));
    }

    #[test]
    fn should_log_io_probe_is_true_when_the_restart_epoch_changed() {
        assert!(should_log_io_probe(true, false, 0));
    }

    #[test]
    fn should_log_io_probe_is_true_when_the_frame_count_changed() {
        assert!(should_log_io_probe(false, true, 0));
    }

    #[test]
    fn should_log_io_probe_is_true_at_the_interval() {
        assert!(should_log_io_probe(false, false, IO_PROBE_LOG_INTERVAL_NS));
    }

    #[test]
    fn should_log_io_probe_is_true_past_the_interval() {
        assert!(should_log_io_probe(
            false,
            false,
            IO_PROBE_LOG_INTERVAL_NS + 1
        ));
    }

    /// ハードウェア不要の構築テスト: `AppleBackend::new()` はまだ閉じている。
    #[test]
    fn new_backend_starts_closed() {
        let backend = AppleBackend::new();
        assert!(!backend.is_open());
        assert_eq!(backend.sample_rate(), 0);
        assert_eq!(backend.last_callback_frames(), 0);
        assert_eq!(backend.output_latency_ns(), 0);
        assert_eq!(backend.output_underrun_count(), 0);
    }

    /// ハードウェア不要: 開いていないバックエンドを閉じようとすると `NotOpen`。
    #[test]
    fn closing_an_unopened_backend_reports_not_open() {
        let mut backend = AppleBackend::new();
        assert!(matches!(backend.close(), Err(BackendError::NotOpen)));
    }

    /// ハードウェア不要: `log_output_latency_once` は `restart_epoch` が進むたびに
    /// もう一度ログを出せる状態に戻る(「一度ログを出した」ことを示す
    /// `logged_output_latency_epoch` が、再び出していい現在の `restart_epoch` と
    /// 一致しなくなるため)。実際に `mw_log!` が出力したことまでは検査できないが、
    /// この状態遷移が「一度だけ」から「世代ごとに一度」へ変わったことを固定化する。
    #[test]
    fn log_output_latency_once_can_log_again_after_the_restart_epoch_advances() {
        let backend = AppleBackend::new();
        backend
            .output_latency_ns
            .store(23_000_000, Ordering::Relaxed);

        // 初回はまだ出していない(`u64::MAX` のまま)ので必ず出し、現在の epoch(0)を
        // 記録する。
        assert_eq!(
            backend.logged_output_latency_epoch.load(Ordering::Relaxed),
            u64::MAX
        );
        backend.log_output_latency_once();
        assert_eq!(
            backend.logged_output_latency_epoch.load(Ordering::Relaxed),
            0
        );

        // 同じ epoch のうちに繰り返し呼んでも、記録した epoch は変わらない(=再ログしない)。
        backend.log_output_latency_once();
        assert_eq!(
            backend.logged_output_latency_epoch.load(Ordering::Relaxed),
            0
        );

        // 動かし直し・作り直し(`UnitControl::play`/`rebuild`)を模して epoch を進める。
        backend.restart_epoch.fetch_add(1, Ordering::Relaxed);
        backend.log_output_latency_once();
        assert_eq!(
            backend.logged_output_latency_epoch.load(Ordering::Relaxed),
            1,
            "restart_epoch が進んだのにもう一度ログを出す状態へ遷移していない"
        );
    }

    /// ハードウェア不要: `log_new_output_underruns` は、音声スレッドが書いている
    /// `callback_frames`(実測フレーム数)が前回ログしたときから変わったときだけ
    /// `logged_callback_frames` を更新する——Control Center 表示中の出力停止からの
    /// 復帰後、`in_number_frames` が希望値より大きい値のまま戻らないことがあるかを
    /// 実機ログで確かめるための診断(モジュール doc「バッファ長の項の出し方」参照)。
    #[test]
    fn log_new_output_underruns_tracks_callback_frames_changes() {
        let backend = AppleBackend::new();
        assert_eq!(backend.logged_callback_frames.load(Ordering::Relaxed), 0);

        backend.callback_frames.store(240, Ordering::Relaxed);
        backend.log_new_output_underruns();
        assert_eq!(backend.logged_callback_frames.load(Ordering::Relaxed), 240);

        // 変わっていなければ、記録値も変わらない。
        backend.log_new_output_underruns();
        assert_eq!(backend.logged_callback_frames.load(Ordering::Relaxed), 240);

        // ウォッチドッグ復帰後に `in_number_frames` が戻らなかった状況を模倣。
        backend.callback_frames.store(2048, Ordering::Relaxed);
        backend.log_new_output_underruns();
        assert_eq!(
            backend.logged_callback_frames.load(Ordering::Relaxed),
            2048,
            "callback_frames の変化を検知できていない"
        );
    }

    /// 実デバイスを開いて数百 ms 鳴らす(無音)。CI・ヘッドレス環境では実行しない。
    /// `cargo test --features backend-native -- --ignored` で手動実行する。
    #[test]
    #[ignore = "実デバイス(既定の出力)を開いて鳴らすため、手元の macOS でのみ実行する"]
    fn opens_the_real_default_output_device_and_renders_silently_for_a_while() {
        let (renderer, _sender, _reclaim, _music_producer, _music_clock, events, _bgm) =
            mw_core::Renderer::build(mw_core::Config::default(), 48_000);

        let mut backend = AppleBackend::new();
        backend
            .open(renderer, events)
            .expect("open should succeed on a machine with a real default output device");

        std::thread::sleep(std::time::Duration::from_millis(300));

        assert!(
            backend.last_callback_frames() > 0,
            "the render callback never ran"
        );
        assert!(backend.sample_rate() > 0);

        backend.close().expect("close should succeed");
        assert!(!backend.is_open());
    }

    /// テスト専用: `render_proc` を直接呼ぶための最小の `CallbackContext`。
    fn test_callback_context(sample_rate: u32) -> *mut CallbackContext {
        let (renderer, _sender, _reclaim, _music_producer, music_clock, _events, _bgm) =
            mw_core::Renderer::build(mw_core::Config::default(), sample_rate);
        Box::into_raw(Box::new(CallbackContext {
            renderer,
            sample_rate,
            device_extra_latency_ns: Arc::new(AtomicU64::new(0)),
            last_device_extra_latency_ns: 0,
            io_buffer_duration_ns: Arc::new(AtomicU64::new(0)),
            last_io_buffer_duration_ns: 0,
            music_clock,
            restart_epoch: Arc::new(AtomicU64::new(0)),
            last_restart_epoch: 0,
            io_probe_lead_ns: Arc::new(AtomicI64::new(0)),
            io_probe_sample_time_gap_frames: Arc::new(AtomicI64::new(0)),
            io_probe_flags: Arc::new(AtomicU32::new(0)),
            last_probe_sample_time: 0.0,
            last_probe_frames: 0,
            consecutive_oversized_callbacks: Arc::new(AtomicU32::new(0)),
            underrun_tracker: OutputUnderrunTracker::new(
                Arc::new(AtomicU64::new(0)),
                Arc::new(AtomicU64::new(0)),
                Arc::new(AtomicU32::new(0)),
            ),
            callback_frames: Arc::new(AtomicU32::new(0)),
            output_latency_ns: Arc::new(AtomicU64::new(0)),
            render_completions: Arc::new(AtomicU64::new(0)),
        }))
    }

    /// ハードウェア不要・ダミー `AudioTimeStamp`(host_time 以外は読まれないので 0 で良い)。
    fn dummy_time_stamp(host_time: u64) -> AudioTimeStamp {
        AudioTimeStamp {
            sample_time: 0.0,
            host_time,
            rate_scalar: 1.0,
            word_clock_time: 0,
            smpte_time: SmpteTime {
                subframes: 0,
                subframe_divisor: 0,
                counter: 0,
                time_type: 0,
                flags: 0,
                hours: 0,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            flags: 0,
            reserved: 0,
        }
    }

    /// 再現テスト(レビュー指摘4): `AudioBufferList::number_buffers` がインターリーブ形式の
    /// 前提(常に1)と異なる値のとき、実装はコメント(`AudioBufferList` の doc)が言う
    /// 「念のため確認する」を満たしていない——現状は `== 0` しか弾かず、2以上は
    /// そのまま `buffers[0]` を使って書き込んでしまう。
    #[test]
    fn render_proc_skips_writing_when_number_buffers_is_not_one() {
        let context_ptr = test_callback_context(48_000);

        const SENTINEL: f32 = 1.234_5;
        let mut buffer = [SENTINEL; 8]; // 4 frames * 2ch
        let mut buffer_list = AudioBufferList {
            // 本来インターリーブ形式では常に1のはずの値を、わざと2にして再現する。
            number_buffers: 2,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };
        let time_stamp = dummy_time_stamp(1_000);

        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                4,
                &mut buffer_list,
            )
        };

        assert_eq!(status, NO_ERR);
        assert_eq!(
            buffer, [SENTINEL; 8],
            "number_buffers != 1 のときは書き込みをスキップするはず"
        );
        let context = unsafe { &*context_ptr };
        assert_eq!(
            context.callback_frames.load(Ordering::Relaxed),
            0,
            "スキップしたときは callback_frames も更新されないはず"
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// `number_buffers == 1`(正常系)では引き続き書き込むことの確認——上のテストが
    /// 「2以上なら弾く」を固定するのに対し、こちらは「1なら通る」を固定する。
    #[test]
    fn render_proc_writes_when_number_buffers_is_one() {
        let context_ptr = test_callback_context(48_000);

        const SENTINEL: f32 = 1.234_5;
        let mut buffer = [SENTINEL; 8]; // 4 frames * 2ch
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };
        let time_stamp = dummy_time_stamp(1_000);

        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                4,
                &mut buffer_list,
            )
        };

        assert_eq!(status, NO_ERR);
        assert_ne!(
            buffer, [SENTINEL; 8],
            "number_buffers == 1 では Renderer::render が書き込むはず"
        );
        let context = unsafe { &*context_ptr };
        assert_eq!(context.callback_frames.load(Ordering::Relaxed), 4);

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// レビュー【高】の再現: `device_extra_latency_ns`(補正項)が書き換わっても、
    /// `render_proc` は現状これを検知せず、`MusicClockPublisher` の世代(generation)が
    /// 進まないまま次のコールバックで `host_time_ns` だけが飛ぶ。これは `clock.rs::
    /// MusicClockSnapshot` の契約(「世代を跨いだ外挿をしてはならない」)に反する——
    /// iOS/tvOS では `IosOutputLatencyWatcher` がルート変化のたびにこの値を書き換える
    /// (`AppleBackend::open` のドキュメント参照)。
    #[test]
    fn render_proc_bumps_music_clock_generation_when_the_latency_correction_changes() {
        let context_ptr = test_callback_context(48_000);

        let mut buffer = [0.0f32; 8]; // 4 frames * 2ch
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };
        let time_stamp = dummy_time_stamp(1_000);

        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                4,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);

        let generation_before = unsafe { (*context_ptr).renderer.music_clock_handle() }
            .snapshot()
            .generation;

        // ルート変化を模す: 監視スレッド(`IosOutputLatencyWatcher`)が補正項を書き換える
        // 想定の再現。ここでは `render_proc` を直接呼ぶテストなので、同じスレッドから
        // store するだけで「検知すべき変化」自体は忠実に再現できる(検知ロジックは
        // この値を読むだけで、どのスレッドが書いたかを区別しない)。
        unsafe {
            (*context_ptr)
                .device_extra_latency_ns
                .store(80_000_000, Ordering::Relaxed) // 80ms(例: A2DP への切替)
        };

        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                4,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);

        let generation_after = unsafe { (*context_ptr).renderer.music_clock_handle() }
            .snapshot()
            .generation;

        assert_ne!(
            generation_before, generation_after,
            "補正項が変わったのに世代が進んでいない(MusicClockSnapshot の契約違反)"
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// 上と対になる固定化: バッファ長の項(`io_buffer_duration_ns`。iOS/tvOS の
    /// `AVAudioSession.IOBufferDuration()` 由来)が書き換わったときも、補正項と同じく
    /// 世代が進むこと(モジュール doc「バッファ長の項の出し方」参照)。
    #[test]
    fn render_proc_bumps_music_clock_generation_when_the_io_buffer_duration_changes() {
        let context_ptr = test_callback_context(48_000);

        let generation_before = render_once_and_read_generation(context_ptr);

        // ルート変化を模す: `IosOutputLatencyWatcher`/`refresh_ios_output_latency` が
        // `device_extra_latency_ns` と同じタイミングで書き換える想定の再現。
        unsafe {
            (*context_ptr)
                .io_buffer_duration_ns
                .store(2_000_000, Ordering::Relaxed) // 2ms(例: IOBufferDuration の変化)
        };

        let generation_after = render_once_and_read_generation(context_ptr);

        assert_ne!(
            generation_before, generation_after,
            "バッファ長の項が変わったのに世代が進んでいない(MusicClockSnapshot の契約違反)"
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// 上の2テストと対になる固定化: 補正項・バッファ長の項が**どちらも変わらない**限り、
    /// 世代は進まないはず(`AppleBackend::open` が最初のコールバックより前に
    /// `last_device_extra_latency_ns`/`last_io_buffer_duration_ns` を実際の初期値で
    /// 埋めておく修正の固定化——ここを 0 のまま放置すると、ルート変化が一度も起きていない
    /// macOS/通常運用でも最初のコールバックで無意味な世代の繰り上げが起きてしまう)。
    /// 複数回のコールバックをまたいでも成り立つことを確認する。
    #[test]
    fn render_proc_does_not_bump_music_clock_generation_when_the_latency_correction_is_unchanged() {
        let context_ptr = test_callback_context(48_000);
        // `test_callback_context` と同じ初期値(0)で明示的に揃えておく——以後一度も
        // 書き換えない。
        unsafe {
            (*context_ptr)
                .device_extra_latency_ns
                .store(0, Ordering::Relaxed);
            (*context_ptr).last_device_extra_latency_ns = 0;
            (*context_ptr)
                .io_buffer_duration_ns
                .store(0, Ordering::Relaxed);
            (*context_ptr).last_io_buffer_duration_ns = 0;
        }

        let mut buffer = [0.0f32; 8]; // 4 frames * 2ch
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };
        let time_stamp = dummy_time_stamp(1_000);

        for _ in 0..3 {
            let status = unsafe {
                render_proc(
                    context_ptr.cast(),
                    std::ptr::null_mut(),
                    &time_stamp,
                    0,
                    4,
                    &mut buffer_list,
                )
            };
            assert_eq!(status, NO_ERR);
        }

        let generation = unsafe { (*context_ptr).renderer.music_clock_handle() }
            .snapshot()
            .generation;
        assert_eq!(
            generation, 0,
            "補正項が一度も変わっていないのに世代が進んでいる(誤検知)"
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// テスト専用: 4 フレームぶんの有効なバッファで `render_proc` を1回呼び、呼んだ後の
    /// 音楽クロックの世代を返す。
    fn render_once_and_read_generation(context_ptr: *mut CallbackContext) -> u32 {
        let mut buffer = [0.0f32; 8]; // 4 frames * 2ch
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };
        let time_stamp = dummy_time_stamp(1_000);
        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                4,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);
        unsafe { (*context_ptr).renderer.music_clock_handle() }
            .snapshot()
            .generation
    }

    /// 出力を動かし直した・作り直した(`restart_epoch` が進んだ)次のコールバックで、
    /// 世代が1つ進む。動かし直しが無ければ進まない。
    #[test]
    fn render_proc_bumps_music_clock_generation_once_after_the_output_is_restarted() {
        let context_ptr = test_callback_context(48_000);

        let before = render_once_and_read_generation(context_ptr);
        assert_eq!(render_once_and_read_generation(context_ptr), before);

        // `UnitControl::play` / `rebuild` が行う書き込みを模す。
        unsafe { (*context_ptr).restart_epoch.fetch_add(1, Ordering::Relaxed) };

        let after = render_once_and_read_generation(context_ptr);
        assert_eq!(after, before + 1);
        assert_eq!(
            render_once_and_read_generation(context_ptr),
            after,
            "動かし直しの後のコールバックで世代が進み続けている"
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// 動かし直し・補正項・バッファ長の項の変化が同じコールバックで見えても、世代は
    /// 1つだけ進む。
    #[test]
    fn render_proc_bumps_music_clock_generation_once_when_restart_and_correction_change_together() {
        let context_ptr = test_callback_context(48_000);
        let before = render_once_and_read_generation(context_ptr);

        unsafe {
            (*context_ptr).restart_epoch.fetch_add(1, Ordering::Relaxed);
            (*context_ptr)
                .device_extra_latency_ns
                .store(80_000_000, Ordering::Relaxed);
            (*context_ptr)
                .io_buffer_duration_ns
                .store(2_000_000, Ordering::Relaxed);
        }

        assert_eq!(render_once_and_read_generation(context_ptr), before + 1);

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// `dummy_time_stamp` に `sample_time`/`flags` も指定できる版
    /// (モジュール doc「診断用プローブ(io probe)」のテスト用)。
    fn probe_time_stamp(host_time: u64, sample_time: f64, flags: u32) -> AudioTimeStamp {
        AudioTimeStamp {
            sample_time,
            flags,
            ..dummy_time_stamp(host_time)
        }
    }

    /// モジュール doc「オーバーサイズのコールバックからの直し」: 期待フレーム数
    /// (`io_buffer_duration_ns` から求める、ここでは 5ms=240 フレーム @ 48kHz)の2倍を超える
    /// コールバックが連続している回数を数え、普段のサイズに戻ったら 0 にリセットする
    /// (`consecutive_oversized_callbacks`、判定は `is_oversized_callback`)。
    #[test]
    fn render_proc_tracks_a_streak_of_oversized_callbacks_and_resets_on_a_normal_sized_callback() {
        let context_ptr = test_callback_context(48_000);
        unsafe {
            (*context_ptr)
                .io_buffer_duration_ns
                .store(5_000_000, Ordering::Relaxed); // 240 frames @ 48kHz
        }
        let time_stamp = dummy_time_stamp(1_000);

        // 2048 フレームは 240*2=480 を超える(実機で観測されている値そのもの)。
        let mut oversized_buffer = aligned_test_buffer(); // 4096 要素 = 2048 フレーム * 2ch
        for expected_streak in 1..=3u32 {
            let mut buffer_list = AudioBufferList {
                number_buffers: 1,
                buffers: [AudioBuffer {
                    number_channels: 2,
                    data_byte_size: (oversized_buffer.len() * std::mem::size_of::<f32>()) as u32,
                    data: oversized_buffer.as_mut_ptr().cast(),
                }],
            };
            let status = unsafe {
                render_proc(
                    context_ptr.cast(),
                    std::ptr::null_mut(),
                    &time_stamp,
                    0,
                    2048,
                    &mut buffer_list,
                )
            };
            assert_eq!(status, NO_ERR);
            assert_eq!(
                unsafe {
                    (*context_ptr)
                        .consecutive_oversized_callbacks
                        .load(Ordering::Relaxed)
                },
                expected_streak
            );
        }

        // 普段のサイズ(240 フレーム)に戻ったら 0 にリセットされる。
        let mut normal_buffer = [0.0f32; 240 * 2];
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (normal_buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: normal_buffer.as_mut_ptr().cast(),
            }],
        };
        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                240,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);
        assert_eq!(
            unsafe {
                (*context_ptr)
                    .consecutive_oversized_callbacks
                    .load(Ordering::Relaxed)
            },
            0,
            "普段のサイズに戻ったのに streak がリセットされていない"
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// `io_buffer_duration_ns` が 0(macOS。または iOS/tvOS でまだ読んでいない)のときは、
    /// `in_number_frames` がどれだけ大きくても streak が進まない(`is_oversized_callback`
    /// の「期待フレーム数が0なら判定しない」が `render_proc` 経由でも成り立つことの固定化)。
    #[test]
    fn render_proc_does_not_track_oversized_callbacks_when_io_buffer_duration_is_zero() {
        let context_ptr = test_callback_context(48_000);
        let time_stamp = dummy_time_stamp(1_000);
        let mut buffer = aligned_test_buffer();
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };

        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                2048,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);
        assert_eq!(
            unsafe {
                (*context_ptr)
                    .consecutive_oversized_callbacks
                    .load(Ordering::Relaxed)
            },
            0
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// モジュール doc「診断用プローブ(io probe)」の **flags**:
    /// `AudioTimeStamp.flags` をそのまま写すだけ。
    #[test]
    fn render_proc_copies_the_raw_timestamp_flags_into_the_probe() {
        let context_ptr = test_callback_context(48_000);
        let time_stamp = probe_time_stamp(1_000, 0.0, 0x1F);

        let mut buffer = [0.0f32; 8]; // 4 frames * 2ch
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };
        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                4,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);
        assert_eq!(
            unsafe { (*context_ptr).io_probe_flags.load(Ordering::Relaxed) },
            0x1F
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// 診断用プローブの **lead**(= 相関点 − コールバックに入った時点の `host_time_ns()`)。
    /// テストの `AudioTimeStamp.host_time` はごく小さい値(1000)なので、ns 化しても
    /// 実際の現在時刻(`host_time_ns()`、プロセスの単調時刻)よりはるかに小さく、
    /// 符号が必ず負になる。
    #[test]
    fn render_proc_computes_a_negative_lead_for_a_tiny_test_host_time() {
        let context_ptr = test_callback_context(48_000);
        let time_stamp = dummy_time_stamp(1_000);

        let mut buffer = [0.0f32; 8];
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };
        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                4,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);
        assert!(
            unsafe { (*context_ptr).io_probe_lead_ns.load(Ordering::Relaxed) } < 0,
            "テストの mHostTime はごく小さい値なので、実際の現在時刻より手前(負)になるはず"
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// 診断用プローブの **mSampleTime の連続性**: 2回目の `sample_time` が
    /// 「1回目の `sample_time` + 1回目のフレーム数」どおりに続いていれば差は0。
    #[test]
    fn render_proc_reports_zero_sample_time_gap_when_continuous() {
        let context_ptr = test_callback_context(48_000);
        let mut buffer = [0.0f32; 8]; // 4 frames * 2ch
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };

        let first = probe_time_stamp(1_000, 1_000.0, 0);
        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &first,
                0,
                4,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);

        let second = probe_time_stamp(2_000, 1_004.0, 0); // 1000 + 4 フレームどおり
        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &second,
                0,
                4,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);
        assert_eq!(
            unsafe {
                (*context_ptr)
                    .io_probe_sample_time_gap_frames
                    .load(Ordering::Relaxed)
            },
            0,
            "前回の sample_time + 前回のフレーム数どおりに続いているので差は0のはず"
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// 上と対になる固定化: コールバックの間が空けば(足りないフレーム数ぶん)差が正に出る。
    #[test]
    fn render_proc_reports_a_positive_sample_time_gap_when_a_callback_is_skipped() {
        let context_ptr = test_callback_context(48_000);
        let mut buffer = [0.0f32; 8];
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };

        let first = probe_time_stamp(1_000, 1_000.0, 0);
        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &first,
                0,
                4,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);

        // 本来 1000 + 4 = 1004 のはずが、1010 まで飛んでいる(6 フレームぶんの欠落)。
        let second = probe_time_stamp(2_000, 1_010.0, 0);
        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &second,
                0,
                4,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);
        assert_eq!(
            unsafe {
                (*context_ptr)
                    .io_probe_sample_time_gap_frames
                    .load(Ordering::Relaxed)
            },
            6
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// `log_new_output_underruns` の io probe ログ判定(`should_log_io_probe`)が
    /// `restart_epoch` の変化を実際に拾うこと——初回呼び出しは「まだ1度もログしていない」
    /// (`u64::MAX` の sentinel)が現在値と食い違うため必ず通る([`log_output_latency_once`]
    /// と同じ idiom)。
    #[test]
    fn log_new_output_underruns_logs_the_io_probe_on_the_first_call() {
        let backend = AppleBackend::new();
        assert_eq!(
            backend
                .logged_io_probe_restart_epoch
                .load(Ordering::Relaxed),
            u64::MAX
        );

        backend.log_new_output_underruns();

        assert_eq!(
            backend
                .logged_io_probe_restart_epoch
                .load(Ordering::Relaxed),
            0,
            "初回呼び出しで現在の restart_epoch(0)を記録しているはず"
        );
        assert_ne!(
            backend.logged_io_probe_host_time_ns.load(Ordering::Relaxed),
            0,
            "ログを出したのでログ時刻が記録されているはず"
        );
    }

    /// 閉じた後(`OpenUnit` を取り出した後)の制御口は、AudioUnit に触れずにエラーを返す。
    /// 復帰確認のワーカーが `close` の後まで走り続けても、閉じたユニットへ Start を撃たない。
    #[test]
    fn unit_control_does_nothing_once_the_unit_is_closed() {
        let restart_epoch = Arc::new(AtomicU64::new(0));
        let control = UnitControl {
            slot: Mutex::new(None),
            sample_rate: 48_000.0,
            restart_epoch: Arc::clone(&restart_epoch),
            device_extra_latency_ns: Arc::new(AtomicU64::new(0)),
            io_buffer_duration_ns: Arc::new(AtomicU64::new(0)),
            render_completions: Arc::new(AtomicU64::new(0)),
            consecutive_oversized_callbacks: Arc::new(AtomicU32::new(0)),
        };

        assert!(control.supports_rebuild());
        assert!(control.pause().is_err());
        assert!(control.play().is_err());
        assert!(control.rebuild().is_err());
        assert_eq!(
            restart_epoch.load(Ordering::Relaxed),
            0,
            "閉じた後の操作で世代を進める合図を出している"
        );
    }

    /// 閾値未満では何もしない(`rebuild()` を試みない——制御口が閉じていても
    /// `restart_epoch` は不変のまま)。
    #[test]
    fn rebuild_if_oversized_callbacks_does_nothing_below_the_threshold() {
        let restart_epoch = Arc::new(AtomicU64::new(0));
        let streak = Arc::new(AtomicU32::new(OVERSIZED_CALLBACK_REBUILD_THRESHOLD - 1));
        let control = UnitControl {
            slot: Mutex::new(None),
            sample_rate: 48_000.0,
            restart_epoch: Arc::clone(&restart_epoch),
            device_extra_latency_ns: Arc::new(AtomicU64::new(0)),
            io_buffer_duration_ns: Arc::new(AtomicU64::new(0)),
            render_completions: Arc::new(AtomicU64::new(0)),
            consecutive_oversized_callbacks: Arc::clone(&streak),
        };

        control.rebuild_if_oversized_callbacks();

        assert_eq!(
            streak.load(Ordering::Relaxed),
            OVERSIZED_CALLBACK_REBUILD_THRESHOLD - 1,
            "閾値未満なのに streak を変えている(試みてもいない)"
        );
        assert_eq!(restart_epoch.load(Ordering::Relaxed), 0);
    }

    /// 閾値に達したら `rebuild()` を試みる(制御口が閉じているので失敗するが、
    /// streak は試みた時点でリセットする——失敗が続いても毎回再試行を積み重ねない)。
    #[test]
    fn rebuild_if_oversized_callbacks_attempts_rebuild_at_the_threshold() {
        let restart_epoch = Arc::new(AtomicU64::new(0));
        let streak = Arc::new(AtomicU32::new(OVERSIZED_CALLBACK_REBUILD_THRESHOLD));
        let control = UnitControl {
            slot: Mutex::new(None),
            sample_rate: 48_000.0,
            restart_epoch: Arc::clone(&restart_epoch),
            device_extra_latency_ns: Arc::new(AtomicU64::new(0)),
            io_buffer_duration_ns: Arc::new(AtomicU64::new(0)),
            render_completions: Arc::new(AtomicU64::new(0)),
            consecutive_oversized_callbacks: Arc::clone(&streak),
        };

        control.rebuild_if_oversized_callbacks();

        assert_eq!(
            streak.load(Ordering::Relaxed),
            0,
            "試みた以上は streak をリセットするはず"
        );
        assert_eq!(
            restart_epoch.load(Ordering::Relaxed),
            0,
            "制御口が閉じているので rebuild() 自体は失敗し、世代は進まないはず"
        );
    }

    /// 実デバイスで、止め直し → 動かし直し、作り直し → 動かし直しのそれぞれの後に
    /// コールバックが再び進むこと(作り直しでは同じ context を新しいユニットへ付け替える)。
    /// CI・ヘッドレス環境では実行しない。
    #[test]
    #[ignore = "実デバイス(既定の出力)を開いて鳴らすため、手元の macOS でのみ実行する"]
    fn restarting_and_rebuilding_the_real_output_resumes_the_callback() {
        let (renderer, _sender, _reclaim, _music_producer, music_clock, events, _bgm) =
            mw_core::Renderer::build(mw_core::Config::default(), 48_000);

        let mut backend = AppleBackend::new();
        backend
            .open(renderer, events)
            .expect("open should succeed on a machine with a real default output device");
        let control = Arc::clone(backend.control.as_ref().expect("open"));

        let wait_for_progress = |label: &str| {
            let before = backend.render_completions.load(Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(200));
            let after = backend.render_completions.load(Ordering::Relaxed);
            assert!(
                after > before,
                "{label}: the render callback did not advance"
            );
        };

        wait_for_progress("after open");
        let generation_open = music_clock.snapshot().generation;

        control.pause().expect("pause");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let paused = backend.render_completions.load(Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(
            backend.render_completions.load(Ordering::Relaxed),
            paused,
            "the callback kept running after AudioOutputUnitStop"
        );
        control.play().expect("play");
        wait_for_progress("after pause + play");
        let generation_restart = music_clock.snapshot().generation;
        assert!(generation_restart > generation_open);

        control.rebuild().expect("rebuild");
        control.play().expect("play after rebuild");
        wait_for_progress("after rebuild + play");
        assert!(music_clock.snapshot().generation > generation_restart);

        backend.close().expect("close should succeed");
        assert!(!backend.is_open());
        assert!(
            control.play().is_err(),
            "the control must be inert after close"
        );
    }
}
