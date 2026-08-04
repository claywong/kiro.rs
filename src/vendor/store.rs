//! 卖家 webhook 事件持久化（SQLite，`vendor_events.db`）
//!
//! 单独一个库、不复用 `traces.db`：后者有保留天数自动清理，会把提取记录一并删掉，
//! 而订单号 / 绑定数量必须长期留存 —— 卖家侧对「同订单号 + 同 count」幂等，一旦
//! 首次提交确定了 count，后续重试必须复用同一个值，改了会 409。
//!
//! @author wangzhong

use std::path::PathBuf;

use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// 查询默认返回条数
pub const DEFAULT_QUERY_LIMIT: usize = 200;

/// 单供应商时期的隐式 id。存量事件按它回填，与
/// [`crate::model::config::DEFAULT_VENDOR_ID`] 必须一致。
pub const DEFAULT_VENDOR_ID: &str = crate::model::config::DEFAULT_VENDOR_ID;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS vendor_events (
    vendor_id          TEXT NOT NULL DEFAULT 'default',
    event_id           TEXT NOT NULL,
    event_type         TEXT NOT NULL,
    purchase_order_id  TEXT,
    batch_order_id     TEXT,
    message            TEXT,
    new_keys           INTEGER,
    dead               INTEGER,
    raw_payload        TEXT,
    received_at        TEXT NOT NULL,
    delivery_count     INTEGER NOT NULL DEFAULT 1,
    acked              INTEGER NOT NULL DEFAULT 0,
    bound_count        INTEGER,
    purchase_status    TEXT,
    purchased          INTEGER,
    imported           INTEGER,
    duplicated         INTEGER,
    failed             INTEGER,
    last_error         TEXT,
    processed_at       TEXT,
    purchase_trigger   TEXT,
    validation_status  TEXT,
    validation_detail  TEXT,
    validated_at       TEXT,
    validation_used    INTEGER NOT NULL DEFAULT 0,
    bound_zone         TEXT,
    PRIMARY KEY (vendor_id, event_id)
);
"#;

/// 索引单独一批执行。
///
/// 不能并到 [`SCHEMA`] 里：打开存量库时 `CREATE TABLE IF NOT EXISTS` 是空操作，
/// 而这些索引引用了尚未迁移出来的 `vendor_id` 列，会直接让开库失败。
/// 必须等 [`migrate_to_multi_vendor`] 跑完再建。
const INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_vendor_events_received
    ON vendor_events (vendor_id, received_at DESC);
CREATE INDEX IF NOT EXISTS idx_vendor_events_acked
    ON vendor_events (vendor_id, acked, received_at DESC);
"#;

/// 存量库补列。`CREATE TABLE IF NOT EXISTS` 对已存在的表不生效，而这个库里的
/// 订单号与绑定数量是长期资产、不能重建，故逐列 ALTER 并忽略「列已存在」错误。
///
/// 注意：`vendor_id` 不在此列表 —— 它要改主键，ALTER 做不到，见
/// [`migrate_to_multi_vendor`]。
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE vendor_events ADD COLUMN purchase_trigger TEXT",
    "ALTER TABLE vendor_events ADD COLUMN validation_status TEXT",
    "ALTER TABLE vendor_events ADD COLUMN validation_detail TEXT",
    "ALTER TABLE vendor_events ADD COLUMN validated_at TEXT",
    "ALTER TABLE vendor_events ADD COLUMN validation_used INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE vendor_events ADD COLUMN batch_order_id TEXT",
    // 分区卖家的成交区域。与 bound_count 同时写入、同样不可改 ——
    // 换区重试等于换了笔单，会重复扣积分。
    "ALTER TABLE vendor_events ADD COLUMN bound_zone TEXT",
];

/// 事件类型。未知类型也落库（`Unknown`），避免卖家新增事件时丢数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorEventKind {
    NewKeysAvailable,
    AllKeysDead,
    /// 密钥因滥用被回收（kiroapp 独有）。不触发提取，但要落库告警 ——
    /// 它意味着本地某张 Key 已被上游作废，与正常失效的处置不同。
    KeyRevokedAbuse,
    /// 卖家的连通性测试推送
    Test,
    Unknown,
}

impl VendorEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewKeysAvailable => "new_keys_available",
            Self::AllKeysDead => "all_keys_dead",
            Self::KeyRevokedAbuse => "key_revoked_abuse",
            Self::Test => "test",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            // `batch.completed` 是 Drop 家早先那版文档里「新批次已上架」的叫法。
            // 现行文档已改用 new_keys_available，但别名留着：两者语义相同（都表示
            // 有新货可提），而对方实现是否同步改过无从确认，漏认一条会错过补货。
            "new_keys_available" | "batch.completed" => Self::NewKeysAvailable,
            "all_keys_dead" => Self::AllKeysDead,
            "key_revoked_abuse" => Self::KeyRevokedAbuse,
            "test" => Self::Test,
            _ => Self::Unknown,
        }
    }
}

/// 提取处理状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurchaseStatus {
    /// 已提交过 purchase，成功
    Done,
    /// 提交过但失败（可用同一 bound_count 重试）
    Failed,
    /// 自动模式主动放弃本次提取（未绑定数量，仍可手动提取）
    Skipped,
}

impl PurchaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// 提取触发方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurchaseTrigger {
    /// 面板手动点击
    Manual,
    /// 自动模式触发
    Auto,
}

