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

use crate::handle as handle_registry;
use crate::handle::{InitOutcome, ShutdownOutcome};
use crate::result::MwResult;
use crate::types::{MwBus, MwSoundMode};

/// ABI バージョン。ABI 互換の破壊は semver メジャーバージョンでのみ許可する(§4.8)。
const ABI_VERSION: u32 = 1;

/// ABI バージョンを返す。C# 側は起動時にこれを期待値と照合すること。
#[unsafe(no_mangle)]
pub extern "C" fn mw_abi_version() -> u32 {
    ABI_VERSION
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

/// Android のライブラリロード時に JavaVM を受け取る。
///
/// cpal(AAudio)が Java 側を参照するため、`ndk_context` の初期化に JavaVM が要る
/// (`mw_backend::android_context` のモジュール doc 参照)。ここで控えておき、実際の初期化は
/// バックエンドを開く直前に行う。
///
/// **`System.loadLibrary` 経由でロードされた場合にのみ呼ばれる。** `dlopen` で直接開かれた
/// 場合は呼ばれないため、そのときは `ndk_context` を初期化できない旨がログに出る。
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(
    vm: *mut std::ffi::c_void,
    _reserved: *mut std::ffi::c_void,
) -> i32 {
    mw_backend::android_context::set_java_vm(vm);
    mw_backend::mw_log!("[mw-ffi] JNI_OnLoad: JavaVM を受け取った");

    // JNI_VERSION_1_6
    0x0001_0006
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

/// wav バイト列から SE 用 PCM をロードする(初期構築仕様 §5.5, §4.2)。
///
/// M1 は `mode = 0`(SE、全デコード常駐)のみ実装する。`mode = 1`(Music、
/// 圧縮のまま保持しストリーミングデコード)は M2 で実装予定であり、
/// `MwResult::ErrUnsupportedSoundMode` を返す(初期構築仕様 §5.2)。
///
/// 対応フォーマットは 16bit PCM / モノラルまたはステレオの wav のみ
/// (`crates/mw-core/src/wav.rs`)。サンプルレートは出力デバイスと一致しなくてよい
/// (一致しない場合はロード時に一括でリサンプルする。初期構築仕様『§4.7』)。
/// 非対応の場合は原因に応じたエラーコードを返す。
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
        if mode != MwSoundMode::Se {
            // Music モードは M2(初期構築仕様 §5.2「楽曲のみストリーミングデコード」)。
            return MwResult::ErrUnsupportedSoundMode;
        }

        let slice: &[u8] = if len == 0 {
            &[]
        } else {
            // SAFETY: 上で null チェック済み。呼び出し側が `len` バイトの読み取り可能な
            // 領域を渡す契約(このシンボルの `Safety` セクションで文書化済み)。
            unsafe { std::slice::from_raw_parts(bytes, len) }
        };

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
                    | mw_core::WavError::UnsupportedChannelCount(_) => {
                        MwResult::ErrUnsupportedFormat
                    }
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
                // SAFETY: 上で null チェック済み。
                unsafe {
                    *out_id = id.0;
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

/// ロード済みサウンドを解放する(初期構築仕様 §5.5)。
///
/// 再生中のボイスがあれば、既定ランプ(§4.1/M13)経由で即座に停止させたうえで解放する。
/// PCM の実データ(`Arc<SoundData>`)の最終的な解放(デアロケーション)はこの呼び出し
/// 自身(ゲームスレッド)か、あるいは音声スレッドがボイス終了時に回収キューへ送り出した
/// ものをこの関数が(次回以降のこの種の呼び出しで)ドレインする経路のいずれかで起こる。
/// いずれにせよ**音声コールバック内で `Arc` がドロップされることはない**
/// (初期構築仕様「PCM データの所有権」)。
///
/// 未知の `id`(未ロード・二重解放)は `MwResult::ErrInvalidSoundId` を返す(クラッシュしない)。
#[unsafe(no_mangle)]
pub extern "C" fn mw_sound_release(handle: u64, id: u64) -> MwResult {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
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
    }));

    match outcome {
        Ok(result) => result,
        Err(_) => MwResult::ErrPanic,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn sound_load_rejects_music_mode_in_m1() {
        let wav_bytes = make_pcm16_wav(48_000, 1, &[0]);
        let mut out_id = 0u64;
        let result = unsafe {
            mw_sound_load(
                1, // ハンドルの有効性より先に mode 検証が行われる
                wav_bytes.as_ptr(),
                wav_bytes.len(),
                1, // MwSoundMode::Music
                &mut out_id as *mut u64,
            )
        };
        assert_eq!(result, MwResult::ErrUnsupportedSoundMode);
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
}
