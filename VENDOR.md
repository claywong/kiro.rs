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

> **`kiro-ooo` 与 `legacy` 最容易混，且配错最难查**：两家同样是 `/api/my/*` +
> `X-API-Key: usr-xxx`，路径与鉴权头几乎一样。但 kiro.ooo 的**余额在 `credits`**，
> 它的 `profile.remaining` 恒为 0。把 kiro.ooo 配成 `legacy` 不会 401 也不会 404，
> 而是余额显示 0、自动提取算出的可提数量恒为 0 —— **整家静默不可用且不报错**。
> 另外它的提货路径是 `/api/my/keys/claim` 而非 `/api/my/purchase`。

> **`drop` 与 `legacy` 也容易混**：两家都用 `/api/my/*` + `X-API-Key: usr-xxx`，
> 路径和鉴权头几乎一样。区别在于 Drop 的金额是**字符串**、库存来自 `/api/status`
> 而非 `/api/my/stock`。把 Drop 配成 `legacy` 不会 401，而是余额与下单结果解析
> 失败、库存查询 404 —— 比报错更难查，因为面板只显示"解析响应失败"。

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

## Credit 限额（自动限制单个 Key 的用量）

从 **kiro.red** 供应商购买的 Key 会**自动设置 4000 美元的 credit 限额**，达到限额后该 Key 自动停止被调度使用。

### 工作原理

1. **自动设置**：从 kiro.red 提取的 Key 入库时自动添加 `"creditLimit": 4000.0`
2. **每分钟统计**：后台任务每分钟从 `traces.db` 统计每个凭证的累计已用 credit
3. **自动过滤**：调度时如果某个 Key 的已用 >= 限额，则不再被选中使用
4. **只统计有效凭证**：已禁用的凭证不计入统计

### 手动设置限额

其他供应商的 Key 默认不限制，如需添加限额可手动编辑 `credentials.json`：

```json
{
  "kiroApiKey": "ksk_xxx",
  "authMethod": "api_key",
  "creditLimit": 3000.0,
  "rpmLimit": 10
}
```

设置 `creditLimit` 后，该凭证的已用 credit 达到限额时会自动停止调度。

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

Kiro 支持七种供应商协议。能力差异由代码里的能力集决定，前端据此隐藏不支持的卡片。

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

`/api/my/*` + `X-API-Key: usr-xxx`。**与 `legacy` 高度相似** —— 路径、鉴权头、
下单参数、事件名、幂等语义全都一样，可以当成 `legacy` 的裁剪版来理解：

| 端点 | 用途 |
|---|---|
| `GET /api/my/profile` | 余额（`remaining`）与已配的 webhook 地址 |
| `GET /api/status` | 系统状态，其中 `keys_stock` 是**可购买数** |
| `POST /api/my/purchase` | 下单，参数 `count` + `client_order_id` |
| `PUT /api/my/webhook`、`POST /api/my/webhook/test` | webhook 地址读写与测试推送 |

- ✅ 余额查询、库存查询、按订单提取
- ✅ Webhook 推送（`new_keys_available` / `all_keys_dead` / `test`）与远程管理
- ✅ 系统状态查询
- ❌ 无兑换码充值、无开号记录、无订单列表（`/api/my/redeem`、`/api/my/gen-logs`、
  `/api/my/purchase-orders` 实测均 404）
- ❌ 无阶梯定价、无积分流水、无密钥列表

**协议名可写** `"drop"` / `"kiro-drop"` / `"drop.kiro.ss"`。

与 `legacy` 的两处真实差异：

**金额是字符串。** 本家返回 `"remaining": "884.400000"`，首家给的是数字。legacy 的
DTO 用 `f64`，直接复用会整份解析失败（余额、下单结果全读不出来），因此本家自带
DTO，用一个 `untagged` 枚举同时接字符串与数字。金额单位是人民币。

**库存来自 `/api/status`。** 本家没有 `/api/my/stock`（404），可购买数取
`/api/status` 的 `keys_stock`。

另外两点：

**订单号形态要校验。** 文档的 webhook 示例里 `purchase_order_id` 是 `batch_xxx`，
但下单接口要求 `client_order_id` 是 32 位十六进制 —— 文档这两处自相矛盾。故后端
先校验形态：合法就直接沿用（与首家一致），不合法则从 `(供应商 id, event_id)` 哈希
派生一个合法值。派生值对同一条推送稳定，重投仍能命中卖家侧的幂等重放。

