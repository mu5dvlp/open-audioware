"""docs_graph.py の自己テスト。一時ディレクトリに小さな Markdown を作って確かめる。
実行: python3 -m unittest scripts/docs-graph/test_docs_graph.py(ワークスペース直下で)"""
import contextlib
import importlib.util
import io
import os
import tempfile
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("docs_graph", os.path.join(_HERE, "docs_graph.py"))
dg = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(dg)


class Repo:
    def __init__(self, d):
        self.d = d

    def write(self, path, text):
        p = os.path.join(self.d, path)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w", encoding="utf-8") as f:
            f.write(text)


class DocsGraphTest(unittest.TestCase):
    def setUp(self):
        self._t = tempfile.TemporaryDirectory()
        self.addCleanup(self._t.cleanup)
        self.r = Repo(self._t.name)

    def analyze(self, config=None):
        return dg.analyze(self.r.d, config or {})

    def kinds(self, result, bucket="findings"):
        return [(x["file"], x["kind"]) for x in result[bucket]]

    # ---- 収集 ----

    def test_collects_markdown_only(self):
        self.r.write("a.md", "# a\n")
        self.r.write("b.txt", "not markdown\n")
        result = self.analyze()
        self.assertEqual(result["nodes"], ["a.md"])

    def test_default_excludes(self):
        self.r.write("a.md", "# a\n")
        self.r.write("node_modules/dep/README.md", "# dep\n")
        self.r.write(".git/COMMIT_EDITMSG.md", "x\n")
        result = self.analyze()
        self.assertEqual(result["nodes"], ["a.md"])

    def test_config_exclude_adds_to_defaults(self):
        self.r.write("a.md", "# a\n")
        self.r.write("generated/x.md", "# x\n")
        result = self.analyze({"exclude": ["generated"]})
        self.assertEqual(result["nodes"], ["a.md"])

    # ---- リンク切れ ----

    def test_broken_link_is_error(self):
        self.r.write("a.md", "[リンク](b.md)\n")
        result = self.analyze()
        self.assertEqual(self.kinds(result), [("a.md", "broken-link")])

    def test_directory_link_is_ok(self):
        self.r.write("a.md", "[フォルダ](sub/)\n")
        self.r.write("sub/b.md", "# b\n")
        result = self.analyze()
        self.assertEqual(result["findings"], [])
        self.assertEqual(result["doc_edges"], [])  # ディレクトリは文書ノードではない

    def test_existing_link_is_ok(self):
        self.r.write("a.md", "[リンク](b.md)\n")
        self.r.write("b.md", "# b\n")
        result = self.analyze()
        self.assertEqual(result["findings"], [])
        self.assertEqual(result["doc_edges"], [("a.md", "b.md")])

    def test_relative_to_source_directory(self):
        self.r.write("docs/a.md", "[リンク](../b.md)\n")
        self.r.write("b.md", "# b\n")
        result = self.analyze()
        self.assertEqual(result["findings"], [])
        self.assertEqual(result["doc_edges"], [("docs/a.md", "b.md")])

    def test_root_relative_link(self):
        self.r.write("docs/a.md", "[リンク](/b.md)\n")
        self.r.write("b.md", "# b\n")
        result = self.analyze()
        self.assertEqual(result["findings"], [])
        self.assertEqual(result["doc_edges"], [("docs/a.md", "b.md")])

    def test_anchor_is_dropped_and_checked(self):
        self.r.write("a.md", "[リンク](b.md#section)\n")
        self.r.write("b.md", "# b\n")
        result = self.analyze()
        self.assertEqual(result["findings"], [])

    # ---- 対象外のリンク ----

    def test_external_and_anchor_only_links_ignored(self):
        self.r.write("a.md", "[外部](https://example.com/x)\n[メール](mailto:a@example.com)\n[同ページ](#top)\n")
        result = self.analyze()
        self.assertEqual(result["findings"], [])
        self.assertEqual(result["doc_edges"], [])

    def test_code_block_and_inline_code_ignored(self):
        self.r.write("a.md", "```\n[リンク](nope.md)\n```\n`[インライン](nope2.md)`\n")
        result = self.analyze()
        self.assertEqual(result["findings"], [])

    def test_reference_style_and_html_links(self):
        self.r.write("a.md", "[ref]: b.md\n<a href=\"c.md\">c</a>\n")
        self.r.write("b.md", "# b\n")
        self.r.write("c.md", "# c\n")
        result = self.analyze()
        self.assertEqual(result["findings"], [])
        self.assertEqual(sorted(result["doc_edges"]), [("a.md", "b.md"), ("a.md", "c.md")])

    # ---- 外へのリンク ----

    def test_outside_link_is_error(self):
        self.r.write("sub/a.md", "[外](../../outside.md)\n")
        result = self.analyze()
        self.assertEqual(self.kinds(result), [("sub/a.md", "outside-link")])

    def test_allow_outside(self):
        self.r.write("sub/a.md", "[外](../../outside.md)\n")
        result = self.analyze({"allow_outside": ["../outside.md"]})
        self.assertEqual(result["findings"], [])

    # ---- 循環 ----

    def test_cycle_is_error(self):
        self.r.write("a.md", "[b](b.md)\n")
        self.r.write("b.md", "[a](a.md)\n")
        result = self.analyze()
        self.assertEqual(len(result["findings"]), 1)
        self.assertEqual(result["findings"][0]["kind"], "cycle")

    def test_cycle_ignore_sources(self):
        self.r.write("index.md", "[a](a.md)\n[b](b.md)\n")
        self.r.write("a.md", "[index](index.md)\n")
        self.r.write("b.md", "[index](index.md)\n")
        result = self.analyze({"cycle_ignore_sources": ["index.md"]})
        self.assertEqual([f for f in result["findings"] if f["kind"] == "cycle"], [])

    def test_cycle_allow(self):
        self.r.write("a.md", "[b](b.md)\n")
        self.r.write("b.md", "[a](a.md)\n")
        result = self.analyze({"cycle_allow": [{"files": ["b.md", "a.md"], "reason": "相互参照が前提"}]})
        self.assertEqual(result["findings"], [])
        self.assertEqual(len(result["allowed"]), 1)
        self.assertIn("相互参照が前提", result["allowed"][0]["message"])

    # ---- 禁止の向き ----

    def test_forbidden_edge(self):
        self.r.write("plans/x.md", "[history](../history.md)\n")
        self.r.write("history.md", "# history\n")
        result = self.analyze({"forbidden": [{"name": "no-plan-to-history", "from": "plans/**",
                                              "to": "history.md", "reason": "向きが逆"}]})
        self.assertEqual(len(result["findings"]), 1)
        f = result["findings"][0]
        self.assertEqual(f["kind"], "forbidden")
        self.assertIn("no-plan-to-history", f["message"])
        self.assertIn("向きが逆", f["message"])

    def test_forbidden_does_not_flag_other_edges(self):
        self.r.write("plans/x.md", "[spec](../spec.md)\n")
        self.r.write("spec.md", "# spec\n")
        result = self.analyze({"forbidden": [{"name": "no-plan-to-history", "from": "plans/**",
                                              "to": "history.md", "reason": "向きが逆"}]})
        self.assertEqual(result["findings"], [])

    # ---- グラフ出力 ----

    def test_render_graph_mermaid(self):
        self.r.write("a.md", "[b](b.md)\n")
        self.r.write("b.md", "# b\n")
        result = self.analyze()
        out = dg.render_graph(result, "mermaid")
        self.assertIn("graph LR", out)
        self.assertIn("-->", out)

    def test_render_graph_dot(self):
        self.r.write("a.md", "[b](b.md)\n")
        self.r.write("b.md", "# b\n")
        result = self.analyze()
        out = dg.render_graph(result, "dot")
        self.assertIn("digraph docs", out)
        self.assertIn("->", out)

    # ---- CLI ----

    def test_main_exit_code(self):
        self.r.write("a.md", "[リンク](b.md)\n")
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(dg.main(["--root", self.r.d]), 1)
            os.remove(os.path.join(self.r.d, "a.md"))
            self.r.write("a.md", "# a\n")
            self.assertEqual(dg.main(["--root", self.r.d]), 0)

    def test_main_reads_config_file(self):
        self.r.write("sub/a.md", "[外](../../outside.md)\n")
        self.r.write("docs-graph.json", '{"allow_outside": ["../outside.md"]}')
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(dg.main(["--root", self.r.d]), 0)


if __name__ == "__main__":
    unittest.main()
