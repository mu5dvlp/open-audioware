#!/usr/bin/env python3
"""Markdown の文書どうしのリンクを依存の辺として検査する(Python 3 標準ライブラリのみ)。
dependency-cruiser / import-linter の Markdown 版。

このファイルは**単体で動く**ように作ってある —— ワークスペース固有の前提はコードに
書かず、すべて設定ファイル(既定で `<root>/docs-graph.json`)で与える。
各リポジトリへ同じファイルをそのままコピーして使ってよい
(コピーするのは `docs_graph.py` と、使うなら `test_docs_graph.py` の2つだけ)。

検査する内容:
  1. リンク切れ(リポジトリ内を指しているのにファイルが無い)
  2. リポジトリの外へ出るリンク(`../` でルートを越える。`allow_outside` に無ければエラー)
  3. 循環(強連結成分。サイズ2以上。`cycle_ignore_sources` で辺を間引き、
     `cycle_allow` に載っている循環は許可)
  4. 禁止の向き(`forbidden` の glob に一致する辺)

対象にするリンクの書式: インラインリンク `[text](path)`、参照リンクの定義
`[label]: path`、`<a href="path">`。コードブロック(``` ```` ```` ```)とインラインコード
(`` `...` ``)の中は見ない。http(s)・mailto 等のスキーム付き・`#anchor` だけのリンクは対象外。
パスはリンクが書かれているファイルからの相対で解決する(`/` 始まりはリポジトリ直下から)。

使い方:
  python3 docs_graph.py                        # カレントディレクトリをリポジトリ直下として検査
  python3 docs_graph.py --root /path/to/repo    # 対象のリポジトリを指定
  python3 docs_graph.py --config other.json     # 設定ファイルを指定(既定: <root>/docs-graph.json)
  python3 docs_graph.py --graph mermaid         # 文書の依存グラフを mermaid で出す(検査結果は stderr へ)
  python3 docs_graph.py --graph dot             # 同、Graphviz dot 形式

終了コード: エラー(リンク切れ・許可の無い外部リンク・許可の無い循環・禁止の向き)が
1件でもあれば 1、無ければ 0。

設定ファイル(JSON)の形:
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
すべての欄は省略できる(省略時は既定値か空リスト)。`exclude` は既定の除外一覧に**追加**
される(既定を上書きしない)。glob は `fnmatch` 形式(`*` は `/` も含めて何文字にでも一致)。
"""
import argparse
import fnmatch
import json
import os
import re
import sys
from pathlib import Path
from urllib.parse import unquote

DEFAULT_EXCLUDES = [
    ".git", "node_modules", "Library", "Temp", "target", "Builds",
    ".claude/worktrees",
]

# スキーム付きのリンク(http: / https: / mailto: / tel: 等)は対象外。
SCHEME_RE = re.compile(r"^[A-Za-z][A-Za-z0-9+.\-]*:(?!\\)")

INLINE_LINK_RE = re.compile(r"\[[^\]]*\]\(\s*<?([^)\s>]+)>?(?:\s+(?:\"[^\"]*\"|'[^']*'))?\s*\)")
REF_DEF_RE = re.compile(r"^\s{0,3}\[[^\]]+\]:\s*<?([^\s>]+)>?(?:\s+(?:\"[^\"]*\"|'[^']*'))?\s*$")
HTML_A_RE = re.compile(r"""<a\b[^>]*\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))""", re.IGNORECASE)
INLINE_CODE_RE = re.compile(r"`[^`\n]*`")
FENCE_RE = re.compile(r"^\s*(```+|~~~+)")


# ---------- 設定 ----------

def load_config(path):
    if path is None or not os.path.isfile(path):
        return {}
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def exclude_patterns(config):
    return list(DEFAULT_EXCLUDES) + list(config.get("exclude", []))


# ---------- ファイル収集 ----------

def _join_rel(relroot, name):
    return name if relroot in (".", "") else relroot + "/" + name