**新货事件不带数量。** `new_keys_available` 没有 `new_keys` 字段，此时自动提取按
「卖家当前 `keys_stock`」与 `autoPurchaseMaxCount` 取小，不会因为缺这个数就提不出来。

> **本家文档改过一次。** 2026-07-31 早先那版走 `/api/v1/reservation`（报价 + 下单，
> 可能返 202 待对账、金额分 USD/CNY 两套、新货事件叫 `batch.completed`），当天即
> 全部撤掉换成上表这套。若日后又对不上，**先比对
> [文档](https://drop.kiro.ss/docs) 而不是猜** —— 上一轮就是照着旧文档实现完，
> 才发现接口已经换掉了。

### `kiro-ooo` - kiro.ooo 自助台协议

`/api/my/*` + `X-API-Key: usr-xxx`。**与 `legacy` 同前缀同鉴权，但不能配成 `legacy`** ——
差异不在路径而在字段语义，配错不会 401/404，而是余额显示 0、自动提取永远提不出东西。

| 端点 | 用途 |
|---|---|
| `GET /api/my/stock` | 扁平库存（**只是退路**，给不出分区）+ **余额（`credits`）** |
| `GET /api/my/stock/regions` | **双区货架，选区的真相来源** + 余额 |
| `GET /api/my/profile` | 账号名与 webhook 地址（**不含余额**） |
| `GET /api/my/credits` | 余额 + 积分流水（`ledger[]`） |
| `POST /api/my/keys/claim` | 提货，参数 `count` + `client_order_id` + **`region`** |
| `GET /api/status` | 系统状态，**免鉴权** |
| `GET /api/my/keys` | 名下密钥，`?history=true` 含已失效，**给密钥正文** |
| `GET /api/my/keys/created-at` | 最早密钥时刻 + 累计个数 |
| `GET /api/my/purchase-orders` | 订单列表（裸数组，最近 50 条） |
| `PUT /api/my/webhook`、`POST /api/my/webhook/test` | webhook 地址读写与测试推送 |

- ✅ 余额、库存、按订单提取、系统状态、订单列表
- ✅ 积分流水、名下密钥（**带正文，可与本地凭据池逐张对账**）、最早密钥时间
- ✅ Webhook 远程管理（`PUT` 写地址 + 测试推送）
- ✅ 阶梯定价（`/api/my/key-price-tiers` 按母号累计产量分档）
- ✅ 分区库存（`us-east-1` / `eu-central-1`，**2026-08-10 起**，见下）
- ❌ 无开号记录（`/api/my/gen-logs` 实测 404）

**协议名可写** `"kiro-ooo"` / `"kiro.ooo"` / `"kiroooo"`。域名里 o 的个数容易数错，
故 `"kirooo"`（两个 o）与 `"kirooooo"`（四个 o）也都认 —— 拼错会直接报错拒启动，
容忍几个近似写法比让人排查「为什么 flavor 不认」划算。

#### 三处必须知道的差异

**余额在 `credits`，不在 `remaining`。** 本家 `/api/my/profile` 返回的
`quota` / `remaining` / `used_quota` **恒为 0**（该家不用这套配额模型），真实余额是
`credits`，且只出现在 `/api/my/stock` 与 `/api/my/credits`。照 `legacy` 映射
`balance ← remaining` 的后果是：面板余额显示 0、自动提取算出的可提数量恒为 0 ——
**整家静默不可用且不报任何错**。这是本家必须独立 flavor 的首要理由。
档案接口没有余额时，后端会补一次 `/api/my/credits?limit=1` 取那个数。

**可提数量按四个字段取小。** `/api/my/stock` 给 `claimable`（可领上限）、`stock`
（可取库存）、`afford`（**按现有积分买得起几个**）、`max`（聚合上限），语义各不相同。
只读 `claimable` 会报出一个提不到的数：实测 `claimable=2` 而 `afford=1`（45 积分、
单价 45）。故取给了值的那几个的最小值，`can_buy` 为 false 时直接归 0。

**分区在独立端点，参数名是 `region` 而非 `zone`。** 2026-08-10 起本家上了双区货架，
早期版本确实不分区。三个坑：

1. **扁平 `/api/my/stock` 给不出分区**，它只报一份数字（实测是「当前开放那个区」的）。
   选区必须查 `/api/my/stock/regions`，每区一张卡
   `{region, label, open, claimable, stock, afford, unit_price, can_buy}`。
2. **区代码是完整 AWS 标识**（`us-east-1` / `eu-central-1`），不是 `legacy` / `kiromarket`
   那种 `us` / `eu` 短码，原样回传给 claim。
3. **claim 不传 `region` 时卖家默认 `us-east-1`**，而美区经常正是关停 0 库存的那个。
   实测同一时刻：美区 `open=false` / `stock=0` / 单价 80，欧区开放 13 个 / 单价 50，
   扁平端点报的是欧区那一份。所以「不带区下单」的症状是**面板显示有货、下单永远失败**，
   而且面板上的单价也是另一个区的。

各区**严格隔离不跨区补货**，故 `zoned_purchase` 能力必须开，由 `resolve_zone` +
`StockInfo::pick_zone` 选「开放有货中最便宜」的区。`afford` 逐区不同（按本区单价算），
两区差价 60%，算可提量时必须带上它。

**发车时间与存活时长靠 `fleet_now` 算，不要硬编码时区。** 货架里每区带
`dispatches[]`（历史车次：`time` 发车时刻、`dead_at` 整车报废时刻、空串表示还活着）
与 `batches[]`（此刻还能提的批次）。所有时刻串**都不带时区**，但顶层 `fleet_now`
是卖家自己的当前时刻，于是：

```
存活时长 =（dead_at 或 fleet_now）− time      // 两端同源，差值与时区无关
发车时刻 = 本机此刻 −（fleet_now − time）     // 换成前端要的 Unix 秒，同样只用差值
```

实测卖家时钟是 UTC+8，**但代码一点都不依赖这个事实** —— 硬编码 8 小时的话卖家
换机房就错，而症状是存活时长整体偏移 8 小时（看着像「刚发车」或「跑了半天」，
不会报错）。缺 `fleet_now` 时两个字段一律留空，宁可不显示也不显示错数。

取哪趟车：**优先 `batches[]` 里最新那批对应的车**（那是此刻真能提到的货，
「这车跑了多久」问的就是它）；本区无货时退回 `dispatches[]` 最新那趟，回答
「上一趟什么时候发的」。车已死用 `dead_at` 封顶，否则死掉的车存活时长还会一直涨。
面板上显示为「存活 9分12秒 · 9分钟前发车」，前端 `describeZoneBatch` 已有渲染。

货架端点不可用或返回空 `regions` 时退回扁平库存，此时 `zones` 为空、提取会被
`NoZoneInStock` 挡下 —— **这是故意的**：不知道哪个区有货时宁可不下单，也不赌默认区。
退回时日志有 `WARN`。

响应逐 Key 仍带 `region`，以它为准；**响应给不出区域时（字符串形态的 `keys[]`、
或降级按 `ksk_` 前缀捞出的那条路径）用本单下过的 `region` 兜底** —— 欧区 Key 若按
全局默认区入库，请求会打到 `q.us-east-1.amazonaws.com` 而全部失败，症状只是
「刚提的 Key 莫名不可用」。真回了混区（带 region 下单后不应再发生）则整单按全局
默认区入库并 `WARN` 记下混了哪些区，**看到那条 WARN 就去核对凭据的 `apiRegion`**。

#### 未经实测的两处

**Webhook 载荷形态未验证。** 要在卖家侧配好地址才能收到推送，接入时没有改动
账号的 webhook 配置。已知的只有文档一句「每次发车我方都会给所有配好 Webhook 的
用户推一条到货通知」，以及推送里带 `client_order_id`。故事件名走宽松归一化，把
`key_new` / `on_key_new` / `keys_available` / `dispatch` / `on_dispatch` 等都映射到
`new_keys_available`，`key_dead` / `all_dead` → `all_keys_dead`；`key_suspect`
（疑似失效）**刻意不映射成全失效** —— 那会在旧 Key 可能还活着时触发补货扣费，
只当告警处理。事件名候选取自本家 `/api/my/notify/prefs` 的开关名，是卖家自己的
通知语汇，大概率同源。**首次收到真实推送后请核对日志里的事件名**，若落成 `unknown`
就要往 `normalize_event_type` 里补一个别名。因此配置默认 `autoPurchase: false`。

**claim 响应形态未验证**（会扣积分，接入时未触发）。文档示例脚本用
`jq -r ".keys[]"` 暗示字符串数组，而 `/api/my/keys` 返回对象数组，两处不一致。
故 DTO 用 untagged 枚举同时接住两种元素形态，外层结构完全不认识时再按 `ksk_`
前缀降级扫描 —— 与 `kiroapp-cc` / `drop` 同一道理：拿到 2xx 积分就已经扣了，
按结构硬解失败等于把付过费的 Key 扔掉。一个都没捞到时告警并提示人工核对扣费。

**`/api/my/redeem` 文档没列但路由存在**（`GET` 返 405 `allow: POST`），故开放了
兑换能力，请求体沿用通用的 `{"code":...}`，响应字段给足别名。形态猜错只会让面板
少显示一个到账数字，兑换本身（同账号同码幂等）不受影响。

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

`legacy`、`kiroapp`（.io）、`drop`、`kiromarket` 与 `kiro-ooo` 支持推送；
`kiroapp-cc` 与 `kirored` 没有 webhook —— 这两家的自动提取要靠
[库存轮询](#库存轮询没有-webhook-的家怎么自动提取)，光开 `autoPurchase` 不会有任何动作。

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

## 库存轮询（没有 webhook 的家怎么自动提取）

自动提取全代码库只有一个触发点：入站 webhook 收到 `new_keys_available`。
所以 `kirored` / `kiroapp-cc` 单独开 `autoPurchase` 是**静默无效**的 —— 面板显示
「自动提取」，却一次都不会动，且因为流程从未启动，连跳过原因都不会留，现象与
「webhook 链路断了」完全一样。

`stockPollIntervalSecs` 补上缺失的那一环：定时查库存，发现新车就**合成**一条
`new_keys_available` 事件塞进同一条管线。

```json
{
  "id": "kirored",
  "flavor": "kirored",
  "autoPurchase": true,
  "stockPollIntervalSecs": 60,
  "stockPollRespectGlobalGate": true
}
```

| 字段 | 含义 |
|---|---|
| `stockPollIntervalSecs` | 轮询间隔秒数，**0 = 关闭（默认）**。下限 60，配小了抬到 60 并告警 |
| `stockPollRespectGlobalGate` | 是否遵循全局总闸 `autoPurchaseEnabled`，默认 `true` |

**与 `autoPurchase` 是 AND 关系。** 轮询非 0 只让轮询器跑起来，下不下单仍由
`autoPurchase` 与各级闸门决定。分开是有意的：**单独开轮询、不开 autoPurchase**，
可以先只观察「轮询能否发现新车」—— 合成事件照样落库、面板上看得到，可人工提取，
不冒扣费风险。确认节奏对了再开 `autoPurchase`。

### `stockPollRespectGlobalGate=false` 会越过全局急停

默认 `true`：全局总闸（`autoPurchaseEnabled`）关闭时轮询直接停，连库存都不查。

**改成 `false` 后，总闸对本家这条轮询链路整体失效 —— 包括下单。** 轮询会继续发现
新车，并且 `try_auto_purchase` 会跳过总闸检查真的扣费。这条路每次触发都打 `WARN`。

代价要清楚：**总闸不再是能一键停掉全部自动扣费的急停**，而它会被健康联动自动
翻转。想停掉本家只有两个办法 —— 关本家的 `autoPurchase`，或把
`stockPollIntervalSecs` 改成 0。

绕过的范围**只有总闸，且只有轮询这条路**：

| 触发来源 | `respectGate=true` | `respectGate=false` |
|---|---|---|
| 卖家 webhook 推送 | 受总闸管 | **仍受总闸管** |
| 本地库存轮询 | 受总闸管 | **越过总闸** |

webhook 那一格是刻意锁死的（见 `AutoPurchaseSource`）：若判据只看
`!respectGate`，推送触发的自动提取会一并放开，比开关名承诺的范围宽得多，而用户
从名字上看不出 webhook 也被放开了。

池闸（`autoPurchasePoolTarget`）、失效授权判定（`LocalCensus`）、并发锁**都不绕**，
所以仍然是有界的，不会无上限扣费。

**判定与扣费全部复用既有管线**，轮询自己一步判定都不做（多一条判定就多一条绕过
闸门的路径）。真正放行要同时满足：新批次出现 **且** 授权通过（本家盘点无存活 Key，
即 `LocalCensus`）**且** 过池闸。所以不是「有新车就买」，而是「我手上没号了、
且正好有新车」才买。

### event_id 取批次身份，这是成败所在

合成事件按 `(vendorId, eventId)` 去重，取错粒度必出事故：

| 取法 | 后果 |
|---|---|
| 固定串 | 第一次之后永远算重投，一次都不会再提 |
| 时间戳 / UUID | 每个轮询周期都是新事件，等于每分钟撞一次授权判定 |
| **卖家侧批次身份**（正解） | 一趟车 = 一条事件 = 一笔订单，重启也不重复提 |

批次身份取 `ZoneStock::departed_at`（kiro.red 是 `latest_batch.import_time`，
kiro.ooo 是 `dispatches[].time`），`eventId = poll:{zone}:{发车 Unix 秒}`。
`poll:` 前缀让面板能一眼分出「卖家推来的」与「我们轮询发现的」。

**卖家给不出发车时刻就不提** —— 没有稳定 id 就会退化成「每轮下一单」，宁可不提。
`zones` 恒空的家（`kiroapp-cc`）因此轮询无效。注意日志里那条「未给出任何分区」
有两种成因：结构上不分区，或**此刻没有健康车次**（kiro.red 只把 `health=good` 的
商品折进 `zones`，车一不健康就空了）—— 实测后者更常见，别当成接口变形去排查。

### 其它约束

- 遵循总闸的开关见上一节 —— 关掉它**会**让本家越过总闸扣费，不要当成只影响发现。
- 失败按 2 的幂次退避，**封顶 16 倍**（60 秒间隔 → 最长 16 分钟）。无上限的退避会
  退到几小时，卖家恢复后半天发现不了新车。
- 已处理过的车不重试，**不看当初是提成了还是跳过了**。下单失败的原因通常不是重试
  能解决的（余额不足 / 卖家拒单），要重试就在面板上按那条事件手动提取。
- 入站可用的家配了轮询会告警：卖家推送更及时也更省，通常不必两者并存。并存不会
  重复下单（`poll:` 前缀与卖家事件天然不同名，真撞上同一趟车由池闸与本家盘点挡住）。

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

- `GET /api/admin/vendor/ledger?vendorId=xxx` - 积分流水（`kiroapp` / `kiromarket` / `kiro-ooo`）
- `GET /api/admin/vendor/keys?vendorId=xxx` - 我的密钥列表（`kiroapp` / `kiromarket` / `kiro-ooo`）

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
无法识别的卖家协议风味 "unknown"，可选值: legacy, kiroapp, kiroapp-cc, drop, kiromarket, kirored, kiro-ooo
```

刻意不回退默认值 —— 拼错的 flavor 若被当成 `legacy`，会对着错误的路径和鉴权头
发请求，症状是一片 401/404，很难定位。

### Q: 两家供应商能用相同的 webhookPathToken 吗？

A: 不能。每家的 token 必须唯一，否则无法正确路由 webhook。

### Q: 如何测试 webhook 是否配置正确？

A: `legacy`、`drop`、`kiromarket` 与 `kiro-ooo` 可以：前端供应商页面有「测试推送」
按钮，点击后让供应商推一条测试消息到已保存的 webhook URL。`kiroapp`（.io）没有这个
API，只能在卖家网页里配好地址后等真实推送；`kiroapp-cc` 与 `kirored` 根本没有 webhook。

注意 `kiro-ooo` 的测试推送能验证「地址能收到」，但**验不出事件名对不对** ——
测试消息的 `event` 通常是 `test`，而真实到货通知的事件名本次未经实测
（见该协议章节）。首次真实推送后要核对日志。

## 完整配置示例

参考项目根目录的 `config.example.json`。
