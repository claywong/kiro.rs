//! Kiro IDE 版本
//!
//! UA（`KiroIDE-<version>-<machineId>`）里的 IDE 版本号**全部钉死**在
//! [`IDE_UA_KIRO_VERSION`]，因为上游把版本号当准入条件，且它必须与
//! [`USAGE_API_AWS_SDK_VERSION`] 成对才经过实测。用量链路与流式链路同取该常量，
//! 只有这一个源头。需要临时对齐别的版本时在 `config.json` 设 `kiroVersion` 覆盖。
//!
//! 本模块同时从官方稳定版元数据端点读取 `currentRelease`，但**仅用于观测**：
//! 官方发版后日志提示钉死值已落后，提醒实测后更新常量。
//!
//! - 进程内缓存（`OnceLock<RwLock<Option<String>>>`）+ 后台定时刷新；
//! - 跨平台 `currentRelease` 一致，任选可用平台的元数据即可；
//! - 获取失败只少一条提示，不影响 UA、不阻塞启动。
//!
//! 注意：用量类 REST 接口（getUsageLimits / ListAvailableModels / setUserPreference）
//! 不使用这里的「最新版本」，而是固定使用 [`USAGE_API_KIRO_VERSION`] +
//! [`USAGE_API_AWS_SDK_VERSION`]，并且必须携带 profileArn。详见那两个常量的说明。

use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::RwLock;
use serde::Deserialize;

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;

/// 官方稳定版元数据端点（`currentRelease` 即当前 IDE 版本，跨平台一致）。
///
/// 注意：必须使用 `linux-x64` / `darwin-*` 路径——`win32-*` 路径在 CDN 上返回 403
/// （Windows 走不同的分发格式）。版本号本身与平台无关，任选可用平台即可。
const METADATA_URL: &str =
    "https://prod.download.desktop.kiro.dev/stable/metadata-linux-x64-stable.json";

/// 用量类接口（getUsageLimits / ListAvailableModels / setUserPreference）固定使用的
/// Kiro IDE 版本。
///
/// 上游把 UA 里的版本号当准入条件，且 profileArn 已从可选变为必填。实测（2026-08-25,
/// BuilderID 账号）四格对照：
///
/// | | 无 profileArn | 带 profileArn |
/// |---|---|---|
/// | `KiroIDE-0.9.2` / sdk 1.0.0 | 403 not authorized | getUsageLimits 200 / ListAvailableModels 403 |
/// | `KiroIDE-0.12.155` / sdk 1.0.34 | 400 Invalid profileArn | 两个接口均 200 |
///
/// 所以两个条件必须同时满足：版本号升到下面这组，且请求带上 profileArn
/// （BuilderID 用占位符 ARN，见 `KiroCredentials::streaming_profile_arn`）。
/// 只改一个都不行。
///
/// 该门槛只作用于 BuilderID / IdC；Social（Github / Google）与 API Key 凭据两组 UA 都通。
///
/// 升级时这两个常量要一起动，混搭（新版本号 + 旧 SDK）未验证过。
pub const USAGE_API_KIRO_VERSION: &str = "0.12.155";

/// 用量类接口 UA 里的 aws-sdk-js 版本，与 [`USAGE_API_KIRO_VERSION`] 配套。
///
/// 流式端点（`generateAssistantResponse`）用的是同一套上游 SDK，故共用该常量，
/// 避免两处各写一份导致升级时漂移（上游把版本号当准入条件，混搭未验证过）。
pub const USAGE_API_AWS_SDK_VERSION: &str = "1.0.34";

/// IDE 类 UA（`KiroIDE-<version>-<machineId>`）钉死使用的 Kiro IDE 版本。
///
/// **有意不跟随自动获取到的最新版本**，理由是版本号与 SDK 版本必须成对：
/// 上游把 UA 里的版本号当准入条件，而 `(0.12.155, aws-sdk-js 1.0.34)` 是 2026-08
/// 实测确认两个受限接口都回 200 的组合。自动获取只给出 IDE 版本，拿不到它配套的
/// SDK 版本，跟版就会发出「新版本号 + 旧 SDK」这种未验证过的混搭。
///
/// 准入是「下限」判定，用已验证可用的组合比追最新更稳妥；也让所有部署的 UA 确定、
/// 可复现，不会因为官方发版而在无代码变更的情况下改变行为。
///
/// 与 [`USAGE_API_KIRO_VERSION`] 取同一个值：全仓库 IDE 版本号只有这一个源头，
/// 用量链路与流式链路不会再各自漂移。升级时连 [`USAGE_API_AWS_SDK_VERSION`]
/// 一起动，并重新实测。
///
/// 需要临时对齐某个特定版本时，在 `config.json` 里显式设 `kiroVersion` 即可覆盖，
/// 不必改代码（见 [`effective_ide`]）。
pub const IDE_UA_KIRO_VERSION: &str = USAGE_API_KIRO_VERSION;

static LATEST_VERSION: OnceLock<RwLock<Option<String>>> = OnceLock::new();

fn cell() -> &'static RwLock<Option<String>> {
    LATEST_VERSION.get_or_init(|| RwLock::new(None))
}

/// 已自动获取到的最新 Kiro IDE 版本（后台刷新成功后才有值）
pub fn cached() -> Option<String> {
    cell().read().clone()
}

