# 多媒体输入（图片 / GIF / 视频）

本项目在 Anthropic → Kiro 的协议转换过程中，会把 `messages[].content` 里的 `image` 内容块转换为上游 Kiro API 的 `images[]` 字段。

为避免上游对“大请求体/异常输入”返回 `400 Improperly formed request`，对 GIF 做了专门处理（抽帧→静态图）。

## 图片（静态）

当前支持的静态图片 `source.media_type`：

- `image/jpeg`
- `image/png`
- `image/webp`

处理策略：

- 读取尺寸后按配置做等比缩放（长边/像素上限）
- 保持原格式重编码（仅在需要缩放时）

## GIF（动图）

### 为什么不能直接透传 GIF

GIF 常见特征是「分辨率不大但字节体积巨大」（多帧 + 调色板 + 压缩特性），如果把整段 GIF base64 原样塞进上游请求体，极易触发上游的请求体大小/校验限制（表现为 `400 Improperly formed request` 这类兜底错误）。

### 当前实现：抽帧输出多张静态图

当检测到 `source.media_type=image/gif` 时，会：

1. 解码 GIF，计算总时长与源帧数
2. 按时间轴采样（见下方采样策略）
3. 对被选中的帧按同样的缩放规则处理，并重编码为静态 `jpeg`
4. 将这些帧按时间顺序写入上游请求的 `images[]`

### 已知问题与排查指引

即使请求体总大小低于本地 `max_request_body_bytes` 的安全阈值，多图（尤其是 GIF 抽帧后的多张 `jpeg`）在某些情况下仍可能触发上游返回 `400 Improperly formed request`（上游兜底错误，原因可能包含但不限于：请求体大小/图片校验/字段约束等）。

排查与应对策略见：`docs/troubleshooting/400-improperly-formed-request.md`。

### 采样策略（固定上限，按时长自适应）

约束：

- 总输出帧数 `<= 20`
- 采样频率 `<= 5 fps`

规则（等价描述）：

- `fps = min(5, floor(20 / ceil(duration_seconds)))`
- `interval_ms = 1000 / fps`
- 若 `duration_seconds` 很大导致 `fps=0`，则按 `interval_ms = ceil(duration_ms / 20)` 做均匀采样

例子：

- 8 秒 GIF：`floor(20/8)=2` → `2 fps` → `interval=500ms` → 最多约 `16` 张
- 4 秒 GIF：`floor(20/4)=5` → `5 fps` → `interval=200ms` → 最多 `20` 张

## 视频（mp4 / mov 等）

当前状态：**未实现**。

说明：

- Anthropic 的 `image` 内容块规范里通常不会发送 `video/*`，因此本项目当前也不会把 `video/mp4`、`video/quicktime` 等媒体类型转换成上游 `images[]`（等价于不支持/忽略）。

### 规划方案（记录，暂不实现）

如果未来需要支持“视频 → 多张静态图”的输入，推荐方案是引入视频解码能力（通常依赖 `ffmpeg/ffprobe`）：

1. `ffprobe` 读取视频时长（毫秒）
2. 复用 GIF 的采样策略计算 `interval_ms`，并限制 `<=20` 帧、`<=5fps`
3. `ffmpeg` 按时间点抽帧（导出 `jpeg/webp`）
4. 每帧按现有缩放规则处理后 base64 化，写入 `images[]`

建议配套的安全/稳定性约束（避免 DoS 与请求体爆炸）：

- 限制输入视频最大字节数与最大时长（超限直接 400）
- 限制抽帧后总输出图片字节数（超限则降低 fps/分辨率/质量，或回退更少帧）
- 使用临时目录并确保清理；并发下避免落盘文件名冲突
