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
クレートまで拾う。** ここでは配布対象の iOS / Android / macOS の3ターゲットを個別に
解決し、その和集合を使う。
"""

from __future__ import annotations

import argparse
import json
import re
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
_NO_NOTICE_LICENSES = {"0BSD", "CC0-1.0", "MIT-0", "Unlicense", "Zlib"}
_TARGET_TRIPLES = (
    "aarch64-apple-ios",
    "aarch64-linux-android",
    "aarch64-apple-darwin",
)
_PACKAGE_NAME = "mw-ffi"


def _run_cargo_metadata(target: str) -> dict:
    out = subprocess.run(
        [
            "cargo",
            "metadata",
            "--format-version",
            "1",
            "--all-features",
            "--filter-platform",
            target,
        ],
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


def _is_proc_macro(package: dict) -> bool:
    return any("proc-macro" in target.get("kind", []) for target in package["targets"])


def _edge_context(dep: dict, context: str) -> str | None:
    kinds = {kind.get("kind") for kind in (dep.get("dep_kinds") or [{"kind": None}])}
    if not kinds & {None, "normal", "build"}:
        return None
    if context == "proc-macro":
        return "proc-macro"
    if context == "build":
        if kinds & {None, "normal", "build"}:
            return "build"
        return None
    if kinds & {None, "normal"}:
        return "normal"
    if "build" in kinds:
        return "build"
    return None


def _classify_target(meta: dict) -> tuple[set[str], set[str], set[str]]:
    """Return normal, proc-macro, and build-only candidate package ids for a target."""
    packages = {package["id"]: package for package in meta["packages"]}
    nodes = {node["id"]: node for node in meta["resolve"]["nodes"]}
    roots = [package["id"] for package in meta["packages"] if package["name"] == _PACKAGE_NAME]
    candidates = {"normal": set(), "proc-macro": set(), "build": set()}
    visited: set[tuple[str, str]] = set()

    def visit(package_id: str, context: str) -> None:
        key = (package_id, context)
        if key in visited:
            return
        visited.add(key)
        package = packages.get(package_id)
        if package is None:
            return
        if _is_proc_macro(package):
            context = "proc-macro"
        candidates[context].add(package_id)
        node = nodes.get(package_id)
        if node is None:
            return
        for dep in node.get("deps", []):
            next_context = _edge_context(dep, context)
            if next_context is not None:
                visit(dep["pkg"], next_context)

    for root in roots:
        visit(root, "normal")
    return candidates["normal"], candidates["proc-macro"], candidates["build"]


def _classify_packages(metadata: list[dict]) -> tuple[dict[str, dict], dict[str, set[str]]]:
    """Classify the union of the supported distribution targets."""
    packages: dict[str, dict] = {}
    workspace_ids: set[str] = set()
    normal: set[str] = set()
    proc_macro: set[str] = set()
    build: set[str] = set()
    for meta in metadata:
        packages.update({package["id"]: package for package in meta["packages"]})
        workspace_ids.update(meta["workspace_members"])
        target_normal, target_proc_macro, target_build = _classify_target(meta)
        normal.update(target_normal)
        proc_macro.update(target_proc_macro)
        build.update(target_build)

    normal -= workspace_ids
    proc_macro -= workspace_ids | normal
    build -= workspace_ids | normal | proc_macro
    return packages, {"normal": normal, "proc-macro": proc_macro, "build": build}


def _license_expression_has_no_notice_option(expression: str | None) -> bool:
    """Check whether an SPDX expression has a selectable no-notice OR branch."""
    if not expression:
        return False
    tokens = re.findall(r"\(|\)|AND|OR|WITH|[A-Za-z0-9.+-]+", expression)
    position = 0

    def parse_atom() -> bool:
        nonlocal position
        if position >= len(tokens):
            return False
        if tokens[position] == "(":
            position += 1
            value = parse_or()
            if position < len(tokens) and tokens[position] == ")":
                position += 1
            return value
        value = tokens[position] in _NO_NOTICE_LICENSES
        position += 1
        if position < len(tokens) and tokens[position] == "WITH":
            position += 2
            return False
        return value

    def parse_and() -> bool:
        nonlocal position
        value = parse_atom()
        while position < len(tokens) and tokens[position] == "AND":
            position += 1
            right = parse_atom()
            value = value and right
        return value

    def parse_or() -> bool:
        nonlocal position
        value = parse_and()
        while position < len(tokens) and tokens[position] == "OR":
            position += 1
            right = parse_and()
            value = value or right
        return value

    return parse_or()


def _append_license_details(lines: list[str], packages: list[dict]) -> None:
    for package in packages:
        lines.append(f"### {package['name']} {package['version']}")
        lines.append("")
        lines.append(f"- SPDX: `{package.get('license') or '(記載なし)'}`")
        if package.get("repository"):
            lines.append(f"- リポジトリ: {package['repository']}")
        lines.append("")
        files = _collect_license_files(package["manifest_path"])
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


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--output",
        type=Path,
        default=Path(__file__).resolve().parent.parent / "THIRD-PARTY-LICENSES.md",
    )
    args = parser.parse_args(argv)

    metadata = [_run_cargo_metadata(target) for target in _TARGET_TRIPLES]
    package_by_id, categories = _classify_packages(metadata)
    category_packages = {
        category: sorted(
            (package_by_id[package_id] for package_id in package_ids),
            key=lambda package: (package["name"].lower(), package["version"]),
        )
        for category, package_ids in categories.items()
    }
    shipped = category_packages["normal"]
    no_notice = [
        package
        for package in shipped
        if _license_expression_has_no_notice_option(package.get("license"))
    ]
    needs_attribution = [package for package in shipped if package not in no_notice]
    needs_offer = [
        package
        for package in needs_attribution
        if any(k in (package.get("license") or "") for k in _NEEDS_SOURCE_OFFER)
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

    lines.append("## 表記が要る一覧")
    lines.append("")
    lines.append(
        f"配布対象3ターゲットの実行時依存 **{len(needs_attribution)} 件**。"
        "この一覧を利用者向けのライセンス表示に載せる。"
    )
    lines.append("")
    lines.append("`表記 + ソース入手告知` は、表記に加えて**ソースコードの入手方法を利用者へ知らせる**。")
    lines.append("いずれも**改変しなければ**、上流の公開リポジトリを案内するだけでよい。")
    lines.append("")
    lines.append("| クレート | バージョン | ライセンス | 区分 | ソース |")
    lines.append("|---|---|---|---|---|")
    for package in needs_attribution:
        source_offer = "表記 + ソース入手告知" if package in needs_offer else "表記"
        repo = package.get("repository") or "(リポジトリ URL の記載なし)"
        lines.append(
            f"| `{package['name']}` | {package['version']} | {package.get('license') or '(記載なし)'} "
            f"| {source_offer} | {repo} |"
        )
    lines.append("")

    lines.append("## 表記不要(参考)")
    lines.append("")
    lines.append(
        f"配布対象に入るが、選択可能な SPDX ライセンスが "
        f"{', '.join(sorted(_NO_NOTICE_LICENSES))} のクレート **{len(no_notice)} 件**。"
        "バイナリ配布時の著作権表示には載せなくてよい扱いだが、ライセンス本文を参考として収録する。"
    )
    lines.append("")
    lines.append("| クレート | バージョン | ライセンス |")
    lines.append("|---|---|---|")
    for package in no_notice:
        lines.append(f"| `{package['name']}` | {package['version']} | {package.get('license') or '(記載なし)'} |")
    lines.append("")

    lines.append("## 配布物に入らないもの")
    lines.append("")
    lines.append(
        f"proc-macro とその依存ヘルパ **{len(category_packages['proc-macro'])} 件**、"
        f"build-dependencies のみから到達するクレート **{len(category_packages['build'])} 件**。"
        "コンパイル時にだけ使われ、配布バイナリには入らないため、利用者向け表示の対象外。"
    )
    lines.append("")
    for title, category in (
        ("proc-macro とその依存ヘルパ", "proc-macro"),
        ("build-dependencies のみから到達", "build"),
    ):
        lines.append(f"### {title}")
        lines.append("")
        lines.append("| クレート | バージョン | ライセンス |")
        lines.append("|---|---|---|")
        for package in category_packages[category]:
            lines.append(f"| `{package['name']}` | {package['version']} | {package.get('license') or '(記載なし)'} |")
        lines.append("")

    lines.append("## 一覧")
    lines.append("")
    lines.append(
        f"配布対象3ターゲットで解決される依存クレート **{len(shipped) + len(category_packages['proc-macro']) + len(category_packages['build'])} 件**。"
        f"内訳は、実行時 {len(shipped)} 件 / proc-macro {len(category_packages['proc-macro'])} 件 / build-only {len(category_packages['build'])} 件。"
    )
    lines.append("")
    lines.append("表記対象と参考欄の詳細は、下のライセンス全文に収録する。")
    lines.append("")

    lines.append("## ライセンス全文")
    lines.append("")
    lines.append("### 配布物に入る依存クレート")
    lines.append("")
    _append_license_details(lines, shipped)
    lines.append("### 配布物に入らない依存クレート")
    lines.append("")
    _append_license_details(lines, category_packages["proc-macro"] + category_packages["build"])

    out_path = args.output
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(
        f"generated: {out_path} ({len(needs_attribution)} need attribution, "
        f"{len(no_notice)} no notice, {len(category_packages['proc-macro'])} proc-macro, "
        f"{len(category_packages['build'])} build-only)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
