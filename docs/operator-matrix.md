# 算子矩阵 v1 —— 前端别名 × go-crud × rushwind FilterExpr

> 本文钉死 `query` 参数（Django 别名语法 JSON）在三个前端的别名拼写、go-crud 的
> 枚举映射、gorm 层（postgres 方言）的 SQL 形态、以及 rushwind 契约面
> （`rushwind-storage-proto/src/wire.rs` → `rushwind-storage-seaorm`）的对应路径，
> 并给出待建「别名层」的映射规范与分歧登记。差分台架的回放语料与豁免集以本文为准。
>
> 锚点（本机路径）：react `frontend/admin/react/src/core/transport/rest/pagination.ts`
> （vue-element、vue-vben 同款 twin，见 §4）；go-crud
> `pagination@v0.0.14/filter/{operator_converter,query_string_converter}.go`；
> gorm 翻译层 `go-crud/gorm@v0.0.24/filter/filter_processor.go`；
> rust `rushwind/crates/rushwind-storage-proto/src/wire.rs`、
> `rushwind/crates/rushwind-storage-seaorm/src/lib.rs`。
> gorm 为 admin 后端活性 ORM 路径（`backend/app/admin/service/internal/data/gorm/*`）。

## 0. 管线位置

**Go 侧（四段）**：URL `query` 参数 → `query_string_converter.Convert`
（顶层数组=AND 组、`$and`/`$or` 递归、键按 `__` 切分为 字段/算子[/datepart|JSON 路由]，
字段名 `ToSnakeCase`）→ `ConverterStringToOperator`（别名 → `paginationV1.Operator` 枚举；
输入经 `ToSnakeCase`+`ToLower` 归一；未知别名 → `OPERATOR_UNSPECIFIED`）→
`FilterCondition{Field, Op, Value|Values, JsonPath?, DatePart?}` →
gorm `Processor.Process` 的 switch（**缺席算子走 default=静默丢弃**；字段经
`isValidFieldExpr` 白名单，不过 → `AddError` → 查询失败）。

**Rust 侧现状（两段 + 一个缺口）**：
① protojson 面——`FilterExpr` 消息经 pbjson 反序列化为 storage-proto 的
`Operator` 枚举，`wire.rs` 折叠为契约 `Op`（含包装）再由 seaorm 翻译；
② AIP 文本面——`filter` 参数经 `rushwind-storage/src/syntax/text.rs`。
③ **Django 别名面不存在**：`rushwind-storage-axum` 的通用表查询面只认
`q`/`token`/`offset`/`page`/`filter`(AIP)/`sort`/`fields`，不消费 `query` 参数；
admin 的 List 请求经本仓生成绑定计划把 `page/pageSize/noPaging/orderBy/query/fieldMask`
绑进类型化 proto 字段后，`query` 字符串在**服务层**需要一个尚不存在的别名→
FilterExpr 转换组件（规范见 §2）。

## 1. 核心矩阵（按 go-crud 枚举逐行）

分类含义：**direct** = 三列一一对应、SQL 语义一致；**fold** = go 算子在 gorm
(pg) 层坍缩为某基础形态，rust 以 wire.rs 的折叠 + seaorm 的等价渲染对位；
**drop** = gorm 层缺席该 case、条件静默消失，rust 侧须同样丢弃（或现有路径有分歧）；
**reject** = go 实际执行而 rust 契约面报 unsupported —— 真实行为分歧，登记豁免或改 wire。

