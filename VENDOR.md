# 供应商（Vendor）配置指南

## 概述

Kiro 支持对接多个 Key 供应商，自动接收 webhook 推送并提取凭据入库。每家供应商的余额、库存、事件列表完全隔离。

> **先分清两个 kiroapp**：`kiroapp.io` 与 `kiroapp.cc` 是**两家不同的供应商**，
> 协议、接口、能力都不一样，配置里必须用不同的 `flavor` 区分：
>
> | 域名 | `flavor` | 接口前缀 | 能力 |
> |---|---|---|---|
> | kiroapp.io | `"kiroapp"` | `/api/me/*` | 功能完整：阶梯定价、积分流水、密钥列表、webhook 推送 |
> | kiroapp.cc | `"kiroapp-cc"` | `/openapi/*` | 只有库存 / 余额 / 提取三个接口，**无 webhook** |
>
> 写成 `"flavor": "kiroapp"` 却填 kiroapp.cc 的地址，会对着不存在的 `/api/me/*`
> 发请求，症状是一片 404。

> **`drop` 与 `legacy` 也容易混**：两家都用 `X-API-Key: usr-xxx`，Key 的形态一样，
> 但接口完全不同（`/api/v1/*` vs `/api/my/*`）。填错的症状是 404 而不是 401 ——
> 鉴权头对得上，路径对不上。

## 快速开始

### 单供应商配置（简化格式）

如果只对接一家供应商，可以使用简化的顶层 `vendor` 对象：

```json
{
  "vendor": {
    "baseUrl": "https://kiroapp.io",
    "apiKey": "km_xxxxxxxxxxxxxxxx",
    "flavor": "kiroapp",
    "webhookPathToken": "whk_your_webhook_token",
    "autoPurchase": true,
    "autoPurchaseMaxCount": 5
  }
}
```

### 多供应商配置

对接多家时用顶层 `vendors` 数组。注意它与 `vendor` **平级**，不是嵌在 `vendor` 里面：

```json
{
  "vendors": [
    {
      "id": "kiroapp-io",
      "name": "kiroapp.io",
      "flavor": "kiroapp",
      "baseUrl": "https://kiroapp.io",
      "apiKey": "km_key_1",
      "webhookPathToken": "whk_token_1",
      "autoPurchase": true,
      "autoPurchaseMaxCount": 10,
      "defaultGroups": ["premium"],
      "defaultRpmLimit": 100
    },
    {
      "id": "kiroapp-cc",
      "name": "kiroapp.cc",
      "flavor": "kiroapp-cc",
      "baseUrl": "https://kiroapp.cc",
      "apiKey": "km_key_2",
      "defaultGroups": ["standard"],
      "defaultRpmLimit": 50
    }
  ]
}
```

`vendor` 与 `vendors` 可同时存在，`vendor` 视为排在最前的一家，按 `id` 去重。

## 配置字段说明

### 必填字段

| 字段 | 说明 | 示例 |
|------|------|------|
| `id` | 供应商唯一标识（英文、数字、`_`、`-`） | `"primary"` |
| `name` | 显示名称（前端标签页显示） | `"主供应商"` |
| `flavor` | 协议类型（见下文） | `"kiroapp"` |
| `baseUrl` | API 基础地址 | `"https://kiroapp.io"` |
| `apiKey` | 鉴权密钥（`legacy` 为 `usr-xxx`，两家 kiroapp 为 `km_xxx`） | `"km_xxxx"` |

### 可选字段

| 字段 | 说明 | 默认值 |
|------|------|--------|
| `webhookPathToken` | Webhook 入站路径 token，不配置则无法接收推送 | 无 |
| `autoPurchase` | 是否自动提取 | `false` |
| `autoPurchaseMaxCount` | 单次提取上限 | `1` |
| `autoPurchaseSchedule` | 时段表（见下文） | 无 |
| `defaultGroups` | 提取入库时写入凭据的分组 | `[]` |
| `defaultRpmLimit` | RPM 限流值 | `300` |
| `defaultApiRegion` | 凭据的 `apiRegion`（空串=沿用全局） | `""` |
| `defaultAuthRegion` | 凭据的 `authRegion`（空串=沿用全局） | `""` |

### 已废弃的旧字段名

