# ADR(Architecture Decision Record)一覧

設計判断とその理由の記録。1判断1ファイル。
📌 実装の経緯は `../history.md`、現行仕様は `../spec/` が正。

| # | タイトル | 状態 |
|---|---|---|
| [0001](0001-security-framework-adoption.md) | セキュリティ枠組みの採用方針 —— サプライチェーン検査の継続と、自前脅威モデルでの unsafe FFI / RT スレッドの扱いに限定する | 採用 |
| [0002](0002-p3-11-async-reopen.md) | 内部再オープン(P3-11)はレジストリの `Mutex` を保持したまま行わず、3段構成へ分割する | 採用 |
