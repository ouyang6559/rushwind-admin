# frontend

三套管理端前端（react / vue-element / vue-vben）与后端构成零改动兼容契约：
前端及其生成客户端（`protoc-gen-typescript-http` 产物，三份字节相同）不做任何修改
即可对接本仓后端（REST :7788，React 版 dev 代理 `.env.development` 的
`VITE_PROXY` 默认即指向 `127.0.0.1:7788`）。线上行为的逐字节对齐基准见
[`../docs/binding-spec.md`](../docs/binding-spec.md)。

任何需要改动前端的诉求都视为契约破坏：必须先更新 binding-spec 并走差分回归，
再在上游变更后整树重新同步。

## React 版（随仓同步快照 + 品牌覆写）

`admin/react/` 是上游 React 前端的**同步快照**，经 `sync-react.sh` 整树同步后自动施加
RushWind 品牌覆写（`admin/brand/`：logo / favicon / 登录页插画 + 品牌文案——这是快照
唯一被允许的对上游偏离），并以双清单校验：

- `react.MANIFEST.sha256` —— 快照终态字节清单（防手改门，CI 同款）
- `react.UPSTREAM.sha256` —— 上游原始树字节清单（漂移检测基线）

```shell
bash frontend/admin/sync-react.sh          # 同步 + 品牌覆写 + 重建双清单
bash frontend/admin/sync-react.sh --check  # 校验门（防手改 exit 1 / 上游漂移 exit 2）
```

**不要手改快照内任何文件**（`node_modules/` 与 `dist/` 不入库）：品牌改动一律进
`admin/brand/` 覆写层（详见其 README）；其余改动视为契约破坏，必须先更新
binding-spec 并走差分回归。同步源路径与覆盖方式见脚本头部说明。

## Vue 版（暂未随仓）

`vue-element/` 与 `vue-vben/` 两版暂不随仓，后续按需以同样的
「整树同步 + MANIFEST 门」方式接入。
