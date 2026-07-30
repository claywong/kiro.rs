# 供应商（Vendor）配置指南

## 概述

Kiro 支持对接多个 Key 供应商，自动接收 webhook 推送并提取凭据入库。每家供应商的余额、库存、事件列表完全隔离。

## 快速开始

### 单供应商配置（简化格式）

如果只对接一家供应商，可以使用简化配置：

```json
{
  "vendor": {
    "baseUrl": "https://api.kiroapp.io",
    "apiKey": "usr-xxxxxxxxxxxxxxxx",
    "flavor": "kiroapp",
    "inboundToken": "whk_your_webhook_token",
    "autoPurchase": true,
    "autoPurchaseMaxCount": 5
  }
}
```

### 多供应商配置

对接多家供应商时，将配置改为数组：

```json
{
  "vendor": {
    "vendors": [
      {
        "id": "primary",
        "name": "主供应商",
        "flavor": "kiroapp",
        "baseUrl": "https://api.kiroapp.io",
        "apiKey": "usr-key-1",
        "inboundToken": "whk_token_1",
        "autoPurchase": true,
        "autoPurchaseMaxCount": 10,
        "defaultGroups": ["premium"],
        "defaultRpmLimit": 100
      },
      {
        "id": "backup",
        "name": "备用供应商",
        "flavor": "legacy",
        "baseUrl": "https://legacy.example.com",
        "apiKey": "usr-key-2",
        "inboundToken": "whk_token_2",
        "autoPurchase": false,
        "defaultGroups": ["standard"],
        "defaultRpmLimit": 50
      }
    ]
  }
}
```

## 配置字段说明

### 必填字段

| 字段 | 说明 | 示例 |
|------|------|------|
| `id` | 供应商唯一标识（英文、数字、`_`、`-`） | `"primary"` |
| `name` | 显示名称（前端标签页显示） | `"主供应商"` |
| `flavor` | 协议类型（见下文） | `"kiroapp"` |
| `baseUrl` | API 基础地址 | `"https://api.kiroapp.io"` |
| `apiKey` | 鉴权密钥 | `"usr-xxxx"` |

### 可选字段

| 字段 | 说明 | 默认值 |
|------|------|--------|
| `inboundToken` | Webhook 入站 token，不配置则无法接收推送 | 无 |
| `autoPurchase` | 是否自动提取 | `false` |
| `autoPurchaseMaxCount` | 单次提取上限 | `1` |
| `autoPurchaseWindows` | 时段表（见下文） | 无 |
| `defaultGroups` | 提取入库时写入凭据的分组 | `[]` |
| `defaultRpmLimit` | RPM 限流值 | `10` |
| `defaultApiRegion` | 凭据的 `apiRegion`（空串=沿用全局） | `""` |
| `defaultAuthRegion` | 凭据的 `authRegion`（空串=沿用全局） | `""` |

## 协议类型（flavor）

Kiro 支持两种供应商协议：

### `legacy` - 首家卖家协议

最早对接的卖家，功能基础：
- ✅ 余额查询 (`/api/my/profile`)
- ✅ 库存查询 (`/api/my/keys`)
- ✅ 按订单提取 (`/api/purchase`)
- ✅ 兑换码充值 (`/api/redeem`)
- ✅ Webhook 推送（新 Key、全部失效）
- ❌ 不支持阶梯定价
- ❌ 不支持逐张密钥元数据

### `kiroapp` - kiroapp.io 协议

新一代协议，功能完整：
- ✅ 所有 `legacy` 功能
- ✅ 阶梯定价（批量提取时单价递减）
- ✅ 逐张密钥元数据（账号、价格、是否有密码）
- ✅ 幂等订单（重复提交不重复扣款）
- ✅ 批次订单 ID（`orderId`）
- ✅ 系统状态查询 (`/api/status`)
- ✅ 开号记录 (`/api/gen-logs`)

**协议名大小写不敏感**：`"kiroapp"` / `"kiroApp"` / `"KIROAPP"` / `"kiroapp.io"` 都能识别。

## 时段表配置

通过 `autoPurchaseWindows` 限制自动提取仅在特定时段生效，可设置不同时段的不同上限：

```json
{
  "autoPurchase": true,
  "autoPurchaseMaxCount": 5,
  "autoPurchaseWindows": [
    { "start": "09:00", "end": "12:00", "maxCount": 3 },
    { "start": "14:00", "end": "23:00", "maxCount": 10 }
  ]
}
```

**行为说明**：
- 当前时刻在 `09:00-12:00` 时，单次最多提取 3 个
- 当前时刻在 `14:00-23:00` 时，单次最多提取 10 个
- 当前时刻不在任何时段内时，回退到 `autoPurchaseMaxCount = 5`
- 未配置时段表则全天使用 `autoPurchaseMaxCount`

## Webhook 配置

每家供应商需要独立配置 webhook 入站地址：

1. 在配置文件中设置 `inboundToken`（建议用随机字符串）
2. 供应商侧配置 webhook URL 为：
   ```
   http://your-server:8990/api/vendor/webhook/{inboundToken}
   ```
3. 后端会根据 token 路由到对应供应商

**示例**：
- 供应商 A 的 `inboundToken = "whk_abc123"`，webhook 地址为 `/api/vendor/webhook/whk_abc123`
- 供应商 B 的 `inboundToken = "whk_xyz789"`，webhook 地址为 `/api/vendor/webhook/whk_xyz789`

## 前端界面

### 单供应商
不显示标签页，直接展示该供应商的内容。

### 多供应商
- 顶部显示标签页切换器
- 每个标签显示供应商名称
- 有未确认事件时显示红点数字
- 余额、库存、事件列表、订单历史完全隔离

## 数据迁移

首次升级到多供应商版本时，系统会自动：
1. 给存量 `vendor_events` 表的记录补 `vendor_id = "default"`
2. 单供应商配置自动转为 `id = "default"` 的供应商

**无需手动操作**，数据兼容性由后端保证。

## API 端点

所有管理接口支持 `?vendorId=xxx` 参数，缺省时使用配置中的第一家：

- `GET /api/admin/vendor/vendors` - 获取供应商清单与能力集
- `GET /api/admin/vendor/status?vendorId=xxx` - 单个供应商状态
- `GET /api/admin/vendor/events?vendorId=xxx` - 事件列表
- `POST /api/admin/vendor/purchase?vendorId=xxx` - 直接提取
- `POST /api/admin/vendor/events/:id/purchase?vendorId=xxx` - 按事件提取

## 常见问题

### Q: 已有单供应商配置，如何迁移到多供应商？

A: 将现有配置包装进 `vendors` 数组即可：

```json
// 旧配置
{
  "vendor": {
    "baseUrl": "...",
    "apiKey": "..."
  }
}

// 新配置
{
  "vendor": {
    "vendors": [
      {
        "id": "default",
        "name": "供应商",
        "flavor": "legacy",
        "baseUrl": "...",
        "apiKey": "..."
      }
    ]
  }
}
```

### Q: flavor 填错会怎样？

A: 启动时会报错并提示可选值：

```
无法识别的卖家协议风味 "unknown"，可选值: legacy, kiroapp
```

### Q: 两家供应商能用相同的 inboundToken 吗？

A: 不能。每家的 token 必须唯一，否则无法正确路由 webhook。

### Q: 如何测试 webhook 是否配置正确？

A: 前端供应商页面有「测试推送」按钮，点击后会让供应商推送一条测试消息到已保存的 webhook URL。

## 完整配置示例

参考项目根目录的 `config.example.json`。
