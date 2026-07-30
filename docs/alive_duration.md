# 凭据存活时间统计功能

## 功能概述

从 v0.7.4 开始，kiro.rs 为每个凭据添加了**存活时间**统计功能。该功能记录凭据从录入到被封禁期间的累计可用时长，帮助用户评估凭据的实际使用寿命和质量。

## 新增字段

在 `KiroCredentials` 结构中新增了三个字段：

### 1. `createdAt` (RFC3339 格式)
- **描述**: 凭据首次录入系统的时间
- **类型**: `Option<String>`
- **序列化名**: `createdAt`
- **行为**:
  - 新增凭据时自动记录当前时间
  - 导入已有凭据时可手动指定
  - 如果未指定，则为 `None`

### 2. `aliveDurationSecs` (秒)
- **描述**: 凭据累计可用时长（秒）
- **类型**: `u64`
- **序列化名**: `aliveDurationSecs`
- **默认值**: `0`
- **行为**:
  - 凭据处于启用状态（`disabled=false`）时，每次成功调用后更新
  - 凭据被禁用时停止累计
  - 重新启用后继续从上次值累加

### 3. `lastAliveUpdateAt` (RFC3339 格式)
- **描述**: 最后一次更新存活时长的时间
- **类型**: `Option<String>`
- **序列化名**: `lastAliveUpdateAt`
- **行为**:
  - 仅在凭据可用时有意义
  - 用于计算增量时长
  - 禁用时不更新此字段（停止计时）

## 工作原理

### 凭据创建
当通过 Admin API 添加新凭据时：
```rust
created_at: Some(now.clone()),        // 记录创建时间
alive_duration_secs: 0,               // 初始存活时长为 0
last_alive_update_at: Some(now),      // 初始化最后更新时间为创建时间
```

### 成功调用后更新
每次凭据成功处理请求后（`report_success`），如果凭据未被禁用：
1. 计算从 `lastAliveUpdateAt` 到当前时间的增量（秒）
2. 将增量累加到 `aliveDurationSecs`
3. 更新 `lastAliveUpdateAt` 为当前时间

```rust
if !entry.disabled {
    if let Some(last_update) = &entry.credentials.last_alive_update_at {
        if let Ok(last_time) = chrono::DateTime::parse_from_rfc3339(last_update) {
            let elapsed_secs = (now - last_time.with_timezone(&Utc)).num_seconds();
            if elapsed_secs > 0 {
                entry.credentials.alive_duration_secs += elapsed_secs as u64;
            }
        }
    }
    entry.credentials.last_alive_update_at = Some(now.to_rfc3339());
}
```

### 凭据被禁用
当凭据被禁用时（无论是手动禁用还是自动禁用）：
1. 先调用 `finalize_alive_duration()` 更新存活时长（累计到禁用时刻）
2. 将 `disabled` 设置为 `true`
3. **不再更新** `lastAliveUpdateAt`（停止计时）

禁用场景包括：
- 连续失败达到阈值（`TooManyFailures`）
- 账号被封禁/停用（`Suspended`）
- 额度已用尽（`QuotaExceeded`）
- Token 刷新失败（`TooManyRefreshFailures`）
- Refresh Token 永久失效（`InvalidRefreshToken`）
- 手动禁用（`Manual`）

### 凭据重新启用
当凭据被重新启用时：
1. 重置 `lastAliveUpdateAt` 为当前时间（重新开始计时）
2. 保留之前累计的 `aliveDurationSecs`（继续累加）

```rust
// 重新启用时，重置最后更新时间为当前时间（重新开始计时）
entry.credentials.last_alive_update_at = Some(now.to_rfc3339());
```

## 使用示例

### 查看凭据存活时间

通过 Admin API 获取凭据信息时，响应中会包含存活时间字段：

```json
{
  "id": 1,
  "email": "user@example.com",
  "disabled": false,
  "createdAt": "2026-07-01T00:00:00Z",
  "aliveDurationSecs": 259200,
  "lastAliveUpdateAt": "2026-07-04T00:00:00Z"
}
```

在这个例子中：
- 凭据创建于 2026-07-01
- 累计可用时长为 259200 秒（3 天）
- 最后一次更新时间为 2026-07-04

### 计算凭据实际存活率

存活时长可以用来评估凭据质量：

```python
# 总时长 = 当前时间 - 创建时间
total_duration = now - created_at

# 存活率 = 存活时长 / 总时长
alive_rate = alive_duration_secs / total_duration.total_seconds()

# 例如：凭据创建 7 天，实际可用 3 天，存活率 = 3/7 ≈ 43%
```

### 识别高质量凭据

- **存活时间长**：说明凭据稳定，很少被封禁
- **存活率高**：说明凭据被禁用后快速恢复，或者很少出现故障
- **创建时间早但存活时间长**：说明凭据质量好，值得长期使用

## 持久化

存活时间字段会随凭据配置持久化到 `credentials.json`：

```json
[
  {
    "refreshToken": "xxx",
    "createdAt": "2026-07-01T00:00:00Z",
    "aliveDurationSecs": 259200,
    "lastAliveUpdateAt": "2026-07-04T00:00:00Z"
  }
]
```

重启后这些统计数据会被保留。

## 向后兼容

- 旧版本的 `credentials.json` 缺少这些字段时，反序列化会使用默认值
- `createdAt` 和 `lastAliveUpdateAt` 为 `None`
- `aliveDurationSecs` 为 `0`
- 这些字段为空或为 0 时不会序列化到 JSON 中（减少文件大小）

## 注意事项

1. **时间精度**: 存活时间以秒为单位统计，不包含毫秒
2. **冷却期间**: 凭据处于临时冷却状态（`throttled_until`）时，只要 `disabled=false`，仍会继续累计存活时间
3. **重启影响**: 如果凭据在系统重启期间保持可用状态，重启期间的时长不会被计入（因为没有成功调用来触发更新）
4. **手动导入**: 导入已有凭据时，可以手动指定 `createdAt` 来记录实际创建时间

## 相关代码位置

- **数据结构定义**: `src/kiro/model/credentials.rs`
- **存活时长更新逻辑**: `src/kiro/token_manager.rs`
  - `report_success()`: 成功调用后更新
  - `set_disabled()`: 禁用时停止计时，启用时重新开始
  - `finalize_alive_duration()`: 禁用前最后更新
- **凭据创建**: `src/admin/service.rs`
  - `add_credential_inner()`: 新增凭据时初始化字段