| 别名拼写（小写归一后逐字） | go 枚举 | gorm/pg SQL | rust 路径（wire.rs→seaorm） | 分类 | 判定 |
|---|---|---|---|---|---|
| `eq` `equal` `equals` | EQ | `f = ?` | `Op::Eq` → `=` | direct | 一致 |
| `ne` `neq` `not` `not_equal` `not_equals` `not-equal` | NEQ | `NOT (f = ?)` | `Op::NotEq` → 否定 | direct | 一致（NULL 行为两侧同排除） |
| `gt` `greater_than` `greater-than` | GT | `f > ?` | `Op::Gt` | direct | 一致 |
| `gte` `greater_than_or_equal` `greater_equals` `greater_or_equal` `greater-or-equal` | GTE | `f >= ?` | `Op::Gte` | direct | 一致 |
| `lt` `less_than` `less-than` | LT | `f < ?` | `Op::Lt` | direct | 一致 |
| `lte` `less_than_or_equal` `less_equals` `less_or_equal` `less-or-equal` | LTE | `f <= ?` | `Op::Lte` | direct | 一致 |
| `like` | LIKE | **无 case → 丢弃** | `Op::Like` → `LIKE ?` | drop | **分歧**：别名层须丢弃 |
| `ilike` `i_like` | ILIKE | `f ILIKE ?`（裸值=大小写不敏感等值） | `Op::Ilike` → `LOWER(f) LIKE LOWER(v)` | direct | 语义等价（ci-等值） |
| `not_like` `notlike` | NOT_LIKE | **无 case → 丢弃** | `Op::NotLike` → `NOT LIKE` | drop | **分歧**：别名层须丢弃 |
| `in` | IN | `f IN (…)`（值=JSON 数组字符串或 values 多值） | `Op::In` | direct | 一致（值通道按 §3 对齐） |
| `nin` `not_in` `notin` | NIN | `f NOT IN (…)` | `Op::NotIn` | direct | 一致 |
| `is_null` `isnull` | IS_NULL | `f IS NULL` | `Op::IsNull` | direct | 一致 |
| `is_not_null` `isnot_null` `isnotnull` `not_isnull` | IS_NOT_NULL | `f IS NOT NULL` | `Op::IsNotNull` | direct | 一致 |
| `between` `range` | BETWEEN | `f >= ? AND f <= ?`（两值） | `Op::Between` | direct | 一致 |
| `contains` | CONTAINS | `f LIKE '%v%'` | `Op::Contains` → seaorm 自包装 `%v%` | direct | 一致（包装在 seaorm 层） |
| `icontains` `i_contains` | ICONTAINS | pg：`f ILIKE '%v%'` | wire：`Icontains → Op::Ilike + wrap('%v%')` → seaorm LOWER+LIKE | fold | 一致 |
| `starts_with` `startswith` | STARTS_WITH | `f LIKE 'v%'` | `Op::StartsWith`（seaorm 包装 `v%`） | direct | 一致 |
| `istarts_with` `i_starts_with` `istartswith` | ISTARTS_WITH | pg：`f ILIKE 'v%'` | wire：`IstartsWith → Op::Ilike + wrap('v%')` | fold | 一致 |
| `ends_with` `endswith` | ENDS_WITH | `f LIKE '%v'` | `Op::EndsWith`（seaorm 包装 `%v`） | direct | 一致 |
| `iends_with` `i_ends_with` `iendswith` | IENDS_WITH | pg：`f ILIKE '%v'` | wire：`IendsWith → Op::Ilike + wrap('%v')` | fold | 一致 |
| `exact` | EXACT | `f = ?`（与 EQ **完全同义**，gorm 注释自认） | wire：`Exact → Op::Eq` | fold | 一致 |
| `iexact` `i_exact` | IEXACT | pg：`f ILIKE ?` 裸值（ci 等值） | wire：**unsupported 报错** | **reject** | **分歧**：Go 执行 / Rust 报错。可折叠（`Ilike` 裸值语义等价）但 wire.rs 现拒绝——待决策（§5-D2） |
| `regexp` `regex` | REGEXP | pg：`f ~ ?` | **unsupported 报错** | **reject** | **分歧**（§5-D3） |
| `iregexp` `i_regexp` `iregex` | IREGEXP | pg：`f ~* ?` | **unsupported 报错** | **reject** | **分歧**（§5-D3） |
| `search` | SEARCH | pg：`to_tsvector(f) @@ plainto_tsquery(?)` | **unsupported 报错** | **reject** | **分歧**（§5-D3） |
| `json_contains` | JSON_CONTAINS | **无 case → 丢弃** | **unsupported 报错** | drop/reject 冲突 | **分歧**：Go 静默丢弃 vs Rust 报错（§5-D1） |
| `array_contains` | ARRAY_CONTAINS | 同上 | 同上 | 同上 | 同上 |
| `exists` | EXISTS | 同上 | 同上 | 同上 | 同上 |

