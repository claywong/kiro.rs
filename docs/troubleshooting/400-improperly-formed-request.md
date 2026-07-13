# 400 `Improperly formed request`（上游兜底错误）排查与应对

本文记录一类常见的上游拒绝：HTTP 400，响应体包含 `{"message":"Improperly formed request."}`。该错误信息非常“兜底”，需要结合本项目的转换逻辑与日志进行定位。

## 现象与影响

- 上游接口：`https://q.us-east-1.amazonaws.com/generateAssistantResponse`
- 典型日志：
  - `400 Bad Request - 请求格式错误 ... {"message":"Improperly formed request.","reason":null}`（见 `logs/docker-03-03.log:2897`）
- 影响：当前请求无法成功处理；通常**重试同一个 payload 并不会改善**（除非是上游瞬态问题，但该错误更像校验拒绝）。

## 本项目里已知可能触发点（按常见度）

1. **空 content / 结构不完整**
   - 上游会拒绝空 `content`（本项目已有兜底占位，避免空内容直发）。
   - 相关逻辑：`src/anthropic/converter.rs`（空 content 兜底注释处）
2. **tool_use / tool_result 配对不一致**
   - 留下“孤立”的 tool_use 或 tool_result，可能触发上游 400。
   - 相关逻辑：`src/anthropic/compressor.rs`（tool 配对修复注释处）
3. **请求体/图片过大导致的上游校验失败**
   - 上游存在未公开的限制；过大时可能不返回明确的 “too large”，而是兜底 400。
   - 本项目有本地阈值 `max_request_body_bytes` 作为安全阈值，并带“二次压缩”（但注意：当前二次压缩**不处理图片**）。
   - 相关逻辑：`src/model/config.rs:183`、`src/anthropic/handlers.rs:737`、`src/anthropic/handlers.rs:125`

## 2026-03-03 这次日志的结论（`logs/docker-03-03.log`）

### 发生了什么

该日志中共有 **10 次** `400 Improperly formed request`，且每次都紧邻一条 “GIF 已抽帧并重编码” 的 INFO 日志，表现出强相关。

抽帧日志特征（每次一致）：

- `sampled_frames=16`
- `output_format="jpeg"`
- `total_final_bytes=2655025`（抽帧后 JPEG 的二进制总字节数，base64 前）

示例：`logs/docker-03-03.log:2889`。

对应请求体大小（每次一致或接近）：

- `kiro_request_body_bytes=3773256`（示例：`logs/docker-03-03.log:2893`）
- 另一组约 `3762029`（同文件后续多次出现）

### 为什么“图片张数不多（16）但体积仍很大”

核心点：**JPEG 是“单张静态图”的压缩**，GIF 抽帧后变成 16 张独立 JPEG，无法利用“视频/动图”的跨帧时序冗余；再叠加 base64 膨胀，体积上升很快。

用这次日志数字做个可核算的拆解：

- 抽帧后 JPEG 二进制总大小：`2,655,025` bytes
- base64 编码后大小约为 `4*ceil(n/3)`：
  - `2,655,025 -> 3,540,036` bytes（约 `3.54MB`）
- 平均每帧：
  - 二进制：`~165,939` bytes/帧
  - base64：`~221,252` bytes/帧（也就是你看到的“约 221KB/张”）
- 最终请求体 `3.76~3.77MB`，意味着除图片 base64 外还有 `~222~233KB` 的 JSON/文本/tools 开销（属于正常范围）

### “GIF 抽帧的图片有先压缩为 jpg 吗？”

有。抽帧实现会把选中的帧编码为 `jpeg`，并以 base64 形式写入上游 `images[]`。

- 抽帧输出格式常量：`src/image.rs:27`
- 编码路径：`src/image.rs:154`、`src/image.rs:419`
- 帧写入上游 images：`src/anthropic/converter.rs:503`

### 为什么 jpg 还会这么大（关键实现细节）