impl PurchaseTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}

/// `all_keys_dead` 事件的失效确认结论。
///
/// 只有 [`Self::ConfirmedDead`] 才允许下一轮自动提取 —— 其余两种都表示
/// "无法确认旧 Key 已失效"，此时自动扣费的依据不成立。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationStatus {
    /// 观察窗口内仍在重查
    Pending,
    /// 名下卖家 Key 已全部失效
    ConfirmedDead,
    /// 仍有健康的卖家 Key，无需补货
    StillAlive,
    /// 窗口结束仍无结论。
    ///
    /// `auto::conclude` 已不再产出该状态（人工禁用 / 无禁用原因的 Key 同样不可用，
    /// 不再阻塞确认），保留仅为兼容历史事件行里已写入的 `inconclusive`。
    Inconclusive,
}

impl ValidationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::ConfirmedDead => "confirmed_dead",
            Self::StillAlive => "still_alive",
            Self::Inconclusive => "inconclusive",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "confirmed_dead" => Some(Self::ConfirmedDead),
            "still_alive" => Some(Self::StillAlive),
            "inconclusive" => Some(Self::Inconclusive),
            _ => None,
        }
    }
}

/// 入站事件（已解析）
#[derive(Debug, Clone)]
pub struct IncomingEvent {
    /// 事件归属的供应商 id。由 webhook 路径 token 匹配到的那一家决定。
    pub vendor_id: String,
    pub event_id: String,
    pub kind: VendorEventKind,
    /// 幂等键。首家需本地生成，kiroapp 在推送里直接给（按批次 + 收件人派生）。
    pub purchase_order_id: Option<String>,
    /// 开号批次 id。仅 `batch_scoped_purchase` 能力的卖家会给，
    /// 下单时带上可只拉该批次产出的 Key。
    pub batch_order_id: Option<String>,
    pub message: Option<String>,
    pub new_keys: Option<u32>,
    pub dead: Option<u32>,
    pub raw_payload: String,
}

/// 落库结果：区分首次收到与重投，便于日志与幂等判断
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// 首次收到
    Inserted,
    /// 重投（`event_id` 已存在），仅累加投递次数
    Duplicate,
}

/// 事件记录（返回给前端）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VendorEventRecord {
    /// 事件归属的供应商 id
    pub vendor_id: String,
    pub event_id: String,
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purchase_order_id: Option<String>,
    /// 开号批次 id（仅部分卖家给），下单时可据此只拉该批次的 Key
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_keys: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dead: Option<u32>,
    pub received_at: String,
    pub delivery_count: u32,
    pub acked: bool,
    /// 首次提交 purchase 时绑定的数量。非空即表示该订单号已被占用，重试只能用此值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound_count: Option<u32>,
    /// "done" / "failed"；None 表示尚未提取过
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purchase_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purchased: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imported: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicated: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processed_at: Option<String>,
    /// "manual" / "auto"；None 表示尚未提取过
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purchase_trigger: Option<String>,
    /// 失效确认结论（仅 `all_keys_dead` 事件有值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_status: Option<String>,
    /// 确认结论的人类可读依据，如「3 张卖家 Key 全部已禁用」
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validated_at: Option<String>,
    /// 该确认结论是否已被某次自动提取消费掉（一次确认只授权一轮提取）
    pub validation_used: bool,
}

/// 提取结果汇总，写回事件行
#[derive(Debug, Clone, Default)]
pub struct PurchaseOutcome {
    pub purchased: u32,
    pub imported: u32,
    pub duplicated: u32,
    pub failed: u32,
    pub last_error: Option<String>,
}

pub type SharedVendorStore = std::sync::Arc<VendorStore>;

/// 卖家事件存储
pub struct VendorStore {
    conn: Mutex<Connection>,
}

