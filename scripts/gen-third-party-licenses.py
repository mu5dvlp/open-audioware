#!/usr/bin/env python3
"""依存クレートのライセンス表記(THIRD-PARTY-LICENSES.md)を生成する。

# なぜ必要か

open-audioware 自身は MIT-0 で、**利用者に著作権表示の保持を求めない**。
しかし依存クレートの義務は消せない —— MIT / Apache-2.0 / BSD 系は表記の保持を求め、
MPL-2.0 はそれに加えて「ソース入手方法の告知」を求める。
利用者(ゲーム)はこれを自分で調べて集める必要があるが、**それをこちらで生成して同梱すれば
利用者の手間は実質ゼロになる。** それがこのスクリプトの目的。

# 何をするか

`cargo metadata` が返す依存グラフを走査し、各クレートの

- 名前 / バージョン / SPDX ライセンス式 / リポジトリ URL
- ソース(`~/.cargo/registry/src/...`)に置かれている LICENSE ファイルの実物

を集めて 1 枚の Markdown にまとめる。ワークスペース自身のクレートは除外する
(あちらは MIT-0 で、表記義務が無い)。

⚠️ **これは法的助言ではない。** 生成物は「一次情報(各クレートの LICENSE ファイル)を
機械的に集めたもの」であり、公開前に人間が目を通すこと。

# 使い方

    make third-party-licenses

⚠️ **`cargo metadata` はターゲットを絞らないと、そのプラットフォームでしか使わない
クレートまで拾う。** ここでは意図的に**全ターゲット**を対象にしている ——
このミドルウェアは macOS / iOS / Android 向けに配布するので、
**どのプラットフォーム向けバイナリを配っても足りる表記**にしておく必要があるため。
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

# LICENSE 本文とみなすファイル名(大文字小文字を無視して前方一致で判定する)。
_LICENSE_FILE_PREFIXES = ("license", "licence", "copying", "notice", "unlicense")

# 本文を丸ごと載せると数 MB になるため、1 ファイルあたりの上限を設ける。
# 超えた場合は冒頭だけ載せ、リポジトリ URL を案内する。
_MAX_LICENSE_CHARS = 20000

# 「表記だけでは足りない」ライセンス。追加の告知が要るので目立たせる。
_NEEDS_SOURCE_OFFER = ("MPL-", "EPL-", "CDDL", "LGPL", "GPL")


def _run_cargo_metadata() -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features"],
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(out.stdout)


def _collect_license_files(manifest_path: str) -> list[tuple[str, str]]:
    """クレートのソースディレクトリから LICENSE 系ファイルを拾う。"""
    root = Path(manifest_path).parent
    found: list[tuple[str, str]] = []
    if not root.is_dir():
        return found
    for entry in sorted(root.iterdir()):
        if not entry.is_file():
            continue
        if not entry.name.lower().startswith(_LICENSE_FILE_PREFIXES):
            continue
        try:
            text = entry.read_text(encoding="utf-8", errors="replace").strip()
        except OSError:
            continue
        if not text:
            continue
        if len(text) > _MAX_LICENSE_CHARS:
            text = text[:_MAX_LICENSE_CHARS] + "\n\n…(以下省略。全文は上記リポジトリを参照)"
        found.append((entry.name, text))
    return found


def _shipped_package_ids(meta: dict) -> set[str]:
    """配布バイナリに実際に入る依存の id を、依存グラフを辿って集める。

    🔴 **`cargo metadata` のパッケージ一覧をそのまま使わないこと。** あちらには
    **dev-dependencies(テスト・ベンチ用)** も入っており、配布物には含まれない
    クレートまで表記してしまう(嘘ではないが、利用者を混乱させる)。

    ここでは `resolve.nodes` を辿り、**normal 依存だけ**を再帰的に集める。
    build-dependencies も除く —— ビルドスクリプトはコンパイル時に走るだけで、
    そのコードは成果物に入らないため。

    ⚠️ **ターゲット(cfg)では絞らない。** macOS / iOS / Android 向けに配布するので、
    どのプラットフォームのバイナリを配っても足りる表記にしておく必要がある。
    """
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    shipped: set[str] = set()
    stack = list(meta["workspace_members"])
    while stack:
        current = stack.pop()
        node = nodes.get(current)
        if node is None:
            continue
        for dep in node.get("deps", []):
            kinds = dep.get("dep_kinds") or [{"kind": None}]
            if not any(k.get("kind") is None for k in kinds):
                continue  # dev / build 依存だけの辺は辿らない
            if dep["pkg"] in shipped:
                continue
            shipped.add(dep["pkg"])
            stack.append(dep["pkg"])
    return shipped


def main() -> int:
    meta = _run_cargo_metadata()
    workspace_ids = set(meta["workspace_members"])
    shipped = _shipped_package_ids(meta) - workspace_ids

    packages = [p for p in meta["packages"] if p["id"] in shipped]
    packages.sort(key=lambda p: (p["name"].lower(), p["version"]))

    needs_offer = [
        p for p in packages
        if any(k in (p.get("license") or "") for k in _NEEDS_SOURCE_OFFER)
    ]

    lines: list[str] = []
    lines.append("# 第三者ライセンス表記(THIRD-PARTY-LICENSES)")
    lines.append("")
    lines.append("⚠️ **このファイルは `make third-party-licenses` が生成する。手で編集しないこと。**")
    lines.append("")
    lines.append("## これは何か")
    lines.append("")
    lines.append("**open-audioware 自身は MIT-0 で、あなたに著作権表示の保持を求めない。**")
    lines.append("しかし open-audioware が内部で使っているクレートには、それぞれの作者が定めた")
    lines.append("表記義務がある。**この 1 枚をあなたのゲームのライセンス表示画面に載せれば、")
    lines.append("依存ぶんの義務は満たせる**(このファイルをそのまま同梱してよい)。")
    lines.append("")
    lines.append("⚠️ **これは法的助言ではない。** 商用配布の前に、自社の基準で確認すること。")
    lines.append("")

    if needs_offer:
        lines.append("## 🔴 表記だけでは足りないもの(ソースの入手方法も告知する)")
        lines.append("")
        lines.append("次のクレートは、表記に加えて**ソースコードの入手方法を利用者へ知らせる**ことを求める。")
        lines.append("いずれも**改変しなければ**、上流の公開リポジトリを案内するだけでよい。")
        lines.append("(**静的リンクしても、あなた自身のコードを公開する義務は生じない** ——")
        lines.append("MPL-2.0 はファイル単位のコピーレフトで、改変したそのファイルだけが対象。)")
        lines.append("")
        lines.append("| クレート | バージョン | ライセンス | ソース |")
        lines.append("|---|---|---|---|")
        for p in needs_offer:
            repo = p.get("repository") or "(リポジトリ URL の記載なし)"
            lines.append(f"| `{p['name']}` | {p['version']} | {p.get('license')} | {repo} |")
        lines.append("")

    lines.append("## 一覧")
    lines.append("")
    lines.append(f"依存クレート **{len(packages)} 件**(open-audioware 自身のクレートは除く)。")
    lines.append("")
    lines.append("| クレート | バージョン | ライセンス |")
    lines.append("|---|---|---|")
    for p in packages:
        lines.append(f"| `{p['name']}` | {p['version']} | {p.get('license') or '(記載なし)'} |")
    lines.append("")

    lines.append("## ライセンス全文")
    lines.append("")
    for p in packages:
        lines.append(f"### {p['name']} {p['version']}")
        lines.append("")
        lines.append(f"- SPDX: `{p.get('license') or '(記載なし)'}`")
        if p.get("repository"):
            lines.append(f"- リポジトリ: {p['repository']}")
        lines.append("")
        files = _collect_license_files(p["manifest_path"])
        if not files:
            lines.append("> ⚠️ **ソースに LICENSE ファイルが見つからなかった。**")
            lines.append("> 上の SPDX 識別子が示す標準の条文が適用される。")
            lines.append("> 公開前に、上記リポジトリで実物を確認すること。")
            lines.append("")
            continue
        for name, text in files:
            lines.append(f"<details><summary><code>{name}</code></summary>")
            lines.append("")
            lines.append("```")
            lines.append(text)
            lines.append("```")
            lines.append("")
            lines.append("</details>")
            lines.append("")

    out_path = Path(__file__).resolve().parent.parent / "THIRD-PARTY-LICENSES.md"
    out_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"generated: {out_path} ({len(packages)} crates, {len(needs_offer)} need a source offer)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
