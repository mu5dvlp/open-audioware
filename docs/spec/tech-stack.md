# 技術スタック

> **この文書は仕様の一部**(旧 `init.md` の §7)。
> 全体の索引と凡例は [`README.md`](README.md)。
>
> 🔴 **コードからは「初期構築仕様『§n 話題名』」の形で参照する**(ファイル名に依存しない)。
> **§番号は移管前と同じ**なので、既存の参照はそのまま有効。


## 7. 技術スタック

### 7.1 Rust ツールチェーン・主要クレート 【仮】

| 項目 | 選定 | 用途・備考 |
|---|---|---|
| Rust | stable(最新)、edition 2024 | MSRV は「最新 stable」で開始し、公開時に固定を検討 |
| cpal | 出力バックエンド | M6。全ターゲットを1系統で |
| symphonia | デコード(wav / ogg vorbis) | M7 |
| rubato | リサンプリング | §4.7 |
| rtrb | SPSC ロックフリーリングバッファ | コマンド / イベント / PCM 供給 |
| csbindgen | C# バインディング生成 | M2。IL2CPP 実績あり |
| cargo-ndk | Android ビルド | .so 生成 |
| criterion | ベンチマーク(任意) | ミキサのブロック処理性能の回帰検知 |

- lint / 整形: `cargo fmt` + `cargo clippy -D warnings`
- **cargo-deny** でライセンスと脆弱性アドバイザリを CI 検査する
  (テンプレートが権利的クリーンさを重視するのと同じ思想。同梱物のライセンスを機械的に保証)
- unsafe は FFI 境界(mw-ffi)と明示的に正当化できる箇所のみ。`#![deny(unsafe_op_in_unsafe_fn)]`

### 7.2 リポジトリ構成 【仮】

```
open-audioware/
  crates/
    mw-core/
    mw-backend/
    mw-ffi/
  unity/                  ← UPM パッケージ(バイナリ、生成バインディング、Mw.Native ラッパ)
  unity-sample/           ← 統合検証用の最小 Unity プロジェクト(unity/ をローカル参照)
  tools/                  ← パッケージ組み立て・バインディング生成の補助スクリプト
  docs/
  Makefile
  Cargo.toml              ← workspace
```

### 7.3 Makefile 【確定】

```
make setup            # ツールチェーン・ターゲット・cargo-ndk 等の導入確認
make lint / format    # fmt + clippy + cargo-deny / 自動整形
make test             # cargo test(mw-core のオフラインレンダリングが主)
make bench            # criterion(任意)
make bindgen          # csbindgen で C# バインディング生成
make build-macos      # .dylib(Unity Editor 用)
make build-ios        # .a → xcframework
make build-android    # .so(cargo-ndk)
make package          # UPM パッケージ組み立て(バイナリ + バインディング + ラッパ)
```

---
