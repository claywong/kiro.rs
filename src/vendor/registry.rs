//! 多卖家注册表：按 id 查服务、按 webhook 路径 token 反查归属
//!
//! 每家卖家一个 [`VendorService`] 实例，共用同一个事件库（行内按 `vendor_id`
//! 区分）与同一个 `AdminService`。注册表本身不可变 —— 卖家清单来自启动时的
//! 配置，运行期不增删，故无需加锁。
//!
//! @author wangzhong

use std::sync::Arc;

use crate::admin::AdminService;
use crate::http_client::ProxyConfig;
use crate::model::config::{Config, TlsBackend, VendorConfig};

use super::pool_gate::PoolGate;
use super::service::VendorService;
use super::store::SharedVendorStore;

/// 多卖家注册表
pub struct VendorRegistry {
    /// 按配置顺序排列。第一项是面板默认选中的那一家。
    services: Vec<Arc<VendorService>>,
    /// 全局提取闸门，各家共用同一个实例 —— 它的意义正在于跨供应商，
    /// 每家一份就退化成了各家自己的上限。
    pool_gate: Arc<PoolGate>,
}

impl VendorRegistry {
    /// 按解析后的卖家清单构建。空清单表示未配置卖家对接，
    /// 此时所有出站接口返回「未配置」、webhook 一律 404。
    pub fn new(
        vendors: Vec<VendorConfig>,
        proxy: Option<ProxyConfig>,
        tls_backend: TlsBackend,
        store: SharedVendorStore,
        admin: Arc<AdminService>,
        pool_target: u32,
    ) -> Self {
        let pool_gate = PoolGate::new(pool_target);
        if pool_target > 0 {
            tracing::info!(pool_target, "全局提取限制已启用");
        }
        let services = vendors
            .into_iter()
            .map(|cfg| {
                tracing::info!(
                    vendor_id = cfg.vendor_id(),
                    name = cfg.display_name(),
                    flavor = cfg.flavor.as_str(),
                    inbound = cfg.inbound_enabled(),
                    auto_purchase = cfg.auto_purchase,
                    per_channel = cfg.auto_purchase_per_channel,
                    "已注册卖家"
                );
                Arc::new(VendorService::new(
                    cfg,
                    proxy.clone(),
                    tls_backend,
                    store.clone(),
                    admin.clone(),
                    Arc::clone(&pool_gate),
                ))
            })
            .collect();
        Self {
            services,
            pool_gate,
        }
    }

    /// 从完整配置构建（合并 `vendor` 单例与 `vendors` 列表）
    pub fn from_config(
        config: &Config,
        proxy: Option<ProxyConfig>,
        tls_backend: TlsBackend,
        store: SharedVendorStore,
        admin: Arc<AdminService>,
    ) -> Self {
        Self::new(
            config.resolved_vendors(),
            proxy,
            tls_backend,
            store,
            admin,
            config.auto_purchase_pool_target,
        )
    }

    /// 全局提取闸门。面板读写阈值走这里，不经过任何单家服务。
    pub fn pool_gate(&self) -> &Arc<PoolGate> {
        &self.pool_gate
    }

    /// 是否配置了任何卖家
    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    pub fn len(&self) -> usize {
        self.services.len()
    }

    /// 全部卖家服务，按配置顺序
    pub fn all(&self) -> &[Arc<VendorService>] {
        &self.services
    }

    /// 面板默认选中的那一家（配置里的第一项）
    pub fn default_service(&self) -> Option<&Arc<VendorService>> {
        self.services.first()
    }

    /// 按 id 精确查找
    pub fn get(&self, vendor_id: &str) -> Option<&Arc<VendorService>> {
        let target = vendor_id.trim();
        self.services.iter().find(|s| s.vendor_id() == target)
    }

    /// 解析请求里的 `vendorId` 参数：给了就必须命中（给错不静默回退到别家，
    /// 否则会对着错误的卖家扣费）；没给则用默认那一家。
    pub fn resolve(&self, vendor_id: Option<&str>) -> Option<&Arc<VendorService>> {
        match vendor_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => self.get(id),
            None => self.default_service(),
        }
    }

    /// 按入站路径 token 反查归属的卖家。
    ///
    /// 各家 token 不同，故一条 webhook 能唯一定位来源。逐个常量时间比对，
    /// 全都不匹配返回 None（调用方一律回 404，不泄露端点是否存在）。
    pub fn find_by_path_token(&self, token: &str) -> Option<&Arc<VendorService>> {
        self.services.iter().find(|s| s.verify_path_token(token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::protocol::VendorFlavor;

    fn cfg(id: &str, flavor: VendorFlavor, token: &str) -> VendorConfig {
        VendorConfig {
            id: id.to_string(),
            name: String::new(),
            flavor,
            base_url: "https://v.example.com".to_string(),
            api_key: "k".to_string(),
            webhook_path_token: token.to_string(),
            default_groups: vec![],
            default_rpm_limit: 300,
            default_priority: None,
            default_api_region: String::new(),
            default_auth_region: String::new(),
            auto_purchase: false,
            auto_purchase_max_count: 1,
            auto_purchase_schedule: vec![],
            auto_purchase_per_channel: false,
            vendor_password: String::new(),
        }
    }

    /// 合并规则不依赖 AdminService，可单独测
    #[test]
    fn 单例与列表合并且按id去重() {
        let mut config = Config::default();
        config.vendor = Some(cfg("default", VendorFlavor::Legacy, "t1"));
        config.vendors = vec![
            cfg("kiroapp", VendorFlavor::Kiroapp, "t2"),
            // 与单例重名：必须被丢弃，否则两家事件互相污染
            cfg("default", VendorFlavor::Kiroapp, "t3"),
        ];

        let resolved = config.resolved_vendors();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].vendor_id(), "default", "单例排在最前");
        assert_eq!(resolved[1].vendor_id(), "kiroapp");
        assert_eq!(
            resolved[0].flavor,
            VendorFlavor::Legacy,
            "保留先出现的那一项"
        );
    }

    #[test]
    fn 配置不完整的项被丢弃() {
        let mut config = Config::default();
        let mut broken = cfg("broken", VendorFlavor::Legacy, "t");
        broken.api_key = "  ".to_string();
        let mut no_url = cfg("nourl", VendorFlavor::Legacy, "t");
        no_url.base_url = String::new();

        config.vendors = vec![broken, no_url, cfg("ok", VendorFlavor::Kiroapp, "t")];
        let resolved = config.resolved_vendors();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].vendor_id(), "ok");
    }

    #[test]
    fn 空配置解析为空清单() {
        let config = Config::default();
        assert!(config.resolved_vendors().is_empty());
    }

    #[test]
    fn id_为空时回退默认值() {
        let mut c = cfg("", VendorFlavor::Legacy, "t");
        assert_eq!(c.vendor_id(), crate::model::config::DEFAULT_VENDOR_ID);
        // 展示名未配时回退 id
        assert_eq!(c.display_name(), crate::model::config::DEFAULT_VENDOR_ID);
        c.name = "首家卖家".to_string();
        assert_eq!(c.display_name(), "首家卖家");
    }
}
