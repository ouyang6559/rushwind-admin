#!/usr/bin/env bash
# sync-react.sh — 把上游 React 前端同步进本仓 frontend/admin/react/，打 RushWind
# 品牌覆写，并维护双清单：
#
#   frontend/admin/sync-react.sh           # 同步（默认源：D:\GoProject\go-wind-admin\frontend\admin\react，
#                                          #        可用 REACT_FRONTEND_SRC 覆盖）+ 品牌覆写
#                                          #        + 重建 react.MANIFEST.sha256 与 react.UPSTREAM.sha256
#   frontend/admin/sync-react.sh --check   # CI 门：快照终态必须与 react.MANIFEST.sha256 逐字节一致
#                                          #        （防手改，退出码 1）；源仓在时另对比
#                                          #        react.UPSTREAM.sha256 做漂移报告（退出码 2，
#                                          #        CI 无源仓自跳过）
#
# 快照唯一被允许的对上游偏离是 RushWind 品牌覆写（frontend/admin/brand/，同步后
# 自动施加）。除此之外任何手改都视为契约破坏；上游变更必须以整树重同步的方式
# 显式接受（重建双清单并跑差分回归）。
# 字节保真：不归一化、不做 EOL 转换（.gitattributes 对 react/** 与双清单钉了 -text）。
set -euo pipefail

MODE="${1:-sync}"
HERE="$(cd "$(dirname "$0")" && pwd)"
DST="$HERE/react"
MANIFEST="$HERE/react.MANIFEST.sha256"
UPSTREAM_MANIFEST="$HERE/react.UPSTREAM.sha256"
SRC="${REACT_FRONTEND_SRC:-/d/GoProject/go-wind-admin/frontend/admin/react}"

# 源仓文件清单：优先 git ls-files（精确排除 node_modules/dist/.idea 等未跟踪内容），
# 非 git 目录退化为 find 剪枝。
list_src() {
  if git -C "$SRC" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    git -C "$SRC" ls-files -z .
  else
    (cd "$SRC" && find . -type d \( -name node_modules -o -name dist -o -name dist.zip -o -name .idea -o -name .turbo \) -prune \
      -o -type f -print0 | sed -z 's|^\./||')
  fi
}

# 源仓原始树哈希清单（漂移基线）。
upstream_hash() {
  list_src | sort -z | while IFS= read -r -d '' f; do
    printf '%s  %s\n' "$(sha256sum "$SRC/$f" | cut -d' ' -f1)" "$f"
  done
}

# 对一个树生成排序 sha256 清单（路径 + 字节哈希，无任何归一化）。
# 剪枝 node_modules/dist：不入库的构建产物，不参与快照清单与校验。
hash_tree() {
  (
    cd "$1" || exit 1
    find . -type d \( -name node_modules -o -name dist \) -prune \
      -o -type f -print0 | sort -z | while IFS= read -r -d '' f; do
      printf '%s  %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "${f#./}"
    done
  )
}

case "$MODE" in
  sync)
    if [[ ! -d "$SRC" ]]; then
      echo "ERROR: react source not found: $SRC (set REACT_FRONTEND_SRC)" >&2
      exit 1
    fi
    # 只清被跟踪的内容：node_modules/ 与 dist/ 不入库，原地保留（重装依赖太贵，
    # 且 dev server 常以本目录为 cwd，顶层目录句柄被握住时删目录会直接失败）。
    find "$DST" -mindepth 1 -maxdepth 1 ! -name node_modules ! -name dist -exec rm -rf {} + 2>/dev/null || true
    mkdir -p "$DST"
    list_src | sort -z | while IFS= read -r -d '' f; do
      mkdir -p "$DST/$(dirname "$f")"
      cp "$SRC/$f" "$DST/$f"
    done
    # RushWind 品牌覆写（快照唯一允许的上游偏离）。
    bash "$HERE/brand/apply-brand.sh" "$DST"
    upstream_hash > "$UPSTREAM_MANIFEST"
    hash_tree "$DST" > "$MANIFEST"
    echo "synced $(grep -c . "$UPSTREAM_MANIFEST") upstream files -> $DST (manifests: $MANIFEST, $UPSTREAM_MANIFEST)"
    ;;
  --check)
    status=0
    # 防手改门：快照终态必须与 MANIFEST 逐字节一致（多余/缺失/改动都算失败）。
    if ! diff -u "$MANIFEST" <(hash_tree "$DST") >/dev/null; then
      echo "ERROR: frontend/admin/react/ does not match react.MANIFEST.sha256 (hand-edit? run sync-react.sh to rebuild)" >&2
      status=1
    fi
    # 漂移检测：源仓在时对比上游基线（CI 无源仓时自跳过）。
    if [[ -d "$SRC" ]] && git -C "$SRC" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
      if ! diff -u "$UPSTREAM_MANIFEST" <(upstream_hash) >/dev/null; then
        echo "WARNING: upstream react source has drifted from the baseline; re-run sync-react.sh to accept" >&2
        [[ $status -eq 0 ]] && status=2
      fi
    fi
    exit $status
    ;;
  *)
    echo "usage: $0 [--check]" >&2
    exit 64
    ;;
esac
