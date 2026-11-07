#!/bin/sh
set -eu
repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if command -v python3 >/dev/null 2>&1; then
  exec python3 "$repo_dir/tools/inori.py" "$@"
elif command -v python >/dev/null 2>&1; then
  exec python "$repo_dir/tools/inori.py" "$@"
fi
printf '%s\n' 'Python 3.11以上が必要です。OSのパッケージ管理ツールからpython3を導入してください。' >&2
exit 1