def is_excluded(relpath_posix, patterns):
    parts = relpath_posix.split("/")
    for pattern in patterns:
        if "/" in pattern:
            if fnmatch.fnmatchcase(relpath_posix, pattern):
                return True
            if any(fnmatch.fnmatchcase("/".join(parts[i:]), pattern) for i in range(len(parts))):
                return True
        else:
            if any(fnmatch.fnmatchcase(p, pattern) for p in parts):
                return True
    return False


def collect_markdown_files(root, patterns):
    """root 以下の *.md を集める(除外に一致するものは除く)。相対パス(posix)のソート済みリスト。"""
    found = []
    for dirpath, dirnames, filenames in os.walk(root):
        relroot = os.path.relpath(dirpath, root).replace(os.sep, "/")
        dirnames[:] = [d for d in dirnames if not is_excluded(_join_rel(relroot, d), patterns)]
        for name in filenames:
            if not name.lower().endswith(".md"):
                continue
            rel = _join_rel(relroot, name)
            if not is_excluded(rel, patterns):
                found.append(rel)
    return sorted(found)


# ---------- リンクの抽出 ----------

def extract_links(abs_path):
    """(行番号, 生のリンク先文字列) のリスト。コードブロック・インラインコードは除く。"""
    links = []
    in_fence = False
    fence_marker = None
    with open(abs_path, "r", encoding="utf-8", errors="replace") as f:
        for lineno, raw_line in enumerate(f, start=1):
            m = FENCE_RE.match(raw_line)
            if m:
                marker = m.group(1)[0]
                if not in_fence:
                    in_fence, fence_marker = True, marker
                elif marker == fence_marker:
                    in_fence, fence_marker = False, None
                continue
            if in_fence:
                continue
            line = INLINE_CODE_RE.sub(" ", raw_line)
            for m in INLINE_LINK_RE.finditer(line):
                links.append((lineno, m.group(1)))
            m = REF_DEF_RE.match(line)
            if m:
                links.append((lineno, m.group(1)))
            for m in HTML_A_RE.finditer(line):
                href = next(g for g in m.groups() if g is not None)
                links.append((lineno, href))
    return links


# ---------- リンクの分類・解決 ----------

def classify_link(root, src_rel, raw_target):
    """
    戻り値: None(対象外) | ("outside", 表示用の相対パス)
            | ("broken", 表示用の相対パス) | ("ok", リポジトリ直下からの相対パス posix)
    """
    target = raw_target.strip()
    if not target or target.startswith("#"):
        return None
    if SCHEME_RE.match(target):
        return None
    target = target.split("#", 1)[0]
    if not target:
        return None
    target = unquote(target)

    src_abs = root / src_rel
    if target.startswith("/"):
        base = root
        target = target.lstrip("/")
    else:
        base = src_abs.parent
    normalized = os.path.normpath(str(base / target)) if target else str(base)

    rel_from_root = os.path.relpath(normalized, str(root)).replace(os.sep, "/")
    if rel_from_root == "." :
        rel_from_root = ""
    if rel_from_root.startswith(".."):
        return "outside", rel_from_root
    if not (os.path.isfile(normalized) or os.path.isdir(normalized)):
        return "broken", rel_from_root
    return "ok", rel_from_root


# ---------- 検査本体 ----------

