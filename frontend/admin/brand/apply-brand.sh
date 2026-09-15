#!/usr/bin/env bash
# apply-brand.sh — 在 sync-react.sh 同步完上游后，为快照打 RushWind 品牌覆写：
#   1. overlay/ 树原样覆盖到快照（logo / favicon / 登录页品牌插画）
#   2. 对声明文件做品牌文案替换（GoWind / 风行 → RushWind / 锐风）
# 品牌覆写是快照**唯一被允许的**对上游偏离：全部改动必须收在本目录内，
# 新增偏离时同步更新 overlay/ 与下方替换表，并在 brand/README.md 登记。
set -euo pipefail

DST="${1:?usage: apply-brand.sh <react-snapshot-dir>}"
HERE="$(cd "$(dirname "$0")" && pwd)"

# 1) overlay 覆盖
if [[ -d "$HERE/overlay" ]]; then
  (cd "$HERE/overlay" && find . -type f -print0) | while IFS= read -r -d '' f; do
    rel="${f#./}"
    mkdir -p "$DST/$(dirname "$rel")"
    cp "$HERE/overlay/$rel" "$DST/$rel"
  done
fi

# 2) 品牌文案替换（逐条显式声明，不做全局盲替换）
replace() {
  local file="$1"
  shift
  sed -i "$@" "$DST/$file"
}

replace src/core/preferences/config/default.ts \
  -e 's/name: "GoWind Admin"/name: "RushWind Admin"/' \
  -e 's/companyName: "GoWind"/companyName: "RushWind"/' \
  -e 's|companySiteLink: "https://www.gowind.cloud"|companySiteLink: "https://github.com/tx7do/rushwind"|'

replace src/locales/zh-CN/_core/auth.json \
  -e 's/风行中后台管理系统/锐风中后台管理系统/' \
  -e 's/Copyright © {year} GoWind/Copyright © {year} RushWind/'

replace src/locales/en-US/_core/auth.json \
  -e 's/GoWind Admin Management System/RushWind Admin Management System/' \
  -e 's/Copyright © {year} GoWind/Copyright © {year} RushWind/'

replace index.html \
  -e 's/content="GoWind Admin React AntD Vite"/content="RushWind Admin React AntD Vite"/' \
  -e 's/name="author" content="GoWind"/name="author" content="RushWind"/'

replace .env \
  -e 's/VITE_APP_TITLE="GoWind Admin"/VITE_APP_TITLE="RushWind Admin"/' \
  -e 's/VITE_APP_NAMESPACE="gowind-admin"/VITE_APP_NAMESPACE="rushwind-admin"/'

echo "brand overlay applied -> $DST"
