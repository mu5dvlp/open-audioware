"""checks.py のテスト(標準ライブラリの unittest だけを使う)。

どれも一時ディレクトリに最小限のファイルを作り、現在の規約(config の中身)に対して
期待する入力 → 期待する出力だけを確かめる。実リポジトリの docs は触らない。
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

import checks


def write(path: Path, content: str = "本文\n") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


class RootMdWhitelistTest(unittest.TestCase):
    def test_allowed_files_pass(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in ("COMMON.md", "CLAUDE.md", "AGENTS.md", "README.md"):
                write(root / name)
            cfg = {"root_md_whitelist": {"dir": ".", "allowed": ["COMMON.md", "CLAUDE.md", "AGENTS.md", "README.md"]}}
            self.assertEqual(checks.check_root_md_whitelist(root, cfg), [])

    def test_extra_root_file_is_violation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "COMMON.md")
            write(root / "NOTES.md")
            cfg = {"root_md_whitelist": {"dir": ".", "allowed": ["COMMON.md"]}}
            violations = checks.check_root_md_whitelist(root, cfg)
            self.assertEqual([v.file for v in violations], ["NOTES.md"])
            self.assertEqual(violations[0].rule_id, "root-md-whitelist")

    def test_subdirectory_files_are_not_checked(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "COMMON.md")
            write(root / "docs" / "whatever.md")
            cfg = {"root_md_whitelist": {"dir": ".", "allowed": ["COMMON.md"]}}
            self.assertEqual(checks.check_root_md_whitelist(root, cfg), [])


class SubdirMdWhitelistTest(unittest.TestCase):
    def test_allowed_files_pass(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "HANDOFF.md")
            write(root / "docs" / "USER-TODO.md")
            cfg = {"subdir_md_whitelist": [{"dir": "docs", "allowed": ["HANDOFF.md", "USER-TODO.md", "LESSONS.md"]}]}
            self.assertEqual(checks.check_subdir_md_whitelist(root, cfg), [])

    def test_extra_file_in_docs_root_is_violation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "HANDOFF.md")
            write(root / "docs" / "RANDOM-PLAN.md")
            cfg = {"subdir_md_whitelist": [{"dir": "docs", "allowed": ["HANDOFF.md"]}]}
            violations = checks.check_subdir_md_whitelist(root, cfg)
            self.assertEqual([v.file for v in violations], ["docs/RANDOM-PLAN.md"])

    def test_nested_subdirectory_is_not_checked(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "HANDOFF.md")
            write(root / "docs" / "plans" / "FOO-PLAN.md")
            cfg = {"subdir_md_whitelist": [{"dir": "docs", "allowed": ["HANDOFF.md"]}]}
            self.assertEqual(checks.check_subdir_md_whitelist(root, cfg), [])


class FilenamePatternTest(unittest.TestCase):
    def _cfg(self) -> dict:
        return {
            "filename_patterns": [
                {
                    "dir": "docs/reviews",
                    "pattern": r"^technical-maturity-review-\d{4}-\d{2}-\d{2}-\d{2}\.md$",
                    "allow_extra": ["REVIEW.md"],
                }
            ]
        }

    def test_matching_name_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "reviews" / "technical-maturity-review-2026-10-10-01.md")
            self.assertEqual(checks.check_filename_patterns(root, self._cfg()), [])

    def test_allow_extra_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "reviews" / "REVIEW.md")
            self.assertEqual(checks.check_filename_patterns(root, self._cfg()), [])

    def test_non_matching_name_is_violation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "reviews" / "review-oct.md")
            violations = checks.check_filename_patterns(root, self._cfg())
            self.assertEqual([v.file for v in violations], ["docs/reviews/review-oct.md"])


class MaxLinesTest(unittest.TestCase):
    def test_under_limit_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "HANDOFF.md", "line\n" * 5)
            cfg = {"max_lines": [{"file": "docs/HANDOFF.md", "max": 200}]}
            self.assertEqual(checks.check_max_lines(root, cfg), [])

    def test_over_limit_is_violation_with_actual_count(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "HANDOFF.md", "line\n" * 5)
            cfg = {"max_lines": [{"file": "docs/HANDOFF.md", "max": 3}]}
            violations = checks.check_max_lines(root, cfg)
            self.assertEqual(len(violations), 1)
            self.assertEqual(violations[0].line, 5)
            self.assertEqual(violations[0].rule_id, "max-lines")


class PointerAtTopTest(unittest.TestCase):
    def _cfg(self) -> dict:
        return {
            "pointer_at_top": [
                {"file": "CLAUDE.md", "requires_file": "COMMON.md", "must_contain": "COMMON.md", "within_lines": 3}
            ]
        }

    def test_reference_within_window_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "COMMON.md")
            write(root / "CLAUDE.md", "まず COMMON.md を読むこと。\n本文\n")
            self.assertEqual(checks.check_pointer_at_top(root, self._cfg()), [])

    def test_missing_reference_is_violation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "COMMON.md")
            write(root / "CLAUDE.md", "本文だけ\n")
            violations = checks.check_pointer_at_top(root, self._cfg())
            self.assertEqual([v.rule_id for v in violations], ["pointer-at-top"])

    def test_reference_outside_window_is_violation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "COMMON.md")
            write(root / "CLAUDE.md", "1\n2\n3\n4\nCOMMON.md はここ\n")
            violations = checks.check_pointer_at_top(root, self._cfg())
            self.assertEqual(len(violations), 1)

    def test_rule_skipped_when_required_file_absent(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "CLAUDE.md", "本文だけ\n")
            self.assertEqual(checks.check_pointer_at_top(root, self._cfg()), [])


class ForbiddenPatternsTest(unittest.TestCase):
    def _cfg(self) -> dict:
        return {
            "global_exclude_dirs": [],
            "forbidden_patterns": [
                {
                    "id": "no-initmd-filename-reference",
                    "regex": r"(?<![\w/-])init\.md\b",
                    "message": "init.md をファイル名で参照しない",
                    "exclude_dirs": ["docs/lessons"],
                }
            ],
        }

    def test_bare_reference_is_violation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "plans" / "FOO-PLAN.md", "詳細は init.md §3 を参照。\n")
            violations = checks.check_forbidden_patterns(root, self._cfg())
            self.assertEqual(len(violations), 1)
            self.assertEqual(violations[0].line, 1)
            self.assertEqual(violations[0].rule_id, "no-initmd-filename-reference")

    def test_compound_filename_is_not_a_false_positive(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(
                root / "docs" / "plans" / "FOO-PLAN.md",
                "[ADR-0046](adr/0046-addressables-init.md) を見る。\n",
            )
            violations = checks.check_forbidden_patterns(root, self._cfg())
            self.assertEqual(violations, [])

    def test_excluded_directory_is_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "lessons" / "01-x.md", "init.md はファイル名ごと廃止済み。\n")
            violations = checks.check_forbidden_patterns(root, self._cfg())
            self.assertEqual(violations, [])


class IterMarkdownFilesTest(unittest.TestCase):
    def test_global_excluded_dirs_are_pruned(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "a.md")
            write(root / "node_modules" / "pkg" / "README.md")
            files = checks.iter_markdown_files(root, ["node_modules"])
            rel = sorted(p.relative_to(root).as_posix() for p in files)
            self.assertEqual(rel, ["docs/a.md"])

    def test_dotdirs_are_pruned(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "docs" / "a.md")
            write(root / ".git" / "COMMIT_EDITMSG.md")
            files = checks.iter_markdown_files(root, [])
            rel = sorted(p.relative_to(root).as_posix() for p in files)
            self.assertEqual(rel, ["docs/a.md"])


if __name__ == "__main__":
    unittest.main()