/// 返回 IDE 类 UA（`KiroIDE-<version>-<machineId>`）该用的 Kiro IDE 版本。
///
/// 取值顺序：用户显式配置的 `kiroVersion` → [`IDE_UA_KIRO_VERSION`]。
/// **不查自动获取到的最新版本**，理由见 [`IDE_UA_KIRO_VERSION`]。
///
/// `configured` 恰好等于 kiro-cli 的默认版本时视为「用户没配过」：该字段默认值是
/// kiro-cli 的产品版本，并不存在名为 `KiroIDE-2.3.0` 的发布，直接发出去会被上游
/// 按准入条件判 403 `User is not authorized to make this call.`
pub fn effective_ide(configured: &str) -> String {
    let configured = configured.trim();
    if configured.is_empty() || configured == CLI_DEFAULT_KIRO_VERSION {
        return IDE_UA_KIRO_VERSION.to_string();
    }
    configured.to_string()
}

/// `config.kiro_version` 的默认值，语义上属于 kiro-cli 而非 Kiro IDE。
///
/// 与 `crate::model::config::default_kiro_version` 保持一致；此处单列一份是为了让
/// [`effective_ide`] 能识别「用户其实没配过」这一情形。改动其一必须同步另一处，
/// 相应断言见本模块测试 `test_cli_default_matches_config_default`。
pub const CLI_DEFAULT_KIRO_VERSION: &str = "2.3.0";

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "currentRelease")]
    current_release: Option<String>,
}

/// 拉取一次最新版本号
pub async fn fetch_latest(
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<String> {
    let client = build_client(proxy, 15, tls_backend)?;
    let resp = client.get(METADATA_URL).send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("获取 Kiro 版本元数据失败: {}", status);
    }
    let meta: Metadata = resp.json().await?;
    meta.current_release
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("元数据缺少 currentRelease"))
}

/// 启动后台任务：立即拉取一次，之后每 `interval` 刷新一次。
///
/// **拉到的版本不参与 UA 构造**（UA 钉死在 [`IDE_UA_KIRO_VERSION`]），仅用于观测：
/// 官方发版后日志会提示钉死值已落后，提醒去实测并更新常量。
/// 失败仅记录告警，不影响服务。
pub fn spawn_refresher(proxy: Option<ProxyConfig>, tls_backend: TlsBackend, interval: Duration) {
    tokio::spawn(async move {
        loop {
            match fetch_latest(proxy.as_ref(), tls_backend).await {
                Ok(version) => {
                    let changed = cached().as_deref() != Some(version.as_str());
                    *cell().write() = Some(version.clone());
                    if changed {
                        if version == IDE_UA_KIRO_VERSION {
                            tracing::info!("官方 Kiro IDE 版本 {}，与 UA 钉死值一致", version);
                        } else {
                            tracing::info!(
                                "官方 Kiro IDE 版本已是 {}，UA 仍钉死 {}（有意为之：\
                                 版本号需与 aws-sdk-js {} 成对实测后才更新，见 IDE_UA_KIRO_VERSION）",
                                version,
                                IDE_UA_KIRO_VERSION,
                                USAGE_API_AWS_SDK_VERSION
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("自动获取 Kiro IDE 版本失败（不影响 UA，仅少一条版本漂移提示）: {}", e);
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_parses_current_release() {
        let json = r#"{"currentRelease":"0.12.301","releases":[]}"#;
        let meta: Metadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.current_release.as_deref(), Some("0.12.301"));
    }

    #[test]
    fn test_effective_falls_back_without_cache() {
        // 显式配置的版本原样返回（钉死逻辑只在未配置时生效）
        let v = effective_ide("0.9.2");
        assert_eq!(v, "0.9.2");
    }
}

// 本地新增：IDE 版本兜底相关测试单独成块，避免与上游 `mod tests` 的改动挤在一起。
#[cfg(test)]
mod ide_fallback_tests {
    use super::*;

    /// `CLI_DEFAULT_KIRO_VERSION` 必须与 config 的默认值一致，否则
    /// `effective_ide` 识别不出「用户没配过」，兜底逻辑会静默失效。
    #[test]
    fn test_cli_default_matches_config_default() {
        let cfg = crate::model::config::Config::default();
        assert_eq!(
            cfg.kiro_version, CLI_DEFAULT_KIRO_VERSION,
            "config 默认 kiroVersion 变了，请同步 CLI_DEFAULT_KIRO_VERSION"
        );
    }

    /// 钉死值不能是 CLI 那个默认值，且必须与用量链路同源。
    #[test]
    fn test_ide_pin_is_not_cli_default() {
        assert_ne!(IDE_UA_KIRO_VERSION, CLI_DEFAULT_KIRO_VERSION);
        assert_eq!(
            IDE_UA_KIRO_VERSION, USAGE_API_KIRO_VERSION,
            "IDE 与用量链路的版本号必须同源，否则两条链路会漂移"
        );
    }

    /// 未配置（或配了 CLI 默认值 / 空串）→ 钉死值；显式配了别的值 → 原样尊重。
    #[test]
    fn test_effective_ide_pins_unless_explicitly_configured() {
        assert_eq!(effective_ide(CLI_DEFAULT_KIRO_VERSION), IDE_UA_KIRO_VERSION);
        assert_eq!(effective_ide(""), IDE_UA_KIRO_VERSION);
        assert_eq!(effective_ide("   "), IDE_UA_KIRO_VERSION);
        assert_eq!(effective_ide("9.9.9"), "9.9.9");
    }

    /// 钉死的核心含义：自动获取到的版本**不得**影响 UA。
    ///
    /// 直接往全局缓存里塞一个不同的版本，`effective_ide` 必须无视它。
    #[test]
    fn test_effective_ide_ignores_fetched_version() {
        *cell().write() = Some("1.0.437".to_string());
        assert_eq!(
            effective_ide(CLI_DEFAULT_KIRO_VERSION),
            IDE_UA_KIRO_VERSION,
            "UA 版本号已钉死，不应跟随自动获取到的版本"
        );
        assert_eq!(cached().as_deref(), Some("1.0.437"), "观测值本身仍应可读");
        *cell().write() = None;
    }
}
