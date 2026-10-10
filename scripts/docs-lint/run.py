#!/usr/bin/env python3
"""docs-lint の実行コマンド。

使い方:
    python3 scripts/docs-lint/run.py [REPO_ROOT] [--config CONFIG_JSON]

REPO_ROOT を省略すると、このファイルの2つ上のディレクトリ(ワークスペースのルート)を使う。
--config を省略すると、このファイルと同じディレクトリの config.json を使う。
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import checks  # noqa: E402  (sys.path を通した後に import する)


def main(argv: list[str] | None = None) -> int:
    script_dir = Path(__file__).resolve().parent
    default_repo_root = script_dir.parent.parent  # scripts/docs-lint -> scripts -> ワークスペース

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "repo_root",
        nargs="?",
        default=str(default_repo_root),
        help="検査するリポジトリのルート(既定: ワークスペースのルート)",
    )
    parser.add_argument(
        "--config",
        default=str(script_dir / "config.json"),
        help="規約を書いた JSON ファイル(既定: scripts/docs-lint/config.json)",
    )
    args = parser.parse_args(argv)

    repo_root = Path(args.repo_root).resolve()
    config_path = Path(args.config).resolve()

    with config_path.open(encoding="utf-8") as fh:
        cfg = json.load(fh)

    violations = checks.run_all(repo_root, cfg)
    violations.sort(key=lambda v: (v.file, v.line or 0, v.rule_id))

    for v in violations:
        print(v.format())

    print(f"docs-lint: {len(violations)} 件の違反({repo_root})")
    return 1 if violations else 0


if __name__ == "__main__":
    raise SystemExit(main())
