//! 手动流量入口开关。
//!
//! 把配置中的期望状态同步到外部系统的指定账号：开启对应 `schedulable=true`，关闭
//! 对应 `schedulable=false`。与健康联动独立，不读取本地健康判据。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use reqwest::Client;
use tokio::sync::Notify;

use crate::model::config::TrafficIngressConfig;

use super::schedulable_client::{SchedulableTarget, push_all};

const APPLIED_NONE: u8 = 0;
const APPLIED_FALSE: u8 = 1;
const APPLIED_TRUE: u8 = 2;

pub struct TrafficIngressState {
    enabled: AtomicBool,
    applied: AtomicU8,
    changed: Notify,
}

impl TrafficIngressState {
    fn new(enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(enabled),
            applied: AtomicU8::new(APPLIED_NONE),
            changed: Notify::new(),
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        if self.enabled.swap(enabled, Ordering::Relaxed) != enabled {
            self.changed.notify_one();
        }
    }

    pub fn applied(&self) -> Option<bool> {
        match self.applied.load(Ordering::Relaxed) {
            APPLIED_FALSE => Some(false),
            APPLIED_TRUE => Some(true),
            _ => None,
        }
    }

    fn set_applied(&self, enabled: bool) {
        self.applied.store(
            if enabled { APPLIED_TRUE } else { APPLIED_FALSE },
            Ordering::Relaxed,
        );
    }
}

pub type SharedTrafficIngressState = Arc<TrafficIngressState>;

/// 配置完整时启动同步任务。启动后会立即把外部账号对齐到持久化的开关状态。
pub fn spawn(config: TrafficIngressConfig, client: Client) -> Option<SharedTrafficIngressState> {
    if !config.is_configured() {
        return None;
    }

    let state = TrafficIngressState::new(config.enabled);
    tracing::info!(
        base_url = %config.normalized_base_url(),
        accounts = ?config.account_ids,
        enabled = config.enabled,
        "流量入口控制器已就绪"
    );
    tokio::spawn(run(Arc::new(config), client, Arc::clone(&state)));
    Some(state)
}

async fn run(config: Arc<TrafficIngressConfig>, client: Client, state: SharedTrafficIngressState) {
    let retry_interval = Duration::from_secs(config.retry_interval_secs.max(5));

    loop {
        let desired = state.enabled();
        let target = SchedulableTarget {
            label: "流量入口",
            base_url: config.normalized_base_url(),
            token: &config.token,
            auth_header: config.auth_header(),
            account_ids: &config.account_ids,
            max_attempts: config.max_attempts,
        };
        let pushed = push_all(&target, &client, desired).await;

        // 切换可能发生在推送途中。只有期望值仍未变化时，才把本轮记为已应用；
        // 否则直接进入下一轮，把最新值覆盖过去。
        if pushed && state.enabled() == desired {
            state.set_applied(desired);
            state.changed.notified().await;
        } else if state.enabled() != desired {
            continue;
        } else {
            tokio::select! {
                _ = tokio::time::sleep(retry_interval) => {}
                _ = state.changed.notified() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn record_schedulable(
        axum::extract::Path(account_id): axum::extract::Path<u64>,
        axum::extract::State(sender): axum::extract::State<
            tokio::sync::mpsc::UnboundedSender<(u64, bool)>,
        >,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> axum::http::StatusCode {
        let enabled = body
            .get("schedulable")
            .and_then(serde_json::Value::as_bool)
            .unwrap();
        sender.send((account_id, enabled)).unwrap();
        axum::http::StatusCode::OK
    }

    #[tokio::test]
    async fn 启动对齐配置且手动切换立即推送() {
        use axum::{Router, routing::post};

        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new()
            .route(
                "/api/v1/admin/accounts/{id}/schedulable",
                post(record_schedulable),
            )
            .with_state(sender);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = TrafficIngressConfig {
            enabled: false,
            base_url: format!("http://{address}"),
            token: "test-token".into(),
            account_ids: vec![42],
            retry_interval_secs: 60,
            max_attempts: 1,
            ..Default::default()
        };
        let state = spawn(config, Client::new()).unwrap();

        let initial = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(initial, (42, false));

        state.set_enabled(true);
        let changed = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(changed, (42, true));

        tokio::time::timeout(Duration::from_secs(1), async {
            while state.applied() != Some(true) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        server.abort();
    }
}
