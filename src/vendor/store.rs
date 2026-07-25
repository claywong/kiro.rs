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
    processed_at       TEXT
);
CREATE INDEX IF NOT EXISTS idx_vendor_events_received
    ON vendor_events (received_at DESC);
CREATE INDEX IF NOT EXISTS idx_vendor_events_acked
    ON vendor_events (acked, received_at DESC);
"#;

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
}

impl PurchaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
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
            "SELECT event_id, event_type, purchase_order_id, message, new_keys, dead,
                    received_at, delivery_count, acked, bound_count, purchase_status,
                    purchased, imported, duplicated, failed, last_error, processed_at
             FROM vendor_events WHERE event_id = ?1",
            [event_id],
            row_to_record,
        )
        .optional()
    }

    /// 事件列表（按接收时间倒序）
    pub fn list_events(&self, limit: usize) -> rusqlite::Result<Vec<VendorEventRecord>> {
        let limit = limit.clamp(1, 1000);
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT event_id, event_type, purchase_order_id, message, new_keys, dead,
                    received_at, delivery_count, acked, bound_count, purchase_status,
                    purchased, imported, duplicated, failed, last_error, processed_at
             FROM vendor_events ORDER BY received_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], row_to_record)?;
        rows.collect()
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
        outcome: &PurchaseOutcome,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE vendor_events SET
                purchase_status = ?2, purchased = ?3, imported = ?4,
                duplicated = ?5, failed = ?6, last_error = ?7, processed_at = ?8
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
            ],
        )?;
        Ok(())
    }
}

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