def analyze(root, config):
    """root 以下を検査する。findings(エラー)・allowed(許可されて黙らせたもの)・
    graph_edges(文書どうしの辺。表示用)・nodes(*.md の一覧)を返す。"""
    root = Path(root)
    patterns = exclude_patterns(config)
    nodes = collect_markdown_files(root, patterns)
    node_set = set(nodes)

    allow_outside = config.get("allow_outside", [])
    cycle_ignore_sources = config.get("cycle_ignore_sources", [])
    cycle_allow = config.get("cycle_allow", [])
    forbidden = config.get("forbidden", [])

    findings = []       # dict(file, line, kind, message)
    allowed = []         # dict(file, line, kind, message) -- 許可済みなので終了コードに数えない
    internal_edges = []  # (src, dst, line) -- リポジトリ内に解決できたリンク全部(文書以外も含む)
    doc_edges = set()    # (src, dst) -- 文書どうしの辺だけ(循環・グラフ表示用)

    for src in nodes:
        for lineno, raw in extract_links(root / src):
            result = classify_link(root, src, raw)
            if result is None:
                continue
            kind, rel = result
            if kind == "outside":
                if any(fnmatch.fnmatchcase(rel, pat) for pat in allow_outside):
                    continue
                findings.append({"file": src, "line": lineno, "kind": "outside-link",
                                  "message": "リポジトリの外へのリンク: %s" % raw})
                continue
            if kind == "broken":
                findings.append({"file": src, "line": lineno, "kind": "broken-link",
                                  "message": "リンク先が無い: %s" % raw})
                continue
            # kind == "ok"
            internal_edges.append((src, rel, lineno))
            if rel in node_set:
                doc_edges.add((src, rel))

    # ---- 禁止の向き ----
    for idx, rule in enumerate(forbidden):
        name = rule.get("name") or ("forbidden-%d" % idx)
        frm, to = rule.get("from", ""), rule.get("to", "")
        reason = rule.get("reason", "")
        seen = set()
        for src, dst, lineno in internal_edges:
            if (src, dst) in seen:
                continue
            if fnmatch.fnmatchcase(src, frm) and fnmatch.fnmatchcase(dst, to):
                seen.add((src, dst))
                findings.append({"file": src, "line": lineno, "kind": "forbidden",
                                  "message": "禁止の向き(%s): %s -> %s%s" % (
                                      name, src, dst, "(%s)" % reason if reason else "")})

    # ---- 循環 ----
    adj = {n: [] for n in nodes}
    for src, dst in doc_edges:
        if any(fnmatch.fnmatchcase(src, pat) for pat in cycle_ignore_sources):
            continue
        adj[src].append(dst)

    for comp in _tarjan_scc(adj):
        if len(comp) < 2:
            continue
        comp_set = frozenset(comp)
        allow_entry = next((e for e in cycle_allow if frozenset(e.get("files", [])) == comp_set), None)
        example = " -> ".join(_find_one_cycle(adj, comp))
        detail = "例: %s(成分 %d 件: %s)" % (example, len(comp), ", ".join(sorted(comp)))
        if allow_entry:
            allowed.append({"file": sorted(comp)[0], "line": None, "kind": "cycle",
                             "message": "循環(許可済み: %s): %s" % (allow_entry.get("reason", ""), detail)})
        else:
            findings.append({"file": sorted(comp)[0], "line": None, "kind": "cycle",
                              "message": "循環: %s" % detail})

    return {"findings": findings, "allowed": allowed, "nodes": nodes,
            "doc_edges": sorted(doc_edges)}


def _find_one_cycle(adj, comp):
    """comp(強連結成分)の中から、具体例として1つの実在する循環のパスを探す。
    start から幅優先で成分内を辿り、start へ戻る辺を1本見つけて経路を作る。"""
    comp_set = set(comp)
    start = min(comp)
    pred = {start: None}
    order = [start]
    i = 0
    while i < len(order):
        node = order[i]
        i += 1
        for w in adj.get(node, []):
            if w in comp_set and w not in pred:
                pred[w] = node
                order.append(w)
    for u in order:
        if u != start and start in adj.get(u, []):
            chain = [u]
            cur = u
            while pred[cur] is not None:
                cur = pred[cur]
                chain.append(cur)
            chain.reverse()
            chain.append(start)
            return chain
    return list(comp) + [start]  # 強連結の前提が崩れていなければ到達しない


