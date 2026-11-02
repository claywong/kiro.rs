//! KiroShop 本地状态库（SQLite）
//!
//! 三张表 + 一张 KV：
//! - `ks_orders`：**在途跟踪**。每笔经本模块下的单（含迁移发现的存量单）都记
//!   一行，`last_seq` 是增量拿号游标，`delivered/imported` 是已到货/已入库计数。
//! - `ks_shop_events`：卖家事件（webhook 与 `/events` 轮询共用，事件 id 主键去重）。
//! - `ks_local_events`：本模块产生的面板告警流水（余额低、补号可领、导入失败…）。
//! - `ks_kv`：杂项游标（余额告警状态机等）。
//!
//! @author wangzhong

use std::path::PathBuf;

use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

/// 在途跟踪的单行
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackedOrder {
    pub order_id: String,
    pub product_id: i64,
    pub product_name: String,
    pub quantity: u32,
    pub amount: Option<f64>,
    pub status: String,
    pub status_text: String,
    pub eta: Option<String>,
    pub lane: String,
    /// manual（面板手动）/ auto（自动补货）/ migrated（启动时发现的存量单）
    pub source: String,
    pub client_ref: Option<String>,
    pub last_seq: i64,
    pub delivered: u32,
    pub imported: u32,
    pub failed_imports: u32,
    pub last_error: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// 面板告警事件
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalEvent {
    pub id: i64,
    pub kind: String,
    pub severity: String,
    pub message: String,
    pub order_id: Option<String>,
    pub acked: bool,
    pub created_at: String,
}

pub struct KiroshopStore {
    conn: Mutex<Connection>,
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

impl KiroshopStore {
    pub fn open(path: PathBuf) -> rusqlite::Result<Self> {
        let conn = if path.as_os_str().is_empty() {
            Connection::open_in_memory()?
        } else {
            Connection::open(&path)?
        };
        Self::init(conn)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS ks_orders (
                order_id       TEXT PRIMARY KEY,
                product_id     INTEGER NOT NULL DEFAULT 0,
                product_name   TEXT NOT NULL DEFAULT '',
                quantity       INTEGER NOT NULL DEFAULT 0,
                amount         REAL,
                status         TEXT NOT NULL DEFAULT '',
                status_text    TEXT NOT NULL DEFAULT '',
                eta            TEXT,
                lane           TEXT NOT NULL DEFAULT '',
                source         TEXT NOT NULL DEFAULT 'manual',
                client_ref     TEXT,
                last_seq       INTEGER NOT NULL DEFAULT 0,
                delivered      INTEGER NOT NULL DEFAULT 0,
                imported       INTEGER NOT NULL DEFAULT 0,
                failed_imports INTEGER NOT NULL DEFAULT 0,
                last_error     TEXT,
                created_at     TEXT,
                updated_at     TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_ks_orders_status ON ks_orders(status);

            CREATE TABLE IF NOT EXISTS ks_shop_events (
                id          INTEGER PRIMARY KEY,
                type        TEXT NOT NULL,
                order_id    TEXT,
                created_at  TEXT,
                received_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS ks_local_events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                kind        TEXT NOT NULL,
                severity    TEXT NOT NULL DEFAULT 'info',
                message     TEXT NOT NULL,
                order_id    TEXT,
                acked       INTEGER NOT NULL DEFAULT 0,
                created_at  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS ks_kv (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ---- 订单跟踪 ----

    /// 插入或按卖家侧最新状态刷新（seq/imported 等本地方段不覆盖）
    pub fn upsert_order(&self, o: &TrackedOrder) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            r#"INSERT INTO ks_orders (order_id, product_id, product_name, quantity, amount,
                     status, status_text, eta, lane, source, client_ref, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
               ON CONFLICT(order_id) DO UPDATE SET
                 product_id=excluded.product_id, product_name=excluded.product_name,
                 quantity=excluded.quantity, amount=excluded.amount,
                 status=excluded.status, status_text=excluded.status_text,
                 eta=excluded.eta, lane=excluded.lane, updated_at=excluded.updated_at"#,
            params![
                o.order_id,
                o.product_id,
                o.product_name,
                o.quantity,
                o.amount,
                o.status,
                o.status_text,
                o.eta,
                o.lane,
                o.source,
                o.client_ref,
                o.created_at,
                now(),
            ],
        )?;
        Ok(())
    }

    /// 只刷新卖家侧状态字段（轮询路径用）
    #[allow(clippy::too_many_arguments)]
    pub fn refresh_order(
        &self,
        order_id: &str,
        status: &str,
        status_text: &str,
        delivered: u32,
        eta: Option<&str>,
    ) -> rusqlite::Result<()> {
        conn_update(
            &self.conn,
            "UPDATE ks_orders SET status=?1, status_text=?2, delivered=?3, eta=?4, updated_at=?5 WHERE order_id=?6",
            params![status, status_text, delivered, eta, now(), order_id],
        )
    }

    /// 推进增量拿号游标与入库计数
    pub fn advance_delivery(
        &self,
        order_id: &str,
        last_seq: i64,
        delivered: u32,
        imported_delta: u32,
        failed_delta: u32,
    ) -> rusqlite::Result<()> {
        conn_update(
            &self.conn,
            "UPDATE ks_orders SET last_seq=max(last_seq, ?1), delivered=?2,
                     imported=imported+?3, failed_imports=failed_imports+?4,
                     last_error=NULL, updated_at=?5 WHERE order_id=?6",
            params![last_seq, delivered, imported_delta, failed_delta, now(), order_id],
        )
    }

    pub fn set_order_error(&self, order_id: &str, error: Option<&str>) -> rusqlite::Result<()> {
        conn_update(
            &self.conn,
            "UPDATE ks_orders SET last_error=?1, updated_at=?2 WHERE order_id=?3",
            params![error, now(), order_id],
        )
    }

    pub fn tracked_order(&self, order_id: &str) -> rusqlite::Result<Option<TrackedOrder>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT order_id, product_id, product_name, quantity, amount, status, status_text,
                    eta, lane, source, client_ref, last_seq, delivered, imported,
                    failed_imports, last_error, created_at, updated_at
             FROM ks_orders WHERE order_id = ?1",
            params![order_id],
            row_to_tracked,
        )
        .optional()
    }

    pub fn tracked_orders(&self) -> rusqlite::Result<Vec<TrackedOrder>> {
        let conn = self.conn.lock();
        query_all(
            &conn,
            "SELECT order_id, product_id, product_name, quantity, amount, status, status_text,
                    eta, lane, source, client_ref, last_seq, delivered, imported,
                    failed_imports, last_error, created_at, updated_at
             FROM ks_orders ORDER BY created_at DESC LIMIT 500",
            [],
        )
    }

    // ---- 卖家事件（去重） ----

    /// 返回 false 表示事件已存在（webhook 与轮询双路径去重）
    pub fn record_shop_event(
        &self,
        id: i64,
        kind: &str,
        order_id: Option<&str>,
        created_at: Option<&str>,
    ) -> rusqlite::Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO ks_shop_events (id, type, order_id, created_at, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, kind, order_id, created_at, now()],
        )?;
        Ok(n > 0)
    }

    /// 事件轮询游标：已入库的最大事件 id（首次为 0，从当前时刻开始追）
    pub fn max_shop_event_id(&self) -> rusqlite::Result<i64> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM ks_shop_events", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0))
    }

    /// 撤掉一条已占坑的事件（处理失败时用，让卖家重试或轮询再次处理）
    pub fn remove_shop_event(&self, id: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM ks_shop_events WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ---- 本地告警 ----

    pub fn add_local_event(
        &self,
        kind: &str,
        severity: &str,
        message: &str,
        order_id: Option<&str>,
    ) -> rusqlite::Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO ks_local_events (kind, severity, message, order_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, severity, message, order_id, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_local_events(&self, limit: u32, unacked_only: bool) -> rusqlite::Result<Vec<LocalEvent>> {
        let conn = self.conn.lock();
        let sql = if unacked_only {
            "SELECT id, kind, severity, message, order_id, acked, created_at
             FROM ks_local_events WHERE acked = 0 ORDER BY id DESC LIMIT ?1"
        } else {
            "SELECT id, kind, severity, message, order_id, acked, created_at
             FROM ks_local_events ORDER BY id DESC LIMIT ?1"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(LocalEvent {
                id: r.get(0)?,
                kind: r.get(1)?,
                severity: r.get(2)?,
                message: r.get(3)?,
                order_id: r.get(4)?,
                acked: r.get::<_, i64>(5)? != 0,
                created_at: r.get(6)?,
            })
        })?;
        rows.collect()
    }

    pub fn unacked_local_count(&self) -> rusqlite::Result<u32> {
        let conn = self.conn.lock();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM ks_local_events WHERE acked = 0",
            [],
            |r| r.get::<_, i64>(0),
        )? as u32)
    }

    /// 确认告警：指定 id 或全部
    pub fn ack_local_events(&self, id: Option<i64>) -> rusqlite::Result<usize> {
        let conn = self.conn.lock();
        let n = match id {
            Some(id) => conn.execute(
                "UPDATE ks_local_events SET acked = 1 WHERE id = ?1",
                params![id],
            )?,
            None => conn.execute("UPDATE ks_local_events SET acked = 1", [])?,
        };
        Ok(n)
    }

    // ---- KV ----

    pub fn kv_get(&self, key: &str) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock();
        conn.query_row("SELECT value FROM ks_kv WHERE key = ?1", params![key], |r| {
            r.get::<_, String>(0)
        })
        .optional()
    }

    pub fn kv_set(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO ks_kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

fn conn_update(
    conn: &Mutex<Connection>,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> rusqlite::Result<()> {
    let conn = conn.lock();
    conn.execute(sql, params)?;
    Ok(())
}

fn row_to_tracked(r: &rusqlite::Row<'_>) -> rusqlite::Result<TrackedOrder> {
    Ok(TrackedOrder {
        order_id: r.get(0)?,
        product_id: r.get(1)?,
        product_name: r.get(2)?,
        quantity: r.get::<_, i64>(3)? as u32,
        amount: r.get(4)?,
        status: r.get(5)?,
        status_text: r.get(6)?,
        eta: r.get(7)?,
        lane: r.get(8)?,
        source: r.get(9)?,
        client_ref: r.get(10)?,
        last_seq: r.get(11)?,
        delivered: r.get::<_, i64>(12)? as u32,
        imported: r.get::<_, i64>(13)? as u32,
        failed_imports: r.get::<_, i64>(14)? as u32,
        last_error: r.get(15)?,
        created_at: r.get(16)?,
        updated_at: r.get(17)?,
    })
}

fn query_all<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> rusqlite::Result<Vec<TrackedOrder>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, row_to_tracked)?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> KiroshopStore {
        KiroshopStore::open_in_memory().unwrap()
    }

    fn tracked(id: &str, qty: u32, status: &str) -> TrackedOrder {
        TrackedOrder {
            order_id: id.to_string(),
            product_id: 403,
            product_name: "Kiro Pro".into(),
            quantity: qty,
            amount: Some(216.0),
            status: status.into(),
            source: "manual".into(),
            created_at: Some("2026-11-02T06:00:00Z".into()),
            ..Default::default()
        }
    }

    #[test]
    fn 订单upsert不覆盖本地游标() {
        let s = store();
        s.upsert_order(&tracked("o1", 3, "paid")).unwrap();
        s.advance_delivery("o1", 2, 2, 2, 0).unwrap();
        // 卖家侧刷新（upsert 同样字段）
        s.upsert_order(&tracked("o1", 3, "paid")).unwrap();
        let o = s.tracked_order("o1").unwrap().unwrap();
        assert_eq!(o.last_seq, 2, "upsert 不得重置增量游标");
        assert_eq!(o.imported, 2, "upsert 不得重置入库计数");
    }

    #[test]
    fn 事件按id去重() {
        let s = store();
        assert!(s.record_shop_event(7, "order.delivery", Some("o1"), None).unwrap());
        assert!(!s.record_shop_event(7, "order.delivery", Some("o1"), None).unwrap());
        assert_eq!(s.max_shop_event_id().unwrap(), 7);
        assert!(s.record_shop_event(9, "ping", None, None).unwrap());
        assert_eq!(s.max_shop_event_id().unwrap(), 9);
    }

    #[test]
    fn 本地告警可单条或全部确认() {
        let s = store();
        s.add_local_event("balance_low", "warn", "余额 10", None).unwrap();
        s.add_local_event("supplement", "info", "有补号", Some("o1")).unwrap();
        assert_eq!(s.unacked_local_count().unwrap(), 2);
        s.ack_local_events(Some(1)).unwrap();
        assert_eq!(s.unacked_local_count().unwrap(), 1);
        let unacked = s.list_local_events(10, true).unwrap();
        assert_eq!(unacked.len(), 1);
        assert_eq!(unacked[0].kind, "supplement");
        s.ack_local_events(None).unwrap();
        assert_eq!(s.unacked_local_count().unwrap(), 0);
    }

    #[test]
    fn 游标只进不退() {
        let s = store();
        s.upsert_order(&tracked("o1", 3, "paid")).unwrap();
        s.advance_delivery("o1", 5, 3, 3, 0).unwrap();
        // 迟到的重复事件重放旧 seq，不得回退
        s.advance_delivery("o1", 2, 3, 0, 0).unwrap();
        assert_eq!(s.tracked_order("o1").unwrap().unwrap().last_seq, 5);
    }

    #[test]
    fn kv读写与覆盖() {
        let s = store();
        assert!(s.kv_get("k").unwrap().is_none());
        s.kv_set("k", "v1").unwrap();
        s.kv_set("k", "v2").unwrap();
        assert_eq!(s.kv_get("k").unwrap().as_deref(), Some("v2"));
    }
}