1. **JPEG 质量参数未显式配置**
   - 当前通过 `image` crate 的 `write_to(..., ImageFormat::Jpeg)` 编码。
   - `image` 默认 JPEG quality 是 `75`（不是“极限压缩”）。
   - 相关代码：`src/image.rs:419`
2. **缩放阈值偏宽，未必会触发显著缩小**
   - 默认长边 `4000`、像素 `4,000,000`，很多帧可能不需要缩放。
   - 相关配置：`src/model/config.rs:167`、`src/model/config.rs:171`

## “把 maxRequestBodyBytes 调到 4MB”会发生什么？

结论：**不会自动“动态降帧/降分辨率”来匹配上限**。

- `max_request_body_bytes` 只用于：
  1) 序列化后做阈值检查
  2) 触发“自适应二次压缩”（仅压缩 tool_result/tool_use_input/长文本/历史）
  3) 仍超限则**本地直接拒绝发送**（返回 `Request too large ...`）
- 相关逻辑：`src/anthropic/handlers.rs:737`、`src/anthropic/handlers.rs:145`、`src/anthropic/handlers.rs:778`

并且：这次失败样本的 `kiro_request_body_bytes` 约 `3.76~3.77MB`，本身就低于 `4,000,000`，所以即便把阈值改成 4MB，也**不会触发**本地超限分支；请求仍会照样发上游，是否 400 取决于上游校验。

## 未修复/待覆盖的 case（与本问题强相关）

1. **图片不参与自适应二次压缩**
   - 目前二次压缩策略只处理文本与历史；当图片是主要体积来源时，缺少“按预算降级”能力。
2. **JPEG 质量/编码策略不可配置**
   - 目前固定走默认 JPEG quality；无法按请求体预算在 60/50/40 等质量下逐步降低。
3. **GIF 抽帧策略与请求体预算无联动**
   - 当前抽帧只受 `<=20 帧`、`<=5fps` 约束；不看“最终请求体离阈值还差多少字节”。
4. **离线诊断受限：日志里的 request_body 经常被截断**
   - `tools/diagnose_improper_request.py` 能关联 request_body，但大包时日志会被截断，导致无法完整复原与精确归因（脚本会标 `W_TRUNCATED_LOG`）。

## 解决方案（记录方案，暂不实现）

如果未来要把这类 400 的概率降到足够低，建议按“预算驱动”的思路补齐图片降级链路（核心是把 `max_request_body_bytes` 从“被动阈值”变成“主动预算”）：

1. **在构建请求体前/后引入 image budget 估算**
   - 计算非图片开销（tools/text/history）后得到可用的 `image_budget_bytes`。
2. **GIF：按预算动态降帧**
   - 在保持覆盖时长的前提下，优先降低 `sampled_frames`（例如 16→8→4）。
3. **按预算动态降分辨率**
   - 对多图场景（含 GIF 帧）使用更严格的 `image_max_pixels_multi`，或把 GIF 帧视为“多图”强制走 multi 阈值。
4. **JPEG：引入可配置质量并做逐级回退**
   - 用 `JpegEncoder::new_with_quality`（例如 75→60→45→35）替代 `write_to` 默认路径，直到落入预算。
5. **观测性**
   - 始终记录：图片张数、图片 base64 总字节、最大单张字节、抽帧结果（帧数/每帧大小分位数），避免只能依赖“截断的 request_body”排查。


## 2026-03-05 增量结论（基于外部日志样本）

> 说明：`docker-03-05.log` 当前未提交到本仓库 `logs/` 目录。以下数据来自 2026-03-05 你提供的脚本输出，并结合原始日志片段复核。

### 数据摘要

- `diagnose_improper_request.py`
  - `Phase 1`：命中 `400 Improperly formed request` 共 `56` 次
  - `Phase 2`：`request_body` 条目 `56`，`complete: 0`、`truncated: 56`
  - 问题标签全部为 `W_TRUNCATED_LOG`
