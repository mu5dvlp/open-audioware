# コントリビュートガイド

open-audioware への貢献を歓迎します。変更を始める前に、[README.md](./README.md) と、変更対象クレートの `CLAUDE.md` を確認してください。特に、音声コールバック経路のリアルタイム安全性と、Rust/C# 間の ABI は機能変更と同じくらい重要です。

## 開発環境の準備

Rust のバージョンはリポジトリ直下の [`rust-toolchain.toml`](./rust-toolchain.toml) で固定しています。現在は Rust 1.98.0 です。macOS/iOS のビルドには Xcode、Android のビルドと Unity サンプルには Unity Hub の Unity 6000.4.1f1 と Android Build Support（NDK を含む）が必要です。

```sh
make setup
```

`make setup` は Rust ターゲット、`cargo-ndk`、`cargo-deny`、`ANDROID_NDK_HOME` を確認します。Unity を使うターゲットは、同一マシン上の Unity 起動を直列化する仕組みを内蔵しています。

## よく使う検査

変更前後に、少なくとも次を実行してください。

```sh
make test
make lint
```

- `make test` は `cargo test --workspace` を実行します。`mw-core` のオフラインレンダリング、音声コールバック相当経路、FFI 統合テストを含みます。
- `make lint` は `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo deny check`、第三者ライセンス表記の鮮度検査、C# ラッパのコンパイル検査を実行します。
- `make csharp-check` は .NET 8 以降で、通常経路と iOS の `__Internal` 経路を Unity 無しでコンパイルします。
- `make doc` は `make doc-coverage` を先に実行したうえで rustdoc を生成します。C ABI の公開関数に説明を追加・変更した場合は `make doc-coverage` も確認してください。
- Unity サンプルを変更した場合は `make unity-test` を実行してください。Unity の起動はリポジトリのロックラッパーを経由します。

### カバレッジ

CI と同じ計測をローカルで行うには、`cargo-llvm-cov` と Rust の `llvm-tools-preview` を用意してから次を実行します。

```sh
cargo llvm-cov --workspace --lcov --output-path lcov.info
```

Codecov の設定は [`codecov.yml`](./codecov.yml) が正です。PR の変更行（patch）の基準値は 70%、プロジェクト全体は `auto` を目標にし、1% の下振れを許容します。実デバイス専用経路などの除外も同ファイルに定義しています。数字を上げるためだけの薄いテストは追加せず、動作や回帰を意味のある形で固定してください。

## リアルタイム安全性

音声コールバック経路、すなわち `mw-core::Renderer::render` とそこから呼ばれる全コードでは、次を守ってください。

- mutex / rwlock の取得や、チャネルからのブロッキング受信を行わない
- ヒープのアロケーション・デアロケーションを行わない。`Vec::push` による暗黙の再確保も含む
- ファイル・ネットワーク I/O、一般的なシステムコール、`println!` 系を行わない
- `unwrap`、`expect`、添字アクセスなどのパニック経路を作らない

コマンド、イベント、PCM の受け渡しは固定容量のキューやロックフリーの仕組みを使い、重い処理は音声スレッドの外へ置きます。詳細な不変条件と検証方法は、[`docs/spec/architecture.md の §5.2〜§5.4`](./docs/spec/architecture.md)、[`crates/mw-core/CLAUDE.md`](./crates/mw-core/CLAUDE.md)、および [`crates/mw-core/tests/realtime_safety.rs`](./crates/mw-core/tests/realtime_safety.rs)を参照してください。

## FFI と ABI

Rust/C# 境界を変更する場合は、次の原則を守ってください。

- FFI 関数はスレッドセーフ、非ブロッキング、エラーコード返却を基本とし、Rust の panic を境界の外へ漏らさない
- 生ポインターは null を検査し、毎フレーム呼ぶ経路では GC アロケーションを発生させない
- C ABI の列挙型を引数に直接使わず、境界では検証可能な整数を使う
- `mw_` プレフィックス、blittable なデータ、呼び出し側バッファの規約を維持する
- `NativeMethods.g.cs` は生成物なので手編集・コミットしない。シグネチャや公開関数の doc コメントを変更したら `make bindgen` を実行する

ABI の確認は二つの層で行います。`make csharp-check` / [`tools/csharp-abi-check/README.md`](./tools/csharp-abi-check/README.md) が C# のフィールド名・型・シグネチャと iOS 分岐を確認し、[`crates/mw-ffi/src/csharp_abi_sync.rs`](./crates/mw-ffi/src/csharp_abi_sync.rs) のテストが列挙値、オフセット、サイズ、アライメント、ABI バージョンを固定します。ABI を変更する PR では、互換性への影響と更新内容を本文に明記してください。FFI の詳細は [`crates/mw-ffi/CLAUDE.md`](./crates/mw-ffi/CLAUDE.md)を参照してください。

## コミットメッセージ

履歴に合わせて Conventional Commits 形式を使ってください。

```text
<種類>(<対象>): <短い要約>
```

使われている主な種類は `feat`、`fix`、`perf`、`refactor`、`test`、`docs`、`ci`、`chore` です。対象は必要に応じて付けます（例: `test(ffi): ABI の同期検査を追加`）。互換性を壊す変更では、既存履歴にある `!` の形式（例: `chore!:`）を使い、本文にも影響を説明してください。

## Issue と PR

再現可能な不具合はバグ報告フォーム、提案は機能リクエストフォームを使ってください。脆弱性は公開 Issue に書かず、[`SECURITY.md`](./SECURITY.md) の手順に従ってください。行動規範については [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md)を確認してください。

PR は `main` 向けに作成し、変更内容、必要な背景、検証結果、ABI への影響、ドキュメントへの影響を説明してください。作業ブランチは `feature/<フェーズ>-<内容>` の形式を基本とします。メンテナーがレビューし、CI が通った変更をマージします。

PR を出す前に、[`pull_request_template.md`](./.github/pull_request_template.md) のチェック項目を確認してください。CI では次を検査します。

- `lint-test`: fmt、clippy（警告をエラー扱い）、cargo-deny、`cargo llvm-cov` によるテストとカバレッジ、C# ラッパの通常/iOS 分岐
- `macos-build`: macOS の dylib と iOS の xcframework のビルド
- `android-build`: `arm64-v8a` の Android `.so` のリンクまでのビルド
- `gitleaks`: Git 履歴全体の秘密情報検査

Unity の EditMode テストは CI のジョブには含まれないため、Unity サンプルを変更した場合はローカルで `make unity-test` を実行し、結果を PR に記載してください。
