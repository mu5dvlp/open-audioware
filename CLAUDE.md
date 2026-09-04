# CLAUDE.md — open-audioware 運用ルール

このファイルは短く保つ。詳細は索引先のドキュメントを参照すること。

📌 **このリポジトリはワークスペース(1つ上の階層)の一部**で、client / server と合わせて
開発している。**セッションを再開するときは、まずワークスペースの `docs/HANDOFF.md` を
読むこと**(現在の状態・次にやること・未解決の問題がそこにある)。文書全体の地図は
ワークスペースの `CLAUDE.md`。

## 唯一の正

リポジトリ直下の**初期構築仕様**が、実装方針・決定事項・フェーズ計画の唯一の正。
特にコードやコメントから参照する際は「初期構築仕様『§n 話題名』」のように書き、
特定のファイル名には依存しないこと。

凡例(初期構築仕様と共通):

- **【確定】** — 合意済み。勝手に変更しない。変更が必要なら必ず確認を取る。
- **【仮】** — 妥当な既定値。実装は進めてよいが、後から変わりうる前提で
  「1箇所を書き換えれば済む」構造(設定構造体 / 定数)にしておく。
- **【未定】** — 未決。実装がそこに到達する前に確認を取る。

## ドキュメント索引

- [`README.md`](./README.md) — 概要・セットアップ・Makefile 一覧
- [`crates/mw-core/CLAUDE.md`](./crates/mw-core/CLAUDE.md) — ミキサ・クロック・デコードコア。
  **リアルタイム安全性規約(音声スレッドでの禁止事項)はここに常設**
- [`crates/mw-backend/CLAUDE.md`](./crates/mw-backend/CLAUDE.md) — 出力デバイス抽象と cpal 実装
- [`crates/mw-ffi/CLAUDE.md`](./crates/mw-ffi/CLAUDE.md) — C ABI 境界・エラーモデル・ハンドル管理
- `docs/` — 肥大化する内容の切り出し先(統合手順・API リファレンス・ADR。M5 以降で拡充)
- [`docs/history.md`](./docs/history.md) — 作業の経緯(索引。本体は `docs/history/`)
- **ワークスペース全体の状態・次にやること** → 1つ上の階層の `docs/HANDOFF.md`
- **踏んだ罠と教訓** → 1つ上の階層の `docs/LESSONS.md`

## Git 運用

- `main` は常にビルドが通る状態を保つ。作業は `feature/<phase>-<内容>` ブランチで行う
- マージ前にローカルで `make lint` `make test` を通す
- フェーズ完了時にタグを打つ(例: `phase-m0`)
- Git LFS は使わない。ビルド成果物(dylib / xcframework / so / 生成 C# バインディング /
  ゴールデン波形)はコミットしない(`.gitignore` を参照)

🔴 **ネイティブバイナリは gitignore されている(追跡は `.meta` だけ)。**
Rust を直しても、client 側で `make build-macos` / `build-ios` / `build-android` を回さないと
**利用側に一切届かない**。回したあとは `strings` / `nm` でバイナリに入ったかまで確認する。
⚠️ ただし**エクスポートシンボルが増減しない内部修正は `nm` / `strings` で新旧を見分けられない**
——その場合の確認手段は実機での聴感だけになる(検証が弱いことを承知で受け入れる)。
📌 **挙動を変えないリファクタ(例: P1-6 / P1-7)は焼き直し不要。** 判断の根拠は
csbindgen が生成する `NativeMethods.g.cs` を前後で diff して同一かどうか。

## リアルタイム安全性

音声コールバック経路(`mw-core::Renderer::render` およびそこから呼ばれる全コード)は
ロック・ヒープアロケーション・ブロッキング IO・パニック経路を禁止する。
詳細と検証手段は `crates/mw-core/CLAUDE.md` を参照。

## Unity 起動の直列化

同一マシン上で並行して動く他セッションもクライアントプロジェクト側で Unity を使うことが
あるため、**いかなる Unity 起動**(`-createProject` / `-runTests` を含む)**の前にも**
`tools/with-unity-lock.sh` 経由でロック(`/tmp/mgct-unity.lock`)を取得すること。
`make unity-sample-create` / `make unity-test` は既にこれを内包している。

## 【仮】【未定】の扱い

実装が【仮】【未定】の項目に到達したら、勝手に確定させず確認を取る
(初期構築仕様 §11「エージェント運用」、§15「未決事項の一覧」)。