impl VendorStore {
    /// 打开（或创建）事件库
    pub fn open(path: PathBuf) -> rusqlite::Result<Self> {
        let path = if path.as_os_str().is_empty() {
            PathBuf::from("vendor_events.db")
        } else {
            path
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!("创建 vendor_events.db 目录失败 {}: {}", parent.display(), e);
        }
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        // 顺序有意义：
        // 1. 补列 —— 旧库可能缺后期新增的列，迁移时要 SELECT 它们
        // 2. 整表迁移到多供应商结构（改主键，只能重建表）
        // 3. 建索引 —— 索引引用 vendor_id，必须等它存在
        apply_migrations(&conn);
        if let Err(e) = migrate_to_multi_vendor(&conn) {
            // 迁移失败不阻断启动，但必须显眼告警 —— 此时旧表仍在，
            // 后续按 vendor_id 的查询会报「no such column」，功能不可用但不丢数据。
            tracing::error!("vendor_events 迁移到多供应商结构失败: {}", e);
        }
        conn.execute_batch(INDEXES)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 内存库（打开文件失败时兜底，保证 Admin 查询不崩）
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(INDEXES)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 落库入站事件。`event_id` 冲突时不覆盖任何业务字段，仅把投递次数 +1
    /// 并返回 [`RecordOutcome::Duplicate`] —— 卖家重投不应清掉已有的提取结果。
    pub fn record_event(&self, event: &IncomingEvent) -> rusqlite::Result<RecordOutcome> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "INSERT INTO vendor_events
                (vendor_id, event_id, event_type, purchase_order_id, batch_order_id,
                 message, new_keys, dead, raw_payload, received_at, delivery_count, acked)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, 0)
             ON CONFLICT(vendor_id, event_id) DO UPDATE SET
                delivery_count = delivery_count + 1",
            rusqlite::params![
                event.vendor_id,
                event.event_id,
                event.kind.as_str(),
                event.purchase_order_id,
                event.batch_order_id,
                event.message,
                event.new_keys,
                event.dead,
                event.raw_payload,
                Utc::now().to_rfc3339(),
            ],
        )?;
        // ON CONFLICT DO UPDATE 也返回 1，靠 delivery_count 判断是否首次
        let count: u32 = conn.query_row(
            "SELECT delivery_count FROM vendor_events
             WHERE vendor_id = ?1 AND event_id = ?2",
            rusqlite::params![event.vendor_id, event.event_id],
            |row| row.get(0),
        )?;
        let _ = changed;
        Ok(if count <= 1 {
            RecordOutcome::Inserted
        } else {
            RecordOutcome::Duplicate
        })
    }

    /// 读取单条事件
    pub fn get_event(
        &self,
        vendor_id: &str,
        event_id: &str,
    ) -> rusqlite::Result<Option<VendorEventRecord>> {
        let conn = self.conn.lock();
        conn.query_row(
            &format!(
                "SELECT {SELECT_COLUMNS} FROM vendor_events
                 WHERE vendor_id = ?1 AND event_id = ?2"
            ),
            rusqlite::params![vendor_id, event_id],
            row_to_record,
        )
        .optional()
    }

    /// 事件列表（按接收时间倒序）。`vendor_id` 为 None 时跨供应商合并返回。
    pub fn list_events(
        &self,
        vendor_id: Option<&str>,
        limit: usize,
    ) -> rusqlite::Result<Vec<VendorEventRecord>> {
        let limit = limit.clamp(1, 1000);
        let conn = self.conn.lock();
        match vendor_id {
            Some(vid) => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {SELECT_COLUMNS} FROM vendor_events
                     WHERE vendor_id = ?1
                     ORDER BY received_at DESC LIMIT ?2"
                ))?;
                let rows = stmt.query_map(rusqlite::params![vid, limit], row_to_record)?;
                rows.collect()
            }
            None => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {SELECT_COLUMNS} FROM vendor_events
                     ORDER BY received_at DESC LIMIT ?1"
                ))?;
                let rows = stmt.query_map([limit], row_to_record)?;
                rows.collect()
            }
        }
    }

    /// 某供应商最近一条 `all_keys_dead` 事件。自动提取要靠它的确认结论授权。
    ///
    /// 必须按 vendor_id 过滤：A 家的「全部失效」不能给 B 家的补货开绿灯。
    pub fn latest_dead_event(
        &self,
        vendor_id: &str,
    ) -> rusqlite::Result<Option<VendorEventRecord>> {
        let conn = self.conn.lock();
        conn.query_row(
            &format!(
                "SELECT {SELECT_COLUMNS} FROM vendor_events
                 WHERE vendor_id = ?1 AND event_type = ?2
                 ORDER BY received_at DESC LIMIT 1"
            ),
            rusqlite::params![vendor_id, VendorEventKind::AllKeysDead.as_str()],
            row_to_record,
        )
        .optional()
    }

    /// 写入失效确认结论
    pub fn set_validation(
        &self,
        vendor_id: &str,
        event_id: &str,
        status: ValidationStatus,
        detail: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE vendor_events SET
                validation_status = ?3, validation_detail = ?4, validated_at = ?5
             WHERE vendor_id = ?1 AND event_id = ?2",
            rusqlite::params![
                vendor_id,
                event_id,
                status.as_str(),
                detail,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// 抢占式消费确认结论：只有 `validation_used = 0` 且结论为 `confirmed_dead`
    /// 时才置位并返回 true。
    ///
    /// 一次确认只授权一轮自动提取 —— 否则同一条 `all_keys_dead` 会给后续每条
    /// `new_keys_available` 都开绿灯，变成无限自动扣费。单条 UPDATE 完成判断与
    /// 写入，并发触发只有一个能拿到。
    pub fn consume_validation(&self, vendor_id: &str, event_id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "UPDATE vendor_events SET validation_used = 1
             WHERE vendor_id = ?1 AND event_id = ?2 AND validation_used = 0
               AND validation_status = ?3",
            rusqlite::params![
                vendor_id,
                event_id,
                ValidationStatus::ConfirmedDead.as_str()
            ],
        )?;
        Ok(changed > 0)
    }

    /// 未确认（未点「已知悉」）的事件数，用于 tab 红点。
    /// `vendor_id` 为 None 时统计全部供应商。
    pub fn unacked_count(&self, vendor_id: Option<&str>) -> rusqlite::Result<u32> {
        let conn = self.conn.lock();
        match vendor_id {
            Some(vid) => conn.query_row(
                "SELECT COUNT(*) FROM vendor_events WHERE vendor_id = ?1 AND acked = 0",
                [vid],
                |row| row.get(0),
            ),
            None => conn.query_row(
                "SELECT COUNT(*) FROM vendor_events WHERE acked = 0",
                [],
                |row| row.get(0),
            ),
        }
    }

    /// 标记事件已知悉。
    ///
    /// - `vendor_id` + `event_id`：标记该供应商的单条
    /// - `vendor_id` + None：标记该供应商全部
    /// - None + None：标记所有供应商全部
    pub fn ack(&self, vendor_id: Option<&str>, event_id: Option<&str>) -> rusqlite::Result<usize> {
        let conn = self.conn.lock();
        match (vendor_id, event_id) {
            (Some(vid), Some(id)) => conn.execute(
                "UPDATE vendor_events SET acked = 1 WHERE vendor_id = ?1 AND event_id = ?2",
                rusqlite::params![vid, id],
            ),
            (Some(vid), None) => conn.execute(
                "UPDATE vendor_events SET acked = 1 WHERE vendor_id = ?1 AND acked = 0",
                [vid],
            ),
            // 不指定供应商时忽略 event_id：跨供应商按 id 改行会误伤同名事件
            (None, _) => conn.execute("UPDATE vendor_events SET acked = 1 WHERE acked = 0", []),
        }
    }

    /// 抢占式绑定提取数量。等价于 `bind_count_zone(.., None)`，
    /// 即不分区卖家的情形。
    #[cfg(test)]
    pub fn bind_count(
        &self,
        vendor_id: &str,
        event_id: &str,
        count: u32,
    ) -> rusqlite::Result<Result<u32, u32>> {
        self.bind_count_zone(vendor_id, event_id, count, None)
            .map(|r| r.map(|(c, _)| c).map_err(|(c, _)| c))
    }

    /// 抢占式绑定提取数量与区域。
    ///
    /// 这是防重复扣费的核心：只有 `bound_count IS NULL`（从未提交过 purchase）时
    /// 才写入并返回 `Ok((count, zone))`；否则返回 `Err((已绑定数量, 已绑定区域))`，
    /// 调用方必须改用这对值重试。单条 UPDATE 完成判断与写入，并发点击「提取」
    /// 只有一个能抢到。
    ///
    /// 数量与区域必须**一起**绑定：卖家的幂等键覆盖整个请求体，同一订单号换区
    /// 重试会被当成新单再扣一次积分。
    pub fn bind_count_zone(
        &self,
        vendor_id: &str,
        event_id: &str,
        count: u32,
        zone: Option<&str>,
    ) -> rusqlite::Result<Result<(u32, Option<String>), (u32, Option<String>)>> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "UPDATE vendor_events SET bound_count = ?3, bound_zone = ?4
             WHERE vendor_id = ?1 AND event_id = ?2 AND bound_count IS NULL",
            rusqlite::params![vendor_id, event_id, count, zone],
        )?;
        if changed > 0 {
            return Ok(Ok((count, zone.map(|s| s.to_string()))));
        }
        let existing: Option<(Option<u32>, Option<String>)> = conn
            .query_row(
                "SELECT bound_count, bound_zone FROM vendor_events
                 WHERE vendor_id = ?1 AND event_id = ?2",
                rusqlite::params![vendor_id, event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((Some(v), z)) => Ok(Err((v, z))),
            // 行存在但从未绑定过（并发下另一方刚清空？）与行不存在同样处理：
            // 交由调用方按「事件不存在」判定
            Some((None, _)) | None => Ok(Ok((count, zone.map(|s| s.to_string())))),
        }
    }

    /// 写回提取结果
    pub fn finish_purchase(
        &self,
        vendor_id: &str,
        event_id: &str,
        status: PurchaseStatus,
        trigger: PurchaseTrigger,
        outcome: &PurchaseOutcome,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE vendor_events SET
                purchase_status = ?3, purchased = ?4, imported = ?5,
                duplicated = ?6, failed = ?7, last_error = ?8, processed_at = ?9,
                purchase_trigger = ?10
             WHERE vendor_id = ?1 AND event_id = ?2",
            rusqlite::params![
                vendor_id,
                event_id,
                status.as_str(),
                outcome.purchased,
                outcome.imported,
                outcome.duplicated,
                outcome.failed,
                outcome.last_error,
                Utc::now().to_rfc3339(),
                trigger.as_str(),
            ],
        )?;
        Ok(())
    }

    /// 记录自动模式的跳过原因。
    ///
    /// 刻意不写 `bound_count` —— 跳过是可逆的，订单号仍未被占用，用户随后
    /// 手动提取时数量依然可选。已提取过的事件不覆盖。
    pub fn record_skip(
        &self,
        vendor_id: &str,
        event_id: &str,
        reason: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE vendor_events SET
                purchase_status = ?5, last_error = ?3, processed_at = ?4,
                purchase_trigger = ?6
             WHERE vendor_id = ?1 AND event_id = ?2 AND bound_count IS NULL",
            rusqlite::params![
                vendor_id,
                event_id,
                reason,
                Utc::now().to_rfc3339(),
                PurchaseStatus::Skipped.as_str(),
                PurchaseTrigger::Auto.as_str(),
            ],
        )?;
        Ok(())
    }
}