早期文档用过下面这组名字，为兼容存量配置仍可识别，**新配置请用正名**：

| 旧名 | 正名 |
|------|------|
| `inboundToken` | `webhookPathToken` |
| `autoPurchaseWindows` | `autoPurchaseSchedule` |
| 时段表里的 `start` / `end` | `from` / `to` |
| 顶层 `kiroapp` 配置块 | `vendors` 里 `"flavor": "kiroapp-cc"` 的一项 |

顶层 `kiroapp` 块指的是 kiroapp**.cc**（历史命名，容易与 `flavor: "kiroapp"`
即 kiroapp**.io** 混淆），启动时会自动转成 id 为 `kiroapp-cc` 的普通供应商。

## 协议类型（flavor）

Kiro 支持三种供应商协议。能力差异由代码里的能力集决定，前端据此隐藏不支持的卡片。

### `legacy` - 首家卖家协议

`/api/my/*` + `X-API-Key: usr-xxx`。

- ✅ 余额查询、库存查询、按订单提取、兑换码充值
- ✅ Webhook 推送（新 Key、全部失效）
- ✅ 系统状态查询、开号记录 —— **仅本协议独有**
- ✅ Webhook 地址可通过 API 远程读写 —— **仅本协议独有**
- ❌ 不支持阶梯定价、逐张密钥元数据、积分流水

### `kiroapp` - kiroapp.io 协议

`/api/me/*` + `Authorization: Bearer km_xxx`。

- ✅ 余额查询、库存查询、按订单提取、兑换码充值
- ✅ Webhook 推送（载荷带 `client_order_id`，幂等键由卖家派生）
- ✅ 阶梯定价（单价按母号累计产量分档，同一单里各 Key 可能不同价）
- ✅ 逐张密钥元数据（账号、价格、是否有密码）
- ✅ 批次订单 ID（`orderId`，可只拉该批次产出的 Key）
- ✅ 积分流水、我的密钥列表、最早密钥时间
- ❌ 无系统状态与开号记录（那是 `legacy` 独有）
- ❌ Webhook 地址**只能在卖家网页**「设置 → Webhook 配置」里填，没有开放 API

**协议名大小写不敏感**：`"kiroapp"` / `"kiroApp"` / `"kiroapp.io"` 都能识别。

### `kiroapp-cc` - kiroapp.cc 协议

`/openapi/*` + `Authorization: Bearer km_xxx`。**只有三个接口**，是能力最少的一家：

- ✅ 库存查询 (`GET /openapi/stock`)
- ✅ 余额查询 (`GET /openapi/balance`)
- ✅ 提取 (`POST /openapi/claim`)，`count=1` 时不带参数，`count>1` 传 `{"count":N}`
- ❌ **无 webhook**，只能在面板上手动提取
- ❌ 无阶梯定价、无积分流水、无密钥列表、无系统状态
- ❌ **claim 无幂等键** —— 超时无法区分「未扣费」与「已扣费但响应丢失」，
  因此不做自动重试；提取失败请到卖家侧核对再决定是否重发

**协议名可写** `"kiroapp-cc"` / `"kiroappCc"` / `"kiroapp.cc"`。

> 提取成功（2xx）时的响应解析刻意宽松：先按文档形态解析 `{"key":..}` /
> `{"keys":[..]}`，失败则降级为按 `ksk_` 前缀扫描（连裸文本响应也能捞出）。
> 原因是这个接口一旦返回 2xx 钱就已经扣了，若因响应结构不认识就报错，
> 等于把付过费的 Key 丢掉。一个都没捞到时会告警并提示人工核对扣费。

### `drop` - Kiro Drop（drop.kiro.ss）协议

`/api/v1/*` + `X-API-Key: usr-xxx`。鉴权头与 `legacy` 相同，但接口形态完全不同，
**只有四个端点**：

| 端点 | 用途 |
|---|---|
| `GET /api/v1/reservation?quantity=N` | 报价 + 库存 + 限购 + 余额（一次拿全） |
| `POST /api/v1/reservation` | 扣余额下单：200 直接出货，202 待对账 |
| `GET /api/v1/orders/{order_id}` | 202 之后轮询取 Key |
| `PUT /api/my/webhook`、`POST /api/my/webhook/test` | webhook 地址读写与测试推送 |

