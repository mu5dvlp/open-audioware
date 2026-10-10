# docs-graph —— Markdown 文書の依存関係の検査

dependency-cruiser / import-linter の Markdown 版。リンクを依存の辺として見て、
**リンク切れ**・**リポジトリの外へ出るリンク**・**循環**・**禁止の向き**を検査する。
Python 3 標準ライブラリだけで動く(`docs_graph.py` 単体で完結)。

## 使い方

```sh
python3 scripts/docs-graph/docs_graph.py                       # カレントディレクトリを検査
python3 scripts/docs-graph/docs_graph.py --root path/to/repo   # 対象のリポジトリを指定
python3 scripts/docs-graph/docs_graph.py --config other.json   # 設定ファイルを指定
python3 scripts/docs-graph/docs_graph.py --graph mermaid       # 依存グラフを mermaid で出す
python3 scripts/docs-graph/docs_graph.py --graph dot           # 同、Graphviz dot 形式
```

終了コードはエラーが1件でもあれば `1`、無ければ `0`。`--graph` を付けると
グラフの本体は stdout、検査結果の一覧は stderr へ分かれる(グラフだけファイルへ
リダイレクトできるように)。

## 検査する内容

| # | 内容 | 既定の扱い |
|---|---|---|
| 1 | リンク切れ(リポジトリ内を指しているのにファイルが無い) | エラー |
| 2 | リポジトリの外へ出るリンク(`../` でルートを越える) | `allow_outside` に一致しない限りエラー |
| 3 | 循環(強連結成分。サイズ2以上) | `cycle_allow` に載っていない限りエラー |
| 4 | 禁止の向き(`forbidden` の glob に一致する辺) | エラー |

対象にするリンクの書式はインラインリンク `[text](path)`、参照リンクの定義
`[label]: path`、`<a href="path">`。コードブロック(` ``` `)とインラインコード
(`` `...` ``)の中は見ない。`http(s):` / `mailto:` 等スキーム付きのリンクと
`#anchor` だけのリンクは対象外。パスはリンクが書かれているファイルからの相対で
解決する(`#anchor` は落とす)。`/` 始まりはリポジトリ直下からの相対として解決する。

## 設定ファイル(`docs-graph.json`)

既定では `<--root>/docs-graph.json` を読む(`--config` で変更できる)。無ければ
既定値(下の表)だけで検査する。すべての欄は省略可。

```json
{
  "exclude": ["dist", "docs/generated/**"],
  "allow_outside": ["../LICENSE", "../shared-docs/**"],
  "cycle_ignore_sources": ["docs/HANDOFF.md", "**/index.md", "**/*-INDEX.md"],
  "cycle_allow": [
    {"files": ["docs/a.md", "docs/b.md"], "reason": "相互参照が前提の仕様。2026-10-10 許可"}
  ],
  "forbidden": [
    {"name": "no-plan-to-history", "from": "docs/plans/**", "to": "*/history.md",
     "reason": "計画から経緯へは張らない(経緯側が計画を指すのが正しい向き)"}
  ]
}
```

| 欄 | 意味 |
|---|---|
| `exclude` | 検査から除く glob(名前だけなら木のどの深さでも一致。`/` を含めば位置も効く)。**既定の除外一覧に追加される**(上書きではない)。既定: `.git` `node_modules` `Library` `Temp` `target` `Builds` `.claude/worktrees` |
| `allow_outside` | リポジトリの外へのリンクのうち許してよいもの。リンクを書いたファイルから見た相対パス(`../` 始まり)に対する glob |
| `cycle_ignore_sources` | この glob に一致する**リンク元**からの辺は、循環の検査からだけ除く(目次・地図・索引のように「一覧から各文書へ張るだけ」のファイル用。リンク切れ・外部リンクの検査は変わらず効く) |
| `cycle_allow` | 許可する循環のリスト。`files` が循環を成すファイルの集合(順不同)に**完全一致**したら許可(理由は `reason` に残す) |
| `forbidden` | 禁止する辺の一覧。`from` / `to` は glob(リポジトリ直下からの相対パスに対して) |

glob は `fnmatch` 形式(`*` は `/` も含めて何文字にでも一致する。`?` は1文字、`[seq]` は文字クラス)。

## 他のリポジトリへ配る

`docs_graph.py`(と使うなら `test_docs_graph.py`)をそのままコピーしてよい。
ワークスペース固有の前提はコードに無く、すべて配置先の `docs-graph.json` で与える。

## テスト

```sh
python3 -m unittest scripts/docs-graph/test_docs_graph.py
```
