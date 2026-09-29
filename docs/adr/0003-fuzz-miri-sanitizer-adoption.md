# 0003. 自前実装の unsafe とパーサに Miri / cargo-fuzz / TSan を順に入れる —— 依存排除で外部クレートの検証を失った分を自分で持つ

## 状態

採用

## 日付

2026-09-29(ユーザー決定)

## 文脈

[ADR-0001](0001-security-framework-adoption.md) は `proptest` / `cargo-fuzz` / `miri` / `loom` が無い空白を
「否定ではなく保留」とした。その後、依存排除計画(ワークスペース `docs/plans/AUDIOWARE-DEPS-PLAN.md`)の
ステップ0・1・3で **rtrb と symphonia を自前実装に置き換えた**。外部クレートのときは上流が fuzz や Miri で
検査していたが、自前のロックフリーリングバッファ(`crates/mw-core/src/ring_buffer.rs`。unsafe のポインタ操作)と
デコーダ(`wav.rs` / `decode.rs`)は、いまこのリポジトリ以外の誰にも叩かれていない。この先ステップ2(rubato)・
4(cpal)でも同じことが起きる。

実測(2026-09-29): unsafe は mw-core 18 / mw-backend 26 / mw-ffi 131(extern "C" 36 本)。
並行性の検証は loom がリングバッファ1本だけ。ツールチェーンは stable 1.98.0 固定で、
fuzz / Miri / サニタイザはすべて nightly が要る。

## 決定

順番は **Miri → cargo-fuzz → TSan**。**ステップ2(rubato の自前化)より前に Miri と fuzz を終える。**

1. **Miri(`make miri`。対象は mw-core だけ)** —— 自前リングバッファの unsafe を未定義動作・データ競合の観点で
   検査する。mw-backend / mw-ffi は cpal(C の音声デバイス)と FFI を呼ぶため Miri では動かせず対象外。
   CI に載せる(コーヒー基準に収まることを実測してから)。
2. **cargo-fuzz を3本** —— `wav::decode` / `decode.rs` の open と pump / `resample` の入力。既存テストの音源を
   シードコーパスにする。**長時間の実行は CI に載せない**(コーヒー基準)。CI にはコーパスの回帰実行だけを載せ、
   長い実行は手元か週1回の定期実行で回す。
3. **TSan(手元専用)** —— RT スレッドと制御スレッドの実行時の競合を見る。loom のモデル検証と補完関係。
   nightly の `-Zsanitizer=thread` は flaky になりやすいので CI には載せない。ASan は純 Rust 部分を Miri が
   見るので入れない。

nightly は **第2のツールチェーンとして日付固定で持つ**(`Makefile` の `MIRI_TOOLCHAIN`)。
stable の固定(`rust-toolchain.toml`)はそのまま。日付を書く場所は Makefile の1箇所だけにし、CI は
`make miri` を呼ぶだけで日付を持たない(`rust-toolchain.toml` と CI で版を二重に持って食い違った事故を
繰り返さないため)。

## 理由

- 依存を自前実装に置き換えるということは、上流の検証(fuzz・Miri・多数の利用者)も一緒に捨てるということ。
  置き換えを進めるほど未検証の unsafe とパーサが増える構造なので、次の置き換え(ステップ2)の前に検査の
  受け皿を作っておく。
- 費用対効果の順に並べた。Miri は既存のテストをそのまま流すだけで unsafe の検査になり、半日で入る。
  fuzz は入力がバンドル済みアセット(攻撃者が握るものではない)なので深刻度は「壊れたアセットで落ちる」止まりだが、
  自前デコーダの境界条件を機械的に洗う手段が他に無い。TSan は補完的で、かつ flaky の管理が要るので最後。
- CI はコーヒー基準(3〜5分)を守る。open-audioware は public で Actions が無料だが、基準は「課金」ではなく
  「待てる長さ」の話なので変えない。

## 影響・トレードオフ

- ADR-0001 の「`proptest` / `cargo-fuzz` / `miri` / `loom` の導入は保留」は本 ADR で改める(`proptest` は引き続き
  未導入。リサンプラやミキサの数値不変条件を固定したくなったら、fuzz の次に検討する)。
- Miri で遅いテスト(反復回数の多いもの)は `#[cfg(miri)]` で回数を減らすか `#[cfg_attr(miri, ignore)]` にする。
  ⚠️ ignore にしたテストは「Miri の対象外」であって「検査済み」ではない。理由をその場に書くこと。
- nightly の日付を上げると Miri の検査が厳しくなって新しく赤が出ることがある。上げるのは手元で `make miri` を
  通してから。

## 追記(決定2の実際の置き場)

cargo-fuzz のターゲット3本は `fuzz/`(ルートの `Cargo.toml` の `[workspace]` から `exclude` した
別ワークスペース)に置いた。`libfuzzer-sys`/`arbitrary` は `fuzz/` 専用の開発時依存で配布物
(mw-core/mw-backend/mw-ffi のビルド成果物)には一切含まれないため、`THIRD-PARTY-LICENSES.md`
の対象外とした。