- ✅ 库存 / 限购 / 余额查询（都由报价接口派生，不额外发请求）
- ✅ 提取，参数名是 `quantity`（不是别家的 `count`），订单号 `client_order_id`
- ✅ Webhook 推送与远程管理（地址只能写、读不回来）
- ❌ 无兑换码充值、无订单列表（只能按 `order_id` 查单条）
- ❌ 无阶梯定价、无积分流水、无密钥列表、无系统状态与开号记录

**协议名可写** `"drop"` / `"kiro-drop"` / `"drop.kiro.ss"`。

几处与别家不同、值得单独知道的地方：

**金额以人民币计。** 报价同时给 USD 与 CNY，实际扣款走 `total_amount_cny`，
因此面板上这家的单价、扣费、余额统一是 CNY，与别家的积分不同币种，不要横向比。

**下单可能是异步的。** 返回 202 + `status:"pending"` 表示钱已经扣了但 Key 还没
定下来。此时后端会拿 `order_id` 每 3 秒轮询一次、最多 20 次（约 1 分钟）。
轮询用尽仍未出货**不报错**，而是把已知信息返回并告警 —— 钱确实扣了，报错会让人
误以为没花钱，请按日志里的 `order_id` 到卖家侧核对。**任何情况下都不要换一个
订单号重下**，那等于再扣一次。

**不传 `max_total_cny`。** 卖家文档建议传它做涨价保护，但同一订单号重试时参数
必须一致（否则 409），而重试时重新报出的价格不保证还是原值 —— 传了会把一次本可
幂等重放的重试变成永久失败。涨价风险由 `autoPurchaseMaxCount` 兜着。

**推送里没有订单号。** 本家的 `batch.completed` 只给 `batch_id`，而下单必须自带
32 位十六进制订单号，因此订单号由后端从 `(供应商 id, event_id)` 哈希派生。同一条
推送重投多少次都得到同一个订单号，卖家侧幂等重放照常生效。

**事件名不同。** 新批次上架叫 `batch.completed`（不是别家的 `new_keys_available`），
后端已归一到同一类，自动提取的判定链条与别家一致。该事件**不带数量**，此时自动
提取按「卖家当前可提取上限」与 `autoPurchaseMaxCount` 取小，不会因为缺这个数就提不出来。

**库存为空时报价接口返 400。** 报价的 `quantity` 参与校验，超过库存直接 400。
后端对「库存不足」这一类文案降级为「可提取 0 个」，面板会正常显示 0 而不是一条
报错；其余 400（如订单号格式错）仍照常上报。

## 时段表配置

通过 `autoPurchaseSchedule` 限制自动提取仅在特定时段生效，可设置不同时段的不同上限：

```json
{
  "autoPurchase": true,
  "autoPurchaseMaxCount": 5,
  "autoPurchaseSchedule": [
    { "from": "09:00", "to": "12:00", "maxCount": 3 },
    { "from": "14:00", "to": "23:00", "maxCount": 10 }
  ]
}
```

**行为说明**：
- 当前时刻在 `09:00-12:00` 时，单次最多提取 3 个
- 当前时刻在 `14:00-23:00` 时，单次最多提取 10 个
- 当前时刻不在任何时段内时，回退到 `autoPurchaseMaxCount = 5`
- 未配置时段表则全天使用 `autoPurchaseMaxCount`
- `to` 早于 `from` 视为跨午夜（如 `22:00`–`02:00`），边界两端都含
- 时刻按**本地时区**判定，容器内需正确设置 `TZ`
- 时刻格式写错的那一段会被忽略并告警，退回兜底上限（宁可少提，不因笔误多扣费）

## Webhook 配置

`legacy`、`kiroapp`（.io）与 `drop` 支持推送，`kiroapp-cc` 没有 webhook。

每家供应商需要独立配置 webhook 入站地址：

1. 在配置文件中设置 `webhookPathToken`（用不可猜测的随机串）
2. 供应商侧配置 webhook URL 为：
   ```
   http://your-server:8990/webhook/vendor/{webhookPathToken}
   ```
3. 后端按 token 反查归属哪一家供应商

