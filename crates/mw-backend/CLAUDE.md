# mw-backend

出力デバイス抽象(`Backend` trait)と、その cpal 実装(`CpalBackend`)。
`mw-core` にのみ依存する(依存の向きは `mw-backend → mw-core`。ワークスペース全体の方針は
`../../CLAUDE.md` を参照)。`mw-ffi` からのみ利用される。

## 現状(M1)

- `backend::Backend` — `open(&mut self, renderer: Renderer, events: Arc<EventQueue>)` /
  `close(&mut self)` / `is_open(&self)`。実装はパニックせず、失敗は必ず `BackendError`
  で返す。`renderer` は**値渡し(ムーブ)**。`Renderer` はコールバックスレッド専用の
  排他所有物としてストリームクロージャへムーブされる(M0 時点の `Arc<Renderer>` 共有から
  変更。`crates/mw-core/src/renderer.rs` のドキュメント参照。ゲームスレッドは
  `mw_core::CommandSender` / `mw_core::ReclaimReceiver` という別ハンドル経由でのみ
  音声スレッドとやり取りする)。`events` は初期構築仕様『§4.6 イベント通知』のキュー
  (M2-6)。`CpalBackend` はこれをストリームのエラー通知経路(`err_fn`、**音声スレッドとは
  別の**非リアルタイムスレッド)から `push_side_channel` で `StreamError` イベントを積むために
  使う(`cpal_backend::classify_stream_error` が `cpal::ErrorKind` を
  `mw_core::StreamErrorReason` へ丸める)。この経路は §5.3 の対象外なので `Mutex` を
  使ってよい——実際に音声コールバック本体(`build_output_stream` のデータコールバック)
  から呼ぶのは引き続き `Renderer::render` のみ(下記「設計意図」参照)。
- `backend::last_callback_frames` / `backend::sample_rate` — オーディオコールバックが実際に
  受け取ったフレーム数と、ネゴシエートされたサンプルレート。**I/O バッファ長の実測値**で、
  iOS の `AVAudioSession` の申告値を裏取りするために使う(`docs/measurement-m1.md` §8.7)。
  音声スレッドはアトミックストア1回だけ行い、読むのはゲームスレッド
  (リアルタイム安全性規約に抵触しない)。初期構築仕様 M3「出力レイテンシ問い合わせ」の第一歩。
- `backend::BackendError` — デバイス無し・対応構成無し・ストリーム構築/開始失敗・
  二重 open・未 open close、の6種。
- `cpal_backend::CpalBackend` — 既定の出力デバイスに **f32 ステレオ**のストリームを開く。
  対応する構成が無い場合(モノ/サラウンド専用デバイス、非対応サンプルフォーマット等)は
  `BackendError::NoSupportedStreamConfig` を返す(サンプルフォーマット変換は未実装、将来課題)。
  デバイスが実際にネゴシエートしたサンプルレートは、コールバックが動き出す
  (`stream.play()`)前に `Renderer::set_sample_rate` で確定させる(バス/ボイスのランプの
  ミリ秒→サンプル数換算に必要。初期構築仕様 §4.1)。オーディオコールバック内では
  cpal の `OutputCallbackInfo::timestamp().playback`(`StreamInstant`)を ns 化して
  `Renderer::render` の `buffer_start_host_time_ns` 引数へ渡す(初期構築仕様『§4.4』の
  デバイスタイムスタンプ相関、M2-5)。呼ぶのは引き続き `Renderer::render` のみ
  (下記「設計意図」の制約を参照。`StreamInstant` を読むのは cpal が既に計算済みの
  構造体を見るだけで、mw-core への別呼び出しを増やしたわけではない)。