def _tarjan_scc(adj):
    """強連結成分を求める(再帰を使わない Tarjan)。adj: {node: [隣接node,...]}"""
    index_counter = [0]
    index, lowlink, on_stack = {}, {}, {}
    stack = []
    result = []

    for start in adj:
        if start in index:
            continue
        work = [(start, iter(adj[start]))]
        index[start] = lowlink[start] = index_counter[0]
        index_counter[0] += 1
        stack.append(start)
        on_stack[start] = True

        while work:
            v, it = work[-1]
            advanced = False
            for w in it:
                if w not in index:
                    index[w] = lowlink[w] = index_counter[0]
                    index_counter[0] += 1
                    stack.append(w)
                    on_stack[w] = True
                    work.append((w, iter(adj.get(w, []))))
                    advanced = True
                    break
                elif on_stack.get(w):
                    lowlink[v] = min(lowlink[v], index[w])
            if advanced:
                continue
            work.pop()
            if work:
                parent = work[-1][0]
                lowlink[parent] = min(lowlink[parent], lowlink[v])
            if lowlink[v] == index[v]:
                comp = []
                while True:
                    w = stack.pop()
                    on_stack[w] = False
                    comp.append(w)
                    if w == v:
                        break
                result.append(comp)
    return result


# ---------- 出力 ----------

def render_report(result):
    lines = []
    findings = result["findings"]
    allowed = result["allowed"]
    if findings:
        lines.append("## エラー(%d件)" % len(findings))
        for x in sorted(findings, key=lambda x: (x["file"], x["line"] or 0, x["kind"])):
            where = "%s:%s" % (x["file"], x["line"]) if x["line"] is not None else x["file"]
            lines.append("%s\t[%s]\t%s" % (where, x["kind"], x["message"]))
    else:
        lines.append("## エラー: 無し")
    if allowed:
        lines.append("")
        lines.append("## 許可済み(参考。終了コードには数えない。%d件)" % len(allowed))
        for x in allowed:
            where = "%s:%s" % (x["file"], x["line"]) if x["line"] is not None else x["file"]
            lines.append("%s\t[%s]\t%s" % (where, x["kind"], x["message"]))
    lines.append("")
    lines.append("文書 %d 件を検査。エラー %d 件。" % (len(result["nodes"]), len(findings)))
    return "\n".join(lines) + "\n"


def render_graph(result, fmt):
    nodes = result["nodes"]
    edges = result["doc_edges"]
    if fmt == "mermaid":
        ids = {n: "n%d" % i for i, n in enumerate(nodes)}
        lines = ["graph LR"]
        for n in nodes:
            label = n.replace('"', "'")
            lines.append('  %s["%s"]' % (ids[n], label))
        for src, dst in edges:
            lines.append("  %s --> %s" % (ids[src], ids[dst]))
        return "\n".join(lines) + "\n"
    if fmt == "dot":
        lines = ["digraph docs {"]
        for n in nodes:
            lines.append('  "%s";' % n.replace('"', '\\"'))
        for src, dst in edges:
            lines.append('  "%s" -> "%s";' % (src.replace('"', '\\"'), dst.replace('"', '\\"')))
        lines.append("}")
        return "\n".join(lines) + "\n"
    raise ValueError("unknown graph format: %s" % fmt)


# ---------- CLI ----------

def main(argv=None):
    ap = argparse.ArgumentParser(description="Markdown 文書どうしのリンクの依存関係を検査する")
    ap.add_argument("--root", default=".", help="検査するリポジトリのルート(既定: カレントディレクトリ)")
    ap.add_argument("--config", default=None, help="設定ファイル(既定: <root>/docs-graph.json)")
    ap.add_argument("--graph", choices=["mermaid", "dot"], default=None,
                    help="文書の依存グラフを指定の形式で stdout に出す(検査結果は stderr へ)")
    args = ap.parse_args(argv)

    root = Path(args.root).resolve()
    config_path = Path(args.config).resolve() if args.config else root / "docs-graph.json"
    config = load_config(config_path)
    result = analyze(root, config)

    if args.graph:
        sys.stdout.write(render_graph(result, args.graph))
        sys.stderr.write(render_report(result))
    else:
        sys.stdout.write(render_report(result))

    return 1 if result["findings"] else 0


if __name__ == "__main__":
    sys.exit(main())
