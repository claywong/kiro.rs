# 上游合并笔记

记录本仓库相对上游（ZyphrZero/kiro.rs）的**有意偏离**，供下次合并时快速判断某处冲突该怎么解。
只记「为什么这样选」，不记代码结构——后者看 git log 即可。

## 一、已让位给上游的决策（冲突时直接取上游，勿再恢复本地版本）

| 领域 | 本地曾经的做法 | 现在 | 原因 |
|---|---|---|---|
| 模型闸门 | 凭据 `supported_models` 白名单硬过滤，空清单也跳过 | 上游语义：调度层只按订阅等级（Opus 需非 Free），具体支持与否交由 `model_cache` 三态（Confirmed 优先 / Unsupported 跳过 / Unknown 放行） | 上游列表临时不可用时不该退化成本地硬白名单；新模型新账号不必等清单回填 |
| 模型映射 | `NON_CLAUDE_MODELS` 白名单 + 逐版本 if-else | 上游的开放透传 + 通用 Claude 规范化 | 白名单注定被上游取代，维护成本高 |
| `GET /v1/models` | 280 行静态 Model 清单 | 上游动态目录（按分组查上游、合并去重、按 ID 排序） | 静态清单每次新模型都要改，且与上游必然冲突 |
| 凭据级模型测试 | `POST /api/admin/credentials/{id}/test-model` + 独立弹窗 + `call_api_with_credential` / `acquire_context_for_credential` | 上游 `POST /api/admin/models/test`，入口在上游「查看可用模型」弹窗的逐行测试按钮 | 上游弹窗已含模型列表 + 逐行测试，本地那套是它的功能子集，只多一个自定义消息框，不值得为此维护一条平行链路 |

`supported_models` 字段本身保留，但**已退化为纯元数据**（Admin 展示用），调度层不再读它。

让位凭据级模型测试后，唯一的能力损失是：上游 `test_model` 用 `provider.call_api()` 由账号池挑号，
即便从某张凭据卡片进入「查看可用模型」，点测试也可能打到别的凭据（响应里的 `credentialId` 会显示
真实命中号，不算误导，但对不上弹窗标题）。若日后确实碍事，从 `4c0511e` 捞回，别重写。

上游 `test_model` 手搓 `ConversationState` 且 `additional_model_request_fields: None`，绕过了
`convert_request_with_mode`——但这不算损失：effort 键名判定（gpt-5.x 族走 `reasoning`，其他走
`output_config`，下错上游 400）由 `converter.rs` 的单测断言到序列化 JSON 那一层，回归靠
`cargo test` 挡，不依赖手点。

### 配套的本地补丁：模型缓存回填（必须保住）

让位给上游的三态语义有个前提——`model_cache` 得有数据。上游只在三条**外部事件**里填充它
（启动预热、`GET /v1/models`、Admin 手动查），业务请求路径不填。于是预热失败或运行中新增的
凭据会永久停在 `Unknown`，`discovery_rank` 对所有候选返回同一档，三态退化成无效维度。

本地补了自愈路径，凡已从上游拿到过模型列表的地方都回填缓存：

- `get_usage_limits_for` 的搭车块（余额刷新每 300s 全量触达所有凭据，是唯一的周期性路径；
  它本来就调了 `ListAvailableModels`，只写 `supported_models` 然后把响应丢掉）。
- `AdminService::update_credential` 之后（改代理会 invalidate 缓存，不重建就退回 `Unknown`）。
- `add_credential_inner` 的「直接导入」分支（`fetch_balance = false` 跳过了余额查询，
  也就跳过了搭车回填；验活路径经 `get_balance` 已覆盖）。

实现放在 `store_model_cache_entry` / `model_cache_guards` / `spawn_local_model_cache_refresh`
三个方法里，单独成块，`refresh_model_cache_for` 尾部改为调用前者。守卫（代数 + epoch）必须在
发起上游请求**之前**采样，否则在途旧请求会覆盖新缓存。上游若自己补了回填，取上游、删本地块。

未修的已知缺口：`start_balance_refresher` 只在配置了 `adminApiKey` 时启动（`main.rs`），
没配 Admin Key 的部署拿不到周期性自愈。另：`cached_model_support` 不看 TTL，stale 的
`Unsupported` 会持续硬过滤该凭据——这属于上游语义，改动前需显式决策。

## 二、本地独有、上游没有的特性（冲突时必须保住）