/// 逐条执行补列语句。「列已存在」是正常情况（新建库已含全部列），静默跳过；
/// 其余错误只告警不阻断启动 —— 事件仍能落库，只是新字段读不到。
fn apply_migrations(conn: &Connection) {
    for sql in MIGRATIONS {
        if let Err(e) = conn.execute(sql, []) {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                tracing::warn!("vendor_events 补列失败（{}）: {}", sql, msg);
            }
        }
    }
}

/// 单供应商 → 多供应商的表迁移。
///
/// 主键从 `event_id` 变成 `(vendor_id, event_id)`，SQLite 的 ALTER 改不了主键，
/// 只能建新表 + 搬数据 + 换名。存量行的 `vendor_id` 回填 [`DEFAULT_VENDOR_ID`]，
/// 与旧配置的隐式 id 对应，历史事件在面板上仍归属原来那一家。
///
/// 整个过程在一个事务里，中途失败回滚，不会留下半迁移的坏状态。
/// 已经是新结构（含 `vendor_id` 列）时直接跳过。
fn migrate_to_multi_vendor(conn: &Connection) -> rusqlite::Result<()> {
    if has_column(conn, "vendor_events", "vendor_id")? {
        return Ok(());
    }

    tracing::info!("vendor_events 迁移到多供应商结构：存量事件归属 {DEFAULT_VENDOR_ID}");

    // 旧表可能缺后期补的列（补列语句在此之前已执行过，故此处应当齐全）。
    // 显式列出列名而非 SELECT *，避免列序变化导致错位。
    conn.execute_batch(&format!(
        r#"
        BEGIN;
        CREATE TABLE vendor_events_new (
            vendor_id          TEXT NOT NULL DEFAULT 'default',
            event_id           TEXT NOT NULL,
            event_type         TEXT NOT NULL,
            purchase_order_id  TEXT,
            batch_order_id     TEXT,
            message            TEXT,
            new_keys           INTEGER,
            dead               INTEGER,
            raw_payload        TEXT,
            received_at        TEXT NOT NULL,
            delivery_count     INTEGER NOT NULL DEFAULT 1,
            acked              INTEGER NOT NULL DEFAULT 0,
            bound_count        INTEGER,
            purchase_status    TEXT,
            purchased          INTEGER,
            imported           INTEGER,
            duplicated         INTEGER,
            failed             INTEGER,
            last_error         TEXT,
            processed_at       TEXT,
            purchase_trigger   TEXT,
            validation_status  TEXT,
            validation_detail  TEXT,
            validated_at       TEXT,
            validation_used    INTEGER NOT NULL DEFAULT 0,
            bound_zone         TEXT,
            PRIMARY KEY (vendor_id, event_id)
        );
        INSERT INTO vendor_events_new
            (vendor_id, event_id, event_type, purchase_order_id, batch_order_id, message,
             new_keys, dead, raw_payload, received_at, delivery_count, acked, bound_count,
             purchase_status, purchased, imported, duplicated, failed, last_error,
             processed_at, purchase_trigger, validation_status, validation_detail,
             validated_at, validation_used, bound_zone)
        SELECT '{DEFAULT_VENDOR_ID}', event_id, event_type, purchase_order_id,
             batch_order_id, message, new_keys, dead, raw_payload, received_at,
             delivery_count, acked, bound_count, purchase_status, purchased, imported,
             duplicated, failed, last_error, processed_at, purchase_trigger,
             validation_status, validation_detail, validated_at, validation_used,
             bound_zone
        FROM vendor_events;
        DROP TABLE vendor_events;
        ALTER TABLE vendor_events_new RENAME TO vendor_events;
        COMMIT;
        "#
    ))
}

