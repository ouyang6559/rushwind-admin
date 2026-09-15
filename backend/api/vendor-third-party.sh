#!/usr/bin/env bash
# vendor-third-party.sh — 从 buf 模块缓存把 go-wind-admin/backend/api/buf.lock 锁定版本的
# 第三方注解 proto 复制进 api/third_party/。
# 依据 docs/binding-spec.md 与 docs/development-plan.md §4-D2：编译期需要这些注解的
# descriptor 声明（prost-reflect 才能解出 google.api.http / errors.code / redact /
# validate / gnostic 扩展）。
set -euo pipefail

CACHE="${LOCALAPPDATA:-$HOME/AppData/Local}/buf/v3/modules/b5"
DEST="$(cd "$(dirname "$0")" && pwd)/third_party"

# buf.lock 锁定的模块 → commit 映射（go-wind-admin/backend/api/buf.lock）。
declare -A MODULE_COMMITS=(
  ["buf.build/googleapis/googleapis"]="c17df5b2beca46928cc87d5656bd5343"
  ["buf.build/kratos/apis"]="c2de25f14fa445a79a054214f31d17a8"
  ["buf.build/tx7do/pagination"]="7e34dd27013f4c67bb09025c73a73980"
  ["buf.build/envoyproxy/protoc-gen-validate"]="daf171c6cdb54629b5f51e345a79e4dd"
  ["buf.build/gnostic/gnostic"]="087bc8072ce44e339f213209e4d57bf0"
  ["buf.build/go-wind/redact"]="d1f98995227e44d6b839e578d9fa1466"
)

# 待 vendor 的文件（相对各自模块 files/ 根的路径）。仅收录 api/protos 实际 import 的
# 注解声明集（见 docs/binding-spec.md §2 的 import 调研）：
#   google/api/*            — google.api.http / field_behavior / HttpBody
#   errors/errors.proto     — Kratos reason→HTTP 状态注解
#   pagination/*            — go-crud 分页请求消息
#   validate/validate.proto — PGV
#   gnostic/openapi/v3/*    — OpenAPI 扩展注解（免鉴权标注）
#   redact/v1/redact.proto  — 静态脱敏注解
declare -A VENDOR_FILES=(
  ["buf.build/googleapis/googleapis"]="google/api/annotations.proto google/api/http.proto google/api/field_behavior.proto google/api/httpbody.proto"
  ["buf.build/kratos/apis"]="errors/errors.proto"
  ["buf.build/tx7do/pagination"]="pagination/v1/pagination.proto"
  ["buf.build/envoyproxy/protoc-gen-validate"]="validate/validate.proto"
  ["buf.build/gnostic/gnostic"]="gnostic/openapi/v3/annotations.proto gnostic/openapi/v3/openapiv3.proto"
  ["buf.build/go-wind/redact"]="redact/v1/redact.proto"
)

for module in "${!VENDOR_FILES[@]}"; do
  commit="${MODULE_COMMITS[$module]}"
  src_root="$CACHE/$module/$commit/files"
  if [[ ! -d "$src_root" ]]; then
    echo "ERROR: buf cache missing $module@$commit" >&2
    exit 1
  fi
  for rel in ${VENDOR_FILES[$module]}; do
    src="$src_root/$rel"
    if [[ ! -f "$src" ]]; then
      echo "ERROR: missing file $rel in $module@$commit" >&2
      exit 1
    fi
    dst="$DEST/$rel"
    mkdir -p "$(dirname "$dst")"
    cp "$src" "$dst"
    echo "vendored $rel  <-  $module@$commit"
  done
done

echo "done."
