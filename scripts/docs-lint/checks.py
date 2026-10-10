"""このプロジェクト独自の docs 規約を確かめる。

Python 標準ライブラリだけで書く(他リポジトリへそのままコピーして使うため)。
規約そのものは `config.json` が持ち、ここにはロジックだけを置く。
新しい規約をここで作らない —— COMMON.md / docs/handoff/*.md / .claude/skills に
既にある、機械で確かめられる決まりだけを拾う。
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


@dataclass(frozen=True)
class Violation:
    """1件の違反。file は repo_root からの相対パス(posix区切り)。"""

    file: str
    line: int | None
    rule_id: str
    message: str

    def format(self) -> str:
        loc = f"{self.file}:{self.line}" if self.line is not None else self.file
        return f"{loc} {self.rule_id} {self.message}"


def _rel(repo_root: Path, path: Path) -> str:
    return path.relative_to(repo_root).as_posix()


def _is_excluded(rel_posix: str, exclude_dirs: Iterable[str]) -> bool:
    """rel_posix(ファイルかディレクトリの相対パス)が exclude_dirs のどれかの下にあるか。"""
    for ex in exclude_dirs:
        ex = ex.strip("/")
        if rel_posix == ex or rel_posix.startswith(ex + "/"):
            return True
    return False


def iter_markdown_files(repo_root: Path, global_exclude_dirs: Iterable[str]) -> list[Path]:
    """repo_root 以下の *.md を、除外ディレクトリを刈りながら列挙する。"""
    results: list[Path] = []
    exclude = list(global_exclude_dirs)
    for dirpath, dirnames, filenames in os.walk(repo_root):
        dirpath_p = Path(dirpath)
        rel_dir = dirpath_p.relative_to(repo_root).as_posix()
        if rel_dir == ".":
            rel_dir = ""
        # 除外ディレクトリはその場で descend しない(大きな生成物を踏まないため)
        pruned = []
        for d in dirnames:
            child_rel = f"{rel_dir}/{d}" if rel_dir else d
            if _is_excluded(child_rel, exclude) or d.startswith("."):
                continue
            pruned.append(d)
        dirnames[:] = pruned
        for fn in filenames:
            if fn.lower().endswith(".md"):
                results.append(dirpath_p / fn)
    return results


def check_root_md_whitelist(repo_root: Path, cfg: dict) -> list[Violation]:
    """repo_root 直下に置いてよい .md ファイルの許可リスト。"""
    rule = cfg.get("root_md_whitelist")
    if not rule:
        return []
    allowed = set(rule["allowed"])
    violations = []
    for entry in sorted(repo_root.iterdir()):
        if entry.is_file() and entry.name.lower().endswith(".md") and entry.name not in allowed:
            violations.append(
                Violation(
                    file=entry.name,
                    line=None,
                    rule_id="root-md-whitelist",
                    message=(
                        f"ルート直下に置けるのは {sorted(allowed)} だけ"
                        f"(COMMON.md『ドキュメントを増やすときの置き場所』)"
                    ),
                )
            )
    return violations


def check_subdir_md_whitelist(repo_root: Path, cfg: dict) -> list[Violation]:
    """指定ディレクトリの直下(サブディレクトリは対象外)に置いてよい .md の許可リスト。"""
    violations = []
    for rule in cfg.get("subdir_md_whitelist", []):
        target_dir = repo_root / rule["dir"]
        if not target_dir.is_dir():
            continue
        allowed = set(rule["allowed"])
        for entry in sorted(target_dir.iterdir()):
            if entry.is_file() and entry.name.lower().endswith(".md") and entry.name not in allowed:
                violations.append(
                    Violation(
                        file=_rel(repo_root, entry),
                        line=None,
                        rule_id="subdir-md-whitelist",
                        message=(
                            f"{rule['dir']}/ 直下に置けるのは {sorted(allowed)} だけ"
                        ),
                    )
                )
    return violations


def check_filename_patterns(repo_root: Path, cfg: dict) -> list[Violation]:
    """特定ディレクトリに置くファイルの名前の形。"""
    violations = []
    for rule in cfg.get("filename_patterns", []):
        target_dir = repo_root / rule["dir"]
        if not target_dir.is_dir():
            continue
        pattern = re.compile(rule["pattern"])
        allow_extra = set(rule.get("allow_extra", []))
        for entry in sorted(target_dir.iterdir()):
            if not (entry.is_file() and entry.name.lower().endswith(".md")):
                continue
            if entry.name in allow_extra:
                continue
            if not pattern.match(entry.name):
                violations.append(
                    Violation(
                        file=_rel(repo_root, entry),
                        line=None,
                        rule_id="filename-pattern",
                        message=f"{rule['dir']}/ のファイル名は {rule['pattern']!r} に合わせる",
                    )
                )
    return violations


def check_max_lines(repo_root: Path, cfg: dict) -> list[Violation]:
    violations = []
    for rule in cfg.get("max_lines", []):
        target = repo_root / rule["file"]
        if not target.is_file():
            continue
        with target.open(encoding="utf-8") as fh:
            n = sum(1 for _ in fh)
        if n > rule["max"]:
            violations.append(
                Violation(
                    file=rule["file"],
                    line=n,
                    rule_id="max-lines",
                    message=f"{n} 行({rule['max']} 行以内のはず)",
                )
            )
    return violations


def check_pointer_at_top(repo_root: Path, cfg: dict) -> list[Violation]:
    """CLAUDE.md / AGENTS.md が、先頭の数行で参照先(例: COMMON.md)を指しているか。"""
    violations = []
    for rule in cfg.get("pointer_at_top", []):
        requires = rule.get("requires_file")
        if requires and not (repo_root / requires).is_file():
            # この階層には参照先が無い(例: 子リポジトリに COMMON.md が無い)。
            # その場合はこの規約自体が当てはまらないので何もしない。
            continue
        target = repo_root / rule["file"]
        if not target.is_file():
            continue
        within = rule.get("within_lines", 10)
        with target.open(encoding="utf-8") as fh:
            head = "".join(fh.readlines()[:within])
        if rule["must_contain"] not in head:
            violations.append(
                Violation(
                    file=rule["file"],
                    line=within,
                    rule_id="pointer-at-top",
                    message=(
                        f"先頭 {within} 行以内に {rule['must_contain']!r} への言及が無い"
                    ),
                )
            )
    return violations


def check_forbidden_patterns(repo_root: Path, cfg: dict) -> list[Violation]:
    """禁止の語句・参照の形(正規表現)。"""
    violations = []
    rules = cfg.get("forbidden_patterns", [])
    if not rules:
        return []
    global_exclude = cfg.get("global_exclude_dirs", [])
    for md_path in iter_markdown_files(repo_root, global_exclude):
        rel = _rel(repo_root, md_path)
        try:
            text = md_path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        lines = text.splitlines()
        for rule in rules:
            if _is_excluded(rel, rule.get("exclude_dirs", [])):
                continue
            pattern = re.compile(rule["regex"])
            for i, line in enumerate(lines, start=1):
                if pattern.search(line):
                    violations.append(
                        Violation(
                            file=rel,
                            line=i,
                            rule_id=rule["id"],
                            message=rule["message"],
                        )
                    )
    return violations


ALL_CHECKS = [
    check_root_md_whitelist,
    check_subdir_md_whitelist,
    check_filename_patterns,
    check_max_lines,
    check_pointer_at_top,
    check_forbidden_patterns,
]


def run_all(repo_root: Path, cfg: dict) -> list[Violation]:
    violations: list[Violation] = []
    for check in ALL_CHECKS:
        violations.extend(check(repo_root, cfg))
    return violations
