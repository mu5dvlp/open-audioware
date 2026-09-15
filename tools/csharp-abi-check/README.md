# C# ラッパのコンパイル検査(P3-8)

**`unity/Runtime/MwNative.cs` と csbindgen 生成物を、Unity 無しでコンパイルするだけ**の
最小プロジェクト。`make csharp-check`(および CI の `lint-test` ジョブ)から呼ばれる。

## 🔴 なぜ要るか

`MwNative.cs` は**手書き**で、生成された blittable 構造体のフィールドを名指しで詰め替えている
(`native.song_frames` 等)。Rust 側でフィールド名・型・関数シグネチャが変わると、
**壊れるのは C# のコンパイルだけ** —— Rust 側の `cargo test` は全部緑のまま通る。

そして **Unity のテストは CI で回さない方針**(ルートの `CLAUDE.md`「コーヒー基準」)なので、
2026-09-15 まで **この C# は CI で一度もコンパイルされていなかった。**
ズレに気付くのは、利用側 Unity プロジェクトが IL2CPP ビルドを回したとき —— つまり
**テンプレート利用者の手元で初めて壊れる。**

⚠️ `MwNative.cs` は Unity 型に一切依存していない(`using` は
`System.Runtime.InteropServices` と生成名前空間の2つだけ)。だから Unity 無しで
コンパイルできる。**この性質は意図して維持すること** —— `UnityEngine` を1つ使った
瞬間にこの検査は成立しなくなる。

## 🔴 構造体レイアウトのほうは Rust 側で見ている

**このプロジェクトが見るのは「名前と型が合っているか」だけで、バイトのレイアウトは見ていない。**
レイアウト(オフセット・サイズ)は `crates/mw-ffi/src/csharp_abi_sync.rs` の
`offset_of!` assert が固定している。**両方無いと P3-8 は埋まらない**:

| ズレ方 | 捕まえるのはどちら |
|---|---|
| フィールド名が変わった / 消えた / 型が変わった | **こちら**(`dotnet build` が落ちる) |
| フィールドの**順番**が変わった / パディングが動いた | `csharp_abi_sync.rs` の `offset_of!` |
| enum の判別子がズレた | `csharp_abi_sync.rs` の既存テスト |

## 使い方

```sh
make csharp-check
```

⚠️ **`cargo build` を先に回す必要がある** —— `unity/Runtime/Generated/NativeMethods.g.cs` は
`crates/mw-ffi/build.rs` が生成する成果物で、リポジトリには入っていない(`.gitignore`)。
`make csharp-check` はそれを自分で確かめる。

📌 **iOS の `__Internal` 経路も別途コンパイルする。** 生成物には
`#if UNITY_IOS && !UNITY_EDITOR` の分岐があり、既定のビルドでは `#else` 側しか通らない。
`build.rs` のコメントにあるとおり、この分岐は過去に実際の iOS リンク失敗
(`Undefined symbol: _JNI_OnLoad`)を生んだ箇所なので、両方の枝をコンパイルする。
