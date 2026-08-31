//! C ABI 境界の関数本体。
//!
//! 初期構築仕様 §5.4(確定):
//! - 全関数スレッドセーフ・非ブロッキング・エラーコード返し(§4.8)
//! - シンボルは `mw_` プレフィックスで統一
//! - panic 境界(catch_unwind)で FFI から Rust panic を漏らさない
//!
//! このファイルは `build.rs` から csbindgen の入力として読まれ、
//! `unity/Runtime/Generated/NativeMethods.g.cs` を自動生成する。
//! 関数シグネチャを変える際はコメントだけでなく実際の型を変更すること
//! (手書きの宣言ズレを構造的に排除するのが csbindgen 採用の目的、M2)。

use std::panic::{self, AssertUnwindSafe};
use std::sync::Once;

use crate::event::MwEvent;
use crate::handle as handle_registry;
use crate::handle::{InitOutcome, ShutdownOutcome};
use crate::result::MwResult;
use crate::types::{MwBus, MwMusicPosition, MwMusicState, MwOutputUnderrunStats, MwSoundMode};

/// ABI バージョン。ABI 互換の破壊は semver メジャーバージョンでのみ許可する(§4.8)。
const ABI_VERSION: u32 = 1;

/// ABI バージョンを返す。C# 側は起動時にこれを期待値と照合すること。
#[unsafe(no_mangle)]
pub extern "C" fn mw_abi_version() -> u32 {
    ABI_VERSION
}

/// ホスト単調時刻をナノ秒で取得する(初期構築仕様『§4.4 音楽クロック』, M2-5)。
///
/// C# 側は起動時にこの値と `Time.realtimeSinceStartupAsDouble` を1回ずつサンプリングし、
/// その差を定数オフセットとして保持することで両者を橋渡しする
/// (`mw_backend::host_time` のモジュール doc に、どの時計を使ったか・
/// `cpal::OutputCallbackInfo` のデバイスタイムスタンプと直接比較できることの
/// 調査結果を記載してある)。`mw_se_schedule` / `mw_music_play_scheduled` の
/// `host_time_ns` 引数はこの関数と同じ時計の値を渡すこと。
///
/// ハンドル不要(`mw_init` 前でも呼べる)。GC アロケーションゼロ(初期構築仕様 §5.4)。
/// アロケーション・ロック・パニック経路を持たないため `catch_unwind` で包んでいない
/// (`mw_abi_version` と同じ扱い)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_host_time_ns() -> u64 {
    mw_backend::host_time_ns()
}

/// ミドルウェアを初期化し、既定の出力デバイスにストリームを開いて再生を開始する。
///
/// 冪等: 既に初期化済みの場合は同一ハンドルを `out_handle` に書き `MwResult::Ok` を返す
/// (Unity Editor のドメインリロード対策、§6)。
///
/// # Safety
/// `out_handle` は書き込み可能な `u64` を指す有効なポインタであるか、null でなければならない。
/// null の場合は書き込みを行わず `MwResult::ErrNullPointer` を返す。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mw_init(out_handle: *mut u64) -> MwResult {
    install_panic_hook();

    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if out_handle.is_null() {
            return None;
        }
        Some(handle_registry::init())
    }));

    match outcome {
        Ok(None) => MwResult::ErrNullPointer,
        Ok(Some(InitOutcome::Opened(h))) | Ok(Some(InitOutcome::AlreadyOpen(h))) => {
            // SAFETY: 上の分岐で null チェック済み。呼び出し規約上、書き込み可能な
            // `u64` を指すポインタであることは呼び出し側の責務(FFI 境界の契約)。
            unsafe {
                *out_handle = h;
            }
            MwResult::Ok
        }
        Ok(Some(InitOutcome::Failed)) => MwResult::ErrBackendOpenFailed,
        Err(_) => MwResult::ErrPanic,
    }
}

/// panic の内容をログへ出すフックを1度だけ入れる。
///
/// `catch_unwind` は panic を `MwResult::ErrPanic` に畳んでしまうため、**何が起きたのかは
/// 呼び出し側に一切伝わらない**。既定の panic ハンドラはメッセージを stderr へ書くが、
/// Android ではプロセスの stderr がどこにも出ないため実機で読めなかった
/// (docs/measurement-m1.md §6.2。実際に `ErrPanic` の原因究明で詰まった)。
/// ここで `mw_backend::mw_log!` 経由にして logcat へ流す。
///
/// 既定のフックは置き換えずに**後ろで呼ぶ**ため、デスクトップでの表示は変わらない。
fn install_panic_hook() {
    static INSTALL: Once = Once::new();

    INSTALL.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            mw_backend::mw_log!("[mw-ffi] panic: {info}");
            previous(info);
        }));
    });
}

/// ミドルウェアを終了し、出力ストリームを停止する。
///
/// 無効なハンドル(未初期化・二重 shutdown・他インスタンスのハンドル)は
/// `MwResult::ErrInvalidHandle` を返す。クラッシュはしない(§4.8)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_shutdown(handle: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| handle_registry::shutdown(handle)));

    match outcome {
        Ok(ShutdownOutcome::Closed) => MwResult::Ok,
        Ok(ShutdownOutcome::CloseFailed) => MwResult::ErrBackendCloseFailed,
        Ok(ShutdownOutcome::InvalidHandle) => MwResult::ErrInvalidHandle,
        Err(_) => MwResult::ErrPanic,
    }
}

// --- M1: SE 再生 ------------------------------------------------------------
//
// 初期構築仕様 §5.5(API 概形)/ §4.2(SE 再生)/ §5.2(スレッドモデル)。
// これらの関数はすべて「コマンドをキューへ積むだけ」で非ブロッキング(§5.4)。
// 実際のミキシング・ボイス管理は音声コールバック側(`mw-core::Mixer::render`)が
// コールバック先頭でキューを消化して行う。
//
// `bus` / `mode` 引数は `extern "C"` 境界を越えて Rust の `#[repr(i32)]` enum を
// 直接受け取らない(呼び出し側が範囲外の値を渡すと enum の未定義動作になりうるため)。
// 代わりに素の `i32` として受け取り、`MwBus::from_raw` / `MwSoundMode::from_raw` で
// 検証してから使う。

/// wav バイト列から SE 用 PCM を、または楽曲用の圧縮バイト列をロードする
/// (初期構築仕様 §5.5, §4.2, §5.2)。
///
/// - `mode = 0`(SE): wav を全デコードしてメモリ常駐させる(M1)。対応フォーマットは
///   16bit PCM / モノラルまたはステレオの wav のみ(`crates/mw-core/src/wav.rs`)。
///   サンプルレートは出力デバイスと一致しなくてよい(一致しない場合はロード時に
///   一括でリサンプルする。初期構築仕様『§4.7』)。非対応の場合は原因に応じた
///   エラーコードを返す。
/// - `mode = 1`(Music, M2-7): デコードせず圧縮バイト列のまま保持する
///   (初期構築仕様『§5.5』「Music(圧縮のまま保持)」)。フォーマットの妥当性検証は
///   ここでは行わない——`mw_music_set` が実際にストリーミング準備を試みた時点で
///   検出する。発行される ID は SE の ID とは空間が分離されている
///   (`crate::handle::MUSIC_ID_FLAG` 参照。同じ ID を `mw_sound_release` に渡すと
///   自動的に正しいストレージへ振り分けられる)。
/// - それ以外の未知の `mode` は `MwResult::ErrUnsupportedSoundMode` を返す。
///
/// # Safety
/// `bytes` は `len` バイトの読み取り可能な領域を指す有効なポインタであるか、
/// `len == 0` の場合に限り null でもよい。`out_id` は書き込み可能な `u64` を指す
/// 有効なポインタであるか、null でなければならない(null は書き込みを行わず
/// `MwResult::ErrNullPointer`)。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mw_sound_load(
    handle: u64,
    bytes: *const u8,
    len: usize,
    mode: i32,
    out_id: *mut u64,
) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if out_id.is_null() {
            return MwResult::ErrNullPointer;
        }
        if bytes.is_null() && len != 0 {
            return MwResult::ErrNullPointer;
        }
        let Some(mode) = MwSoundMode::from_raw(mode) else {
            return MwResult::ErrUnsupportedSoundMode;
        };

        let slice: &[u8] = if len == 0 {
            &[]
        } else {
            // SAFETY: 上で null チェック済み。呼び出し側が `len` バイトの読み取り可能な
            // 領域を渡す契約(このシンボルの `Safety` セクションで文書化済み)。
            unsafe { std::slice::from_raw_parts(bytes, len) }
        };

        match mode {
            // SAFETY: `out_id` は呼び出し元(このすぐ上)で null チェック済み。
            MwSoundMode::Se => unsafe { load_se(handle, slice, out_id) },
            MwSoundMode::Music => unsafe { load_music(handle, slice, out_id) },
        }
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// `mw_sound_load` の SE(`mode = 0`)経路。全デコードしてメモリ常駐させる(M1)。
///
/// # Safety
/// `out_id` は書き込み可能な `u64` を指す有効なポインタであること
/// (呼び出し元 `mw_sound_load` が null チェック済み)。
unsafe fn load_se(handle: u64, slice: &[u8], out_id: *mut u64) -> MwResult {
    // wav のサンプルレートと出力デバイスのレートが一致しない場合はロード時に
    // 一括リサンプルして吸収する(初期構築仕様『§4.7』, `mw_core::wav::decode` 参照)。
    // そのため出力レートを先に(ハンドル経由で)確定させておく必要がある。
    let Some(output_sample_rate) =
        handle_registry::with_instance(handle, |instance| instance.backend_sample_rate())
    else {
        return MwResult::ErrInvalidHandle;
    };

    let sound_data = match mw_core::wav::decode(slice, output_sample_rate) {
        Ok(data) => data,
        Err(err) => {
            mw_backend::mw_log!("[mw-ffi] mw_sound_load: decode failed: {err}");
            return match err {
                mw_core::WavError::InvalidSampleRate(_) => MwResult::ErrUnsupportedSampleRate,
                mw_core::WavError::Resample(_) => MwResult::ErrDecodeFailed,
                mw_core::WavError::UnsupportedFormatTag(_)
                | mw_core::WavError::UnsupportedBitsPerSample(_)
                | mw_core::WavError::UnsupportedChannelCount(_) => MwResult::ErrUnsupportedFormat,
                mw_core::WavError::Truncated
                | mw_core::WavError::NotRiff
                | mw_core::WavError::NotWave
                | mw_core::WavError::MissingFmtChunk
                | mw_core::WavError::MissingDataChunk => MwResult::ErrDecodeFailed,
            };
        }
    };

    let assigned_id = handle_registry::with_instance(handle, |instance| {
        instance.drain_reclaimed();
        let mut sounds = instance
            .sounds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sounds.insert(sound_data)
    });

    match assigned_id {
        Some(id) => {
            // SAFETY: 呼び出し元の契約(この関数の Safety セクション)。
            unsafe {
                *out_id = id.0;
            }
            MwResult::Ok
        }
        None => MwResult::ErrInvalidHandle,
    }
}

/// `mw_sound_load` の Music(`mode = 1`, M2-7)経路。デコードせず圧縮バイト列の
/// まま保持する(初期構築仕様『§5.5』)。
///
/// # Safety
/// `out_id` は書き込み可能な `u64` を指す有効なポインタであること
/// (呼び出し元 `mw_sound_load` が null チェック済み)。
unsafe fn load_music(handle: u64, slice: &[u8], out_id: *mut u64) -> MwResult {
    let bytes = slice.to_vec();
    let assigned_id =
        handle_registry::with_instance(handle, |instance| instance.insert_music_bytes(bytes));

    match assigned_id {
        Some(id) => {
            // SAFETY: 呼び出し元の契約(この関数の Safety セクション)。
            unsafe {
                *out_id = id;
            }
            MwResult::Ok
        }
        None => MwResult::ErrInvalidHandle,
    }
}

/// ロード済みサウンド(SE または楽曲)を解放する(初期構築仕様 §5.5)。
///
/// `id` の最上位ビット(`crate::handle::MUSIC_ID_FLAG`)を見て、SE 用ストレージ
/// (`mw_core::SoundStorage`)・楽曲用ストレージ(`Instance::music_bytes`)の
/// どちらを解放すべきか自動的に振り分ける(呼び出し側が意識する必要はない)。
///
/// - SE: 再生中のボイスがあれば、既定ランプ(§4.1/M13)経由で即座に停止させたうえで
///   解放する。PCM の実データ(`Arc<SoundData>`)の最終的な解放(デアロケーション)は
///   この呼び出し自身(ゲームスレッド)か、あるいは音声スレッドがボイス終了時に
///   回収キューへ送り出したものをこの関数が(次回以降のこの種の呼び出しで)
///   ドレインする経路のいずれかで起こる。いずれにせよ**音声コールバック内で `Arc`
///   がドロップされることはない**(初期構築仕様「PCM データの所有権」)。
/// - 楽曲: 拒否せず即座に解放する。**再生中の楽曲を release した場合でも安全**
///   (`crate::handle::Instance::remove_music_bytes` のドキュメント参照——
///   `mw_music_set` はバイト列の複製をデコードスレッドへ渡す設計のため、
///   ここでの解放はデコード中の再生に一切影響しない)。
///
/// 未知の `id`(未ロード・二重解放)は `MwResult::ErrInvalidSoundId` を返す(クラッシュしない)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_sound_release(handle: u64, id: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if handle_registry::is_music_id(id) {
            release_music(handle, id)
        } else {
            release_se(handle, id)
        }
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

