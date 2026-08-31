<p align="center">
  <img src="docs/assets/logo.png" alt="Open Audioware — open source audio middleware" width="520" />
</p>

# open-audioware

**Open Audioware** — 音楽ゲーム向けに自作している、CRI Ware の大まかな代替となる
オーディオミドルウェア(Rust)。
CRI の全機能を再現するのではなく、音ゲーに必要な機能だけを絞って実装する。

- OS のローレイテンシ音声 API を直接叩き、タップ SE の発音遅延と音楽クロックの精度を
  Unity 既定オーディオより一段引き上げることが目的
- 実装方針・決定事項・フェーズ計画の唯一の正は **初期構築仕様**
  (リポジトリ直下のドキュメント。初期構築完了後は `CLAUDE.md` と `docs/` に引き継がれる)
- 運用ルールの索引は [`CLAUDE.md`](./CLAUDE.md) を参照

現在のフェーズ: **M1(SE 再生)★価値の核**。M0(基盤)に加えて、ミキサ・ボイスプール・
バス音量とフェード・ソフトクリッパ・wav ロード・SE の即時発音・オフラインレンダリングでの
テスト群・リアルタイム安全性の自動検証(コールバック内アロケーションゼロの固定化)が
揃っている。実機での Unity 既定実装との遅延 A/B 計測(§1 成功基準の最重要ゲート)は
`docs/measurement-m1.md` の手順に沿ってユーザー協働で実施する。

## ライセンス

**MIT-0(MIT No Attribution)。** 全文は [`LICENSE`](./LICENSE)。

### あなたが負う義務

**open-audioware 自体については、ゼロ。**

| | |
|---|---|
| 著作権表示・クレジット | **不要** |
| ロゴの表示 | **不要** |
| 利用の登録・申請・報告 | **不要** |
| ロイヤリティ・売上報告 | **不要** |
| 商用利用 | **可**(無償・無制限) |
| 改変・フォーク・再配布 | **可**(改変を公開する義務なし) |
| クローズドソース製品への組み込み | **可** |
| 機能制限版 / 製品版の区別 | **無い**(これが全部) |

MIT-0 は MIT から「著作権表示を複製に含めること」という一文だけを取り除いたもので、
OSI が承認した正式なオープンソースライセンス。**表記すら要らない**ようにしてあるのは、
「使うたびに法務へ相談が要る」状態そのものを無くしたいため。
(もちろん、クレジットを入れてもらえるのは嬉しい。**義務ではないというだけ。**)

### 🔴 ただし、依存ライブラリの義務は消せない

これは正直に書いておく。**open-audioware を組み込んだアプリを配布するとき、
あなたは第三者ライブラリの表記義務を負う。** これはこちらの都合ではどうにもならない ——
それぞれのライブラリの作者が定めた条件だから。

**そのぶんの手間はこちらで引き受ける。**
[`THIRD-PARTY-LICENSES.md`](./THIRD-PARTY-LICENSES.md) を自動生成してあるので、
**これをそのままゲームのライセンス表示画面に載せれば足りる。**

- 依存クレート **107 件**。ほとんどは MIT / Apache-2.0 / BSD 系で、**表記のみ**
- うち **9 件は MPL-2.0**(音声デコーダの `symphonia` 一族と `audio_thread_priority`)。
  こちらは**表記に加えてソースの入手方法の告知**が要る。生成物に URL 一覧が入っている
- 🔴 **MPL-2.0 でも、あなたのゲームのソースを公開する義務は生じない。**
  ファイル単位のコピーレフトで、**MPL のファイルそのものを改変したとき**にだけ、
  そのファイルの公開義務が生じる。静的リンクして使うぶんには伝播しない

`make lint` が **THIRD-PARTY-LICENSES.md の鮮度を検査する**ので、
依存を足したまま表記を更新し忘れる事故は起きない。

### 商標

ライセンスは**名称の使用許諾を含まない**(これは MIT 系すべてに共通)。
フォークを別名で配布するのは自由だが、**本家と誤認させる形で "open-audioware" を
名乗らないこと**。

### ⚠️ 免責

このドキュメントは**法的助言ではない**。商用配布の前に、自社の基準で確認すること。

## セットアップ

前提:

- Rust stable(rustup 管理。`rust-version` は各クレートの `Cargo.toml` を参照。edition 2024)
- Xcode(iOS ビルド。xcframework 作成に `xcodebuild` を使う)
- Unity Hub + Unity **6000.4.1f1**、Android Build Support(NDK 込み)
  (`unity-sample/` の EditMode テスト実行・Android クロスビルドに使用)