Rust 契约独有、无任何别名可达：`Op::NotBetween`（go 无 NOT_BETWEEN 枚举、
无别名拼写 → 不可达冗余，登记不动）。
`Operator::Unspecified`（未知别名的落点）：wire.rs 报错；Go 侧条件被附加后由
gorm default **静默丢弃** → 别名层对未知别名必须静默丢弃（§2）。

## 2. 待建别名层规范（admin 服务层组件）

- **位置**：admin 模块服务层，仓储调用之前；输入=List 请求 proto 的 `query`
  字符串字段，输出=`rushwind_storage::FilterExpr`。不落进 rushwind-storage-axum
  （那是通用 AIP/protojson 面，参数契约不同）。
- **键分割必须按 go-crud `MakeFieldFilter` 语义自实现**：
  - 1 段（无 `__`）→ EQ；键含 `.` → JSON 路径条件（EQ + 路径值）。
  - 2 段 `f__op` → 算子=keys[1]；值为 JSON 数组 → `Values` 多值，否则单值字符串化。
  - 3 段 `f__x__op2` → **算子=keys[2]**；x 为有效 datepart → 条件携带 DatePart
    （活性路径惰性，见 §3）；否则按 JSON 字段路由；op2 非法 → **整条丢弃**
    （`hasOperations` 门）。
  - **不可复用 rust-utils `query_parser::parse_filter_field`**：它固定取 parts[1]
    为算子，三段键语义与 go-crud（取末段）相悖。
- **别名→Op 映射** = §1 的 direct+fold 两类合集，与 wire.rs 的 proto 枚举面
  **完全同构**（`Exact→Eq`、`Icontains 族→Ilike+wrap` 等）→ 建议把 wire.rs 的
  折叠逻辑提取为共享函数，两面共用一张表。
- **丢弃集（静默，不报错）**：`like`、`not_like`、`notlike`、`json_contains`、
  `array_contains`、`exists`、一切未知别名（UNSPECIFIED 语义）。
- **拒绝集**：`regexp` `regex` `iregexp` `i_regexp` `iregex` `search`、
  `iexact` `i_exact` —— Go 侧**执行**而 rust 契约面 unsupported；别名层按
  §5 决策处理（默认：同样报错，差分豁免登记；如 D2 采纳折叠则改走 `Ilike`）。
- **空值跳过**：比较/模式类算子遇空白单值 → 丢弃该条件；IN/NIN 无有效
  values/数组 → 丢弃；IS_NULL 族忽略值（对位 gorm `requiresValue`/空值分支）。
- **字段名**：`ToSnakeCase` 后交由 storage `validate`（列存在性/arity/类型）；
  对位 gorm 的 `ToSnakeCase` + `isValidFieldExpr` 白名单（不过 → 两侧各自错误面，
  差分台架钉状态码与包络）。

## 3. 结构特性矩阵

| 特性 | Go（query_string_converter + gorm） | rust 现状/别名层要求 |
|---|---|---|
| 顶层数组 | = AND 组；子数组扁平化进当前 AND 组 | `FilterExpr::all`；扁平化同 |
| `$and`/`$or` 键 | 互斥（同持两者→错误）；值须数组；递归成 ExprType_AND/OR 组 | `all`/`any`；互斥校验同 |
| JSON 路径键（`a.b`） | EQ+JsonPath 条件；gorm 侧 `JsonbFieldExpr` 列表达式 + 白名单 | 无对应；别名层按"字段+路径"建 EQ 条件，交 storage validate（预计拒） |
| 多值（值=JSON 数组） | → `Values`（IN/NIN/BETWEEN 消费） | `Op::In/NotIn/Between` 的 values |
| 三段键 datepart | DatePart 载荷；**gorm Process 不消费（惰性，仅测试引用）** | 别名层携带但不消费；差分豁免登记 |
| 空字段/空算子段 | 丢弃该键 | 同 |
| 字段白名单不过 | gorm `AddError` → 查询失败 | storage `validate` → `StorageError`；错误面待差分 |
| 值字符串化 | `AnyToString` | 同（数值/布尔保持原类型按 wire.rs `value_to_contract`） |
| `orderBy` / `fieldMask` / `page`/`pageSize`/`noPaging` | 走 proto 字段绑定（本仓生成绑定计划），不属别名层 | 已覆盖（绑定计划） |