fn release_se(handle: u64, id: u64) -> MwResult {
    let result = handle_registry::with_instance(handle, |instance| {
        instance.drain_reclaimed();
        let removed = {
            let mut sounds = instance
                .sounds
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sounds.remove(mw_core::SoundId(id))
        };
        match removed {
            Some(arc) => {
                let sent = instance
                    .command_sender
                    .send(mw_core::Command::StopVoicesUsingSound { sound_id: id });
                // ストレージ側が保持していた複製をここ(ゲームスレッド)でドロップする。
                // 音声スレッド側の複製(再生中ボイスがあれば)はまだ生きている。
                drop(arc);
                if sent {
                    MwResult::Ok
                } else {
                    MwResult::ErrCommandQueueFull
                }
            }
            None => MwResult::ErrInvalidSoundId,
        }
    });
    result.unwrap_or(MwResult::ErrInvalidHandle)
}

fn release_music(handle: u64, id: u64) -> MwResult {
    let result =
        handle_registry::with_instance(handle, |instance| match instance.remove_music_bytes(id) {
            Some(_bytes) => MwResult::Ok,
            None => MwResult::ErrInvalidSoundId,
        });
    result.unwrap_or(MwResult::ErrInvalidHandle)
}

/// SE を即時発音する(初期構築仕様 §4.2, §5.5)。
///
/// 「次のオーディオコールバックで必ず発音される」— 遅延は1バッファ + 出力レイテンシのみ。
/// 発音に成功すると `out_voice` に不透明なボイス ID を書き込む(`mw_voice_stop` /
/// `mw_voice_set_volume` へ渡せる)。
///
/// # Safety
/// `out_voice` は書き込み可能な `u64` を指す有効なポインタであるか、null でなければならない。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mw_se_play(
    handle: u64,
    id: u64,
    bus: i32,
    volume: f32,
    out_voice: *mut u64,
) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if out_voice.is_null() {
            return MwResult::ErrNullPointer;
        }
        let Some(bus) = MwBus::from_raw(bus) else {
            return MwResult::ErrInvalidBus;
        };

        let result = handle_registry::with_instance(handle, |instance| {
            instance.drain_reclaimed();
            // I/O バッファ長の実測値を1回だけ残す(docs/measurement-m1.md §8.7)。
            // 初回の発音時点ならコールバックは既に走っている。
            instance.log_buffer_info_once();
            let sound = {
                let sounds = instance
                    .sounds
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                sounds.get(mw_core::SoundId(id))
            };
            let Some(sound) = sound else {
                return MwResult::ErrInvalidSoundId;
            };

            let voice_serial = instance.next_voice_serial();
            let sent = instance.command_sender.send(mw_core::Command::PlaySe {
                voice_serial,
                sound_id: id,
                sound,
                bus: bus.to_core(),
                volume,
            });
            if sent {
                // SAFETY: 上で null チェック済み。
                unsafe {
                    *out_voice = voice_serial;
                }
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// SE をサンプル精度で予約発音する(初期構築仕様『§4.5 スケジュール発音』, M2-5)。
///
/// 用途はメトロノームとキャリブレーション用クリック。`host_time_ns` は
/// `mw_host_time_ns()` と同じ時計の値を渡すこと。該当バッファのレンダリング時に
/// **バッファ内オフセットサンプル位置**から発音する(バッファ境界への丸めはしない)。
///
/// 予約時刻が既に過去(処理される時点でバッファ先頭より前)の場合は、取りこぼして
/// 無音にするのではなく、発見可能な最速のサンプル(そのバッファの先頭)で
/// 即座に発音する。
///
/// 予約キューは固定容量(`Config::schedule_queue_capacity`)。この呼び出し自体は
/// コマンドキューへ積めた時点で成功を返すが、音声スレッドが実際に予約キューへ
/// 挿入する段階で満杯だった場合は**その予約は発音されない**
/// (`mw_core::Renderer::se_schedule_overflow_count` で検知できる。イベント通知への
/// 昇格は M2-6 以降)。
///
/// # Safety
/// `out_voice` は書き込み可能な `u64` を指す有効なポインタであるか、null でなければならない。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mw_se_schedule(
    handle: u64,
    id: u64,
    bus: i32,
    volume: f32,
    host_time_ns: u64,
    out_voice: *mut u64,
) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if out_voice.is_null() {
            return MwResult::ErrNullPointer;
        }
        let Some(bus) = MwBus::from_raw(bus) else {
            return MwResult::ErrInvalidBus;
        };

        let result = handle_registry::with_instance(handle, |instance| {
            instance.drain_reclaimed();
            let sound = {
                let sounds = instance
                    .sounds
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                sounds.get(mw_core::SoundId(id))
            };
            let Some(sound) = sound else {
                return MwResult::ErrInvalidSoundId;
            };

            let voice_serial = instance.next_voice_serial();
            let sent = instance.command_sender.send(mw_core::Command::SeSchedule {
                host_time_ns,
                entry: mw_core::ScheduledSe {
                    voice_serial,
                    sound_id: id,
                    sound,
                    bus: bus.to_core(),
                    volume,
                },
            });
            if sent {
                // SAFETY: 上で null チェック済み。
                unsafe {
                    *out_voice = voice_serial;
                }
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// ボイスを停止する(既定ランプ経由。初期構築仕様 M13/§4.2)。
///
/// 無効・既に終了したボイス ID はコマンドとして送出されるが、音声スレッド側で
/// 静かに無視される(不透明なシリアル方式のため、ゲームスレッド側だけでは
/// 「まだ有効か」を同期的に判定できない。§5.2 のコマンドキュー設計上の制約であり、
/// クラッシュや誤動作にはつながらない)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_voice_stop(handle: u64, voice: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let result = handle_registry::with_instance(handle, |instance| {
            instance.drain_reclaimed();
            let sent = instance.command_sender.send(mw_core::Command::StopVoice {
                voice_serial: voice,
            });
            if sent {
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// ボイスの音量を変更する(既定ランプ経由。初期構築仕様 M13/§4.2)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_voice_set_volume(handle: u64, voice: u64, volume: f32) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let result = handle_registry::with_instance(handle, |instance| {
            instance.drain_reclaimed();
            let sent = instance
                .command_sender
                .send(mw_core::Command::SetVoiceVolume {
                    voice_serial: voice,
                    volume,
                });
            if sent {
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// バス音量を変更する(既定ランプ経由。初期構築仕様 M13/§4.1)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_bus_set_volume(handle: u64, bus: i32, volume: f32) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(bus) = MwBus::from_raw(bus) else {
            return MwResult::ErrInvalidBus;
        };
        let result = handle_registry::with_instance(handle, |instance| {
            instance.drain_reclaimed();
            let sent = instance
                .command_sender
                .send(mw_core::Command::SetBusVolume {
                    bus: bus.to_core(),
                    volume,
                });
            if sent {
                // M3(案A): 再オープン後の復元用キャッシュ(`Instance::attempt_reopen`)。
                instance.note_bus_volume(bus.to_core(), volume);
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// バスをフェードする(呼び出し側指定の時間、ms。初期構築仕様 §4.1/§5.5)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_bus_fade(handle: u64, bus: i32, target: f32, ms: f32) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(bus) = MwBus::from_raw(bus) else {
            return MwResult::ErrInvalidBus;
        };
        let result = handle_registry::with_instance(handle, |instance| {
            instance.drain_reclaimed();
            let sent = instance.command_sender.send(mw_core::Command::BusFade {
                bus: bus.to_core(),
                target,
                ms,
            });
            if sent {
                // M3(案A): フェードの収束後の値(`target`)をキャッシュする
                // (`Instance::restore_bus_volumes` のドキュメント参照)。
                instance.note_bus_volume(bus.to_core(), target);
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// 楽曲を予約再生する(初期構築仕様『§4.3 楽曲再生』, M2-5)。
///
/// 指定ホスト時刻にサンプル境界で発音する(`host_time_ns` は `mw_host_time_ns()` と
/// 同じ時計の値を渡すこと)。プリロール完了前(準備中)に予約時刻が到来した場合は
/// エラーにせず、「準備完了後、可能な最速時刻」へ繰り下げる 【仮】。繰り下げが発生したかは
/// 現時点では Rust 内部(`mw_core::Renderer::music_schedule_deferred`)からのみ
/// 確認できる——FFI 公開・イベント通知への昇格は後続作業(M2-6 以降)。
///
/// 楽曲のロード API(`mw_music_set` 相当)はまだ実装していない(M2-5 のスコープ外)ため、
/// 現状はこの呼び出しだけでは実際に音は鳴らない(楽曲ボイスが `Loading` のまま予約が
/// 繰り下げられ続ける)。予約発火の仕組み自体は `mw-core` のオフラインレンダリング
/// テストで検証済み。
#[unsafe(no_mangle)]
pub extern "C" fn mw_music_play_scheduled(handle: u64, host_time_ns: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let result = handle_registry::with_instance(handle, |instance| {
            instance.drain_reclaimed();
            let sent = instance
                .command_sender
                .send(mw_core::Command::MusicPlayScheduled { host_time_ns });
            if sent {
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

// --- M2-7: 楽曲再生 -----------------------------------------------------------
//
// 初期構築仕様『§4.3 楽曲再生』/ 『§4.4 音楽クロック』/ 『§5.5』。
// デコードスレッドの設計(1本だけ立ててデコーダをチャネル越しに差し替える、
// 待ち方・ポーリング間隔)は `crate::decode_thread` モジュール doc を参照。

/// 楽曲のストリーミング再生を準備する(初期構築仕様『§4.3 楽曲再生』『§5.5』)。
///
/// `sound_id` は `mw_sound_load(mode = Music)` が返した ID であること(SE の ID を
/// 渡すと `MwResult::ErrInvalidSoundId` を返す——`crate::handle::MUSIC_ID_FLAG` に
/// よる ID 空間分離が効いている)。
///
/// デコーダ(`mw_core::SymphoniaDecoder::open`)は**この呼び出しの中、ゲームスレッドで
/// 開く**。ヘッダ読み取りでアロケーションが発生するが、ここはゲームスレッド経路
/// なので初期構築仕様『§5.3』のリアルタイム安全性規約には抵触しない(デコード
/// スレッドへは構築済みのデコーダをそのまま渡すだけで、以後の実際のデコード作業
/// ―パケット読み・リサンプル―はすべてデコードスレッド側で行われる)。
///
/// **プリロールが済むまで状態は `Loading` のまま。** `mw_music_set` はデコーダを
/// 差し替えるだけで、プリロールの完了を待たずに `MwResult::Ok` を返す
/// (非ブロッキング、初期構築仕様『§5.4』)。C# 側は [`mw_music_state`] が
/// `Ready` を返すまでポーリングする、という契約になる。
///
/// ## 曲の切り替えで前曲の PCM が漏れる問題への対処
///
/// リングバッファに前曲の PCM が残ったまま新しい曲が始まると、曲頭に前の曲が
/// 数フレーム鳴ってしまう(`rtrb` は**消費側しか pop できない**ため、生産側
/// (デコードスレッド)は自分でリングバッファを掃除できない)。M2-3 で入れた
/// シーク調停(エポック ack 方式、`mw_core::stream` モジュール doc「シークの
/// 調停」参照)がそのまま使えるため、次の順序を**必ず**守って処理する:
///
/// 1. 先に**デコーダを差し替える**(`Instance::send_decoder`)
/// 2. そのあとで `Command::MusicPrepare`(状態機械を `Loading` から仕切り直す。
///    M2-7 で追加)→ `Command::MusicSeek { frames: 0 }`(リングバッファの
///    掃除と epoch 更新を駆動する本体)の順で送る。**`MusicStop` は挟まない**
///    (理由は下の実装内コメントを参照——`Prepare` 済みなら常に no-op であり、
///    かつ `Prepare` 無しで `Stop` → `Seek` と送ると無音のまま `Playing` に居座る)
///
/// **順序が逆だとデコードスレッドが古いデコーダのまま新しい epoch を ack して
/// しまい壊れる**(先にシークだけ送ってしまうと、デコードスレッドが古いデコーダを
/// シークして ack した「あと」に新しいデコーダへ差し替わり、シーク後に古い曲の
/// 続きが積まれてしまう)。`MusicSeek` は消費側の `request_seek` を駆動するので、
/// リングの掃除(一次防御)と epoch 更新が走り、ack が揃うまで古い PCM を
/// 読み捨てる二次防御も効く(`mw_core::stream` モジュール doc 参照)。
///
/// # Safety
/// この関数はポインタを取らない(引数はすべて値渡し)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_music_set(handle: u64, sound_id: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if !handle_registry::is_music_id(sound_id) {
            return MwResult::ErrInvalidSoundId;
        }

        let result = handle_registry::with_instance(handle, |instance| {
            let Some(bytes) = instance.get_music_bytes(sound_id) else {
                return MwResult::ErrInvalidSoundId;
            };
            let output_sample_rate = instance.backend_sample_rate();

            // `SymphoniaDecoder::open` は `Vec<u8>` を値で要求するため複製する
            // (`Instance::remove_music_bytes` のドキュメント参照——この複製により
            // デコードスレッドはストレージと独立したメモリを持つことになる)。
            let decoder =
                match mw_core::SymphoniaDecoder::open((*bytes).clone(), output_sample_rate) {
                    Ok(decoder) => decoder,
                    Err(err) => {
                        mw_backend::mw_log!("[mw-ffi] mw_music_set: decode open failed: {err}");
                        return match err {
                            mw_core::DecodeError::InvalidSampleRate(_) => {
                                MwResult::ErrUnsupportedSampleRate
                            }
                            mw_core::DecodeError::UnsupportedChannelCount(_) => {
                                MwResult::ErrUnsupportedFormat
                            }
                            mw_core::DecodeError::Symphonia(_)
                            | mw_core::DecodeError::NoAudioTrack
                            | mw_core::DecodeError::Resample(_)
                            | mw_core::DecodeError::ResetRequired => MwResult::ErrDecodeFailed,
                        };
                    }
                };

            // 1) 先にデコーダを差し替える(このドキュメントの「曲の切り替えで
            //    前曲の PCM が漏れる問題への対処」参照。順序は変えないこと)。
            if !instance.send_decoder(Box::new(decoder)) {
                // 実運用では起こらない(`Instance::send_decoder` のドキュメント
                // 参照)防御的分岐。
                return MwResult::ErrCommandQueueFull;
            }

            // 2) そのあとでコマンドを送る。2通で足りる:
            //    - `MusicPrepare`: 新しい曲として状態機械を仕切り直す(`MusicVoice::prepare`
            //      が状態・位置・ループ区間・ゲイン・保留中の遷移を無条件に初期化する)。
            //      **`MusicStop` ではこれの代わりにならない** —— `stop` は再生中だと
            //      フェードアウトを予約するだけで状態は `Playing` のまま残り、続く
            //      `MusicSeek` がその予約(`pending_settle`)を破棄してしまうため、
            //      ゲイン 0 のまま `Playing` に居座って新しい曲が永久に無音になる
            //      (`mixer.rs` の再現テスト参照)。
            //    - `MusicSeek { frames: 0 }`: リングバッファの掃除と位置 0 への巻き戻しを
            //      駆動する(消費側の `request_seek` を通す唯一の経路)。
            //
            //    `MusicPrepare` の後に `MusicStop` を挟む必要は無い。コマンドは同一
            //    コールバックの先頭で発行順に処理されるため、その時点の状態は必ず
            //    `Loading` であり `stop` は定義上の no-op になる(送っても何も起きない
            //    ぶん、コマンドキューの枠を1つ無駄に使うだけ)。
            let sent = instance.command_sender.send(mw_core::Command::MusicPrepare)
                && instance
                    .command_sender
                    .send(mw_core::Command::MusicSeek { frames: 0 });
            if sent {
                // M3(案A): 再オープン後に同じ曲を読み直すための復元キャッシュ
                // (`Instance::attempt_reopen`/`Instance::restore_music`)。
                instance.note_music_set(sound_id);
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// 楽曲ボイスの再生状態を取得する(初期構築仕様『§4.3』『§5.5』)。
///
/// `out_state` には [`crate::types::MwMusicState`] の判別子(`Loading=0` /
/// `Ready=1` / `Playing=2` / `Paused=3`)を書き込む。素の `i32` として渡す理由は
/// `crates/mw-ffi/CLAUDE.md`「enum を FFI 引数に直接使わない理由」を参照
/// (`MwMusicState` 自体は csbindgen が C# enum を生成するので、呼び出し側は
/// 受け取った `int` をそのままキャストして使える)。
///
/// `mw_music_set` の呼び出し後は `Ready` になるまでこれをポーリングすること
/// (`mw_music_set` のドキュメント参照)。
///
/// # Safety
/// `out_state` は書き込み可能な `i32` を指す有効なポインタであるか、null で
/// なければならない(null は書き込みを行わず `MwResult::ErrNullPointer`)。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mw_music_state(handle: u64, out_state: *mut i32) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if out_state.is_null() {
            return MwResult::ErrNullPointer;
        }
        let result = handle_registry::with_instance(handle, |instance| {
            let state = instance.music_clock_snapshot().state;
            MwMusicState::from_core(state) as i32
        });
        match result {
            Some(value) => {
                // SAFETY: 上で null チェック済み。
                unsafe {
                    *out_state = value;
                }
                MwResult::Ok
            }
            None => MwResult::ErrInvalidHandle,
        }
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// 楽曲を一時停止する(初期構築仕様『§4.3』の `mw_music_pause` に相当)。
///
/// `MusicVoice::pause` は既定ランプでフェードアウトしてから `Paused` へ収束する
/// (M13)。`Playing` 以外からの呼び出しは音声スレッド側で no-op として無視される
/// (クラッシュしない)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_music_pause(handle: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let result = handle_registry::with_instance(handle, |instance| {
            let sent = instance.command_sender.send(mw_core::Command::MusicPause);
            if sent {
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// 巻き戻し付きで再開する(初期構築仕様『§4.3』の `mw_music_resume_at` に相当。
/// テンプレート仕様「中断対応」の「数秒巻き戻し + カウントダウン再開」の受け皿)。
///
/// `frames` へ再位置決めしたうえで既定ランプでフェードインする。`Loading` 中は
/// 音声スレッド側で無視される(クラッシュしない)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_music_resume_at(handle: u64, frames: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let result = handle_registry::with_instance(handle, |instance| {
            let sent = instance
                .command_sender
                .send(mw_core::Command::MusicResumeAt { frames });
            if sent {
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// 楽曲ボイスをシークする(初期構築仕様『§4.3』の `mw_music_seek` に相当)。
///
/// ランプを経由しない不連続そのもの(`mw_core::Command::MusicSeek` のドキュメント
/// 参照)。音楽クロックの世代カウンタ(§4.4)が進む。
#[unsafe(no_mangle)]
pub extern "C" fn mw_music_seek(handle: u64, frames: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let result = handle_registry::with_instance(handle, |instance| {
            let sent = instance
                .command_sender
                .send(mw_core::Command::MusicSeek { frames });
            if sent {
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// 楽曲を停止する(初期構築仕様『§4.3』の `mw_music_stop` に相当)。
///
/// `Playing` 中は既定ランプでフェードアウトしてから位置を 0 に戻し `Ready` へ、
/// `Paused` 中はランプ無しでその場で `Ready` へ戻る(`mw_core::MusicVoice::stop`
/// 参照)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_music_stop(handle: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let result = handle_registry::with_instance(handle, |instance| {
            let sent = instance.command_sender.send(mw_core::Command::MusicStop);
            if sent {
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// 楽曲のループ区間を設定・解除する(初期構築仕様『§4.3』の `mw_music_set_loop` に
/// 相当。選曲プレビュー用、フェードイン/アウト付き)。
///
/// **`begin_frames == 0 && end_frames == 0` はループ解除を意味する**
/// (オーケストレータの決定、【仮】。変更する場合は
/// [`LOOP_CLEAR_SENTINEL`] の1箇所を直せばよい)。それ以外で `begin_frames >=
/// end_frames` は不正な区間として `MwResult::ErrInvalidLoopRegion` を返す
/// (`mw_core::MusicVoice::set_loop` は同じ状況を音声スレッド側で黙ってループ無しに
/// 丸めるが——リアルタイム安全性のためパニックできない設計——FFI 境界では
/// 黙って捨てず明示的に拒否する)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_music_set_loop(handle: u64, begin_frames: u64, end_frames: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let region = if (begin_frames, end_frames) == LOOP_CLEAR_SENTINEL {
            None
        } else if begin_frames >= end_frames {
            return MwResult::ErrInvalidLoopRegion;
        } else {
            Some((begin_frames, end_frames))
        };

        let result = handle_registry::with_instance(handle, |instance| {
            let sent = instance
                .command_sender
                .send(mw_core::Command::MusicSetLoop { region });
            if sent {
                // M3(案A): 再オープン後の復元用キャッシュ(`Instance::restore_music`)。
                instance.note_music_loop(region);
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// [`mw_music_set_loop`] のループ解除を表す `(begin_frames, end_frames)` の組
/// (【仮】、オーケストレータの決定)。ここ1箇所を書き換えれば規約全体が変わる。
const LOOP_CLEAR_SENTINEL: (u64, u64) = (0, 0);

/// 音楽クロックのスナップショットを取得する(初期構築仕様『§4.4 音楽クロック』
/// 『§5.5』)。**毎フレーム呼ばれる関数なので GC アロケーションゼロ**
/// (初期構築仕様『§5.4』)——呼び出し側が確保した `out` へ blittable な
/// [`MwMusicPosition`] を直接書き込む(`mw_poll_events` と同じ設計方針)。
///
/// フィールドは `mw_core::MusicClockSnapshot` に対応する
/// (`song_frames`/`host_time_ns`/`sample_rate`/`state`/`is_playing`/`generation`)。
/// `bool` 相当のフィールド(`is_playing`)を `u8` にした理由は
/// [`MwMusicPosition`] のドキュメントを参照。
///
/// # Safety
/// `out` は書き込み可能な [`MwMusicPosition`] を指す有効なポインタであるか、
/// null でなければならない(null は書き込みを行わず `MwResult::ErrNullPointer`)。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mw_music_get_position(handle: u64, out: *mut MwMusicPosition) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if out.is_null() {
            return MwResult::ErrNullPointer;
        }
        let result = handle_registry::with_instance(handle, |instance| {
            let snapshot = instance.music_clock_snapshot();
            MwMusicPosition {
                song_frames: snapshot.song_frames,
                host_time_ns: snapshot.host_time_ns,
                sample_rate: snapshot.sample_rate,
                state: MwMusicState::from_core(snapshot.state),
                is_playing: snapshot.is_playing as u8,
                generation: snapshot.generation,
            }
        });
        match result {
            Some(position) => {
                // SAFETY: 上で null チェック済み。
                unsafe {
                    *out = position;
                }
                MwResult::Ok
            }
            None => MwResult::ErrInvalidHandle,
        }
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

// --- M4-3: BGM のネイティブ化 --------------------------------------------------
//
// 初期構築仕様『§2』M14: 「楽曲ボイスは同時に1本のみ」の原則を守ったまま、BGM 用に
// **2本目の楽曲ボイス**を追加する。楽曲ボイス(上の M2-7 ブロック)との分担:
//
// - **共有するもの**: 圧縮バイト列のストレージ・ID 空間(`mw_sound_load(mode =
//   Music)` / `mw_sound_release` はそのまま流用できる——「デコード前の圧縮バイト列を
//   保持する」という表現そのものへの分離であり、どちらのボイスで鳴らすかとは無関係)、
//   ストリーミングデコードの仕組み一式(`SymphoniaDecoder` / `decode_thread::spawn`)、
//   状態機械(`MusicVoice` をそのまま転用。ループ・フェードの実装も共有)。
// - **分けるもの**: リングバッファとデコードスレッドは BGM 専用にもう1本立てる
//   (`crate::handle::Instance::bgm_decoder_tx` 等)。**クロックは発行しない**
//   (`mw_music_get_position` 相当は無い。発行元は楽曲ボイスに固定。M14)。
//   サンプル精度の予約再生・巻き戻し付き再開も BGM には無い(不要なため、
//   `mw_core::Command` に対応するコマンドを意図的に持たせていない)。
//
// バス音量は**楽曲と同じ Bgm バス**を共有する(`mw_bus_set_volume`/`mw_bus_fade` を
// そのまま使う——新しい BGM 専用バスは追加しない。両者が同時に鳴ることは運用上
// 想定していないが、鳴った場合も単純に加算されるだけで壊れない設計になっている。
// `mw_core::mixer::Mixer::render` のコメント参照)。

/// BGM のストリーミング再生を準備する(`mw_music_set` の BGM 版)。
///
/// `sound_id` は `mw_sound_load(mode = Music)` が返した ID であること(SE の ID を
/// 渡すと `MwResult::ErrInvalidSoundId`)。楽曲(`mw_music_set`)と同じ理由・同じ順序
/// (デコーダ差し替え → `BgmPrepare` → `BgmSeek{0}`)で、前トラックの PCM が
/// リングバッファに残って曲頭へ漏れる問題に対処する(`mw_music_set` のドキュメント
/// 「曲の切り替えで前曲の PCM が漏れる問題への対処」を参照。BGM 版でも同じ罠がある)。
///
/// **非ブロッキング。** プリロール完了を待たずに戻る。状態は [`mw_bgm_state`] が
/// `Ready` を返すまでポーリングすること。
#[unsafe(no_mangle)]
pub extern "C" fn mw_bgm_set(handle: u64, sound_id: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if !handle_registry::is_music_id(sound_id) {
            return MwResult::ErrInvalidSoundId;
        }

        let result = handle_registry::with_instance(handle, |instance| {
            let Some(bytes) = instance.get_music_bytes(sound_id) else {
                return MwResult::ErrInvalidSoundId;
            };
            let output_sample_rate = instance.backend_sample_rate();

            let decoder =
                match mw_core::SymphoniaDecoder::open((*bytes).clone(), output_sample_rate) {
                    Ok(decoder) => decoder,
                    Err(err) => {
                        mw_backend::mw_log!("[mw-ffi] mw_bgm_set: decode open failed: {err}");
                        return match err {
                            mw_core::DecodeError::InvalidSampleRate(_) => {
                                MwResult::ErrUnsupportedSampleRate
                            }
                            mw_core::DecodeError::UnsupportedChannelCount(_) => {
                                MwResult::ErrUnsupportedFormat
                            }
                            mw_core::DecodeError::Symphonia(_)
                            | mw_core::DecodeError::NoAudioTrack
                            | mw_core::DecodeError::Resample(_)
                            | mw_core::DecodeError::ResetRequired => MwResult::ErrDecodeFailed,
                        };
                    }
                };

            // `mw_music_set` と同じ順序厳守(デコーダ差し替え → Prepare → Seek{0})。
            // 理由はこの関数のドキュメント、および `mw_music_set` 実装内コメント参照。
            if !instance.send_bgm_decoder(Box::new(decoder)) {
                return MwResult::ErrCommandQueueFull;
            }

            let sent = instance.command_sender.send(mw_core::Command::BgmPrepare)
                && instance
                    .command_sender
                    .send(mw_core::Command::BgmSeek { frames: 0 });
            if sent {
                // M3(案A): 再オープン後の復元用キャッシュ(`Instance::restore_bgm`)。
                instance.note_bgm_set(sound_id);
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// BGM ボイスの再生状態を取得する(`mw_music_state` の BGM 版)。
///
/// `out_state` には [`crate::types::MwMusicState`] の判別子を書き込む(`mw_music_state`
/// と同じ理由で素の `i32`。`crates/mw-ffi/CLAUDE.md`「enum を FFI 引数に直接使わない
/// 理由」参照)。**クロックの一部ではない**——BGM は曲位置・世代カウンタを持たない
/// (M14)ため、`mw_music_get_position` に相当する BGM 版の関数は無い。
///
/// # Safety
/// `out_state` は書き込み可能な `i32` を指す有効なポインタであるか、null で
/// なければならない(null は書き込みを行わず `MwResult::ErrNullPointer`)。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mw_bgm_state(handle: u64, out_state: *mut i32) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if out_state.is_null() {
            return MwResult::ErrNullPointer;
        }
        let result = handle_registry::with_instance(handle, |instance| {
            MwMusicState::from_core(instance.bgm_state()) as i32
        });
        match result {
            Some(value) => {
                // SAFETY: 上で null チェック済み。
                unsafe {
                    *out_state = value;
                }
                MwResult::Ok
            }
            None => MwResult::ErrInvalidHandle,
        }
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// BGM を再生する(`Ready` からのみ有効、既定ランプでフェードインする。初期構築仕様
/// 『§2』M14「フェード」)。`Ready` 以外からの呼び出しは音声スレッド側で無視される
/// (クラッシュしない。`mw_core::MusicVoice::play` 参照)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_bgm_play(handle: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let result = handle_registry::with_instance(handle, |instance| {
            let sent = instance.command_sender.send(mw_core::Command::BgmPlay);
            if sent {
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// BGM を停止する(`mw_music_stop` の BGM 版)。`Playing` 中は既定ランプでフェード
/// アウトしてから位置を 0 に戻し `Ready` へ、`Paused` 中は即座に `Ready` へ
/// (`mw_core::MusicVoice::stop` 参照)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_bgm_stop(handle: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let result = handle_registry::with_instance(handle, |instance| {
            let sent = instance.command_sender.send(mw_core::Command::BgmStop);
            if sent {
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// BGM のループ区間を設定・解除する(`mw_music_set_loop` の BGM 版。初期構築仕様
/// 『§2』M14「ループ再生」)。「BGM トラック全体をループさせたい」場合は、呼び出し側が
/// 既に把握している総フレーム数を使って `(0, total_frames)` を渡すこと——
/// `mw_music_set_loop` と同様、ネイティブ側は総フレーム数を問い合わせる API を
/// 持たない(呼び出し側が素材メタデータ等から把握している前提。同じ設計判断)。
///
/// `begin_frames == 0 && end_frames == 0` はループ解除([`LOOP_CLEAR_SENTINEL`] と
/// 同じ規約)。それ以外で `begin_frames >= end_frames` は `MwResult::ErrInvalidLoopRegion`。
#[unsafe(no_mangle)]
pub extern "C" fn mw_bgm_set_loop(handle: u64, begin_frames: u64, end_frames: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let region = if (begin_frames, end_frames) == LOOP_CLEAR_SENTINEL {
            None
        } else if begin_frames >= end_frames {
            return MwResult::ErrInvalidLoopRegion;
        } else {
            Some((begin_frames, end_frames))
        };

        let result = handle_registry::with_instance(handle, |instance| {
            let sent = instance
                .command_sender
                .send(mw_core::Command::BgmSetLoop { region });
            if sent {
                // M3(案A): 再オープン後の復元用キャッシュ(`Instance::restore_bgm`)。
                instance.note_bgm_loop(region);
                MwResult::Ok
            } else {
                MwResult::ErrCommandQueueFull
            }
        });
        result.unwrap_or(MwResult::ErrInvalidHandle)
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// 出力レイテンシ(ns)を取得する(初期構築仕様『§5.5』`mw_get_output_latency_ns`)。
///
/// `Backend::output_latency_ns` の実測値をそのまま返す。**0 は「まだ不明」を
/// 意味する**(オーディオコールバックが1度も走っていない。実測上 0ns ちょうどに
/// なることはまず無いため、この特殊値を「たまたま 0ns だった」と取り違える実害は
/// 無い——`mw_backend::Backend::output_latency_ns` のドキュメント参照)。
///
/// あわせて、`CpalBackend::log_output_latency_once`(1インスタンスにつき1回だけ
/// ログへ残す)をこのゲームスレッド経路から呼ぶ(**コールバック内から呼んでは
/// いけない**——`mw_log!` はアロケーションとロックを伴うため、初期構築仕様
/// 『§5.3』のリアルタイム安全性規約に抵触する。`Instance::log_buffer_info_once`
/// と同じ配線パターン)。
///
/// # Safety
/// `out_ns` は書き込み可能な `u64` を指す有効なポインタであるか、null で
/// なければならない(null は書き込みを行わず `MwResult::ErrNullPointer`)。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mw_get_output_latency_ns(handle: u64, out_ns: *mut u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if out_ns.is_null() {
            return MwResult::ErrNullPointer;
        }
        let result = handle_registry::with_instance(handle, |instance| {
            instance.log_output_latency_once();
            instance.output_latency_ns()
        });
        match result {
            Some(ns) => {
                // SAFETY: 上で null チェック済み。
                unsafe {
                    *out_ns = ns;
                }
                MwResult::Ok
            }
            None => MwResult::ErrInvalidHandle,
        }
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

/// 出力コールバックのアンダーラン(の疑い)統計を取得する(初期構築仕様『§2』M3
/// 「アンダーラン検知・テレメトリ」)。
///
/// **`mw_poll_events` で読める `MwEventKind::Underrun` とは別物。** あちらは
/// 楽曲/BGM のデコードリングバッファがデータ供給に追いつかず無音で埋めたことの
/// 検知(M2)。こちらは音声コールバック自体の間隔が想定より開いたこと——OS 側の
/// 出力バッファが実際に枯渇した(音が途切れた)ことの直接的な兆候、またはその
/// 一歩手前の状態を示す検知(M3)。両者の関係・検知方法の詳細は
/// `mw_backend::underrun` モジュール doc / [`MwOutputUnderrunStats`] のドキュメント
/// を参照。
///
/// `out` には累計検知回数(`count`)・直近の検知時刻(`last_host_time_ns`、
/// `mw_host_time_ns()` と同じ時計。未検知なら 0)・直近まで連続して検知した回数
/// (`consecutive_count`)を書き込む。GC アロケーションゼロ。
///
/// あわせて、新たに検知した分があれば[`CpalBackend::log_new_output_underruns`]
/// (`mw_backend::CpalBackend`)をこのゲームスレッド経路から呼ぶ(**コールバック内
/// から呼んではいけない**——`mw_log!` はアロケーションとロックを伴うため、初期構築
/// 仕様『§5.3』のリアルタイム安全性規約に抵触する。`mw_get_output_latency_ns` と
/// 同じ配線パターン)。
///
/// # Safety
/// `out` は書き込み可能な [`MwOutputUnderrunStats`] を指す有効なポインタである
/// か、null でなければならない(null は書き込みを行わず `MwResult::ErrNullPointer`)。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mw_get_output_underrun_stats(
    handle: u64,
    out: *mut MwOutputUnderrunStats,
) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if out.is_null() {
            return MwResult::ErrNullPointer;
        }
        let result = handle_registry::with_instance(handle, |instance| {
            instance.log_new_output_underruns();
            instance.output_underrun_stats()
        });
        match result {
            Some((count, last_host_time_ns, consecutive_count)) => {
                // SAFETY: 上で null チェック済み。
                unsafe {
                    *out = MwOutputUnderrunStats {
                        count,
                        last_host_time_ns,
                        consecutive_count,
                    };
                }
                MwResult::Ok
            }
            None => MwResult::ErrInvalidHandle,
        }
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
}

// --- M2-6: イベント通知 -----------------------------------------------------
//
// 初期構築仕様 §4.6(確定)/ §5.4(GC アロケーションゼロ)/ §2 M4(C → C# の
// コールバックはしない。C# 側が毎フレームポーリングする)。

/// イベントをポーリングする(初期構築仕様『§4.6 イベント通知』, M2-6)。
///
/// C → C# のコールバックはしない(『§2』M4, 確定)。呼び出し側が確保した `buf`
/// (要素数 `cap`)へ blittable な [`MwEvent`] を直接書き込む。GC アロケーションゼロ
/// (初期構築仕様『§5.4』)。
///
/// **戻り値は他の FFI 関数と異なる**: 成功時は `MwResult::Ok`(常に0)固定ではなく、
/// **実際に書き込んだ件数**(0以上)を返す——毎フレーム呼ぶ関数として、呼び出し側が
/// 知りたいのはまさにこの件数であり、`0` を「成功」に固定してしまうと別の出力引数で
/// 件数を返す必要が生じ、アロケーションゼロという目的に対してかえって遠回りになる
/// (`buf`/`cap` 自体は元々呼び出し側所有のバッファなので、この設計でも新たな
/// アロケーションは発生しない)。失敗時は他の関数と同じ `MwResult` の負の値
/// (`as i32`)を返す——「エラーは負の整数」という初期構築仕様『§4.8』の規約自体は
/// 破っていない(`MwResult::Ok` 以外の成功値が無いだけ)。
///
/// `out_dropped` には、キューが固定容量(【仮】64)を超えて溢れたために
/// **今回のポーリングで新たに判明した**破棄イベント件数を書き込む(黙って捨てない。
/// 『§4.6』)。前回までに報告済みの分は含まない——累積は呼び出し側の責務にしない。
///
/// ## M3(案A): Android(AAudio)切断からの内部再オープン
///
/// 初期構築仕様『§6』(確定)「ストリーム自体の切断(AAudio の disconnect 等)は
/// ミドルウェア内部で再オープンし、イベントで通知する」の入口はここ。ドレイン中に
/// `MwEventKind::StreamError { reason: DeviceUnavailable }` を観測すると
/// [`crate::handle::Instance::note_stream_error`] が内部再オープンの候補として記録し、
/// この関数の最後で [`handle_registry::maybe_reopen`] を呼んで(バックオフの都合が
/// 良ければ)実際に試みる——**この関数自体が「ゲームスレッドから毎フレーム呼ばれる」
/// 関数であることを利用しており、専用のポンプ・監視スレッドは新設していない**
/// (依頼書「⚠️ 再オープンはゲームスレッド側でやること」)。
///
/// 🔴 **iOS/tvOS では次の呼び出しが丸ごとコンパイルから除かれる**(`cfg`)。
/// iOS には実機で確認済みの既存の復帰経路(`mw-backend::ios_interruption`)が別途
/// あり、同じ `Event::StreamError { reason: DeviceUnavailable }` に対して二重に
/// 反応させないため——「どちらの経路を通るか」を実行時の条件分岐ではなく
/// コンパイル時の `cfg` 1箇所だけで確定させている
/// (`handle_registry::maybe_reopen` のドキュメント参照)。
///
/// この関数は「アロケーションゼロで毎フレーム呼ぶ」という上の設計方針の対象では
/// あるが、内部再オープンの試行自体は稀にしか起きず(切断イベントを観測した直後、
/// かつバックオフの間隔条件を満たしたときだけ)、その1回に限っては
/// `CpalBackend::open` 相当の重い処理(デバイス列挙・ストリーム構築)をこの呼び出しの
/// 中で同期的に行う。通常フレーム(切断が起きていない大多数のフレーム)は
/// `ReopenPolicy::is_due` の安価なチェックのみで即座に戻るため、定常状態の
/// 非ブロッキング性は保たれる。
///
/// # Safety
/// `cap > 0` の場合、`buf` は `cap` 個の [`MwEvent`] を書き込み可能な有効なポインタで
/// なければならない。`cap <= 0` の場合は `buf` が null でもよい(書き込みを行わない。
/// `mw_sound_load` の `len == 0` と同じ流儀)。`out_dropped` は書き込み可能な `u32` を
/// 指す有効なポインタであるか、null でなければならない(null は書き込みを行わず
/// `MwResult::ErrNullPointer` を返す)。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mw_poll_events(
    handle: u64,
    buf: *mut MwEvent,
    cap: i32,
    out_dropped: *mut u32,
) -> i32 {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        if out_dropped.is_null() {
            return Err(MwResult::ErrNullPointer);
        }
        // 負の容量は 0 として扱う(黙って落とさず、かつ新しいエラーコードも増やさない)。
        let cap = cap.max(0) as usize;
        if buf.is_null() && cap != 0 {
            return Err(MwResult::ErrNullPointer);
        }

        let result = handle_registry::with_instance(handle, |instance| {
            let mut written = 0usize;
            let (_, dropped) = instance.events.drain(cap, |event| {
                // M3(案A): DeviceUnavailable を観測したら内部再オープンの候補として
                // 記録する(`reason` 以外は素通り。詳細はメソッド doc 参照)。
                if let mw_core::Event::StreamError { reason } = event {
                    instance.note_stream_error(reason);
                }
                // SAFETY: `written < cap` は `drain` の `max` 引数(=`cap`)により
                // 保証される。`buf` は上で null/cap の整合を確認済みで、`cap` 個
                // 書き込み可能なポインタであることは呼び出し側の契約(Safety 節)。
                unsafe {
                    *buf.add(written) = MwEvent::from_core(event);
                }
                written += 1;
            });
            // SAFETY: 上で null チェック済み。
            unsafe {
                *out_dropped = dropped;
            }
            written as i32
        });
        result.ok_or(MwResult::ErrInvalidHandle)
    }));

    // M3(案A): このスコープの外(=上の `with_instance` が既にレジストリロックを
    // 解放した後)で呼ぶ——`with_instance` の中から呼ぶと同じ `Mutex` を二重に
    // ロックしにいってデッドロックする(`handle_registry::maybe_reopen` は
    // `init`/`shutdown` と同じレジストリロックを独立に取得する設計のため)。
    // iOS/tvOS ではこの呼び出しが丸ごとコンパイルされない(関数doc参照)。
    // パニックしても `mw_poll_events` 全体の結果(`outcome`)を巻き込まないよう、
    // ここだけ独立して `catch_unwind` で保護する。
    #[cfg(not(any(target_os = "ios", target_os = "tvos")))]
    {
        let _ = panic::catch_unwind(AssertUnwindSafe(|| handle_registry::maybe_reopen(handle)));
    }

    match outcome {
        Ok(Ok(written)) => written,
        Ok(Err(err)) => err as i32,
        Err(_) => MwResult::ErrPanic as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MwEventKind;

    /// 最小限の 48kHz PCM16 wav バイト列を組み立てる(テスト専用。バイナリはコミットしない
    /// —初期構築仕様 §8「ゴールデン波形はテスト時にコードで生成」)。
    ///
    /// `mw_core::wav` の `#[cfg(test)]` ゴールデンヘルパはこのクレートの通常ビルドからは
    /// 見えない(mw-core を通常の依存として使う限り `cfg(test)` は伝播しない)ため、
    /// ここに同等の最小実装を持つ。
    fn make_pcm16_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let bytes_per_sample = 2u32;
        let block_align = bytes_per_sample as u16 * channels;
        let byte_rate = sample_rate * block_align as u32;
        let data_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let data_size = data_bytes.len() as u32;
        let fmt_size: u32 = 16;
        let riff_size = 4 + (8 + fmt_size) + (8 + data_size);

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&fmt_size.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_size.to_le_bytes());
        out.extend_from_slice(&data_bytes);
        out
    }

    #[test]
    fn abi_version_is_one() {
        assert_eq!(mw_abi_version(), 1);
    }

    #[test]
    fn init_rejects_null_out_pointer() {
        let result = unsafe { mw_init(std::ptr::null_mut()) };
        assert_eq!(result, MwResult::ErrNullPointer);
    }

    #[test]
    fn shutdown_with_bogus_handle_is_invalid_handle_not_a_crash() {
        let result = mw_shutdown(0xDEAD_BEEF_u64);
        assert_eq!(result, MwResult::ErrInvalidHandle);
    }

    /// M0 の終了条件 + M1(SE 再生)の一連の流れを1つの統合テストとしてまとめてある。
    /// グローバルレジストリ(`handle::registry`)はプロセス全体で共有される単一インスタンス
    /// なので、init/shutdown をまたぐテストは他のテスト関数と並行実行すると競合しうる。
    /// そのため init するテストはこの1本に集約する。
    ///
    /// CI やヘッドレス環境では出力デバイスが無いことがあるため、その場合は
    /// クラッシュせずエラーコードを返せていることまでを確認する(§4.8)。
    #[test]
    fn init_se_lifecycle_then_shutdown_or_gracefully_reports_no_device() {
        let mut handle_out: u64 = 0;
        let init_result = unsafe { mw_init(&mut handle_out as *mut u64) };
        match init_result {
            MwResult::Ok => {
                assert_ne!(handle_out, 0);
                run_se_lifecycle(handle_out);
                assert_eq!(mw_shutdown(handle_out), MwResult::Ok);
            }
            MwResult::ErrBackendOpenFailed => {
                // デバイス無し環境(CI 等)。落ちずにエラーコードを返せていることが重要。
            }
            other => panic!("unexpected mw_init result: {other:?}"),
        }
    }

    /// 初期構築仕様 §8「Unity 統合テスト」の Rust 版に相当する一連の流れ:
    /// wav ロード → SE 発音 → ボイス操作 → バス操作 → サウンド解放。
    fn run_se_lifecycle(handle: u64) {
        let wav_bytes = make_pcm16_wav(48_000, 2, &[1000, -1000, 2000, -2000]);

        let mut sound_id: u64 = 0;
        let load_result = unsafe {
            mw_sound_load(
                handle,
                wav_bytes.as_ptr(),
                wav_bytes.len(),
                0, // MwSoundMode::Se
                &mut sound_id as *mut u64,
            )
        };
        assert_eq!(load_result, MwResult::Ok);
        assert_ne!(sound_id, 0);

        let mut voice_id: u64 = 0;
        let play_result = unsafe {
            mw_se_play(
                handle,
                sound_id,
                2, // MwBus::Se
                0.8,
                &mut voice_id as *mut u64,
            )
        };
        assert_eq!(play_result, MwResult::Ok);
        assert_ne!(voice_id, 0);

        // M2-5: 予約発音(サンプル精度の詳細な数値検証は mw-core 側のオフライン
        // レンダリングテストで行う。ここでは実ハンドル越しにコマンドが素通りすること
        // だけを確認する)。
        let mut scheduled_voice_id = 0u64;
        let schedule_result = unsafe {
            mw_se_schedule(
                handle,
                sound_id,
                2, // MwBus::Se
                1.0,
                mw_host_time_ns(), // 「今」= 発見可能な最速のサンプルで即座に発音される
                &mut scheduled_voice_id as *mut u64,
            )
        };
        assert_eq!(schedule_result, MwResult::Ok);
        assert_ne!(scheduled_voice_id, 0);

        assert_eq!(
            mw_music_play_scheduled(handle, mw_host_time_ns()),
            MwResult::Ok,
            "the command must be accepted even though no music has been loaded yet"
        );

        assert_eq!(mw_voice_set_volume(handle, voice_id, 0.5), MwResult::Ok);
        assert_eq!(mw_voice_stop(handle, voice_id), MwResult::Ok);
        assert_eq!(mw_bus_set_volume(handle, 2, 0.9), MwResult::Ok);
        assert_eq!(mw_bus_fade(handle, 0, 1.0, 20.0), MwResult::Ok);
        assert_eq!(mw_sound_release(handle, sound_id), MwResult::Ok);

        // 解放済み ID の再利用は無効 ID として検出される(クラッシュしない)。
        let mut out_voice = 0u64;
        let replay = unsafe { mw_se_play(handle, sound_id, 2, 1.0, &mut out_voice as *mut u64) };
        assert_eq!(replay, MwResult::ErrInvalidSoundId);

        // 出力デバイスのレートと一致しない wav も、ロード時の一括リサンプル
        // (`mw_core::wav::decode`, 初期構築仕様『§4.7』)でエラーにならず読み込める。
        // 実機のデバイスレートは環境依存(cpal がネゴシエートする)なので、ここでは
        // 「44.1kHz と 48kHz のどちらであっても両方ロードできる」ことを見る
        // (device rate と偶然一致した側はバイパス経路、もう一方はリサンプル経路を通る)。
        let wav_44_1k = make_pcm16_wav(44_100, 2, &[500, -500, 1000, -1000]);
        let mut resampled_id: u64 = 0;
        let resampled_load = unsafe {
            mw_sound_load(
                handle,
                wav_44_1k.as_ptr(),
                wav_44_1k.len(),
                0,
                &mut resampled_id as *mut u64,
            )
        };
        assert_eq!(resampled_load, MwResult::Ok);
        assert_ne!(resampled_id, 0);
        assert_eq!(mw_sound_release(handle, resampled_id), MwResult::Ok);

        // サンプルレートが 0(壊れたファイル)は明確なエラーコードで拒否する。
        let wav_zero_rate = make_pcm16_wav(0, 2, &[0, 0]);
        let mut invalid_id: u64 = 0;
        let invalid_load = unsafe {
            mw_sound_load(
                handle,
                wav_zero_rate.as_ptr(),
                wav_zero_rate.len(),
                0,
                &mut invalid_id as *mut u64,
            )
        };
        assert_eq!(invalid_load, MwResult::ErrUnsupportedSampleRate);

        run_event_poll_checks(handle);
        run_music_lifecycle(handle);
        run_music_preview_style_rapid_switch(handle);
        run_bgm_lifecycle(handle);
        run_reopen_lifecycle(handle);
    }

    /// M2-7: 楽曲再生の一連の流れ(実ハンドル越し)。`run_se_lifecycle` と同じ理由
    /// (グローバルレジストリはプロセス全体で共有される単一インスタンス)でここに
    /// 集約する——デコードスレッドは `mw_init` の中で1本しか立たないため、
    /// 別の `#[test]` 関数として独立に `mw_init` するとレジストリ・デコード
    /// スレッドの両方で競合しうる。
    fn run_music_lifecycle(handle: u64) {
        // Music モードは圧縮のまま保持するだけでロード時に妥当性検証をしない
        // (`load_music` 参照)ため、実際に有効な wav バイト列を渡しておく
        // (`mw_music_set` が `SymphoniaDecoder::open` でデコードを試みるため)。
        // 短い素材でも、総フレーム数に達し次第 EOF 経由で `is_ready()` が true に
        // なる(`mw_core::stream` の `is_ready` ドキュメント参照)ので十分。
        let music_bytes = make_pcm16_wav(48_000, 2, &vec![12345i16; 400]);

        let mut music_id: u64 = 0;
        let load_result = unsafe {
            mw_sound_load(
                handle,
                music_bytes.as_ptr(),
                music_bytes.len(),
                1, // MwSoundMode::Music
                &mut music_id as *mut u64,
            )
        };
        assert_eq!(load_result, MwResult::Ok);
        assert_ne!(music_id, 0);
        assert!(
            handle_registry::is_music_id(music_id),
            "an id issued for Music mode must have MUSIC_ID_FLAG set"
        );

        // SE も並行してロードしておき、ID 空間の分離を実ハンドル越しに確認する
        // 土台にする(このあと楽曲 ID を release しても SE 側が無事であることを見る)。
        let se_bytes = make_pcm16_wav(48_000, 2, &[1000, -1000]);
        let mut se_id: u64 = 0;
        let se_load_result = unsafe {
            mw_sound_load(
                handle,
                se_bytes.as_ptr(),
                se_bytes.len(),
                0, // MwSoundMode::Se
                &mut se_id as *mut u64,
            )
        };
        assert_eq!(se_load_result, MwResult::Ok);
        assert!(!handle_registry::is_music_id(se_id));

        // 楽曲 ID を SE 専用の API へ渡すとエラーになる(ストレージが分かれている
        // ため、SoundStorage 側には存在しない ID として自然に弾かれる)。
        let mut out_voice = 0u64;
        assert_eq!(
            unsafe { mw_se_play(handle, music_id, 1, 1.0, &mut out_voice as *mut u64) },
            MwResult::ErrInvalidSoundId,
            "a music id must not be usable as an SE id"
        );

        // mw_music_set: ストリーミング準備を開始する(非ブロッキング、
        // プリロール完了は待たない)。
        assert_eq!(mw_music_set(handle, music_id), MwResult::Ok);

        // Ready になるまでポーリングする(デコードスレッドが pump するのを待つ、
        // `mw_music_set` のドキュメント「状態は Loading のまま」契約の検証)。
        let mut state = -1i32;
        let became_ready = wait_until(
            || {
                let r = unsafe { mw_music_state(handle, &mut state as *mut i32) };
                r == MwResult::Ok && state == MwMusicState::Ready as i32
            },
            std::time::Duration::from_secs(5),
        );
        assert!(
            became_ready,
            "must become Ready once preroll completes; last observed state={state}"
        );

        // 音楽クロックのスナップショット(GC アロケーションゼロの blittable 構造体)。
        let mut position = MwMusicPosition {
            song_frames: 0,
            host_time_ns: 0,
            sample_rate: 0,
            state: MwMusicState::Loading,
            is_playing: 0,
            generation: 0,
        };
        assert_eq!(
            unsafe { mw_music_get_position(handle, &mut position as *mut MwMusicPosition) },
            MwResult::Ok
        );
        assert_eq!(position.state, MwMusicState::Ready);
        assert_eq!(position.is_playing, 0);

        // ループ区間: 不正な区間は拒否、有効な区間→解除で通る。
        assert_eq!(
            mw_music_set_loop(handle, 5, 3),
            MwResult::ErrInvalidLoopRegion
        );
        assert_eq!(mw_music_set_loop(handle, 0, 200), MwResult::Ok);
        assert_eq!(
            mw_music_set_loop(handle, 0, 0),
            MwResult::Ok,
            "(0, 0) must be accepted as \"clear the loop\""
        );

        // 楽曲制御 API 一式(コマンドキューへ積めることのみを見る。サンプル精度の
        // 数値的な検証は mw-core 側のオフラインレンダリングテストで実施済み)。
        assert_eq!(
            mw_music_play_scheduled(handle, mw_host_time_ns()),
            MwResult::Ok
        );
        assert_eq!(mw_music_pause(handle), MwResult::Ok);
        assert_eq!(mw_music_resume_at(handle, 0), MwResult::Ok);
        assert_eq!(mw_music_seek(handle, 10), MwResult::Ok);
        assert_eq!(mw_music_stop(handle), MwResult::Ok);

        // 出力レイテンシ(実デバイスの有無に関わらず、書き込み自体は成功するはず。
        // 0 は「まだ未計測」を意味するだけで失敗ではない)。
        let mut latency_ns = 0u64;
        assert_eq!(
            unsafe { mw_get_output_latency_ns(handle, &mut latency_ns as *mut u64) },
            MwResult::Ok
        );

        // 出力コールバックのアンダーラン(の疑い)統計。実デバイスの有無に関わらず
        // 書き込み自体は成功し、このテストでは実際のコールバックに人工的な間隔異常を
        // 混ぜていないので count は 0 のはず(アンダーラン検知そのものの単体テストは
        // `mw_backend::underrun` — 実デバイス無しで「モックの」コールバック系列を
        // 直接駆動して確認済み)。
        let mut underrun_stats = MwOutputUnderrunStats {
            count: 1,
            last_host_time_ns: 1,
            consecutive_count: 1,
        };
        assert_eq!(
            unsafe {
                mw_get_output_underrun_stats(
                    handle,
                    &mut underrun_stats as *mut MwOutputUnderrunStats,
                )
            },
            MwResult::Ok
        );
        assert_eq!(underrun_stats.count, 0);
        assert_eq!(underrun_stats.last_host_time_ns, 0);
        assert_eq!(underrun_stats.consecutive_count, 0);

        // 楽曲バイト列を release しても SE 側は無事(ID 空間分離が効いていることの
        // 実ハンドル越しの確認、依頼書のテスト要件)。
        assert_eq!(mw_sound_release(handle, music_id), MwResult::Ok);
        assert_eq!(
            mw_sound_release(handle, music_id),
            MwResult::ErrInvalidSoundId,
            "double release of a music id must be rejected, not crash"
        );

        let mut se_voice = 0u64;
        assert_eq!(
            unsafe { mw_se_play(handle, se_id, 2, 1.0, &mut se_voice as *mut u64) },
            MwResult::Ok,
            "releasing a music id must not affect unrelated SE storage"
        );
        assert_eq!(mw_sound_release(handle, se_id), MwResult::Ok);
    }

    /// M4 終了条件(選曲プレビューのネイティブ化)の回帰テスト。実ハンドル + 実
    /// デコードスレッド越しに、`mw-core::mixer::tests::preview_style_rapid_track_switch_
    /// never_leaks_previous_audio_and_settles_on_latest_track` と同じシナリオ
    /// (連打での曲切り替え、ロード中の切り替え)を踏む。
    ///
    /// **選曲プレビューは新しい FFI を必要としない**——`mw_music_set` /
    /// `mw_music_play_scheduled` / `mw_music_seek` / `mw_music_set_loop` /
    /// `mw_music_stop` / `mw_music_state` がそのまま「選曲プレビュー用」として
    /// 初期構築仕様『§4.3』に明記されている(`mw_music_set_loop` の doc 参照)。
    /// このテストは、その既存の口を「プレビューの実際の使われ方」の形で通して
    /// 固定化する回帰テストであって、新しい実装を検証するものではない
    /// (`decode_thread::spawn` の「最新のデコーダを採用する」設計・
    /// `mw_music_set` の Prepare→Seek リセットは M2-7 で既に実装済み)。
    fn run_music_preview_style_rapid_switch(handle: u64) {
        // 3曲ぶんロードしておく(選曲リストを連打で切り替える想定)。値をそれぞれ
        // 変えておき、「最後に選んだ曲以外の音が紛れ込んでいないか」を機械的な値の
        // 一致では確認できない(実デバイスが無い CI では実際の出力を録音できない)ため、
        // ここでは状態遷移(スタックしないこと)とコマンドが素通りすることだけを見る
        // ——PCM 自体の非混入は `mw-core` 側のオフラインレンダリングテストで
        // 数値的に確認済み(このファイル冒頭のコメント参照)。
        let track_a = make_pcm16_wav(48_000, 2, &vec![100i16; 4_000]);
        let track_b = make_pcm16_wav(48_000, 2, &vec![200i16; 4_000]);
        let track_c = make_pcm16_wav(48_000, 2, &vec![300i16; 4_000]);

        let mut id_a: u64 = 0;
        let mut id_b: u64 = 0;
        let mut id_c: u64 = 0;
        assert_eq!(
            unsafe {
                mw_sound_load(
                    handle,
                    track_a.as_ptr(),
                    track_a.len(),
                    1,
                    &mut id_a as *mut u64,
                )
            },
            MwResult::Ok
        );
        assert_eq!(
            unsafe {
                mw_sound_load(
                    handle,
                    track_b.as_ptr(),
                    track_b.len(),
                    1,
                    &mut id_b as *mut u64,
                )
            },
            MwResult::Ok
        );
        assert_eq!(
            unsafe {
                mw_sound_load(
                    handle,
                    track_c.as_ptr(),
                    track_c.len(),
                    1,
                    &mut id_c as *mut u64,
                )
            },
            MwResult::Ok
        );

        // ユーザーが曲 A を選び、プレビュー再生が始まる(Ready を待ってから
        // 「今すぐ」再生する——選曲プレビューはサンプル精度の予約が要らないため、
        // `mw_music_play_scheduled(handle, mw_host_time_ns())` で「今」を渡すだけでよい)。
        assert_eq!(mw_music_set(handle, id_a), MwResult::Ok);
        let mut state = -1i32;
        assert!(
            wait_until(
                || {
                    let r = unsafe { mw_music_state(handle, &mut state as *mut i32) };
                    r == MwResult::Ok && state == MwMusicState::Ready as i32
                },
                std::time::Duration::from_secs(5),
            ),
            "track A must become Ready; last observed state={state}"
        );
        assert_eq!(
            mw_music_play_scheduled(handle, mw_host_time_ns()),
            MwResult::Ok
        );

        // ここが本題: ユーザーが即座に B → C と連打で切り替える。B の Ready を
        // 一度も待たない(依頼書の「ロード中に切り替えたらどうなるか」そのもの)。
        assert_eq!(mw_music_set(handle, id_b), MwResult::Ok);
        assert_eq!(mw_music_set(handle, id_c), MwResult::Ok);

        // 連打してもスタックせず、最後に指定した曲(C)へ収束する。
        let mut settled_state = -1i32;
        let became_ready = wait_until(
            || {
                let r = unsafe { mw_music_state(handle, &mut settled_state as *mut i32) };
                r == MwResult::Ok && settled_state == MwMusicState::Ready as i32
            },
            std::time::Duration::from_secs(5),
        );
        assert!(
            became_ready,
            "must not get stuck in Loading after two rapid switches while the previous \
             track was still loading; last observed state={settled_state}"
        );

        // C も選曲プレビューの一連の操作(途中から再生・ループ・停止)を素直に
        // 受け付ける(`mw_music_seek`/`mw_music_set_loop` は「選曲プレビュー用」と
        // 初期構築仕様『§4.3』に明記されている口そのもの)。
        assert_eq!(mw_music_seek(handle, 100), MwResult::Ok);
        assert_eq!(mw_music_set_loop(handle, 0, 500), MwResult::Ok);
        assert_eq!(
            mw_music_play_scheduled(handle, mw_host_time_ns()),
            MwResult::Ok
        );
        assert_eq!(mw_music_stop(handle), MwResult::Ok);
        assert_eq!(mw_music_set_loop(handle, 0, 0), MwResult::Ok);

        assert_eq!(mw_sound_release(handle, id_a), MwResult::Ok);
        assert_eq!(mw_sound_release(handle, id_b), MwResult::Ok);
        assert_eq!(mw_sound_release(handle, id_c), MwResult::Ok);
    }

    /// M4-3: BGM の一連の流れ(実ハンドル越し)。`run_music_lifecycle` と対称の
    /// 構成にしてあるが、BGM には無いもの(サンプル精度の予約再生・巻き戻し付き
    /// 再開・音楽クロックのスナップショット)は当然テストしない
    /// (`ffi.rs` の「M4-3: BGM のネイティブ化」ブロック doc「共有するもの/分けるもの」参照)。
    fn run_bgm_lifecycle(handle: u64) {
        // 楽曲と同じロード経路(mode = Music)を共有していることをここでも確認する
        // (ID 空間・ストレージが共有である以上、当然ロードも同じ関数で済む)。
        let bgm_bytes = make_pcm16_wav(48_000, 2, &vec![9999i16; 400]);

        let mut bgm_id: u64 = 0;
        let load_result = unsafe {
            mw_sound_load(
                handle,
                bgm_bytes.as_ptr(),
                bgm_bytes.len(),
                1, // MwSoundMode::Music
                &mut bgm_id as *mut u64,
            )
        };
        assert_eq!(load_result, MwResult::Ok);
        assert_ne!(bgm_id, 0);
        assert!(
            handle_registry::is_music_id(bgm_id),
            "BGM tracks share the Music-mode id space with the song voice"
        );

        // SE の ID は BGM API に渡せない(音楽 ID 空間の分離は BGM でも同じ)。
        let se_bytes = make_pcm16_wav(48_000, 2, &[10, -10]);
        let mut se_id: u64 = 0;
        assert_eq!(
            unsafe {
                mw_sound_load(
                    handle,
                    se_bytes.as_ptr(),
                    se_bytes.len(),
                    0,
                    &mut se_id as *mut u64,
                )
            },
            MwResult::Ok
        );
        assert_eq!(
            mw_bgm_set(handle, se_id),
            MwResult::ErrInvalidSoundId,
            "an SE id must not be usable as a BGM id"
        );
        assert_eq!(mw_sound_release(handle, se_id), MwResult::Ok);

        // mw_bgm_set: 非ブロッキング。プリロール完了は待たない。
        assert_eq!(mw_bgm_set(handle, bgm_id), MwResult::Ok);

        let mut state = -1i32;
        let became_ready = wait_until(
            || {
                let r = unsafe { mw_bgm_state(handle, &mut state as *mut i32) };
                r == MwResult::Ok && state == MwMusicState::Ready as i32
            },
            std::time::Duration::from_secs(5),
        );
        assert!(
            became_ready,
            "BGM must become Ready once preroll completes; last observed state={state}"
        );

        // ループ区間: 不正な区間は拒否、有効な区間→解除で通る(楽曲 API と同じ規約)。
        assert_eq!(
            mw_bgm_set_loop(handle, 5, 3),
            MwResult::ErrInvalidLoopRegion
        );
        assert_eq!(
            mw_bgm_set_loop(handle, 7, 7),
            MwResult::ErrInvalidLoopRegion
        );
        assert_eq!(mw_bgm_set_loop(handle, 0, 200), MwResult::Ok);
        assert_eq!(
            mw_bgm_set_loop(handle, 0, 0),
            MwResult::Ok,
            "(0, 0) must be accepted as \"clear the loop\""
        );

        // 再生 → 停止(コマンドが素通りすることのみ確認。フェードの数値検証は
        // mw-core 側のオフラインレンダリングテストで実施済み)。
        assert_eq!(mw_bgm_play(handle), MwResult::Ok);
        assert_eq!(mw_bgm_stop(handle), MwResult::Ok);

        assert_eq!(mw_sound_release(handle, bgm_id), MwResult::Ok);
        assert_eq!(
            mw_sound_release(handle, bgm_id),
            MwResult::ErrInvalidSoundId,
            "double release of a BGM id must be rejected, not crash"
        );
    }

    /// M3「Android(AAudio)切断復旧」案A の統合テスト(実ハンドル + 実デコードスレッド +
    /// 実 `CpalBackend` 越し)。
    ///
    /// **実デバイスを本当に切断する手段がこの環境には無い**(`docs/history/
    /// 03-2026-08-31.md`「実機でしか確認できない範囲」参照)。このテストが実行されて
    /// いる時点で `mw_init` は既に成功している(呼び出し元
    /// `init_se_lifecycle_then_shutdown_or_gracefully_reports_no_device` が
    /// `MwResult::Ok` を確認済み)ので、実デバイスは存在する。ここでは
    /// `Event::StreamError { reason: DeviceUnavailable }` をイベントキューへ直接注入して
    /// 「切断が観測された」ことだけをシミュレートし、そこから先
    /// (`CpalBackend::close()` → `Renderer::build_with_events()` →
    /// `CpalBackend::open()` と、状態復元コマンドの再送)はすべて本物のコードパスを
    /// 踏む。**再オープンに失敗したときに無限ループしないことの保証は、ここではなく
    /// `crate::reopen::tests`(バックエンドを必要としない純粋な状態機械のテスト)が
    /// 担っている**——この環境には `backend.open()` を確実に失敗させる手段
    /// (モックバックエンド)が無いため。
    fn run_reopen_lifecycle(handle: u64) {
        // 0.1秒ぶん(48kHz)。シークの余地を持たせるため十分な長さにする。
        let music_bytes = make_pcm16_wav(48_000, 2, &vec![7_777i16; 4_800]);
        let mut music_id: u64 = 0;
        assert_eq!(
            unsafe {
                mw_sound_load(
                    handle,
                    music_bytes.as_ptr(),
                    music_bytes.len(),
                    1, // MwSoundMode::Music
                    &mut music_id as *mut u64,
                )
            },
            MwResult::Ok
        );

        let se_bytes = make_pcm16_wav(48_000, 2, &[42, -42]);
        let mut se_id: u64 = 0;
        assert_eq!(
            unsafe {
                mw_sound_load(
                    handle,
                    se_bytes.as_ptr(),
                    se_bytes.len(),
                    0, // MwSoundMode::Se
                    &mut se_id as *mut u64,
                )
            },
            MwResult::Ok
        );

        // 楽曲を準備し、Ready を待ってからループ・バス音量を設定して再生する
        // (依頼書「復元すべき状態」を一通り成立させてから切断をシミュレートする)。
        assert_eq!(mw_music_set(handle, music_id), MwResult::Ok);
        let mut state = -1i32;
        assert!(
            wait_until(
                || {
                    let r = unsafe { mw_music_state(handle, &mut state as *mut i32) };
                    r == MwResult::Ok && state == MwMusicState::Ready as i32
                },
                std::time::Duration::from_secs(5),
            ),
            "music must become Ready before we can test reopen restoration"
        );

        assert_eq!(mw_music_set_loop(handle, 100, 4_000), MwResult::Ok);
        assert_eq!(
            mw_bus_set_volume(handle, 1 /* MwBus::Bgm */, 0.42),
            MwResult::Ok
        );
        assert_eq!(
            mw_music_play_scheduled(handle, mw_host_time_ns()),
            MwResult::Ok
        );

        // 実際に再生位置が進み、`Playing` になるまで待つ(「復元された位置が0の
        // ままではない」ことの検証に意味を持たせるため)。
        let mut position_before = MwMusicPosition {
            song_frames: 0,
            host_time_ns: 0,
            sample_rate: 0,
            state: MwMusicState::Loading,
            is_playing: 0,
            generation: 0,
        };
        let advanced = wait_until(
            || {
                let r = unsafe {
                    mw_music_get_position(handle, &mut position_before as *mut MwMusicPosition)
                };
                r == MwResult::Ok
                    && position_before.is_playing == 1
                    && position_before.song_frames > 0
            },
            std::time::Duration::from_secs(5),
        );
        assert!(
            advanced,
            "music must actually start playing before reopen-restoration can be tested \
             meaningfully; last observed position={position_before:?}"
        );
        let generation_before = position_before.generation;
        let frames_before = position_before.song_frames;

        // SoundId が(切断前は当然)有効であることの前提確認。
        let mut se_voice = 0u64;
        assert_eq!(
            unsafe { mw_se_play(handle, se_id, 2, 1.0, &mut se_voice as *mut u64) },
            MwResult::Ok
        );

        // --- 切断イベントを注入する(実機の AAudio disconnect の代わり) ---
        let injected = handle_registry::with_instance(handle, |instance| {
            instance
                .events
                .push_side_channel(mw_core::Event::StreamError {
                    reason: mw_core::StreamErrorReason::DeviceUnavailable,
                });
        });
        assert!(
            injected.is_some(),
            "handle must be valid before injecting the event"
        );

        // `mw_poll_events` がこのイベントをドレインし、同じ呼び出しの中で内部再オープン
        // の候補として記録する(`mw_poll_events` のドキュメント「M3(案A)」参照)。
        let mut event_buf = [MwEvent {
            kind: MwEventKind::RouteChanged,
            payload: 0,
        }; 8];
        let mut dropped = 0u32;
        let written = unsafe {
            mw_poll_events(
                handle,
                event_buf.as_mut_ptr(),
                event_buf.len() as i32,
                &mut dropped as *mut u32,
            )
        };
        assert!(
            written >= 1,
            "the injected StreamError event must be observable via mw_poll_events"
        );
        assert!(
            event_buf[..written as usize]
                .iter()
                .any(|e| e.kind == MwEventKind::StreamError),
            "must contain the injected StreamError event, got {:?}",
            &event_buf[..written as usize]
        );

        // 再オープンが試行され、成功したことを診断アクセサで確認する
        // (`Instance::reopen_diagnostics`)。`mw_poll_events` を呼ぶたびに
        // `maybe_reopen` がバックオフ条件を満たせば試行するので、数回ポーリングする
        // 形でも「再オープンが起きること」の検証として十分。
        let reopened_successfully = wait_until(
            || {
                let mut buf = [MwEvent {
                    kind: MwEventKind::RouteChanged,
                    payload: 0,
                }; 8];
                let mut dropped = 0u32;
                let _ = unsafe {
                    mw_poll_events(
                        handle,
                        buf.as_mut_ptr(),
                        buf.len() as i32,
                        &mut dropped as *mut u32,
                    )
                };
                handle_registry::with_instance(handle, |instance| instance.reopen_diagnostics())
                    .map(|(pending, _attempts, exhausted)| !pending && !exhausted)
                    .unwrap_or(false)
            },
            std::time::Duration::from_secs(5),
        );
        assert!(
            reopened_successfully,
            "attempt_reopen must succeed when a real output device is still available \
             (this environment cannot force backend.open() to fail — see module doc)"
        );

        // --- 復元の検証 ---

        let mut position_after = MwMusicPosition {
            song_frames: 0,
            host_time_ns: 0,
            sample_rate: 0,
            state: MwMusicState::Loading,
            is_playing: 0,
            generation: 0,
        };

        // 1) 楽曲の再生位置が復元されている(いちばん重要。ゼロから始め直す実装だと
        //    ここで song_frames が伸びても 0 近辺からの再カウントになってしまう)。
        //    `song_frames` は `MusicSeek` の時点で(まだ `Loading` のうちから)
        //    直ちに反映されるため、`is_playing` も一緒に条件へ入れて
        //    「デコードが追いつき実際に鳴り始めた」ところまで待つ
        //    (`song_frames > 0` だけだと Seek 直後の一瞬で満たされてしまい、
        //    その後 `state` が `Playing` に遷移するのを待たずに検証してしまう)。
        let position_restored = wait_until(
            || {
                let r = unsafe {
                    mw_music_get_position(handle, &mut position_after as *mut MwMusicPosition)
                };
                r == MwResult::Ok
                    && position_after.song_frames > 0
                    && position_after.is_playing == 1
            },
            std::time::Duration::from_secs(5),
        );
        assert!(
            position_restored,
            "song position must be restored (not stuck at 0) after the internal reopen; \
             last observed={position_after:?}"
        );
        assert!(
            position_after.song_frames + 1_000 >= frames_before,
            "restored position ({}) must not have reset back near zero relative to the \
             pre-disconnect position ({frames_before}) — a small amount of slack is allowed \
             for re-decode/seek overhead, but a reset to zero would indicate the restore \
             command sequence did not actually run",
            position_after.song_frames
        );
        assert_eq!(
            position_after.state,
            MwMusicState::Playing,
            "must resume playing automatically since it was Playing before the disconnect"
        );

        // 2) generation が bump されている(不連続の通知。依頼書「🔴 generation を
        //    必ず bump すること」)。
        assert_ne!(
            position_after.generation, generation_before,
            "generation must change across an internal reopen so the client can detect \
             the discontinuity and stop interpolating across it"
        );

        // 3) ロード済みの SoundId が引き続き有効(`Instance::sounds`/`music_bytes` は
        //    再オープンが一切触れないストレージのため)。
        let mut se_voice_after = 0u64;
        assert_eq!(
            unsafe { mw_se_play(handle, se_id, 2, 1.0, &mut se_voice_after as *mut u64) },
            MwResult::Ok,
            "a SoundId loaded before the reopen must remain valid afterwards"
        );

        // 4) バス音量・ループ設定の復元キャッシュ(`Instance::note_bus_volume`/
        //    `note_music_loop`)がこの一連の流れを通じて正しく保持されている
        //    ——`Instance::attempt_reopen` はここから読んでコマンドを再送する。
        let cached_bus_volume = handle_registry::with_instance(handle, |instance| {
            instance.bus_volume_for_test(mw_core::BusId::Bgm)
        });
        assert_eq!(cached_bus_volume, Some(0.42));
        let cached_loop =
            handle_registry::with_instance(handle, |instance| instance.music_loop_for_test());
        assert_eq!(cached_loop, Some(Some((100, 4_000))));

        // 後始末。
        assert_eq!(mw_music_stop(handle), MwResult::Ok);
        assert_eq!(mw_sound_release(handle, music_id), MwResult::Ok);
        assert_eq!(mw_sound_release(handle, se_id), MwResult::Ok);
    }

    /// `f` が `true` を返すまで短い間隔でポーリングする(タイムアウト付き)。
    /// デコードスレッドのポーリング周期(`crate::decode_thread::POLL_INTERVAL`)に
    /// 対して十分粗い間隔で待つことで、フレーク耐性を上げつつビジーループにしない。
    fn wait_until(mut f: impl FnMut() -> bool, timeout: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        loop {
            if f() {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// M2-6: `mw_poll_events` の一連の流れ(実ハンドル越し)。
    ///
    /// 実際の発生源(音声コールバック・cpal のエラー通知経路)からの配線は
    /// `mw-core`/`mw-backend` 側のテストで検証済み。ここでは FFI 境界そのもの
    /// (`Instance::events` への直接注入 → `mw_poll_events` での取り出し)が
    /// 依頼書のテスト要件2〜4を満たすことを確認する。
    fn run_event_poll_checks(handle: u64) {
        // 前提: 空の状態から始める(0件、破棄数0)。
        let mut buf = [MwEvent {
            kind: MwEventKind::RouteChanged,
            payload: 0,
        }; 8];
        let mut dropped = 0u32;
        let n = unsafe { mw_poll_events(handle, buf.as_mut_ptr(), buf.len() as i32, &mut dropped) };
        assert_eq!(n, 0, "queue must start out empty");
        assert_eq!(dropped, 0);

        // 依頼書のテスト要件3: ポーリングでキューが空になる/2回目は0件。
        let injected = handle_registry::with_instance(handle, |instance| {
            instance.events.push_realtime(mw_core::Event::MusicEnded);
            instance
                .events
                .push_side_channel(mw_core::Event::StreamError {
                    reason: mw_core::StreamErrorReason::DeviceUnavailable,
                });
        });
        assert!(injected.is_some(), "handle must still be valid");

        let n = unsafe { mw_poll_events(handle, buf.as_mut_ptr(), buf.len() as i32, &mut dropped) };
        assert_eq!(n, 2);
        assert_eq!(dropped, 0);
        assert_eq!(
            buf[0],
            MwEvent {
                kind: MwEventKind::MusicEnded,
                payload: 0
            }
        );
        assert_eq!(
            buf[1],
            MwEvent {
                kind: MwEventKind::StreamError,
                payload: mw_core::StreamErrorReason::DeviceUnavailable as u64
            }
        );

        let n2 =
            unsafe { mw_poll_events(handle, buf.as_mut_ptr(), buf.len() as i32, &mut dropped) };
        assert_eq!(n2, 0, "the queue must be empty after being fully drained");
        assert_eq!(dropped, 0);

        // 依頼書のテスト要件4: 呼び出し側バッファが積まれた件数より小さいとき、
        // 残りが次回のポーリングで取れる(取りこぼさない)。
        handle_registry::with_instance(handle, |instance| {
            for i in 0..5u32 {
                instance
                    .events
                    .push_realtime(mw_core::Event::Underrun { frames: i });
            }
        });
        let mut small_buf = [MwEvent {
            kind: MwEventKind::RouteChanged,
            payload: 0,
        }; 2];
        let n_first = unsafe {
            mw_poll_events(
                handle,
                small_buf.as_mut_ptr(),
                small_buf.len() as i32,
                &mut dropped,
            )
        };
        assert_eq!(n_first, 2);
        assert_eq!(dropped, 0);
        let n_rest =
            unsafe { mw_poll_events(handle, buf.as_mut_ptr(), buf.len() as i32, &mut dropped) };
        assert_eq!(
            n_rest, 3,
            "the remaining 3 entries must survive to be picked up by the next poll"
        );
        assert_eq!(dropped, 0);

        // 異常系(null / cap 0 / 不正ハンドル)で落ちないこと。
        assert_eq!(
            unsafe { mw_poll_events(handle, std::ptr::null_mut(), 0, &mut dropped) },
            0,
            "cap=0 with a null buf must be accepted (no bytes to write)"
        );
        assert_eq!(
            unsafe {
                mw_poll_events(
                    handle,
                    buf.as_mut_ptr(),
                    buf.len() as i32,
                    std::ptr::null_mut(),
                )
            },
            MwResult::ErrNullPointer as i32
        );
    }

    #[test]
    fn poll_events_rejects_null_out_dropped() {
        let mut buf = [MwEvent {
            kind: MwEventKind::RouteChanged,
            payload: 0,
        }; 4];
        let result = unsafe { mw_poll_events(1, buf.as_mut_ptr(), 4, std::ptr::null_mut()) };
        assert_eq!(result, MwResult::ErrNullPointer as i32);
    }

    #[test]
    fn poll_events_rejects_null_buf_when_capacity_is_positive() {
        let mut dropped = 0u32;
        let result =
            unsafe { mw_poll_events(1, std::ptr::null_mut(), 4, &mut dropped as *mut u32) };
        assert_eq!(result, MwResult::ErrNullPointer as i32);
    }

    #[test]
    fn poll_events_with_zero_capacity_and_null_buf_does_not_crash() {
        let mut dropped = 0u32;
        let result = unsafe {
            mw_poll_events(
                0xDEAD_BEEF_u64,
                std::ptr::null_mut(),
                0,
                &mut dropped as *mut u32,
            )
        };
        assert_eq!(result, MwResult::ErrInvalidHandle as i32);
    }

    #[test]
    fn poll_events_rejects_negative_capacity_without_crashing() {
        let mut dropped = 0u32;
        let result = unsafe {
            mw_poll_events(
                0xDEAD_BEEF_u64,
                std::ptr::null_mut(),
                -1,
                &mut dropped as *mut u32,
            )
        };
        assert_eq!(result, MwResult::ErrInvalidHandle as i32);
    }

    #[test]
    fn poll_events_with_invalid_handle_is_invalid_handle_not_a_crash() {
        let mut buf = [MwEvent {
            kind: MwEventKind::RouteChanged,
            payload: 0,
        }; 4];
        let mut dropped = 0u32;
        let result = unsafe {
            mw_poll_events(
                0xDEAD_BEEF_u64,
                buf.as_mut_ptr(),
                4,
                &mut dropped as *mut u32,
            )
        };
        assert_eq!(result, MwResult::ErrInvalidHandle as i32);
    }

    #[test]
    fn sound_load_accepts_music_mode_but_still_requires_a_valid_handle() {
        // M2-7 より前は mode=1(Music)自体が非対応で `ErrUnsupportedSoundMode` を
        // 返していた(このテストの旧名 `sound_load_rejects_music_mode_in_m1` の由来)。
        // 今は Music モードは認識される有効な mode 値なので、mode 検証自体は通り、
        // 後続のハンドル検証(`load_music` 内の `with_instance`)で弾かれる
        // ——`ErrUnsupportedSoundMode` ではなく `ErrInvalidHandle` になることを固定化する。
        let wav_bytes = make_pcm16_wav(48_000, 1, &[0]);
        let mut out_id = 0u64;
        let result = unsafe {
            mw_sound_load(
                0xDEAD_BEEF_u64,
                wav_bytes.as_ptr(),
                wav_bytes.len(),
                1, // MwSoundMode::Music
                &mut out_id as *mut u64,
            )
        };
        assert_eq!(result, MwResult::ErrInvalidHandle);
    }

    #[test]
    fn sound_load_rejects_unknown_mode() {
        let wav_bytes = make_pcm16_wav(48_000, 1, &[0]);
        let mut out_id = 0u64;
        let result = unsafe {
            mw_sound_load(
                1,
                wav_bytes.as_ptr(),
                wav_bytes.len(),
                99,
                &mut out_id as *mut u64,
            )
        };
        assert_eq!(result, MwResult::ErrUnsupportedSoundMode);
    }

    #[test]
    fn sound_load_rejects_null_out_id() {
        let wav_bytes = make_pcm16_wav(48_000, 1, &[0]);
        let result = unsafe {
            mw_sound_load(
                1,
                wav_bytes.as_ptr(),
                wav_bytes.len(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(result, MwResult::ErrNullPointer);
    }

    #[test]
    fn sound_load_with_invalid_handle_is_invalid_handle_not_a_crash() {
        let wav_bytes = make_pcm16_wav(48_000, 2, &[0, 0]);
        let mut out_id = 0u64;
        let result = unsafe {
            mw_sound_load(
                0xDEAD_BEEF_u64,
                wav_bytes.as_ptr(),
                wav_bytes.len(),
                0,
                &mut out_id as *mut u64,
            )
        };
        assert_eq!(result, MwResult::ErrInvalidHandle);
    }

    #[test]
    fn se_play_rejects_invalid_bus() {
        let mut out_voice = 0u64;
        let result = unsafe { mw_se_play(1, 1, 99, 1.0, &mut out_voice as *mut u64) };
        assert_eq!(result, MwResult::ErrInvalidBus);
    }

    #[test]
    fn se_play_rejects_null_out_voice() {
        let result = unsafe { mw_se_play(1, 1, 0, 1.0, std::ptr::null_mut()) };
        assert_eq!(result, MwResult::ErrNullPointer);
    }

    #[test]
    fn bus_set_volume_rejects_invalid_bus() {
        assert_eq!(mw_bus_set_volume(1, 99, 1.0), MwResult::ErrInvalidBus);
    }

    #[test]
    fn bus_fade_rejects_invalid_bus() {
        assert_eq!(mw_bus_fade(1, 99, 1.0, 10.0), MwResult::ErrInvalidBus);
    }

    #[test]
    fn voice_stop_and_set_volume_with_invalid_handle_are_invalid_handle_not_a_crash() {
        assert_eq!(
            mw_voice_stop(0xDEAD_BEEF_u64, 1),
            MwResult::ErrInvalidHandle
        );
        assert_eq!(
            mw_voice_set_volume(0xDEAD_BEEF_u64, 1, 0.5),
            MwResult::ErrInvalidHandle
        );
    }

    #[test]
    fn sound_release_with_unknown_id_is_invalid_sound_id_not_a_crash() {
        // ハンドル自体が無効な場合は先にハンドル検証で弾かれる。
        assert_eq!(
            mw_sound_release(0xDEAD_BEEF_u64, 1),
            MwResult::ErrInvalidHandle
        );
    }

    // --- M2-5: ホスト単調時刻 / 予約発音 --------------------------------------

    #[test]
    fn host_time_ns_is_monotonically_non_decreasing_across_ffi_boundary() {
        let a = mw_host_time_ns();
        let b = mw_host_time_ns();
        assert!(b >= a, "a={a}, b={b}");
    }

    #[test]
    fn se_schedule_rejects_invalid_bus() {
        let mut out_voice = 0u64;
        let result = unsafe { mw_se_schedule(1, 1, 99, 1.0, 0, &mut out_voice as *mut u64) };
        assert_eq!(result, MwResult::ErrInvalidBus);
    }

    #[test]
    fn se_schedule_rejects_null_out_voice() {
        let result = unsafe { mw_se_schedule(1, 1, 0, 1.0, 0, std::ptr::null_mut()) };
        assert_eq!(result, MwResult::ErrNullPointer);
    }

    #[test]
    fn se_schedule_with_invalid_handle_is_invalid_handle_not_a_crash() {
        let mut out_voice = 0u64;
        let result =
            unsafe { mw_se_schedule(0xDEAD_BEEF_u64, 1, 0, 1.0, 0, &mut out_voice as *mut u64) };
        assert_eq!(result, MwResult::ErrInvalidHandle);
    }

    #[test]
    fn music_play_scheduled_with_invalid_handle_is_invalid_handle_not_a_crash() {
        assert_eq!(
            mw_music_play_scheduled(0xDEAD_BEEF_u64, 0),
            MwResult::ErrInvalidHandle
        );
    }

    // --- M2-7: 楽曲再生(ハンドル無し/インスタンス無しで固定化できる契約) --------
    //
    // 実ハンドル越しの一連の流れ(mw_music_set → Ready ポーリング → 楽曲制御 API →
    // release)は `run_music_lifecycle` に集約済み(理由はそちらのドキュメント参照)。
    // ここでは「無効ハンドルで全新規関数がクラッシュせず ErrInvalidHandle を返す」
    // 「null ポインタで ErrNullPointer を返す」「ID 空間分離が効いている」という、
    // インスタンス無しでも固定化できる契約に絞る。

    #[test]
    fn music_set_rejects_an_se_shaped_id_before_touching_the_handle() {
        // MUSIC_ID_FLAG が立っていない ID(SE 用の ID 空間)を渡すとエラーになる。
        // ハンドルの有効性より先にこの検証が行われる(`sound_load_rejects_unknown_mode`
        // と同じ流儀: 値の検証はハンドル探索より前)。
        assert_eq!(mw_music_set(1, 42), MwResult::ErrInvalidSoundId);
    }

    #[test]
    fn music_set_with_invalid_handle_is_invalid_handle_not_a_crash() {
        // MUSIC_ID_FLAG は立っているが、ロードされていない(存在しない)楽曲 ID。
        let unloaded_music_id = handle_registry::MUSIC_ID_FLAG | 1;
        assert_eq!(
            mw_music_set(0xDEAD_BEEF_u64, unloaded_music_id),
            MwResult::ErrInvalidHandle
        );
    }

    #[test]
    fn music_state_rejects_null_out_state() {
        let result = unsafe { mw_music_state(1, std::ptr::null_mut()) };
        assert_eq!(result, MwResult::ErrNullPointer);
    }

    #[test]
    fn music_state_with_invalid_handle_is_invalid_handle_not_a_crash() {
        let mut state = 0i32;
        let result = unsafe { mw_music_state(0xDEAD_BEEF_u64, &mut state as *mut i32) };
        assert_eq!(result, MwResult::ErrInvalidHandle);
    }

    #[test]
    fn music_get_position_rejects_null_out() {
        let result = unsafe { mw_music_get_position(1, std::ptr::null_mut()) };
        assert_eq!(result, MwResult::ErrNullPointer);
    }

    #[test]
    fn music_get_position_with_invalid_handle_is_invalid_handle_not_a_crash() {
        let mut position = MwMusicPosition {
            song_frames: 0,
            host_time_ns: 0,
            sample_rate: 0,
            state: MwMusicState::Loading,
            is_playing: 0,
            generation: 0,
        };
        let result = unsafe {
            mw_music_get_position(0xDEAD_BEEF_u64, &mut position as *mut MwMusicPosition)
        };
        assert_eq!(result, MwResult::ErrInvalidHandle);
    }

    #[test]
    fn music_pause_resume_seek_stop_with_invalid_handle_are_invalid_handle_not_a_crash() {
        assert_eq!(mw_music_pause(0xDEAD_BEEF_u64), MwResult::ErrInvalidHandle);
        assert_eq!(
            mw_music_resume_at(0xDEAD_BEEF_u64, 0),
            MwResult::ErrInvalidHandle
        );
        assert_eq!(
            mw_music_seek(0xDEAD_BEEF_u64, 0),
            MwResult::ErrInvalidHandle
        );
        assert_eq!(mw_music_stop(0xDEAD_BEEF_u64), MwResult::ErrInvalidHandle);
    }

    #[test]
    fn music_set_loop_rejects_invalid_region_before_touching_the_handle() {
        // begin >= end かつループ解除((0, 0))でもない場合は不正。ハンドルの
        // 有効性より先にこの検証が行われる(無効ハンドルでも区間検証の結果が
        // そのまま返ることで確認できる)。
        assert_eq!(
            mw_music_set_loop(0xDEAD_BEEF_u64, 10, 10),
            MwResult::ErrInvalidLoopRegion
        );
        assert_eq!(
            mw_music_set_loop(0xDEAD_BEEF_u64, 10, 5),
            MwResult::ErrInvalidLoopRegion
        );
    }

    #[test]
    fn music_set_loop_zero_zero_is_treated_as_clear_not_as_an_invalid_region() {
        // (0, 0) はループ解除として扱われ、不正区間の検証には引っかからない。
        // ハンドルが無効なので最終的な戻り値は ErrInvalidHandle になるが、これが
        // ErrInvalidLoopRegion では *ない* こと自体が「(0, 0) は解除として区間検証を
        // 通過した」ことの証拠になる。
        assert_eq!(
            mw_music_set_loop(0xDEAD_BEEF_u64, 0, 0),
            MwResult::ErrInvalidHandle
        );
    }

    #[test]
    fn music_set_loop_with_a_valid_region_and_invalid_handle_is_invalid_handle_not_a_crash() {
        assert_eq!(
            mw_music_set_loop(0xDEAD_BEEF_u64, 0, 100),
            MwResult::ErrInvalidHandle
        );
    }

    #[test]
    fn get_output_latency_ns_rejects_null_out_ns() {
        let result = unsafe { mw_get_output_latency_ns(1, std::ptr::null_mut()) };
        assert_eq!(result, MwResult::ErrNullPointer);
    }

    #[test]
    fn get_output_latency_ns_with_invalid_handle_is_invalid_handle_not_a_crash() {
        let mut out_ns = 0u64;
        let result = unsafe { mw_get_output_latency_ns(0xDEAD_BEEF_u64, &mut out_ns as *mut u64) };
        assert_eq!(result, MwResult::ErrInvalidHandle);
    }

    #[test]
    fn get_output_underrun_stats_rejects_null_out() {
        let result = unsafe { mw_get_output_underrun_stats(1, std::ptr::null_mut()) };
        assert_eq!(result, MwResult::ErrNullPointer);
    }

    #[test]
    fn get_output_underrun_stats_with_invalid_handle_is_invalid_handle_not_a_crash() {
        let mut out = MwOutputUnderrunStats {
            count: 0,
            last_host_time_ns: 0,
            consecutive_count: 0,
        };
        let result = unsafe {
            mw_get_output_underrun_stats(0xDEAD_BEEF_u64, &mut out as *mut MwOutputUnderrunStats)
        };
        assert_eq!(result, MwResult::ErrInvalidHandle);
    }

    #[test]
    fn sound_release_with_a_music_shaped_id_does_not_crash_and_does_not_touch_se_storage() {
        // ハンドル自体は無効なので、最終的には(SE 経路と同じく)ErrInvalidHandle が
        // 先に返る。ここでの主眼は「楽曲 ID を渡した経路(`release_music`)が
        // クラッシュしないこと」——`mw_sound_release` の分岐(`is_music_id`)が
        // 正しく `release_music` 側へ振り分けていることの間接証拠でもある。
        let music_shaped_id = handle_registry::MUSIC_ID_FLAG | 999;
        assert_eq!(
            mw_sound_release(0xDEAD_BEEF_u64, music_shaped_id),
            MwResult::ErrInvalidHandle
        );
    }
}