- `host_time::host_time_ns` — ホスト単調時刻を ns で返す(初期構築仕様『§4.4』, M2-5)。
  macOS/iOS/tvOS は `mach_absolute_time` + `mach_timebase_info`、Android(と CI 専用の
  Linux)は `clock_gettime(CLOCK_MONOTONIC)`。**cpal 0.18.1 の `StreamInstant` を
  各ホストがどう生成しているかを実装まで確認したうえで、同じ式をそのまま再現している**
  (調査結果と根拠は `host_time.rs` のモジュール doc に記載済み)。これにより
  `mw_host_time_ns()`(FFI)が返す値と `OutputCallbackInfo` のデバイスタイムスタンプが
  直接比較可能になる。`mw-ffi::mw_host_time_ns` がここへ薄く委譲する。

- `ios_session::configure` — iOS / tvOS で AVAudioSession(カテゴリ・希望サンプルレート・
  希望 I/O バッファ長)を設定し、採用された実値をログへ出す。`CpalBackend::open` の冒頭、
  **cpal に触る前に**呼ぶ(iOS の cpal はデバイス列挙をセッションの現在状態から作るため)。
  iOS / tvOS 以外では何もしない。初期構築仕様 §14 のリスク表は Obj-C シムを想定していたが、
  cpal 0.18 が `objc2-avf-audio` を持ち込むため Rust から直接設定している(詳細は同ファイルの
  モジュールコメント)。設定値はすべて【仮】で、変更が要るのはこのファイル1枚。

- `ios_interruption`(M3)— iOS / tvOS の `AVAudioSessionInterruptionNotification` /
  `UIApplicationDidBecomeActiveNotification` を監視し、割り込み終了時・アプリのアクティブ化時に
  セッション再アクティブ化 + ストリーム再始動(`pause()`→`play()`)を試みる。実機報告
  「バックグラウンドから戻ると SE だけ無音になる」の修正(cpal 0.18.1 の iOS 実装は
  interruption 通知を監視しておらず、`Stream::play()` の内部フラグも OS 主導の停止に
  追随しないため、単純な再呼び出しでは復帰しない——詳細はモジュール doc)。`CpalBackend`
  はストリームを `Arc<cpal::Stream>` として持ち、この監視の復帰ハンドラと共有する。
  復帰ロジックは `InterruptionState`(OS 呼び出しを含まない純粋な状態機械)として切り出し、
  単体テストで遷移を固定化してある(実機の割り込みそのものは自動テスト不可のため)。
  `mw_core::Event::AudioInterruptionBegan`/`AudioInterruptionEnded { recovered }`
  として `mw_poll_events` 経由で C# 側からも観測できる。iOS / tvOS 以外では no-op。
  Android の AAudio disconnect は同じ経路では塞げていない(`docs/history.md` 2026-08-28
  「M3 着手」参照——`Renderer` の再構築が要るため今回は見送り)。

## 設計意図

- 初期構築仕様 M6(【仮】): 立ち上げは cpal で macOS Editor / iOS / Android を1系統に揃える。
  計測の結果 cpal で遅延目標(iOS ≤ 20ms / Android ≤ 40ms)に届かない場合、
  Android は oboe 直叩き、iOS は RemoteIO 直叩きに置換する可能性がある。
  その置き換えは `Backend` trait を実装する新しい struct を追加するだけで済むようにしてある
  — `mw-ffi` 側は `Backend` トレイトオブジェクト(または将来的にジェネリクス)越しにしか
  バックエンドを触らない設計を維持すること。
- オーディオコールバック(`build_output_stream` に渡すクロージャ)は音声スレッド上で実行される。
  ここから呼ぶのは `Renderer::render` のみに保つこと
  (`mw-core/CLAUDE.md` のリアルタイム安全性規約がこの経路にも適用される)。

## Android ビルド時の依存について

`cpal` 0.18 は Android で `ndk` / `ndk-context` / `jni` クレート経由で AAudio を直接叩く
実装を持ち、追加の feature flag は不要(`oboe` クレートには依存しない)。
`cargo ndk` でのクロスビルドに特別な設定は要らない(`make build-android` 参照)。
