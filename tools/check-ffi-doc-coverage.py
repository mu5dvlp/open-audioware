#!/usr/bin/env python3
"""C ABI としてエクスポートしている全関数に doc コメントがあることを検査する(M5)。

なぜ機械で見張るか:
  API リファレンスは **rustdoc(`make doc`)を正**にしてある(手書きの一覧は必ず実装から
  乖離するため。docs/integration.md §6)。その前提が成り立つのは
  「エクスポート関数に doc コメントが必ずある」ときだけで、
  1つ欠けると **リファレンスにその関数が説明なしで並ぶ** ことになる。

判定:
  `pub extern "C" fn` / `pub unsafe extern "C" fn` の直前(属性行・空行は飛ばす)に
  `///` が1行でもあれば「記載あり」とする。
"""
from __future__ import annotations

import glob
import io
import os
import re
import sys

FN = re.compile(r'pub\s+(?:unsafe\s+)?extern\s+"C"\s+fn\s+(\w+)')


def undocumented(path: str) -> list[str]:
    lines = io.open(path, encoding="utf-8").read().split("\n")
    missing = []
    for i, line in enumerate(lines):
        m = FN.search(line)
        if not m:
            continue
        j = i - 1
        found = False
        while j >= 0:
            stripped = lines[j].strip()
            if stripped.startswith("///"):
                found = True
                break
            # 属性(#[no_mangle] 等)と空行は跨いで遡る。それ以外に当たったら打ち切り。
            if stripped.startswith("#[") or stripped == "":
                j -= 1
                continue
            break
        if not found:
            missing.append(f"{os.path.basename(path)}: {m.group(1)}")
    return missing


def main() -> int:
    root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
    pattern = os.path.join(root, "crates", "mw-ffi", "src", "*.rs")
    files = sorted(glob.glob(pattern))
    if not files:
        print(f"[check-ffi-doc-coverage] 対象が見つかりません: {pattern}", file=sys.stderr)
        return 1

    total = 0
    missing: list[str] = []
    for path in files:
        text = io.open(path, encoding="utf-8").read()
        total += len(FN.findall(text))
        missing.extend(undocumented(path))

    if total == 0:
        # エクスポートが0件になるのは、検出パターンが実装と食い違ったときの典型。
        # 「0/0 で緑」にすると検査が形骸化するので落とす。
        print("[check-ffi-doc-coverage] エクスポート関数が1件も見つかりませんでした。"
              "検出パターンが実装と食い違っている可能性があります。", file=sys.stderr)
        return 1

    if missing:
        print(f"[check-ffi-doc-coverage] doc コメントの無いエクスポート関数が "
              f"{len(missing)} 件あります(全 {total} 件):", file=sys.stderr)
        for name in missing:
            print(f"  - {name}", file=sys.stderr)
        print("API リファレンスは rustdoc を正にしているため、"
              "説明なしの関数が並ぶことになります(docs/integration.md §6)。", file=sys.stderr)
        return 1

    print(f"[check-ffi-doc-coverage] OK: エクスポート関数 {total} 件すべてに doc コメントがあります。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
