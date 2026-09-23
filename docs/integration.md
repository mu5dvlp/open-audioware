# 統合手順 —— 導出プロジェクトへの導入

マイルストーン M5(ハードニング)の成果物。**この文書だけを読んで導入できる**ことを目標に書いてある。
実装方針・決定事項の唯一の正は初期構築仕様、各クレートの設計は
`crates/*/CLAUDE.md`、作業の経緯は [`history.md`](history.md)。

🔴 **前提として押さえておくべき性質が2つある。**

| 性質 | 何を意味するか |
|---|---|
| **ネイティブバイナリはリポジトリに入っていない**(`.gitignore`。追跡されるのは `.meta` だけ) | `git clone` しただけでは**音が鳴らない**。プラットフォームごとにビルドする手順が必ず要る |
| **導入は任意**(未導入でもゲームは動く) | 利用側は「入っていれば使う / 無ければ Unity 既定実装」を**コンパイル時に**切り替える。切替の仕組みは下の「4. 未導入構成との共存」 |

---

## 1. 何を導入するのか

`unity/` ディレクトリ**そのもの**が UPM パッケージ(`com.mu5dvlp.open-audioware`)。
別途アーカイブを作る必要はなく、`file:` 参照で読み込む。

```
open-audioware/
  crates/            ← Rust 実装(mw-core / mw-backend / mw-ffi)
  unity/             ← 🔴 これが UPM パッケージ
    package.json
    Runtime/
      Generated/NativeMethods.g.cs   ← csbindgen が生成(コミットしない)
      Plugins/macOS/libmw_ffi.dylib  ← ビルド成果物(コミットしない)
      Plugins/iOS/…xcframework       ← 同上
      Plugins/Android/libmw_ffi.so   ← 同上
```

⚠️ **`Generated/` と `Plugins/` は生成物**なので、`git clone` 直後は存在しない。
`make package` がこの2つの存在を確認してくれる(下記)。

---

## 2. セットアップ(初回のみ)

```
make setup      # Rust ツールチェイン・ターゲット・cargo-ndk 等の導入
make lint       # 動く状態か確認(clippy)
make test       # cargo test --workspace
```

⚠️ **Android を焼くなら `cargo ndk` と NDK が必要**(`make setup` が面倒を見る範囲は
`Makefile` の `setup` ターゲットを読むこと。環境依存なので、ここに手順を写して二重管理しない)。

---

## 3. ビルドと配置

**プラットフォームごとに1コマンド。** 出力先は `unity/Runtime/Plugins/<platform>/` で、
Unity 側が `.meta` で対象プラットフォームを既に絞ってある。

| コマンド | 出力 | 用途 |
|---|---|---|
| `make bindgen` | `unity/Runtime/Generated/NativeMethods.g.cs` | C# バインディング。**関数シグネチャはターゲットに依存しない**ので、ホストビルド1回で足りる |
| `make build-macos` | `libmw_ffi.dylib` | **Unity Editor で鳴らすため**。ホストアーチでビルドする(Intel / Apple Silicon どちらでも `--target` を明示しない) |
| `make build-ios` | `libmw_ffi.xcframework` | `aarch64-apple-ios` の静的ライブラリを `xcodebuild -create-xcframework` で包む |
| `make build-android` | `libmw_ffi.so` | `cargo ndk`(ABI / API レベルは Makefile の変数) |
| `make package` | (検証のみ) | 必須ファイルが揃っているかを確認する。揃っていなければ何を先に実行すべきかを表示する |

🔴 **Rust を直したら、使う側のプラットフォーム向けに焼き直すまで一切届かない。**
焼いたあとは `nm` / `strings` でバイナリに入ったところまで確認する ——
⚠️ ただし**エクスポートシンボルが増減しない内部修正は `nm` / `strings` では新旧を見分けられない**。
その場合の確認手段は実機での聴感だけになる(検証が弱いことを承知で受け入れる)。
📌 **挙動を変えないリファクタは焼き直し不要。** 判断の根拠は
`NativeMethods.g.cs` を前後で diff して同一かどうか。

---

## 4. Unity プロジェクトへの参照と、未導入構成との共存

### 4-1. 参照する

`Packages/manifest.json` に1行:

```json
"com.mu5dvlp.open-audioware": "file:../../open-audioware/unity"
```

⚠️ **相対パスはプロジェクトの `Packages/` からの相対**。上の例は
「ワークスペースに client と open-audioware が並んでいる」配置のもの。

### 4-2. 🔴 導入の有無をコンパイル時に切り替える(ここが設計の要)

利用側は **asmdef の `versionDefines`** でシンボルを立て、
**`defineConstraints`** でアセンブリごと消す。テンプレート(client)側の実例:

