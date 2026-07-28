# 上游合并笔记

记录本仓库相对上游（ZyphrZero/kiro.rs）的**有意偏离**，供下次合并时快速判断某处冲突该怎么解。
只记「为什么这样选」，不记代码结构——后者看 git log 即可。

## 一、已让位给上游的决策（冲突时直接取上游，勿再恢复本地版本）

| 领域 | 本地曾经的做法 | 现在 | 原因 |
|---|---|---|---|
| 模型闸门 | 凭据 `supported_models` 白名单硬过滤，空清单也跳过 | 上游语义：调度层只按订阅等级（Opus 需非 Free），具体支持与否交由 `model_cache` 三态（Confirmed 优先 / Unsupported 跳过 / Unknown 放行） | 上游列表临时不可用时不该退化成本地硬白名单；新模型新账号不必等清单回填 |
| 模型映射 | `NON_CLAUDE_MODELS` 白名单 + 逐版本 if-else | 上游的开放透传 + 通用 Claude 规范化 | 白名单注定被上游取代，维护成本高 |
| `GET /v1/models` | 280 行静态 Model 清单 | 上游动态目录（按分组查上游、合并去重、按 ID 排序） | 静态清单每次新模型都要改，且与上游必然冲突 |

`supported_models` 字段本身保留，但**已退化为纯元数据**（Admin 展示用），调度层不再读它。

## 二、本地独有、上游没有的特性（冲突时必须保住）

- **卖家 / Key 供应商对接**：`src/vendor/` 整个模块。webhook 接入、自动/手动提取模式、事件库降级内存。
  - 连带约束：`main.rs` 里 `AdminService` 必须在**顶层**构建（而非仅 Admin API 分支内），因为 webhook 要复用
    `import_one_credential`，不能依赖 `adminApiKey` 是否配置。上游把它放在分支内，每次都会冲突，保持本地写法。
- **自定义模型注册表**：`src/model/custom_models.rs`。承担上游透传管不了的四件事：别名映射
  （客户端名 ≠ 后端名）、上下文窗口覆盖、reasoning 键名判定（gpt-5.x 族走 `reasoning`，其他走
  `output_config`，下错上游 400）、`/v1/models` 元数据优先。
- **凭据级模型测试**：`POST /api/admin/credentials/{id}/test-model`。与上游的 `/models/test`
  职责不同——后者由账号池挑凭据，前者指定凭据+自定义 message，排查单账号问题用。两者并存，非冗余。
- **调度增强**：换号重试排除集（`excluded_ids`）、salvage 兜底跨层按 RPM 余量选号、
  TTFT EWMA 同层调度、黏性放开条件（跨层倒挂自愈）。
- **成本核算**：`cost_ledger`、凭据购买成本、货币符号配置。

## 三、需要「融合」而非取舍的点

`token_manager.rs` 的 `select_next_credential_excluding` / `acquire_context_impl` 是双方改动的
交汇处，历史上每次合并都冲突。当前形态：

- priority 分支排序是**三级**：发现档（Confirmed 优先，上游）→ 优先级层（双方）→ TTFT EWMA（本地）。
  上游原版只有前两级。上游若再加维度，按「加一个排序键」的方式并入，别重写整个 `min_by`。
- `acquire_context_impl` 参数是双方并集：`(model, group, excluded_ids, salvage, update_current)`。
  `update_current` 是上游给 Admin 只读查询用的，`excluded_ids`/`salvage` 是本地换号重试用的。
  写回 `current_id` 的条件是 `update_current && excluded_ids.is_empty()`。

## 四、降低未来冲突的约定

1. **本地新增的 `use` 名字单独成行**，不要插进上游按字母排序的 `use {...}` 块中间。
   上游那些块每次增删都会重排，插在里面等于保证冲突。已按此整理 `admin/{service,handlers,router}.rs`。
2. **本地新增方法避免与上游可能的命名撞车**。这次 `AdminService::with_kiro_provider` 和
   `KiroProvider::token_manager` 双方各自加了同名方法，git 不视为冲突（不同位置），直到编译期才报
   duplicate definition。新增构造器/访问器时考虑加本地前缀。
3. **本地测试单独放**，别插进上游 `mod tests` 中间。上游测试保持原样不改动——它们是判断「我是否
   改变了上游意图」最快的探针。若某个上游测试必须改才能过，说明存在语义分歧，需要显式决策而非
   顺手改断言。
4. **勤拉上游**。这次 base 是 v0.7.2，本地 41 个提交 vs 上游 3 个，提交数比例越悬殊越难解。
5. **不要与上游并行实现同一件事**。判据：如果上游也在解决这个问题，就等上游的方案；本地只做上游
   明确不管的事（vendor 对接、成本核算）。第一节那三条全是违反此判据的代价。