- **卖家 / Key 供应商对接**：`src/vendor/` 整个模块。webhook 接入、自动/手动提取模式、事件库降级内存。
  - 连带约束：`main.rs` 里 `AdminService` 必须在**顶层**构建（而非仅 Admin API 分支内），因为 webhook 要复用
    `import_one_credential`，不能依赖 `adminApiKey` 是否配置。上游把它放在分支内，每次都会冲突，保持本地写法。
- **自定义模型注册表**：`src/model/custom_models.rs`。承担上游透传管不了的四件事：别名映射
  （客户端名 ≠ 后端名）、上下文窗口覆盖、reasoning 键名判定（gpt-5.x 族走 `reasoning`，其他走
  `output_config`，下错上游 400）、`/v1/models` 元数据优先。
- **调度增强**：换号重试排除集（`excluded_ids`）、salvage 兜底跨层按 RPM 余量选号、
  TTFT EWMA 同层调度、黏性放开条件（跨层倒挂自愈）。
- **凭证近 1 分钟额度消耗**：`src/admin/recent_spend.rs` + `GET /api/admin/credentials/recent-spend`，
  凭证卡「1分钟耗」读数。计的是上游 credits 消耗速率，用于观测单凭证负载。

### 2026-07-31 kiroapp.io / kiroapp.cc 命名去歧义 + 独立路径合并

**背景**：`kiroapp` 这个词在三处指向不同东西，导致配置和文档长期错位：

| 出现位置 | 实际指向 |
|---|---|
| `flavor: "kiroapp"` | kiroapp**.io**（`/api/me/*`，功能完整，有 webhook） |
| `flavor: "kiroapp-cc"` | kiroapp**.cc**（`/openapi/*`，仅库存/余额/提取，无 webhook） |
| 顶层 `kiroapp` 配置块 | kiroapp**.cc**（历史命名，最早作为「次级卖家」独立接入） |

**做的决策**：

1. **kiroapp.cc 的两套实现合并为一套**。`ec3ab19` 建了 `kiroapp-cc` flavor 但没删老的独立路径，
   同一家卖家有两条代码路径。已删除 `src/vendor/kiroapp.rs`、`kiroapp_service.rs`、
   `admin-ui/src/components/kiroapp-card.tsx` 与 `/api/admin/vendor/kiroapp/*` 两个接口，
   统一走 flavor。`VendorState` 随之只剩 `registry` 一个字段。
2. **老路径的防御性解析必须保住**，已移植进 `flavor_kiroapp_cc.rs`：claim 返回 2xx 时先按文档形态
   严格解析，失败则按 `ksk_` 前缀扫（连裸文本响应也能捞）。原因是该接口无幂等键，2xx 即已扣费，
   按 JSON 硬解失败就等于把付过费的 Key 丢掉。配套 10 个测试一并移植。
   同时把 kiroapp.cc 的**嵌套错误体** `{"error":{"message":..}}` 解析也带了过来 ——
   `client.rs` 里通用的 `VendorError` 只认扁平的 `{"error":"文本"}`，直接复用会把错误信息丢成原文片段。
3. **顶层 `kiroapp` 配置块保留但标记废弃**，重命名为 `LegacyKiroappCcConfig`，
   由 `Config::resolved_vendors()` 自动转成 id/flavor 均为 `kiroapp-cc` 的普通卖家项，排在最后
   （显式 `vendors` 配置同 id 时胜出）。不直接删是为了不让存量 `config.json` 启动失败。
4. **修了三组字段名错位**（都加了 serde alias 兼容存量配置）：
   - `inboundToken` → 正名 `webhookPathToken`。旧名会被**静默忽略**，
     导致 `inbound_enabled()` 为 false、webhook 一律 404，极难定位。
   - `autoPurchaseWindows` → 正名 `autoPurchaseSchedule`，同样静默忽略。
   - 时段表内的 `start`/`end` → 正名 `from`/`to`。这两个是**必填字段**，
     旧名会导致启动直接失败（`missing field from`），不是静默忽略。
5. **`config.example.json` 原本根本无法解析**（`vendors` 被错误地嵌在 `vendor` 里，
   报 `missing field baseUrl`）—— 照抄示例的人起不来服务。已改为顶层 `vendors` 数组，
   并新增测试 `示例配置能解析且卖家齐全` 用 `include_str!` 拿真实结构体解析它。
   **这个测试是防回归的关键**：此前示例与结构体错位这么久没被发现，就是因为没有任何测试碰过它。

6. **`VendorFlavor` 的 `Serialize` 改成手写**，输出 `kiroapp-cc` 而非 derive 的 `kiroappCc`。
   面板切换提取模式会 `config.save()` 把整个文件写回（`VendorService::set_mode`），
   derive 形态会把用户手写的 `kiroapp-cc` 悄悄改成另一种拼法。两种都能解析，但文档、
   `as_str()`、`all_names()` 报错提示统一用连字符形态，写回也必须一致。

