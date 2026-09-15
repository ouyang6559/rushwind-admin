# frontend

本仓为 `go-wind-admin` 后端的 Rust 复刻（rushwind-admin），**不复制前端代码**。

兼容性契约：三个管理端前端（react / vue-element / vue-vben）继续使用
`go-wind-admin` 仓 `frontend/` 下的现有代码与生成客户端
（`protoc-gen-typescript-http` 产物，三份字节相同），不做任何修改即可对接本仓后端。
线上行为的逐字节对齐基准见 [`../docs/binding-spec.md`](../docs/binding-spec.md)。

因此本目录仅作为结构占位，保持与原版仓库相同的顶层布局
（`backend/` + `frontend/` + `docs/`）。任何需要改动前端的诉求都视为契约破坏，
必须先更新 binding-spec 并走差分回归。
