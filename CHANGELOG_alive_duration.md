# 凭据存活时间功能 - 变更日志

## 功能概述

为凭据添加存活时间统计，记录凭据从录入到被封禁期间的累计可用时长。

## 新增字段

### KiroCredentials 结构体新增三个字段：

1. **`created_at`** (`Option<String>`)
   - 凭据创建时间（RFC3339 格式）
   - 新增凭据时自动记录
   - 可手动指定（用于导入已有凭据）

2. **`alive_duration_secs`** (`u64`)
   - 累计存活时长（秒）
   - 初始值为 0
   - 凭据可用期间持续累计
   - 被禁用时停止累计

3. **`last_alive_update_at`** (`Option<String>`)
   - 最后一次更新存活时长的时间（RFC3339 格式）
   - 用于计算增量时长
   - 禁用时不更新（停止计时）

## 修改的文件

### 数据结构层 (`src/kiro/model/credentials.rs`)
- ✅ 在 `KiroCredentials` 结构体中添加三个新字段
- ✅ 更新 `Debug` 实现以包含新字段
- ✅ 添加序列化/反序列化支持（camelCase）
- ✅ 添加 `skip_serializing_if` 避免空值序列化
- ✅ 更新所有测试用例中的结构体初始化
- ✅ 添加针对存活时间字段的单元测试（4 个新测试）

### 凭据管理层 (`src/kiro/token_manager.rs`)
- ✅ 在 `CredentialEntry` impl 中添加 `finalize_alive_duration()` 辅助方法
- ✅ 在 `report_success()` 中添加存活时长更新逻辑
- ✅ 在 `set_disabled()` 中处理禁用/启用时的存活时长更新
- ✅ 在所有自动禁用位置调用 `finalize_alive_duration()`：
  - `report_failure_for_request()` - 连续失败禁用
  - `report_suspended_for_request()` - 账号封禁
  - `report_quota_exhausted_for_request()` - 额度用尽
  - `report_refresh_failure()` - Token 刷新失败
  - `report_refresh_token_invalid()` - Token 永久失效
  - `disable_quota_exceeded()` - 手动禁用超额凭据

### 服务层 (`src/admin/service.rs`)
- ✅ 在 `add_credential_inner()` 中初始化新字段
- ✅ 创建凭据时记录创建时间
- ✅ 初始化 `alive_duration_secs` 为 0
- ✅ 初始化 `last_alive_update_at` 为创建时间

## 行为说明

### 存活时间累计机制

1. **凭据创建**：初始化三个字段，存活时长为 0

2. **成功调用**：
   - 如果凭据未被禁用（`disabled=false`）
   - 计算从上次更新到现在的时长增量
   - 累加到 `alive_duration_secs`
   - 更新 `last_alive_update_at` 为当前时间

3. **凭据禁用**：
   - 先调用 `finalize_alive_duration()` 更新存活时长
   - 设置 `disabled=true`
   - 停止更新 `last_alive_update_at`（停止计时）

4. **凭据启用**：
   - 重置 `last_alive_update_at` 为当前时间
   - 保留之前累计的 `alive_duration_secs`
   - 继续累计存活时间

### 持久化

- 所有三个字段随凭据配置持久化到 `credentials.json`
- 空值或零值不会序列化（减少文件大小）
- 重启后统计数据保留

### 向后兼容

- 旧版本的 `credentials.json` 缺少这些字段时使用默认值
- 不影响现有功能
- 无需手动迁移数据

## 测试覆盖

新增 4 个单元测试：
- ✅ `test_alive_duration_fields_parsing` - 字段解析测试
- ✅ `test_alive_duration_fields_serialization` - 字段序列化测试
- ✅ `test_alive_duration_fields_default_not_serialized` - 空值不序列化测试
- ✅ `test_alive_duration_fields_roundtrip` - 序列化往返测试

所有现有测试（767 个）全部通过，无回归。

## 使用场景

1. **评估凭据质量**：存活时间长的凭据更稳定可靠
2. **计算存活率**：存活时长 / 总时长，识别频繁被封的账号
3. **优化采购策略**：根据历史存活数据选择优质供应商
4. **监控账号健康度**：结合成功次数、失败次数等指标综合评估
5. **成本分析**：按实际可用时长计算凭据的性价比

## 示例

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

解读：
- 创建于 7 月 1 日
- 累计可用 259200 秒（3 天）
- 最后活跃于 7 月 4 日
- 如果当前是 7 月 8 日，总存在时间 7 天，存活率 ≈ 43%

## 文档

详细文档见：`docs/alive_duration.md`

## 作者

@wangzhong

## 日期

2026-07-30
