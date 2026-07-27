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

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS vendor_events (
    event_id           TEXT PRIMARY KEY,
    event_type         TEXT NOT NULL,
    purchase_order_id  TEXT,
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
    validation_used    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_vendor_events_received
    ON vendor_events (received_at DESC);
CREATE INDEX IF NOT EXISTS idx_vendor_events_acked
    ON vendor_events (acked, received_at DESC);
"#;

/// 存量库补列。`CREATE TABLE IF NOT EXISTS` 对已存在的表不生效，而这个库里的
/// 订单号与绑定数量是长期资产、不能重建，故逐列 ALTER 并忽略「列已存在」错误。
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE vendor_events ADD COLUMN purchase_trigger TEXT",
    "ALTER TABLE vendor_events ADD COLUMN validation_status TEXT",
    "ALTER TABLE vendor_events ADD COLUMN validation_detail TEXT",
    "ALTER TABLE vendor_events ADD COLUMN validated_at TEXT",
    "ALTER TABLE vendor_events ADD COLUMN validation_used INTEGER NOT NULL DEFAULT 0",
];

/// 事件类型。未知类型也落库（`Unknown`），避免卖家新增事件时丢数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorEventKind {
    NewKeysAvailable,
    AllKeysDead,
    Unknown,
}

impl VendorEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewKeysAvailable => "new_keys_available",
            Self::AllKeysDead => "all_keys_dead",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "new_keys_available" => Self::NewKeysAvailable,
            "all_keys_dead" => Self::AllKeysDead,
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
    /// 窗口结束仍无结论（含仅被人工禁用的情况）
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
    pub event_id: String,
    pub kind: VendorEventKind,
    pub purchase_order_id: Option<String>,
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
    pub event_id: String,
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purchase_order_id: Option<String>,
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
        apply_migrations(&conn);
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 内存库（打开文件失败时兜底，保证 Admin 查询不崩）
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
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
                (event_id, event_type, purchase_order_id, message, new_keys, dead,
                 raw_payload, received_at, delivery_count, acked)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, 0)
             ON CONFLICT(event_id) DO UPDATE SET
                delivery_count = delivery_count + 1",
            rusqlite::params![
                event.event_id,
                event.kind.as_str(),
                event.purchase_order_id,
                event.message,
                event.new_keys,
                event.dead,
                event.raw_payload,
                Utc::now().to_rfc3339(),
            ],
        )?;
        // ON CONFLICT DO UPDATE 也返回 1，靠 delivery_count 判断是否首次
        let count: u32 = conn.query_row(
            "SELECT delivery_count FROM vendor_events WHERE event_id = ?1",
            [&event.event_id],
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
    pub fn get_event(&self, event_id: &str) -> rusqlite::Result<Option<VendorEventRecord>> {
        let conn = self.conn.lock();
        conn.query_row(
            &format!("SELECT {SELECT_COLUMNS} FROM vendor_events WHERE event_id = ?1"),
            [event_id],
            row_to_record,
        )
        .optional()
    }

    /// 事件列表（按接收时间倒序）
    pub fn list_events(&self, limit: usize) -> rusqlite::Result<Vec<VendorEventRecord>> {
        let limit = limit.clamp(1, 1000);
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SELECT_COLUMNS} FROM vendor_events
             ORDER BY received_at DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit], row_to_record)?;
        rows.collect()
    }

    /// 最近一条 `all_keys_dead` 事件。自动提取要靠它的确认结论授权。
    pub fn latest_dead_event(&self) -> rusqlite::Result<Option<VendorEventRecord>> {
        let conn = self.conn.lock();
        conn.query_row(
            &format!(
                "SELECT {SELECT_COLUMNS} FROM vendor_events
                 WHERE event_type = ?1
                 ORDER BY received_at DESC LIMIT 1"
            ),
            [VendorEventKind::AllKeysDead.as_str()],
            row_to_record,
        )
        .optional()
    }

    /// 写入失效确认结论
    pub fn set_validation(
        &self,
        event_id: &str,
        status: ValidationStatus,
        detail: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE vendor_events SET
                validation_status = ?2, validation_detail = ?3, validated_at = ?4
             WHERE event_id = ?1",
            rusqlite::params![event_id, status.as_str(), detail, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// 抢占式消费确认结论：只有 `validation_used = 0` 且结论为 `confirmed_dead`
    /// 时才置位并返回 true。
    ///
    /// 一次确认只授权一轮自动提取 —— 否则同一条 `all_keys_dead` 会给后续每条
    /// `new_keys_available` 都开绿灯，变成无限自动扣费。单条 UPDATE 完成判断与
    /// 写入，并发触发只有一个能拿到。
    pub fn consume_validation(&self, event_id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "UPDATE vendor_events SET validation_used = 1
             WHERE event_id = ?1 AND validation_used = 0
               AND validation_status = ?2",
            rusqlite::params![event_id, ValidationStatus::ConfirmedDead.as_str()],
        )?;
        Ok(changed > 0)
    }

    /// 未确认（未点「已知悉」）的事件数，用于 tab 红点
    pub fn unacked_count(&self) -> rusqlite::Result<u32> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM vendor_events WHERE acked = 0",
            [],
            |row| row.get(0),
        )
    }

    /// 标记事件已知悉。`event_id` 为 None 时标记全部。
    pub fn ack(&self, event_id: Option<&str>) -> rusqlite::Result<usize> {
        let conn = self.conn.lock();
        match event_id {
            Some(id) => conn.execute(
                "UPDATE vendor_events SET acked = 1 WHERE event_id = ?1",
                [id],
            ),
            None => conn.execute("UPDATE vendor_events SET acked = 1 WHERE acked = 0", []),
        }
    }

    /// 抢占式绑定提取数量。
    ///
    /// 这是防重复扣费的核心：只有 `bound_count IS NULL`（从未提交过 purchase）时
    /// 才写入并返回 `Ok(count)`；否则返回 `Err(已绑定的值)`，调用方必须改用该值重试。
    /// 单条 UPDATE 完成判断与写入，并发点击「提取」只有一个能抢到。
    pub fn bind_count(&self, event_id: &str, count: u32) -> rusqlite::Result<Result<u32, u32>> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "UPDATE vendor_events SET bound_count = ?2
             WHERE event_id = ?1 AND bound_count IS NULL",
            rusqlite::params![event_id, count],
        )?;
        if changed > 0 {
            return Ok(Ok(count));
        }
        let existing: Option<u32> = conn
            .query_row(
                "SELECT bound_count FROM vendor_events WHERE event_id = ?1",
                [event_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        match existing {
            Some(v) => Ok(Err(v)),
            // 行不存在：交由调用方按「事件不存在」处理
            None => Ok(Ok(count)),
        }
    }

    /// 写回提取结果
    pub fn finish_purchase(
        &self,
        event_id: &str,
        status: PurchaseStatus,
        trigger: PurchaseTrigger,
        outcome: &PurchaseOutcome,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE vendor_events SET
                purchase_status = ?2, purchased = ?3, imported = ?4,
                duplicated = ?5, failed = ?6, last_error = ?7, processed_at = ?8,
                purchase_trigger = ?9
             WHERE event_id = ?1",
            rusqlite::params![
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
    pub fn record_skip(&self, event_id: &str, reason: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE vendor_events SET
                purchase_status = ?4, last_error = ?2, processed_at = ?3,
                purchase_trigger = ?5
             WHERE event_id = ?1 AND bound_count IS NULL",
            rusqlite::params![
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

/// 所有查询共用的列清单，保证列序与 [`row_to_record`] 的下标一致
const SELECT_COLUMNS: &str = "event_id, event_type, purchase_order_id, message, new_keys, dead,
     received_at, delivery_count, acked, bound_count, purchase_status,
     purchased, imported, duplicated, failed, last_error, processed_at,
     purchase_trigger, validation_status, validation_detail, validated_at, validation_used";

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<VendorEventRecord> {
    Ok(VendorEventRecord {
        event_id: row.get(0)?,
        event_type: row.get(1)?,
        purchase_order_id: row.get(2)?,
        message: row.get(3)?,
        new_keys: row.get(4)?,
        dead: row.get(5)?,
        received_at: row.get(6)?,
        delivery_count: row.get(7)?,
        acked: row.get::<_, i64>(8)? != 0,
        bound_count: row.get(9)?,
        purchase_status: row.get(10)?,
        purchased: row.get(11)?,
        imported: row.get(12)?,
        duplicated: row.get(13)?,
        failed: row.get(14)?,
        last_error: row.get(15)?,
        processed_at: row.get(16)?,
        purchase_trigger: row.get(17)?,
        validation_status: row.get(18)?,
        validation_detail: row.get(19)?,
        validated_at: row.get(20)?,
        validation_used: row.get::<_, i64>(21)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> VendorStore {
        VendorStore::open_in_memory().expect("内存库初始化失败")
    }

    fn event(id: &str) -> IncomingEvent {
        IncomingEvent {
            event_id: id.to_string(),
            kind: VendorEventKind::NewKeysAvailable,
            purchase_order_id: Some("0123456789abcdef0123456789abcdef".to_string()),
            message: Some("新一轮 10 个 Key 已就绪".to_string()),
            new_keys: Some(10),
            dead: None,
            raw_payload: "{}".to_string(),
        }
    }

    #[test]
    fn 首次落库与重投判定() {
        let s = store();
        let e = event("e1");
        assert_eq!(s.record_event(&e).unwrap(), RecordOutcome::Inserted);
        assert_eq!(s.record_event(&e).unwrap(), RecordOutcome::Duplicate);
        let rec = s.get_event("e1").unwrap().unwrap();
        assert_eq!(rec.delivery_count, 2);
        assert_eq!(rec.new_keys, Some(10));
    }

    #[test]
    fn 重投不清掉已有提取结果() {
        let s = store();
        let e = event("e1");
        s.record_event(&e).unwrap();
        s.bind_count("e1", 5).unwrap().unwrap();
        s.finish_purchase(
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
        let rec = s.get_event("e1").unwrap().unwrap();
        assert_eq!(rec.bound_count, Some(5));
        assert_eq!(rec.purchase_status.as_deref(), Some("done"));
        assert_eq!(rec.imported, Some(5));
    }

    #[test]
    fn count_只能绑定一次() {
        let s = store();
        s.record_event(&event("e1")).unwrap();
        assert_eq!(s.bind_count("e1", 5).unwrap(), Ok(5));
        // 换数量重试 → 返回已绑定值，调用方必须复用
        assert_eq!(s.bind_count("e1", 10).unwrap(), Err(5));
        assert_eq!(s.bind_count("e1", 5).unwrap(), Err(5));
    }

    #[test]
    fn 失败后仍保留绑定值供重试() {
        let s = store();
        s.record_event(&event("e1")).unwrap();
        s.bind_count("e1", 7).unwrap().unwrap();
        s.finish_purchase(
            "e1",
            PurchaseStatus::Failed,
            PurchaseTrigger::Manual,
            &PurchaseOutcome {
                last_error: Some("余额不足".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let rec = s.get_event("e1").unwrap().unwrap();
        assert_eq!(rec.bound_count, Some(7));
        assert_eq!(rec.purchase_status.as_deref(), Some("failed"));
        assert_eq!(rec.last_error.as_deref(), Some("余额不足"));
    }

    #[test]
    fn 未确认计数与确认() {
        let s = store();
        s.record_event(&event("e1")).unwrap();
        s.record_event(&event("e2")).unwrap();
        assert_eq!(s.unacked_count().unwrap(), 2);
        s.ack(Some("e1")).unwrap();
        assert_eq!(s.unacked_count().unwrap(), 1);
        s.ack(None).unwrap();
        assert_eq!(s.unacked_count().unwrap(), 0);
    }

    #[test]
    fn 列表按时间倒序且可限量() {
        let s = store();
        for i in 0..5 {
            s.record_event(&event(&format!("e{i}"))).unwrap();
        }
        assert_eq!(s.list_events(3).unwrap().len(), 3);
        assert_eq!(s.list_events(100).unwrap().len(), 5);
    }

    fn dead_event(id: &str) -> IncomingEvent {
        IncomingEvent {
            event_id: id.to_string(),
            kind: VendorEventKind::AllKeysDead,
            purchase_order_id: None,
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
        assert!(!s.consume_validation("d1").unwrap());

        s.set_validation("d1", ValidationStatus::ConfirmedDead, "全部失效")
            .unwrap();
        assert!(s.consume_validation("d1").unwrap());
        // 第二轮 new_keys_available 不能再靠同一条确认扣费
        assert!(!s.consume_validation("d1").unwrap());
        assert!(s.get_event("d1").unwrap().unwrap().validation_used);
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
            s.set_validation("d1", st, "x").unwrap();
            assert!(!s.consume_validation("d1").unwrap(), "{}", st.as_str());
        }
    }

    #[test]
    fn 只取最近一条失效事件() {
        let s = store();
        s.record_event(&event("k1")).unwrap();
        s.record_event(&dead_event("d1")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.record_event(&dead_event("d2")).unwrap();
        let latest = s.latest_dead_event().unwrap().unwrap();
        assert_eq!(latest.event_id, "d2");
    }

    #[test]
    fn 无失效事件时返回空() {
        let s = store();
        s.record_event(&event("k1")).unwrap();
        assert!(s.latest_dead_event().unwrap().is_none());
    }

    #[test]
    fn 跳过不占订单号且已提取的不被覆盖() {
        let s = store();
        s.record_event(&event("e1")).unwrap();
        s.record_skip("e1", "本地仍有健康 Key").unwrap();
        let rec = s.get_event("e1").unwrap().unwrap();
        assert_eq!(rec.purchase_status.as_deref(), Some("skipped"));
        assert_eq!(rec.purchase_trigger.as_deref(), Some("auto"));
        // 关键：数量未绑定，用户仍可手动按任意数量提取
        assert_eq!(rec.bound_count, None);

        // 已绑定过的事件不该被跳过记录覆盖
        s.bind_count("e1", 2).unwrap().unwrap();
        s.finish_purchase(
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
        s.record_skip("e1", "不该覆盖").unwrap();
        let rec = s.get_event("e1").unwrap().unwrap();
        assert_eq!(rec.purchase_status.as_deref(), Some("done"));
        assert_eq!(rec.purchase_trigger.as_deref(), Some("auto"));
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
        let rec = s.get_event("old-1").unwrap().expect("历史记录丢失");
        assert_eq!(rec.bound_count, Some(7), "历史绑定数量被破坏");
        assert_eq!(rec.purchase_status.as_deref(), Some("done"));
        // 新列对历史行为空，且 validation_used 取默认值
        assert_eq!(rec.purchase_trigger, None);
        assert_eq!(rec.validation_status, None);
        assert!(!rec.validation_used);

        // 补列后新字段可正常读写
        s.set_validation("old-1", ValidationStatus::ConfirmedDead, "测试")
            .unwrap();
        assert_eq!(
            s.get_event("old-1").unwrap().unwrap().validation_status.as_deref(),
            Some("confirmed_dead")
        );

        // 重复打开（再跑一次 ALTER）不应报错或丢数据
        drop(s);
        let s = VendorStore::open(path.clone()).expect("二次打开失败");
        assert_eq!(s.get_event("old-1").unwrap().unwrap().bound_count, Some(7));

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
        let rec = s.get_event("e9").unwrap().unwrap();
        assert_eq!(rec.event_type, "unknown");
    }
}
