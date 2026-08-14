#!/usr/bin/env bash
# Unity 起動をマシン全体で直列化するためのロックラッパー。
#
# 同じマシン上で並行して動く他セッションもクライアントプロジェクト側で Unity を
# 使うことがあるため、いかなる Unity 起動(バッチモードの -createProject /
# -runTests を含む)の前にもこのロックを取得すること。
#
# 使い方: tools/with-unity-lock.sh <command> [args...]
set -euo pipefail

LOCK_DIR="/tmp/mgct-unity.lock"

release() {
  rmdir "${LOCK_DIR}" 2>/dev/null || true
}
trap release EXIT INT TERM

until mkdir "${LOCK_DIR}" 2>/dev/null; do
  sleep 15
done

"$@"
