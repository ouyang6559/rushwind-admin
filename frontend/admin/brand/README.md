# RushWind 品牌覆写层

`sync-react.sh` 每次从上游整树同步后，会执行本目录的 `apply-brand.sh` 为快照打
RushWind 品牌覆写。**品牌覆写是快照唯一被允许的对上游偏离**，包含：

| 内容 | 覆写方式 |
|------|---------|
| `public/logo.png`（200×200，RushWind R 字徽标） | `overlay/` 整文件覆盖（源：rushwind 仓 `assets/logo/png/icon-512.png` 缩放） |
| `public/favicon.ico`（16/32/48 三档） | `overlay/` 整文件覆盖；**程序化生成**：`tools/make-favicon.py`（PIL+numpy 按 rushwind-icon.svg 几何复绘）。设计要点：不用徽章底（深色标签栏 dark-on-dark 糊团）、无圆角容器（四角全透明）、48/32/16 全用简化加粗形（进风口/尾迹短线在 favicon 尺寸下是游离噪点）、亮渐变直接示人。改几何后 `python tools/make-favicon.py` 重生成（顺带输出 `tools/favicon-preview.png` 深浅底预览） |
| `src/components/bussiness/AuthLayout/icons/SloganIcon.tsx`（登录页品牌插画） | `overlay/` 整文件覆盖（R 字主标 + 环流 + 风痕动效，青→天→靛渐变） |
| 品牌文案（系统名 / 版权 / meta / `VITE_APP_TITLE` 等） | `apply-brand.sh` 内逐条显式 sed 替换 |

设计上刻意**不替换**的：

- `GOWIND_CRYPTO_KEY` 环境变量名（`defaultImageUpload.ts` 注释与生成客户端）——
  功能性标识符，与后端配置对齐，非品牌语义；生成客户端本身是字节契约面。
- `.env.production` 的 `VITE_API_URL` / `VITE_SSE_URL`——部署端点，按实际部署填写。

## 门禁语义（sync-react.sh 双清单）

- `react.MANIFEST.sha256` —— 快照**最终态**（含品牌覆写）的字节清单，防手改门，CI 跑。
- `react.UPSTREAM.sha256` —— 上游原始树的字节清单，漂移检测基线：上游有变更时
  `sync-react.sh --check` 报 exit 2，重跑 `sync-react.sh`（同步 + 覆写 + 重建双清单）显式接受。