```json
{
  "name": "App.Audio.Native",
  "references": ["App.Domain", "App.Infrastructure", "Mw.Native", "…"],
  "defineConstraints": ["MG_AUDIO_MIDDLEWARE"],
  "versionDefines": [
    { "name": "com.mu5dvlp.open-audioware", "expression": "0.1.0", "define": "MG_AUDIO_MIDDLEWARE" }
  ]
}
```

**こうしておくと何が起きるか。**

| パッケージ | `MG_AUDIO_MIDDLEWARE` | 結果 |
|---|---|---|
| 参照あり | 立つ | `App.Audio.Native` がコンパイルされ、ネイティブ実装が DI に載る |
| 参照なし | 落ちる | **アセンブリごと存在しなくなり**、Unity 既定実装へ自動的に戻る |

🔴 **`#if` をあちこちに書かないこと。** アセンブリ境界で丸ごと切り替えるので、
ネイティブ実装を参照するコードは `App.Audio.Native` の中だけに閉じる。
📌 これは「ミドルウェアを入れない導出プロジェクトが**何も消さずに**そのまま動く」ための構造で、
導入を任意にしている理由そのもの。

### 4-3. 未導入構成が壊れていないことを確認する

**片方の構成しか回さないと、もう片方だけで壊れる配線に気付けない**(実際に踏んでいる)。
テンプレート側には一時的にパッケージ参照を外して回す仕組みがある:

```
make test-no-middleware            # EditMode
make test-no-middleware-playmode   # PlayMode(任意・スモーク)
```

⚠️ 実行後は成功・失敗・Ctrl-C いずれでも `manifest.json` / `packages-lock.json` を
必ず元に戻す(スクリプト側が `trap` で保証している)。

---

## 5. 動作確認

### 5-1. サンプルプロジェクトで確認する

```
make unity-sample-create   # 空の Unity プロジェクトを作る(初回のみ)
make unity-test            # EditMode テストを回す
```

⚠️ `unity-sample-create` の直後に **`Packages/manifest.json` への追記が手作業で要る**
(コマンドが必要な行を表示する)。

### 5-2. 🔴 Unity 起動は必ず直列化する

同一マシンで動く他の作業もクライアントプロジェクト側で Unity を使うため、
**いかなる Unity 起動でも**(`-createProject` / `-runTests` を含む)
`tools/with-unity-lock.sh` 経由でロック(`/tmp/mgct-unity.lock`)を取る。
`make unity-sample-create` / `make unity-test` は既に内包している。

---

## 6. API リファレンス

**ハンドコピーした一覧はここに置かない**(必ず実装から乖離するため)。正は次の2つ:

| 何を見たいか | どこ |
|---|---|
| **C ABI**(31 関数。`mw_init` / `mw_se_play` / `mw_music_*` / `mw_bgm_*` / `mw_bus_*` / `mw_poll_events` 等) | `make doc` で生成される rustdoc。**全エクスポート関数に doc コメントがある**ことは `make doc-coverage` が機械で確認する |
| **C# から見える形** | `unity/Runtime/Generated/NativeMethods.g.cs`(csbindgen 生成)。`make bindgen` で作る |

境界の約束(エラーモデル・ハンドル管理・所有権)は
[`crates/mw-ffi/CLAUDE.md`](../crates/mw-ffi/CLAUDE.md) が正。
⚠️ **音声スレッドでの禁止事項(リアルタイム安全性)**は
[`crates/mw-core/CLAUDE.md`](../crates/mw-core/CLAUDE.md) に常設 —— 拡張する前に必ず読むこと。

---

## 7. まだ埋まっていないもの(M5 の残り)

| 項目 | 状態 |
|---|---|
| 実機プロファイル(CPU / 電力) | ⬜ **実機が要る**。M1 の A/B 計測と同じ方式で測る([`measurement-m1.md`](measurement-m1.md)) |
| 長時間試験 | ⬜ 実機で放置して確認する類のもの |
| UPM パッケージ名 / 組織 ID | ✅ **決定**(MU7。ユーザー判断 2026-09-23「Open Audioware か Open Source Audio Middleware になっていれば大丈夫」)—— `displayName` は **Open Source Audio Middleware**、`name` は `com.mu5dvlp.open-audioware` のまま(load-bearing なのは `name` のほう)|
| リリース自動化 | ⬜ 未着手。`make package` は現状「必須ファイルの存在確認」まで |
| 設計 ADR | ⬜ 未着手。⚠️ **初期構築仕様と各クレートの `CLAUDE.md` に既に書かれている決定を ADR へ写すと、正が2つになる。** 何を ADR に切り出すかを決めてから作ること |