## 4. 线上活性面（三前端实测）

三前端共用同款 transport（react `src/core/transport/rest/pagination.ts`；
vue-element `src/core/transport/rest/pagination.ts`；vue-vben
`apps/admin/src/transport/rest/pagination.ts`）。`makeQueryString` 实际发射：

- **裸键 EQ**：数值/布尔值；字符串但字段名命中 ID 豁免正则
  `/(_id$|Id$|ID$|^id$)/`；键已带算子后缀（`hasOperatorSuffix`，
  其表 = go-crud operatorMap 键集的小写镜像）→ 原样透传。
- **`__contains`**：其余全部字符串值（模糊匹配约定）。
- `needCleanTenant` 时剔除 `tenant_id`/`tenantId`；清理后空对象 → 不发 query。
- `orderBy` 缺省 `['-created_at']`（JSON 数组）；`noPaging` = paging 缺失时为 true。

⇒ **线上活性算子集 = {EQ（裸键）, CONTAINS（`__contains`）}**。矩阵其余行均处
防御/休眠面：只有手工构造或前端改版才会触达，由差分回放语料负责钉死两侧一致。

附注（无对位影响）：前端守卫只做 `toLowerCase`，go 侧还过 `ToSnakeCase`
（如 `notEqual`→`not_equal` 在 Go 命中、在前端守卫表 miss）→ 前端对这类拼写会
追加以 `__contains` 结尾的三段键，Go 侧三段语义取末段算子=contains——该行为对
两个后端字节相同，不构成跨后端分歧，但回放语料应包含一例以钉住。

## 5. 分歧登记（差分豁免集种子）

- **D1（丢弃 vs 报错）**：`json_contains`/`array_contains`/`exists` 与未知别名
  ——Go 静默丢弃、rust 契约面报 unsupported。别名层按"静默丢弃"对位 Go；
  protojson 面（wire.rs 报错）与 Go 的 protojson 面差异另记（Go 的 protojson 面
  同样进 gorm switch → 同样丢弃，故 wire.rs 的报错是**跨后端真实分歧**，若
  FilterExpr 消息可从请求体到达需豁免或改 wire）。
- **D2（iexact）**：Go 执行（pg ILIKE 裸值=ci 等值），wire.rs 拒绝。技术上
  `Iexact → Ilike`（裸值）为语义精确折叠；是否推动 wire.rs 修改属 rushwind 仓
  决策，默认按拒绝+豁免登记。
- **D3（正则/全文）**：`regexp` 族与 `search` —— Go 在 pg 上真实执行
  （`~`/`~*`/tsvector），rust 无对应算子。活性面前端不发射；默认拒绝+豁免登记。
- **D4（`like`/`not_like`）**：go 枚举存在但 gorm 丢弃；rust 有真实 Op。别名层
  必须丢弃（§2 丢弃集），否则产生 Go 不存在的过滤行为。
- **D5（错误面）**：字段白名单/校验失败的两通道错误（gorm AddError vs
  StorageError）在状态码与包络上的差异 —— 存储阶段差分钉死后回填。

## 6. 与计划的联动

- 本文档满足 Phase 0 清单第 6 项（算子矩阵 v1）。
- §2 规范是 Phase 2/3 服务层实现 `query` 参数的输入契约。
- §5 种子并入差分台架的豁免集（台架落地时引用本文节号）。