```sh
make setup
```

で以下を確認・導入する:

- rustup ターゲット: `aarch64-apple-darwin` / `x86_64-apple-darwin` / `aarch64-apple-ios` /
  `aarch64-linux-android`
- `cargo-ndk`(未導入なら `cargo install`)
- `cargo-deny`(未導入なら `cargo install`)
- `ANDROID_NDK_HOME`(既定値は Unity Hub 同梱の NDK。環境変数で上書き可能)

### Intel Mac に関する注記

初期構築仕様のビルドターゲット表は Apple Silicon(`aarch64-apple-darwin`)を前提に
書かれているが、開発機が **Intel Mac(x86_64-apple-darwin)** の場合、
Unity Editor 用 dylib は **ホストアーチ**でビルドする。
`make build-macos` は `--target` を明示せず cargo のデフォルト(ホスト)ターゲットを
使うため、Apple Silicon 機で実行すれば自動的に `aarch64-apple-darwin` の dylib になる
— Makefile・コードのどちらもアーチ非依存に書いてあるため、機種の違いを気にする必要はない。

CI の macOS ランナー(`macos-latest`)は Apple Silicon のため、CI で生成される dylib は
`aarch64-apple-darwin` になる。ローカル(Intel Mac)と CI とで生成物のアーチが異なる点は
想定内(どちらも「ホストアーチのビルドが動く」という同じ Makefile ロジックの結果)。

## Makefile コマンド一覧

| コマンド | 内容 |
|---|---|
| `make setup` | ツールチェーン・ターゲット・cargo-ndk 等の導入確認 |
| `make lint` | `cargo fmt --check` + `cargo clippy -D warnings` + `cargo deny check` + 第三者表記の鮮度検査 |
| `make third-party-licenses` | [`THIRD-PARTY-LICENSES.md`](./THIRD-PARTY-LICENSES.md) を再生成。**依存を足したら必ず実行** |
| `make third-party-licenses-check` | 再生成して差分が無いか検査(`make lint` から呼ばれる) |
| `make format` | `cargo fmt`(自動整形) |
| `make test` | `cargo test --workspace`(mw-core のオフラインレンダリングが主戦場) |
| `make bench` | criterion ベンチ(未導入。M1 以降のミキサ実装後に追加) |
| `make bindgen` | csbindgen で `unity/Runtime/Generated/NativeMethods.g.cs` を生成 |
| `make build-macos` | `.dylib` をビルドし `unity/Runtime/Plugins/macOS/` へ配置(ホストアーチ) |
| `make build-ios` | `aarch64-apple-ios` 静的ライブラリ → `xcframework` |
| `make build-android` | `cargo-ndk` で `arm64-v8a` の `.so` を生成 |
| `make package` | UPM パッケージ組み立て(M0 はスタブ。必須ファイルの存在確認のみ) |
| `make unity-sample-create` | `unity-sample/` プロジェクトを新規作成(初回のみ。要 Unity ロック) |
| `make unity-test` | `unity-sample/` の EditMode テストを実行(要 Unity ロック) |
| `make clean` | `cargo clean` |

`unity-sample-create` / `unity-test` は Unity をバッチモードで起動する。同一マシン上の
他プロセスと衝突しないよう `tools/with-unity-lock.sh` がロック(`/tmp/mgct-unity.lock`)を
取得してから実行する。

## リポジトリ構成

```
open-audioware/
  crates/
    mw-core/     … OS 非依存のミキサ・クロック・デコードコア
    mw-backend/  … 出力デバイス抽象(Backend trait)+ cpal 実装
    mw-ffi/      … C ABI 境界(csbindgen の入力)
  unity/         … UPM パッケージ(バイナリ、生成バインディング、Mw.Native ラッパ)
  unity-sample/  … 統合検証用の最小 Unity プロジェクト(unity/ を file: 参照)
  tools/         … 補助スクリプト(Unity ロックラッパー等)
  docs/          … 肥大化する内容の切り出し先(統合手順・API リファレンス・ADR。M5 以降)
  Makefile
  Cargo.toml     … workspace
  deny.toml      … cargo-deny 設定(ライセンス allow リスト・アドバイザリ)
```

各クレートの設計・不変条件(特にリアルタイム安全性規約)は `crates/<name>/CLAUDE.md` を参照。
