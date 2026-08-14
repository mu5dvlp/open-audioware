# mw-backend

出力デバイス抽象(`Backend` trait)と、その cpal 実装(`CpalBackend`)。
`mw-core` にのみ依存する(依存の向きは `mw-backend → mw-core`。ワークスペース全体の方針は
`../../CLAUDE.md` を参照)。`mw-ffi` からのみ利用される。

## 現状(M0)

- `backend::Backend` — `open(&mut self, renderer: Arc<Renderer>)` / `close(&mut self)` /
  `is_open(&self)`。実装はパニックせず、失敗は必ず `BackendError` で返す。
- `backend::BackendError` — デバイス無し・対応構成無し・ストリーム構築/開始失敗・
  二重 open・未 open close、の6種。
- `cpal_backend::CpalBackend` — 既定の出力デバイスに **f32 ステレオ**のストリームを開く。
  対応する構成が無い場合(モノ/サラウンド専用デバイス、非対応サンプルフォーマット等)は
  `BackendError::NoSupportedStreamConfig` を返す(サンプルフォーマット変換は未実装、将来課題)。

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