- `analyze_compression.py`
  - 扫描行数：`56,969`
  - 匹配请求：`6,196`
  - 有压缩统计：`3,518`
  - 上下文窗口平均使用率：`12.6%`
  - `>95%` 仅 `2` 次，`100%`（溢出）为 `0`
  - 上游“输入过长拒绝”：`1` 次
  - 本地“请求体超限拒绝”：`0` 次
  - 自适应二次压缩触发：`0` 次

### 新增观察（相对 03-03）

1. **400 不再只出现在 3.7MB+ 级别请求**
   - 仍有大量失败集中在 `3.7MB ~ 4.2MB`（主簇）
   - 但也出现了 `131,244 bytes`、`480,892 bytes`、`1,148,944 bytes` 等失败样本
2. **“仅靠 token 视角”不足以解释失败**
   - 多数失败请求的 `estimated_input_tokens` 在约 `1.5w ~ 3.5w`
   - 与 200k context window 相比并不高
3. **失败前压缩链路对“字节体积”帮助有限**
   - 失败样本里 `history_turns_removed=0`（历史截断未触发）
   - 多数失败样本 `bytes_saved_total` 仅几十字节
4. **日志截断是观测问题，不是根因本身**
   - `W_TRUNCATED_LOG` 说明 `request_body` 打印被截断
   - 不能据此推断“请求 JSON 一定损坏”

### 结论更新

- 根因进一步收敛为：
  - **上游对请求体“结构 + 字节形态”的校验拒绝**（尤其多模态/历史拼接场景）
  - 而非单纯“token 上下文超限”
- 03-03 的 GIF 大包结论仍成立，但 03-05 证明了：
  - **不是只有超大包才会触发**
  - 仍需针对“请求结构完整性”和“字节预算”双线治理

### 需要重点补齐的能力（优先级）

1. **字节预算驱动的请求治理**
   - 在发送前统一计算：`body_bytes`、`history_bytes`、`images_bytes(base64)`、`max_single_image_bytes`
2. **真正可触发的二次降级**
   - 当命中预算阈值时，优先裁剪高成本块（历史图片、大 tool_result、超长记忆片段）
3. **400 定向重试策略**
   - 第一次 400 后按“降级模板”重发，而不是原样重试
4. **可观测性增强**
   - 不再依赖完整 `request_body` 打印，改为结构化统计日志

## 2026-03-05 本地新版回归结果（`claude-sonnet-4.5`）

> 说明：以下结果来自 2026-03-05 本地执行 `tools/test_400_improperly_formed.py`（TC-01~TC-09，全量跑）。

### 结果摘要

- 运行命令：
  - `uv run python3 tools/test_400_improperly_formed.py --base-url http://localhost:8990 --api-key <redacted> --model claude-sonnet-4.5`
- 汇总：
  - 总计 `34`（含 TC-09 阶梯压测 16 次）
  - 通过 `31`、失败 `0`、跳过 `0`、观测 `3`
  - **无案例泄漏到上游 `Improperly formed request`**

### 关键观察

1. `TC-03`（单 GIF）与 `TC-08d`（混合场景）均返回 `200`
   - 日志不再出现 `GIF 帧解码失败/image truncated`，而是稳定出现 `GIF 已抽帧并重编码`
2. `TC-09` 阶梯压测（`0.5MB ~ 4.0MB`，步长 `0.5MB`，每档 `2` 次）全部 `200`
   - 未出现“首次失败拐点”
3. 大图路径触发了“文件过大强制重编码”
   - 典型日志：`original_bytes=511877 -> final_bytes=407244`（压缩约 `20.4%`）
   - 这意味着脚本里的 `payload_est`（进入 `/v1/messages` 前估算）不等于最终上游请求体字节数

### 当前结论（截至 2026-03-05）