/// 表里是否存在某列
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        // PRAGMA table_info 的第 2 列是列名
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 所有查询共用的列清单，保证列序与 [`row_to_record`] 的下标一致
const SELECT_COLUMNS: &str = "vendor_id, event_id, event_type, purchase_order_id, batch_order_id,
     message, new_keys, dead, received_at, delivery_count, acked, bound_count, purchase_status,
     purchased, imported, duplicated, failed, last_error, processed_at,
     purchase_trigger, validation_status, validation_detail, validated_at, validation_used";

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<VendorEventRecord> {
    Ok(VendorEventRecord {
        vendor_id: row.get(0)?,
        event_id: row.get(1)?,
        event_type: row.get(2)?,
        purchase_order_id: row.get(3)?,
        batch_order_id: row.get(4)?,
        message: row.get(5)?,
        new_keys: row.get(6)?,
        dead: row.get(7)?,
        received_at: row.get(8)?,
        delivery_count: row.get(9)?,
        acked: row.get::<_, i64>(10)? != 0,
        bound_count: row.get(11)?,
        purchase_status: row.get(12)?,
        purchased: row.get(13)?,
        imported: row.get(14)?,
        duplicated: row.get(15)?,
        failed: row.get(16)?,
        last_error: row.get(17)?,
        processed_at: row.get(18)?,
        purchase_trigger: row.get(19)?,
        validation_status: row.get(20)?,
        validation_detail: row.get(21)?,
        validated_at: row.get(22)?,
        validation_used: row.get::<_, i64>(23)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> VendorStore {
        VendorStore::open_in_memory().expect("内存库初始化失败")
    }

    /// 默认供应商的事件
    fn event(id: &str) -> IncomingEvent {
        event_for(DEFAULT_VENDOR_ID, id)
    }

    fn event_for(vendor: &str, id: &str) -> IncomingEvent {
        IncomingEvent {
            vendor_id: vendor.to_string(),
            event_id: id.to_string(),
            kind: VendorEventKind::NewKeysAvailable,
            purchase_order_id: Some("0123456789abcdef0123456789abcdef".to_string()),
            batch_order_id: None,
            message: Some("新一轮 10 个 Key 已就绪".to_string()),
            new_keys: Some(10),
            dead: None,
            raw_payload: "{}".to_string(),
        }
    }

    const V: &str = DEFAULT_VENDOR_ID;

    #[test]
    fn 首次落库与重投判定() {
        let s = store();
        let e = event("e1");
        assert_eq!(s.record_event(&e).unwrap(), RecordOutcome::Inserted);
        assert_eq!(s.record_event(&e).unwrap(), RecordOutcome::Duplicate);
        let rec = s.get_event(V, "e1").unwrap().unwrap();
        assert_eq!(rec.delivery_count, 2);
        assert_eq!(rec.new_keys, Some(10));
        assert_eq!(rec.vendor_id, V);
    }

    #[test]
    fn 重投不清掉已有提取结果() {
        let s = store();
        let e = event("e1");
        s.record_event(&e).unwrap();
        s.bind_count(V, "e1", 5).unwrap().unwrap();
        s.finish_purchase(
            V,
            "e1",
            PurchaseStatus::Done,
            PurchaseTrigger::Manual,
            &PurchaseOutcome {
                purchased: 5,
                imported: 5,
                ..Default::default()
            },
        )
        .unwrap();
        // 卖家重投同一事件
        s.record_event(&e).unwrap();
        let rec = s.get_event(V, "e1").unwrap().unwrap();
        assert_eq!(rec.bound_count, Some(5));
        assert_eq!(rec.purchase_status.as_deref(), Some("done"));
        assert_eq!(rec.imported, Some(5));
    }

    #[test]
    fn count_只能绑定一次() {
        let s = store();
        s.record_event(&event("e1")).unwrap();
        assert_eq!(s.bind_count(V, "e1", 5).unwrap(), Ok(5));
        // 换数量重试 → 返回已绑定值，调用方必须复用
        assert_eq!(s.bind_count(V, "e1", 10).unwrap(), Err(5));
        assert_eq!(s.bind_count(V, "e1", 5).unwrap(), Err(5));
    }

    #[test]
    fn 数量与区域一起绑定且区域不可改() {
        let s = store();
        s.record_event(&event("e1")).unwrap();
        assert_eq!(
            s.bind_count_zone(V, "e1", 5, Some("eu")).unwrap(),
            Ok((5, Some("eu".to_string())))
        );
        // 同数量重试：返回已绑定的区，调用方必须沿用它而不是本次选出的区
        assert_eq!(
            s.bind_count_zone(V, "e1", 5, Some("us")).unwrap(),
            Err((5, Some("eu".to_string())))
        );
        // 换数量同样被拒，并带回区域
        assert_eq!(
            s.bind_count_zone(V, "e1", 9, Some("eu")).unwrap(),
            Err((5, Some("eu".to_string())))
        );
    }

    #[test]
    fn 绑定区域可为空_不分区卖家() {
        let s = store();
        s.record_event(&event("e1")).unwrap();
        assert_eq!(s.bind_count_zone(V, "e1", 3, None).unwrap(), Ok((3, None)));
        assert_eq!(
            s.bind_count_zone(V, "e1", 3, None).unwrap(),
            Err((3, None))
        );
    }

    #[test]
    fn 失败后仍保留绑定值供重试() {
        let s = store();
        s.record_event(&event("e1")).unwrap();
        s.bind_count(V, "e1", 7).unwrap().unwrap();
        s.finish_purchase(
            V,
            "e1",
            PurchaseStatus::Failed,
            PurchaseTrigger::Manual,
            &PurchaseOutcome {
                last_error: Some("余额不足".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let rec = s.get_event(V, "e1").unwrap().unwrap();
        assert_eq!(rec.bound_count, Some(7));
        assert_eq!(rec.purchase_status.as_deref(), Some("failed"));
        assert_eq!(rec.last_error.as_deref(), Some("余额不足"));
    }

    #[test]
    fn 未确认计数与确认() {
        let s = store();
        s.record_event(&event("e1")).unwrap();
        s.record_event(&event("e2")).unwrap();
        assert_eq!(s.unacked_count(Some(V)).unwrap(), 2);
        s.ack(Some(V), Some("e1")).unwrap();
        assert_eq!(s.unacked_count(Some(V)).unwrap(), 1);
        s.ack(Some(V), None).unwrap();
        assert_eq!(s.unacked_count(Some(V)).unwrap(), 0);
    }

    #[test]
    fn 列表按时间倒序且可限量() {
        let s = store();
        for i in 0..5 {
            s.record_event(&event(&format!("e{i}"))).unwrap();
        }
        assert_eq!(s.list_events(Some(V), 3).unwrap().len(), 3);
        assert_eq!(s.list_events(Some(V), 100).unwrap().len(), 5);
    }

    fn dead_event(id: &str) -> IncomingEvent {
        dead_event_for(DEFAULT_VENDOR_ID, id)
    }

    fn dead_event_for(vendor: &str, id: &str) -> IncomingEvent {
        IncomingEvent {
            vendor_id: vendor.to_string(),
            event_id: id.to_string(),
            kind: VendorEventKind::AllKeysDead,
            purchase_order_id: None,
            batch_order_id: None,
            message: Some("本轮全部失效".to_string()),
            new_keys: None,
            dead: Some(3),
            raw_payload: "{}".to_string(),
        }
    }

    #[test]
    fn 确认结论只能被消费一次() {
        let s = store();
        s.record_event(&dead_event("d1")).unwrap();
        // 未确认时不可消费
        assert!(!s.consume_validation(V, "d1").unwrap());

        s.set_validation(V, "d1", ValidationStatus::ConfirmedDead, "全部失效")
            .unwrap();
        assert!(s.consume_validation(V, "d1").unwrap());
        // 第二轮 new_keys_available 不能再靠同一条确认扣费
        assert!(!s.consume_validation(V, "d1").unwrap());
        assert!(s.get_event(V, "d1").unwrap().unwrap().validation_used);
    }

    #[test]
    fn 非确认失效的结论不可消费() {
        let s = store();
        s.record_event(&dead_event("d1")).unwrap();
        for st in [
            ValidationStatus::Pending,
            ValidationStatus::StillAlive,
            ValidationStatus::Inconclusive,
        ] {
            s.set_validation(V, "d1", st, "x").unwrap();
            assert!(!s.consume_validation(V, "d1").unwrap(), "{}", st.as_str());
        }
    }

    #[test]
    fn 只取最近一条失效事件() {
        let s = store();
        s.record_event(&event("k1")).unwrap();
        s.record_event(&dead_event("d1")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.record_event(&dead_event("d2")).unwrap();
        let latest = s.latest_dead_event(V).unwrap().unwrap();
        assert_eq!(latest.event_id, "d2");
    }

    #[test]
    fn 无失效事件时返回空() {
        let s = store();
        s.record_event(&event("k1")).unwrap();
        assert!(s.latest_dead_event(V).unwrap().is_none());
    }

    #[test]
    fn 跳过不占订单号且已提取的不被覆盖() {
        let s = store();
        s.record_event(&event("e1")).unwrap();
        s.record_skip(V, "e1", "本地仍有健康 Key").unwrap();
        let rec = s.get_event(V, "e1").unwrap().unwrap();
        assert_eq!(rec.purchase_status.as_deref(), Some("skipped"));
        assert_eq!(rec.purchase_trigger.as_deref(), Some("auto"));
        // 关键：数量未绑定，用户仍可手动按任意数量提取
        assert_eq!(rec.bound_count, None);

        // 已绑定过的事件不该被跳过记录覆盖
        s.bind_count(V, "e1", 2).unwrap().unwrap();
        s.finish_purchase(
            V,
            "e1",
            PurchaseStatus::Done,
            PurchaseTrigger::Auto,
            &PurchaseOutcome {
                purchased: 2,
                imported: 2,
                ..Default::default()
            },
        )
        .unwrap();
        s.record_skip(V, "e1", "不该覆盖").unwrap();
        let rec = s.get_event(V, "e1").unwrap().unwrap();
        assert_eq!(rec.purchase_status.as_deref(), Some("done"));
        assert_eq!(rec.purchase_trigger.as_deref(), Some("auto"));
    }

    // ============ 多供应商隔离 ============

    /// 两家卖家的 event_id 撞车时必须各自独立成行，否则第二家的事件会被
    /// 当成重投丢掉 —— 这是多供应商最危险的失败模式。
    #[test]
    fn 不同供应商相同事件id互不干扰() {
        let s = store();
        assert_eq!(
            s.record_event(&event_for("a", "same-id")).unwrap(),
            RecordOutcome::Inserted
        );
        assert_eq!(
            s.record_event(&event_for("b", "same-id")).unwrap(),
            RecordOutcome::Inserted,
            "另一家的同名事件不能被判为重投"
        );

        // 各自绑定不同数量，互不影响
        assert_eq!(s.bind_count("a", "same-id", 3).unwrap(), Ok(3));
        assert_eq!(s.bind_count("b", "same-id", 8).unwrap(), Ok(8));
        assert_eq!(s.get_event("a", "same-id").unwrap().unwrap().bound_count, Some(3));
        assert_eq!(s.get_event("b", "same-id").unwrap().unwrap().bound_count, Some(8));
    }

    #[test]
    fn 列表与计数按供应商过滤() {
        let s = store();
        s.record_event(&event_for("a", "a1")).unwrap();
        s.record_event(&event_for("a", "a2")).unwrap();
        s.record_event(&event_for("b", "b1")).unwrap();

        assert_eq!(s.list_events(Some("a"), 100).unwrap().len(), 2);
        assert_eq!(s.list_events(Some("b"), 100).unwrap().len(), 1);
        // 不传供应商时跨家合并，供状态总览用
        assert_eq!(s.list_events(None, 100).unwrap().len(), 3);

        assert_eq!(s.unacked_count(Some("a")).unwrap(), 2);
        assert_eq!(s.unacked_count(Some("b")).unwrap(), 1);
        assert_eq!(s.unacked_count(None).unwrap(), 3);
    }

    #[test]
    fn 确认只对本家生效() {
        let s = store();
        s.record_event(&event_for("a", "x")).unwrap();
        s.record_event(&event_for("b", "y")).unwrap();
        s.ack(Some("a"), None).unwrap();
        assert_eq!(s.unacked_count(Some("a")).unwrap(), 0);
        assert_eq!(s.unacked_count(Some("b")).unwrap(), 1, "不该顺手确认另一家");

        // 不指定供应商则全部确认
        s.ack(None, None).unwrap();
        assert_eq!(s.unacked_count(None).unwrap(), 0);
    }

    /// A 家的「全部失效」不能给 B 家的自动补货开绿灯
    #[test]
    fn 失效确认不跨供应商授权() {
        let s = store();
        s.record_event(&dead_event_for("a", "d-a")).unwrap();
        s.set_validation("a", "d-a", ValidationStatus::ConfirmedDead, "a 家全失效")
            .unwrap();

        // B 家没有失效事件，查不到
        assert!(s.latest_dead_event("b").unwrap().is_none());
        // 也不能消费 A 家的确认
        assert!(!s.consume_validation("b", "d-a").unwrap());
        // A 家自己可以
        assert!(s.consume_validation("a", "d-a").unwrap());
    }

    #[test]
    fn 批次id落库并可读回() {
        let s = store();
        let mut e = event_for("kiroapp", "k1");
        e.batch_order_id = Some("batch-42".to_string());
        s.record_event(&e).unwrap();
        let rec = s.get_event("kiroapp", "k1").unwrap().unwrap();
        assert_eq!(rec.batch_order_id.as_deref(), Some("batch-42"));
    }

    /// 存量库（无新列）打开后应自动补列，且历史的订单号与绑定数量不受影响。
    /// 这个库里的 bound_count 是不可再生的资产 —— 补列一旦写错就是永久损失。
    #[test]
    fn 存量库补列且不丢历史绑定() {
        let dir = std::env::temp_dir().join(format!("kiro_vendor_mig_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        let _ = std::fs::remove_file(&path);

        // 按旧 schema 建库并灌入一条已提取完成的历史记录
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE vendor_events (
                    event_id TEXT PRIMARY KEY, event_type TEXT NOT NULL,
                    purchase_order_id TEXT, message TEXT, new_keys INTEGER, dead INTEGER,
                    raw_payload TEXT, received_at TEXT NOT NULL,
                    delivery_count INTEGER NOT NULL DEFAULT 1,
                    acked INTEGER NOT NULL DEFAULT 0, bound_count INTEGER,
                    purchase_status TEXT, purchased INTEGER, imported INTEGER,
                    duplicated INTEGER, failed INTEGER, last_error TEXT, processed_at TEXT
                );
                INSERT INTO vendor_events
                    (event_id, event_type, purchase_order_id, received_at,
                     bound_count, purchase_status, purchased, imported)
                VALUES ('old-1', 'new_keys_available', 'abc', '2026-07-01T00:00:00Z',
                        7, 'done', 7, 7);",
            )
            .unwrap();
        }

        let s = VendorStore::open(path.clone()).expect("打开存量库失败");
        // 存量行归属默认供应商
        let rec = s.get_event(V, "old-1").unwrap().expect("历史记录丢失");
        assert_eq!(rec.vendor_id, DEFAULT_VENDOR_ID, "存量行未回填默认供应商");
        assert_eq!(rec.bound_count, Some(7), "历史绑定数量被破坏");
        assert_eq!(rec.purchase_status.as_deref(), Some("done"));
        assert_eq!(rec.purchase_order_id.as_deref(), Some("abc"));
        // 新列对历史行为空，且 validation_used 取默认值
        assert_eq!(rec.purchase_trigger, None);
        assert_eq!(rec.validation_status, None);
        assert_eq!(rec.batch_order_id, None);
        assert!(!rec.validation_used);
        // bound_zone 是本轮新增列：迁移后必须可写可读，否则分区绑定在存量库上会炸
        s.record_event(&event("new-1")).unwrap();
        assert_eq!(
            s.bind_count_zone(V, "new-1", 2, Some("eu")).unwrap(),
            Ok((2, Some("eu".to_string())))
        );
        assert_eq!(
            s.bind_count_zone(V, "new-1", 2, Some("eu")).unwrap(),
            Err((2, Some("eu".to_string())))
        );

        // 补列后新字段可正常读写
        s.set_validation(V, "old-1", ValidationStatus::ConfirmedDead, "测试")
            .unwrap();
        assert_eq!(
            s.get_event(V, "old-1")
                .unwrap()
                .unwrap()
                .validation_status
                .as_deref(),
            Some("confirmed_dead")
        );

        // 迁移后可以接入第二家，且与存量行互不干扰
        s.record_event(&event_for("kiroapp", "old-1")).unwrap();
        assert_eq!(
            s.get_event("kiroapp", "old-1").unwrap().unwrap().bound_count,
            None,
            "新供应商的同名事件不该继承存量行的绑定数量"
        );
        assert_eq!(s.get_event(V, "old-1").unwrap().unwrap().bound_count, Some(7));

        // 重复打开（再跑一次迁移）不应报错或丢数据
        drop(s);
        let s = VendorStore::open(path.clone()).expect("二次打开失败");
        assert_eq!(s.get_event(V, "old-1").unwrap().unwrap().bound_count, Some(7));
        // 存量 old-1、本测试新写的 new-1、以及第二家的 old-1，共 3 行
        assert_eq!(s.list_events(None, 100).unwrap().len(), 3, "二次迁移丢了数据");
        // 分区绑定要能跨重开存活
        assert_eq!(
            s.get_event(V, "new-1").unwrap().unwrap().bound_count,
            Some(2)
        );

        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 未知事件类型也落库() {
        let s = store();
        let mut e = event("e9");
        e.kind = VendorEventKind::from_str("some_new_event");
        assert_eq!(e.kind, VendorEventKind::Unknown);
        s.record_event(&e).unwrap();
        let rec = s.get_event(V, "e9").unwrap().unwrap();
        assert_eq!(rec.event_type, "unknown");
    }
}
