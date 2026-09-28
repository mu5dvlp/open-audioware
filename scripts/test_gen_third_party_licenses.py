#!/usr/bin/env python3
"""gen-third-party-licenses.py の依存分類と SPDX 判定のテスト。"""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path


_SCRIPT = Path(__file__).with_name("gen-third-party-licenses.py")
_SPEC = importlib.util.spec_from_file_location("gen_third_party_licenses", _SCRIPT)
assert _SPEC and _SPEC.loader
generator = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(generator)


def _package(package_id: str, name: str, *, proc_macro: bool = False) -> dict:
    kind = ["proc-macro"] if proc_macro else ["lib"]
    return {
        "id": package_id,
        "name": name,
        "version": "1.0.0",
        "license": "MIT",
        "manifest_path": "/does/not/exist/Cargo.toml",
        "targets": [{"kind": kind}],
    }


def _dependency(package_id: str, kind: str | None) -> dict:
    return {"pkg": package_id, "dep_kinds": [{"kind": kind}]}


class LicenseGeneratorTest(unittest.TestCase):
    def test_no_notice_option_requires_a_selectable_branch(self) -> None:
        has_option = generator._license_expression_has_no_notice_option
        self.assertTrue(has_option("Zlib OR Apache-2.0"))
        self.assertTrue(has_option("(MIT OR Apache-2.0) OR CC0-1.0"))
        self.assertTrue(has_option("Unlicense OR MIT"))
        self.assertTrue(has_option("MIT-0"))
        self.assertFalse(has_option("MIT OR Apache-2.0"))
        self.assertFalse(has_option("MIT AND Zlib"))
        self.assertFalse(has_option("Unlicense WITH LLVM-exception"))

    def test_classifies_runtime_proc_macro_and_build_only_graphs(self) -> None:
        packages = [
            _package("mw-ffi", "mw-ffi"),
            _package("runtime", "runtime"),
            _package("shared", "shared"),
            _package("macro", "macro", proc_macro=True),
            _package("macro-helper", "macro-helper"),
            _package("macro-dev", "macro-dev"),
            _package("build", "build"),
            _package("build-helper", "build-helper"),
            _package("dev", "dev"),
        ]
        nodes = [
            {
                "id": "mw-ffi",
                "deps": [
                    _dependency("runtime", None),
                    _dependency("macro", None),
                    _dependency("build", "build"),
                    _dependency("dev", "dev"),
                ],
            },
            {"id": "runtime", "deps": [_dependency("shared", None)]},
            {"id": "shared", "deps": []},
            {
                "id": "macro",
                "deps": [_dependency("macro-helper", None), _dependency("macro-dev", "dev")],
            },
            {"id": "macro-helper", "deps": [_dependency("shared", None)]},
            {"id": "macro-dev", "deps": []},
            {"id": "build", "deps": [_dependency("build-helper", None)]},
            {"id": "build-helper", "deps": [_dependency("shared", None)]},
            {"id": "dev", "deps": []},
        ]
        meta = {
            "packages": packages,
            "workspace_members": ["mw-ffi"],
            "resolve": {"nodes": nodes},
        }

        package_by_id, categories = generator._classify_packages([meta])

        self.assertEqual(set(package_by_id), {package["id"] for package in packages})
        self.assertEqual(categories["normal"], {"runtime", "shared"})
        self.assertEqual(categories["proc-macro"], {"macro", "macro-helper"})
        self.assertEqual(categories["build"], {"build", "build-helper"})
        self.assertNotIn("dev", set().union(*categories.values()))


if __name__ == "__main__":
    unittest.main()