- 在当前代码与当前测试脚本下，**未能复现历史日志中的上游 400**。
- 历史结论（03-03/03-05 外部日志）仍保留为“曾发生过的真实现象”，但在本地最新版上暂无复现。
- 后续策略：暂不继续为“纯构造压测”扩展复杂变体，等待新的真实失败样本后再做定向排查。

## 实测案例矩阵（可直接执行）

下面给一组“先复现、再验证修复”的测试案例，按优先级从高到低。
> 注意：该矩阵是“复现导向”用例，不代表在当前版本上一定会稳定失败。

### 前置条件

- 服务启动：`make dev` 或 `cargo run`
- 打开 DEBUG/INFO 日志
- 准备脚本：`tools/diagnose_improper_request.py`、`tools/analyze_compression.py`
- 每个案例记录：
  - HTTP 状态码
  - 是否出现 `Improperly formed request`
  - `kiro_request_body_bytes`
  - `estimated_input_tokens`
  - 压缩统计（`bytes_saved_total` / `history_turns_removed`）

### TC-01 空消息内容（基线防线）

- 目标：验证本地空 content 拦截仍有效
- 输入：`messages=[{"role":"user","content":""}]`
- 预期：本地 `400`（不是上游 `Improperly formed request`）
- 参考：`tools/test_empty_content.py`

### TC-02 tool_use/tool_result 配对异常（结构完整性）

- 目标：验证“孤立 tool 事件”不会漏到上游
- 输入：构造包含 `tool_result` 但缺失对应 `tool_use` 的历史
- 预期：
  - 理想：本地修复后正常转发或本地明确报错
  - 禁止：直接落到上游兜底 `Improperly formed request`

### TC-03 单 GIF 大包（03-03 复现）

- 目标：复现 `3.7MB+` 主簇失败
- 输入：与历史相近的长 GIF，触发抽帧（约 16 帧）
- 预期（当前版本）：高概率上游 `400 Improperly formed request`
- 观测重点：`GIF 已抽帧并重编码` + `kiro_request_body_bytes` 约 MB 级

### TC-04 多图中包（03-05 非极大包失败复现）

- 目标：验证“非 3MB+ 也可能失败”
- 输入：2~4 张中等图片 + 长 Mnemosyne 文本 + 较长历史
- 预期（当前版本）：存在失败概率，且 `kiro_request_body_bytes` 可在 `0.4MB~1.5MB`

### TC-05 高文本低图片（排除法）

- 目标：区分“纯文本过长”与“多模态结构问题”
- 输入：超长文本（含 quoted message / memory），不带图片
- 预期：
  - 若成功：说明图片链路是更强触发因子
  - 若失败：需要进一步检查结构字段和历史拼接

### TC-06 流式模式回归（03-05 两次流式失败样本）

- 目标：验证流式路径和非流式路径的差异
- 输入：与 TC-04 类似 payload，分别 `stream=true/false`
- 预期：记录两种模式失败率差异；若仅一侧高失败，优先排查该路径转换差异

### TC-07 字节阈值阶梯测试（预算验证）

- 目标：确认“失败拐点”区间
- 输入：同一语义 payload，逐步增加图片/历史（例如 0.2MB、0.5MB、1MB、1.5MB、3MB+）
- 预期：得到一条“请求体字节 vs 失败率”曲线
- 输出：作为后续 `max_request_body_bytes` 与降级策略参数依据

### TC-08 失败后降级重试（修复验收用）

- 目标：验证未来修复方案是否有效
- 步骤：
  - 先发送原始大 payload（预期第一次失败）
  - 按降级策略（裁历史图片/降帧/降质）自动重试
- 预期：第二次成功率明显提升，且日志可解释“裁剪了什么”

### 建议的验收标准

- 复现场景（TC-03/04/06）可稳定触发（用于验证问题存在）
- 引入修复后：
  - `Improperly formed request` 发生率显著下降
  - 不出现“本地静默截断导致语义错乱”
  - 日志可以清晰说明每次降级动作与节省字节
