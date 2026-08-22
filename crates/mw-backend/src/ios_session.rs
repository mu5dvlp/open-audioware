//! iOS / tvOS の AVAudioSession 設定。
//!
//! **なぜ必要か**: セッションを設定しないと iOS 既定の I/O バッファ長(通常 ~23ms)が
//! そのまま出力遅延に乗る。第1回の A/B 計測(`docs/measurement-m1.md` §7)で本ミドルウェアの
//! SE 遅延が中央値 25ms(方式の系統誤差を考えると真値は 25〜40ms 程度)となり、
//! 初期構築仕様 §1 の目標(iOS ≤ 20ms)に届かなかった。その第一容疑者がこれ。
//!
//! **実装方針の補足**: 初期構築仕様 §14 のリスク表は「iOS の AVAudioSession 設定は Rust から
//! 直接触れない → 最小の Obj-C シムを xcframework に同梱(M3)」としていたが、cpal 0.18 が
//! `objc2-avf-audio` を依存に持ち込むため Rust から直接設定できる。Obj-C ファイルを増やさない
//! ぶん、§1 M1【確定】「コア・プラットフォーム層とも Rust で書き、OS 依存部のみ薄いシムを許容」
//! により沿う。差し替えが要るのは本ファイル1枚。
//!
//! iOS / tvOS 以外では何もしない(macOS Editor・Android は各 OS の既定に任せる)。

/// 希望する I/O バッファ長(秒)【仮】。
///
/// 5ms は「音ゲーの SE として体感で遅れない」ことを狙った暫定値。OS は希望どおりの値を
/// 採用するとは限らない(ハードウェアの粒度に丸められる)ため、実際に採用された値は
/// `configure` が必ずログへ出す。短くしすぎるとコールバック頻度が上がりアンダーランの
/// リスクが増えるので、計測結果を見て調整すること。
pub const PREFERRED_IO_BUFFER_DURATION_SEC: f64 = 0.005;

/// 希望するハードウェアサンプルレート(Hz)【仮】。
///
/// `mw-core` のデコーダは 48kHz のみ受け付け(`mw_core::wav`)、リサンプラも未実装
/// (初期構築仕様 §4.7)。ハード側も 48kHz に寄せておくとレート不一致による
/// 再生ピッチのずれを構造的に避けられる。
pub const PREFERRED_SAMPLE_RATE_HZ: f64 = 48_000.0;

/// AVAudioSession をこのミドルウェア向けに設定する。
///
/// 失敗しても致命傷にはしない(ホストアプリがセッションを管理している場合や、
/// OS が希望値を拒否する場合がある)。失敗はすべてログに残して処理を続行する。
///
/// **呼ぶ順序が重要**: iOS では cpal のデバイス列挙(チャンネル数・サンプルレート)が
/// AVAudioSession の現在の状態から作られるため、cpal に触る前に呼ぶこと。
#[cfg(any(target_os = "ios", target_os = "tvos"))]
pub fn configure() {
    imp::configure();
}

/// iOS / tvOS 以外では何もしない。
#[cfg(not(any(target_os = "ios", target_os = "tvos")))]
pub fn configure() {}

#[cfg(any(target_os = "ios", target_os = "tvos"))]
mod imp {
    use objc2_avf_audio::{AVAudioSession, AVAudioSessionCategoryPlayback};

    use super::{PREFERRED_IO_BUFFER_DURATION_SEC, PREFERRED_SAMPLE_RATE_HZ};

    pub fn configure() {
        // SAFETY: `sharedInstance` はプロセス唯一の AVAudioSession を返す。以降の設定は
        // いずれも失敗を NSError で返す API で、パニックはしない(§4.8 の思想)。
        unsafe {
            let session = AVAudioSession::sharedInstance();

            // カテゴリ【仮】: 効果音・BGM を鳴らすアプリなので Playback。
            // 消音スイッチで無音にならず、Bluetooth も HFP(通話用・モノラル)ではなく
            // A2DP(ステレオ)側のルートが選ばれる。後者は実利があり、モノラルルートでは
            // `CpalBackend` が f32 ステレオ構成を見つけられず初期化ごと失敗する
            // (`docs/measurement-m1.md` §7.6-1)。
            //
            // 注意: ホスト(Unity 等)も起動時に自前のカテゴリを設定する。ここは
            // `mw_init` の時点で後から上書きする形になるため、ホストの音声挙動
            // (消音スイッチの扱い・他アプリとのミックス)も一緒に変わる。
            match AVAudioSessionCategoryPlayback {
                Some(category) => {
                    if let Err(err) = session.setCategory_error(category) {
                        eprintln!("[mw-backend] AVAudioSession setCategory failed: {err:?}");
                    }
                }
                None => {
                    eprintln!("[mw-backend] AVAudioSessionCategoryPlayback is unavailable");
                }
            }

            if let Err(err) = session.setPreferredSampleRate_error(PREFERRED_SAMPLE_RATE_HZ) {
                eprintln!("[mw-backend] AVAudioSession setPreferredSampleRate failed: {err:?}");
            }

            if let Err(err) =
                session.setPreferredIOBufferDuration_error(PREFERRED_IO_BUFFER_DURATION_SEC)
            {
                eprintln!(
                    "[mw-backend] AVAudioSession setPreferredIOBufferDuration failed: {err:?}"
                );
            }

            if let Err(err) = session.setActive_error(true) {
                eprintln!("[mw-backend] AVAudioSession setActive(true) failed: {err:?}");
            }

            // OS が実際に採用した値。希望どおりとは限らないため必ず残す。
            // 計測結果の解釈に直結するので、記録(`docs/measurement-m1.md` §7.1)へ
            // 転記できるようこの1行で完結させている。
            eprintln!(
                "[mw-backend] AVAudioSession configured: sample_rate={} Hz, io_buffer={:.3} ms, \
                 output_latency={:.3} ms, output_channels={}",
                session.sampleRate(),
                session.IOBufferDuration() * 1000.0,
                session.outputLatency() * 1000.0,
                session.outputNumberOfChannels(),
            );
        }
    }
}