**示例**：
- 供应商 A 的 `webhookPathToken = "whk_abc123"` → `/webhook/vendor/whk_abc123`
- 供应商 B 的 `webhookPathToken = "whk_xyz789"` → `/webhook/vendor/whk_xyz789`

注意路径是 `/webhook/vendor/...`，**不在 `/api` 下**，也不需要 `adminApiKey`
认证 —— 对方推送端不带签名，那个不可猜测的路径段本身就是唯一凭证，比对不上
直接 404。入站只负责落库与告警，不触发任何扣费。

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
3. 存量顶层 `kiroapp` 块（即 kiroapp.cc）转为 `id = "kiroapp-cc"` 的供应商
4. 旧字段名（`inboundToken` / `autoPurchaseWindows` / `start` / `end`）按别名识别

**无需手动操作**，数据兼容性由后端保证。

## API 端点

所有管理接口支持 `?vendorId=xxx` 参数，缺省时使用配置中的第一家：

- `GET /api/admin/vendor/vendors` - 获取供应商清单与能力集
- `GET /api/admin/vendor/status?vendorId=xxx` - 单个供应商状态
- `GET /api/admin/vendor/events?vendorId=xxx` - 事件列表
- `POST /api/admin/vendor/events/ack?vendorId=xxx` - 确认事件
- `POST /api/admin/vendor/purchase?vendorId=xxx` - 直接提取
- `POST /api/admin/vendor/events/{eventId}/purchase?vendorId=xxx` - 按事件提取
- `GET /api/admin/vendor/orders?vendorId=xxx` - 订单历史
- `PUT /api/admin/vendor/mode?vendorId=xxx` - 切换自动/手动模式
- `POST /api/admin/vendor/redeem?vendorId=xxx` - 兑换码充值

以下两个仅部分协议支持，不支持时返回「该卖家不支持…」：

- `GET /api/admin/vendor/ledger?vendorId=xxx` - 积分流水（仅 `kiroapp`）
- `GET /api/admin/vendor/keys?vendorId=xxx` - 我的密钥列表（仅 `kiroapp`）

Webhook 远程管理仅 `legacy` 支持：

- `PUT /api/admin/vendor/webhook?vendorId=xxx` - 设置 webhook URL
- `POST /api/admin/vendor/webhook/test?vendorId=xxx` - 触发测试推送

## 常见问题

### Q: 已有单供应商配置，如何迁移到多供应商？

A: 不迁移也能用 —— `vendor` 单例会被视为 id 为 `default` 的一家。想改成数组形式时，
注意 `vendors` 是**顶层字段**，不要嵌进 `vendor` 里：

```json
// 旧配置
{
  "vendor": {
    "baseUrl": "...",
    "apiKey": "..."
  }
}

// 新配置（vendors 与 vendor 平级）
{
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
```

保留 `id: "default"` 是有意的：存量 webhook 事件按这个 id 落库，改了会让历史事件
与新配置对不上。

### Q: 顶层 `kiroapp` 配置块还能用吗？

A: 能，但已废弃。它指的是 kiroapp**.cc**，启动时自动转成 id 为 `kiroapp-cc`、
flavor 为 `kiroapp-cc` 的普通供应商。新配置请直接写进 `vendors`。若 `vendors` 里
已有同 id 的项，以显式配置为准。

### Q: flavor 填错会怎样？

A: 启动时会报错并提示可选值，**不会静默回退**：

```
无法识别的卖家协议风味 "unknown"，可选值: legacy, kiroapp, kiroapp-cc
```

刻意不回退默认值 —— 拼错的 flavor 若被当成 `legacy`，会对着错误的路径和鉴权头
发请求，症状是一片 401/404，很难定位。

### Q: 两家供应商能用相同的 webhookPathToken 吗？

A: 不能。每家的 token 必须唯一，否则无法正确路由 webhook。

### Q: 如何测试 webhook 是否配置正确？

A: 仅 `legacy` 协议可以：前端供应商页面有「测试推送」按钮，点击后让供应商推一条
测试消息到已保存的 webhook URL。`kiroapp`（.io）没有这个 API，只能在卖家网页里
配好地址后等真实推送；`kiroapp-cc` 根本没有 webhook。

## 完整配置示例

参考项目根目录的 `config.example.json`。