**踩过的坑**：给外层加 `autoPurchaseWindows` 别名后测试反而失败，暴露出内层 `start`/`end` 也对不上。
只加外层别名会把「静默忽略」变成「硬启动失败」，反而更糟。三层名字要一起兼容。

**验证示例配置的正确方法**：光断言「能解析」是不够的 —— 未知键不报错、只被丢掉，
这正是 `inboundToken` 静静失效的机制。测试 `示例配置无静默忽略的键且往返稳定` 用的是
**反序列化后再序列化回来逐键比对**：被忽略的键在回程里会消失，被改写的值也会暴露
（第 6 条就是这么发现的）。往后改示例配置或增删配置字段，以这个测试为准。

**顺手修的 bug**：`ClaimResult::into_purchase_result` 在 0 个 Key 时用 `points_cost / 0` 算单价，
得到 inf/NaN，序列化进 JSON 会变 null 或报错。已加 `purchased > 0` 守卫，但保留 `total_debit`
（人工核对扣费时需要看到钱确实扣了）。

**上游关系**：`src/vendor/` 整个模块是本地独有，上游没有对应实现，这次改动无融合风险。
但注意第 2 条移植的宽松解析是本地特有的保命逻辑，若上游日后自己做卖家对接，合并时不要被取代。

## 三、需要「融合」而非取舍的点

`token_manager.rs` 的 `select_next_credential_excluding` / `acquire_context_impl` 是双方改动的
交汇处，历史上每次合并都冲突。当前形态：

- priority 分支排序是**三级**：发现档（Confirmed 优先，上游）→ 优先级层（双方）→ TTFT EWMA（本地）。
  上游原版只有前两级。上游若再加维度，按「加一个排序键」的方式并入，别重写整个 `min_by`。
- `acquire_context_impl` 参数是双方并集：`(model, group, excluded_ids, salvage, update_current)`。
  `update_current` 是上游给 Admin 只读查询用的，`excluded_ids`/`salvage` 是本地换号重试用的。
  写回 `current_id` 的条件是 `update_current && excluded_ids.is_empty()`。

### 2026-07-29 合并上游 v0.7.4（403 自愈节流 + IDC relogin 凭据替换）

上游 `b7077b5`。7 处冲突，其中 5 处是「双方各自新增」（结构体字段、独立函数），全保留；
本地新增部分按第四节约定另起一块并加注释，不与上游字段交错。两处需要判断：

- **可用性判断抽取**：上游把 `select_next_credential_excluding` 的过滤条件抽成
  `entry_available_for_request`（disabled / throttled / 模型分组匹配 / `Unsupported` 过滤），
  取上游。但本地的 `excluded_ids` 与 RPM 滑动窗口**故意留在调用点**，不并入该 helper——
  helper 还被 `has_available_for_request`（判断「是否还有号可用」）复用，那里不该受
  单次换号重试的排除集与瞬时 RPM 影响，并进去会让「全灭」判定误报。
  本地原来在 `filter_map` 尾部用 `.then_some` 过滤 `Unsupported`，与上游放进 helper 语义等价，
  已删本地那行。
- **自愈重选的调用**：上游把内联的「全灭即全量重置」重写为受控 `try_self_heal`
  （冷却间隔 + 连续轮数上限 + 按模型隔离轮次 + 持久化 `last_self_heal_at`），取上游，
  本地内联版本整块作废。但上游自愈成功后重选走的是 `select_next_credential`
  ——那是 `#[cfg(test)]` 版本且不带排除集，本地改为 `select_next_credential_excluding(model,
  group, excluded_ids, salvage)`，否则自愈后会把刚失败的号重新选回来。上游若后续给
  `try_self_heal` 之后的重选加参数，以本地这行为准。

`reset_health` 是上游新增，只清失败/禁用/冷却 + `clear_self_heal_streak`，**不清**本地的
`ttft_ewma` / `recent_requests`：前者是性能估计（清了要重新探测），后者是 RPM 窗口
（清了等于绕过限速）。保持上游原样即可，不要顺手加。

验证：`cargo build` 通过，`cargo test` 693 passed，`npx tsc --noEmit` 无输出。
上游 `test_default_is_account_suspended` 等新测试原样保留、未改断言。

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
   明确不管的事（vendor 对接、本地运维便利接口）。第一节那几条全是违反此判据的代价。
