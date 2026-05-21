//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use chrono::{DateTime, Duration, Timelike, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::http_client::ProxyConfig;
use crate::kiro::auth::idc::{self, BUILDER_ID_START_URL};
use crate::kiro::auth::social;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::Config;

use super::error::AdminServiceError;
use super::proxy_pool::{GetUrlResult, ProxyPoolManager};
use super::types::{
    AddCredentialRequest, AddCredentialResponse, AssignProxyRequest, BalanceResponse,
    BatchAddProxyRequest, CredentialStatusItem, CredentialsStatusResponse,
    EnableOverageAllResult, ImageUpdateResponse, LoadBalancingModeResponse, PollIdcLoginResponse,
    ProxyPoolEntry, ProxyPoolResponse, QuotaExceededResult, SetLoadBalancingModeRequest,
    SetUpdateConfigRequest, StartIdcLoginRequest, StartIdcLoginResponse, StartSocialLoginRequest,
    StartSocialLoginResponse, UpdateCheckInfo, UpdateConfigResponse, UpdateCredentialRequest,
    UpdateRefreshTokenRequest,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 在线检查更新结果缓存时间（秒），30 分钟。
/// 在线检查更新结果缓存时间（秒），30 分钟。
/// Docker Hub 的 tags 接口对匿名访问有 IP 维度的限流，30 分钟 TTL 既能让用户
/// 看到红点提醒，又能避免短时间内重复请求被限流。
const UPDATE_CHECK_TTL_SECS: i64 = 1800;

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

/// 缓存的"检查更新"结果
#[derive(Debug, Clone)]
struct CachedUpdateCheck {
    /// 缓存时间
    cached_at: DateTime<Utc>,
    /// 拉取到的更新信息
    info: UpdateCheckInfo,
}

#[derive(Debug, Clone)]
struct RuntimeUpdateConfig {
    image: String,
    previous_image: Option<String>,
    auto_apply: bool,
    auto_apply_time: String,
}

impl RuntimeUpdateConfig {
    fn from_config(config: &Config) -> Self {
        Self {
            image: config.update_image.clone(),
            previous_image: config.update_previous_image.clone(),
            auto_apply: config.update_auto_apply,
            auto_apply_time: config.update_auto_apply_time.clone(),
        }
    }

    fn response(&self) -> UpdateConfigResponse {
        UpdateConfigResponse {
            image: self.image.clone(),
            previous_image: self.previous_image.clone(),
            auto_apply: self.auto_apply,
            auto_apply_time: self.auto_apply_time.clone(),
        }
    }
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
    /// 代理 IP 池管理器
    proxy_pool: ProxyPoolManager,
    /// 在线镜像更新运行时配置
    update_config: Mutex<RuntimeUpdateConfig>,
    /// 最近一次"检查更新"结果（带 TTL，用于减少 GitHub API 调用）
    update_check_cache: Mutex<Option<CachedUpdateCheck>>,
    /// 进行中的 IdC 设备授权会话
    idc_sessions: Arc<Mutex<HashMap<String, IdcAuthSession>>>,
    /// 进行中的 Social 登录会话
    social_sessions: Arc<Mutex<HashMap<String, SocialAuthSession>>>,
}

/// Social 登录会话状态
struct SocialAuthSession {
    auth_endpoint: String,
    /// 发起时生成的 state，用于 CSRF 验证
    state: String,
    code_verifier: String,
    redirect_uri: String,
    expires_at: DateTime<Utc>,
    /// 收到 OAuth 回调时的数据（code + login_option + path）
    callback_rx: tokio::sync::Mutex<tokio::sync::oneshot::Receiver<social::OAuthCallbackData>>,
    cred_template: KiroCredentials,
    proxy: Option<ProxyConfig>,
    /// Drop 时自动关闭回调服务器并释放端口
    _server_handle: social::ServerHandle,
    /// 重新登录时更新此凭据的 Token（非 None 时更新已有凭据而非创建新凭据）
    relogin_target_id: Option<u64>,
}

/// IdC 设备授权会话状态
struct IdcAuthSession {
    region: String,
    client_id: String,
    client_secret: String,
    device_code: String,
    expires_at: DateTime<Utc>,
    poll_interval: i64,
    /// 登录成功后写入的凭据配置
    cred_template: KiroCredentials,
    /// 用于发起 token 请求的代理
    proxy: Option<ProxyConfig>,
    /// 重新登录时更新此凭据的 Token（非 None 时更新已有凭据而非创建新凭据）
    relogin_target_id: Option<u64>,
}

/// 解析自动更新触发时间（`HH:MM`，本地 24 小时制）。允许 `H:M` 简写，
/// 例如 `3:0`；解析失败时返回原字符串，便于错误信息提示。
fn parse_auto_apply_time(value: &str) -> Result<(u32, u32), AdminServiceError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AdminServiceError::InvalidCredential(
            "自动更新时间不能为空".to_string(),
        ));
    }
    let mut parts = trimmed.splitn(2, ':');
    let hour_str = parts.next().unwrap_or("");
    let minute_str = parts.next().unwrap_or("");
    let hour: u32 = hour_str.parse().map_err(|_| {
        AdminServiceError::InvalidCredential(format!(
            "自动更新时间格式无效：{}（应为 HH:MM）",
            value
        ))
    })?;
    let minute: u32 = minute_str.parse().map_err(|_| {
        AdminServiceError::InvalidCredential(format!(
            "自动更新时间格式无效：{}（应为 HH:MM）",
            value
        ))
    })?;
    if hour > 23 || minute > 59 {
        return Err(AdminServiceError::InvalidCredential(format!(
            "自动更新时间超出范围：{}（HH 0-23，MM 0-59）",
            value
        )));
    }
    Ok((hour, minute))
}

/// 把 HH:MM 规范化成 `HH:MM`（两位补零），方便存储和比较。
fn normalize_auto_apply_time(value: &str) -> Result<String, AdminServiceError> {
    let (h, m) = parse_auto_apply_time(value)?;
    Ok(format!("{:02}:{:02}", h, m))
}

fn validate_image_ref(image: &str) -> Result<(), AdminServiceError> {
    let value = image.trim();
    if value.is_empty() {
        return Err(AdminServiceError::InvalidCredential(
            "镜像地址不能为空".to_string(),
        ));
    }
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(AdminServiceError::InvalidCredential(
            "镜像地址不能包含空白或控制字符".to_string(),
        ));
    }
    // 仅允许 Docker Hub 镜像：可以省略 host（如 `zyphrzero/kiro-rs:latest`），
    // 也可以显式写 `docker.io/...` 或 `registry-1.docker.io/...`。
    let allowed_prefixes = ["docker.io/", "registry-1.docker.io/"];
    let host_segment = value.split('/').next().unwrap_or("");
    let looks_like_dockerhub = !host_segment.contains('.') && !host_segment.contains(':');
    if !allowed_prefixes.iter().any(|p| value.starts_with(p)) && !looks_like_dockerhub {
        return Err(AdminServiceError::InvalidCredential(
            "在线更新只支持 Docker Hub 镜像（如 owner/image:tag 或 docker.io/owner/image:tag）"
                .to_string(),
        ));
    }
    let path_parts: Vec<&str> = value
        .trim_start_matches("docker.io/")
        .trim_start_matches("registry-1.docker.io/")
        .split('/')
        .collect();
    if path_parts.len() < 2 || path_parts.iter().any(|part| part.is_empty()) {
        return Err(AdminServiceError::InvalidCredential(
            "Docker Hub 镜像需为 owner/image[:tag] 格式".to_string(),
        ));
    }
    Ok(())
}

/// Docker Hub Hub API（`hub.docker.com/v2/repositories/<owner>/<repo>/tags`）
/// 返回 JSON 中我们关心的字段。仅依赖该接口的稳定字段，新增字段不影响解析。
#[derive(Debug, Deserialize)]
struct DockerHubTagsResponse {
    #[serde(default)]
    results: Vec<DockerHubTag>,
}

#[derive(Debug, Deserialize)]
struct DockerHubTag {
    #[serde(default)]
    name: String,
    #[serde(default)]
    last_updated: String,
}

/// GitHub `repos/{owner}/{repo}/releases/tags/{tag}` 返回 JSON 中我们关心
/// 的字段，用于在「检查更新」结果里附带本次发布的 changelog。
#[derive(Debug, Deserialize)]
struct GitHubRelease {
    #[serde(default)]
    name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    published_at: String,
}

/// 把镜像引用拆成 `(owner, repo)`；忽略可选的 `:tag` 与 `docker.io/` 前缀。
///
/// 例如：
/// - `zyphrzero/kiro-rs:latest` → `("zyphrzero", "kiro-rs")`
/// - `docker.io/library/redis` → `("library", "redis")`
fn dockerhub_owner_repo(image: &str) -> Option<(String, String)> {
    let trimmed = image
        .trim()
        .trim_start_matches("docker.io/")
        .trim_start_matches("registry-1.docker.io/")
        .split('@') // 去除 @sha256:... 摘要
        .next()?
        .split(':') // 去除 :tag
        .next()?;
    let mut parts = trimmed.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((owner, repo))
}

/// 比较两个 semver 字符串。仅按 `MAJOR.MINOR.PATCH` 三段数字比较，忽略
/// 预发布后缀；解析失败的段当作 0 处理（最坏情况下"无更新"）。
fn compare_semver(current: &str, latest: &str) -> std::cmp::Ordering {
    parse_semver_core(current).cmp(&parse_semver_core(latest))
}

/// 解析 semver 三段数字，解析失败的段作 0；用于 latest tag 的稳定排序。
fn parse_semver_core(value: &str) -> [u32; 3] {
    let core = value
        .trim_start_matches('v')
        .split(|c: char| c == '-' || c == '+')
        .next()
        .unwrap_or("");
    let mut out = [0u32; 3];
    for (i, part) in core.splitn(3, '.').enumerate() {
        if i >= 3 {
            break;
        }
        out[i] = part.parse::<u32>().unwrap_or(0);
    }
    out
}

/// 判断字符串是否是合法的 semver-like tag（必须以数字 `MAJOR.MINOR.PATCH` 开头）。
/// 这样 `latest` / `rolling` / `dev` 等非版本 tag 会被自动排除。
fn is_semver_tag(value: &str) -> bool {
    let core = value.trim().trim_start_matches('v');
    let mut parts = core.split(|c: char| c == '-' || c == '+').next().unwrap_or("").split('.');
    let first = parts.next().unwrap_or("");
    !first.is_empty() && first.chars().all(|c| c.is_ascii_digit())
}

/// 当前构建类型。docker 镜像里通过 `apply_image_update` 调 compose 重建，
/// 因此固定为 "docker-compose"；非容器场景的二进制升级路径暂未实现。
const BUILD_TYPE: &str = "docker-compose";

/// GitHub Release 仓库名（owner/repo）。
/// 镜像版本号从 Docker Hub 取，但 release notes / changelog 由 GitHub Release
/// 维护，需要单独拉取。
const GITHUB_RELEASES_REPO: &str = "ZyphrZero/kiro.rs";

/// 容器名 / hostname：在线更新执行流程会用容器自身的 compose 标签来发现 compose 文件位置和 service 名。
const SELF_CONTAINER_HOSTNAME_ENV: &str = "HOSTNAME";

/// 默认的容器内 compose 文件挂载位置，由仓库 docker-compose.yml 直接挂载得到。
const DEFAULT_IN_CONTAINER_COMPOSE_PATH: &str = "/app/config/docker-compose.yml";

/// 用于回退的本地镜像 tag。每次成功更新前，都会把当前运行镜像打到这个 tag 上，
/// 这样即使远端 latest 已被覆盖，本地仍保留一份可用的旧镜像。
const ROLLBACK_IMAGE_TAG: &str = "kiro-rs:rollback";

/// 通过 `docker inspect <hostname> --format <fmt>` 读取当前容器的某项元数据。
fn inspect_self_container(format: &str) -> Result<String, AdminServiceError> {
    let hostname = std::env::var(SELF_CONTAINER_HOSTNAME_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            AdminServiceError::InternalError(
                "无法获取当前容器 hostname（HOSTNAME 环境变量为空）".to_string(),
            )
        })?;

    let raw = run_command("docker", &["inspect", "--format", format, &hostname]).map_err(|e| {
        AdminServiceError::InternalError(format!(
            "通过 docker inspect 读取容器信息失败：{}。请确认容器内已安装 docker CLI 并挂载 /var/run/docker.sock。",
            e
        ))
    })?;

    Ok(raw.lines().next().unwrap_or("").trim().to_string())
}

/// 通过 docker CLI 读取当前容器的 compose 元数据（compose 文件路径 + service 名）。
///
/// 容器需要满足两个前置条件，才能在容器内自动完成在线更新：
/// 1. 已挂载 `/var/run/docker.sock`，使 `docker` 命令能访问宿主机 daemon。
/// 2. 容器是由 `docker compose` 启动的（compose 会自动在容器上写入
///    `com.docker.compose.project.config_files` 与 `com.docker.compose.service` 标签）。
///
/// 返回的 compose 文件路径来源是宿主机视角；调用方需要再用
/// [`resolve_in_container_compose_path`] 将其映射为容器内能读到的路径。
fn detect_compose_metadata() -> Result<(String, String), AdminServiceError> {
    let raw = inspect_self_container(
        "{{index .Config.Labels \"com.docker.compose.project.config_files\"}}|{{index .Config.Labels \"com.docker.compose.service\"}}",
    )?;

    let mut parts = raw.splitn(2, '|');
    let compose_file = parts.next().unwrap_or("").trim().to_string();
    let service = parts.next().unwrap_or("").trim().to_string();

    if service.is_empty() {
        return Err(AdminServiceError::InternalError(
            "当前容器缺少 docker compose service 标签，无法自动识别。请使用 `docker compose -f docker-compose.yml up -d` 启动容器后再尝试在线更新。".to_string(),
        ));
    }

    Ok((compose_file, service))
}

/// 读取当前容器正在运行的镜像引用与镜像 ID。
///
/// 返回 (`image_ref`, `image_id`)。`image_ref` 是 `.Config.Image`，可能是 tag
/// 或 sha256；`image_id` 是 `.Image`，固定为 sha256 摘要，备份打 tag 时用它。
fn detect_running_image() -> Result<(String, String), AdminServiceError> {
    let image_ref = inspect_self_container("{{.Config.Image}}")?;
    let image_id = inspect_self_container("{{.Image}}")?;
    if image_id.is_empty() {
        return Err(AdminServiceError::InternalError(
            "无法读取当前容器的镜像 ID".to_string(),
        ));
    }
    Ok((image_ref, image_id))
}

/// 给镜像 ID 打 `kiro-rs:rollback` tag，作为回退镜像。
fn tag_rollback_image(image_id: &str) -> Result<String, AdminServiceError> {
    run_command("docker", &["tag", image_id, ROLLBACK_IMAGE_TAG])
}

/// 检查本地是否还存在 `kiro-rs:rollback` 镜像。
fn rollback_image_present() -> bool {
    run_command("docker", &["image", "inspect", ROLLBACK_IMAGE_TAG]).is_ok()
}

/// 一次更新流程中重复使用的 compose 上下文：宿主机 project dir、容器内 yml 路径、
/// 服务名。聚合到一起避免每次都重新 detect。
struct ComposeContext {
    host_project_dir: String,
    in_container_compose: String,
    service: String,
}

impl ComposeContext {
    fn detect() -> Result<Self, AdminServiceError> {
        let (host_compose_file, service) = detect_compose_metadata()?;
        let in_container_compose = resolve_in_container_compose_path(&host_compose_file)?;
        ensure_compose_file_exists(&in_container_compose)?;
        let host_project_dir = std::path::Path::new(&host_compose_file)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| ".".to_string());
        Ok(Self {
            host_project_dir,
            in_container_compose,
            service,
        })
    }

    fn compose_up(&self, image: &str) -> Result<String, AdminServiceError> {
        let env = [("KIRO_RS_IMAGE", image)];
        run_command_with_env(
            "docker",
            &[
                "compose",
                "--project-directory",
                self.host_project_dir.as_str(),
                "-f",
                self.in_container_compose.as_str(),
                "up",
                "-d",
                self.service.as_str(),
            ],
            &env,
        )
    }
}

/// 把 docker compose 标签里的宿主机路径映射成容器内可读的路径。
///
/// 容器内运行 `docker compose -f <path>` 时，CLI 会在本地（容器内）解析 yml，
/// 因此必须挂载好 yml 文件。仓库默认 docker-compose.yml 已经挂载到
/// `/app/config/docker-compose.yml`，这里以该路径作为兜底。
fn resolve_in_container_compose_path(host_path: &str) -> Result<String, AdminServiceError> {
    let host_trim = host_path.trim();
    if !host_trim.is_empty() && std::path::Path::new(host_trim).is_file() {
        return Ok(host_trim.to_string());
    }
    if std::path::Path::new(DEFAULT_IN_CONTAINER_COMPOSE_PATH).is_file() {
        return Ok(DEFAULT_IN_CONTAINER_COMPOSE_PATH.to_string());
    }
    Err(AdminServiceError::InternalError(format!(
        "compose 文件在容器内不可读。docker compose 标签指向宿主机路径 {host_trim:?}，且默认挂载点 {DEFAULT_IN_CONTAINER_COMPOSE_PATH} 也不存在。请在 docker-compose.yml 中保留 `./docker-compose.yml:{DEFAULT_IN_CONTAINER_COMPOSE_PATH}:ro` 这条挂载，并确认宿主机当前目录下有 docker-compose.yml 文件。"
    )))
}

/// 在执行 compose 前，确保 compose 文件实际存在且是普通文件。
///
/// 这里专门处理 Docker bind mount 的常见陷阱：当宿主机源文件不存在时，
/// Docker 会把目标路径自动创建成空目录，导致 `docker compose -f <path>` 报
/// `read <path>: is a directory`。提前给出可操作的提示比让命令失败更友好。
fn ensure_compose_file_exists(path: &str) -> Result<(), AdminServiceError> {
    let metadata = std::fs::metadata(path).map_err(|e| {
        AdminServiceError::InternalError(format!(
            "compose 文件 {} 不可访问: {}。请确认宿主机上该文件存在并已挂载到容器内的同一路径。",
            path, e
        ))
    })?;
    if metadata.is_dir() {
        return Err(AdminServiceError::InternalError(format!(
            "compose 文件 {} 实际是一个目录。常见原因是 docker-compose.yml 在宿主机上不存在，被 Docker 自动创建为空目录。请在宿主机上提供真实的 docker-compose.yml 后重新启动容器。",
            path
        )));
    }
    if !metadata.is_file() {
        return Err(AdminServiceError::InternalError(format!(
            "compose 文件 {} 不是普通文件，无法用于 docker compose -f",
            path
        )));
    }
    Ok(())
}

fn command_output_text(stdout: &[u8], stderr: &[u8]) -> String {
    let mut out = String::new();
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    if !stdout.trim().is_empty() {
        out.push_str(stdout.trim_end());
        out.push('\n');
    }
    if !stderr.trim().is_empty() {
        out.push_str(stderr.trim_end());
        out.push('\n');
    }
    out
}

fn run_command_with_env(
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<String, AdminServiceError> {
    let mut command = Command::new(program);
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }

    let output = command
        .output()
        .map_err(|e| AdminServiceError::InternalError(format!("执行 {} 失败: {}", program, e)))?;
    let text = command_output_text(&output.stdout, &output.stderr);
    if !output.status.success() {
        return Err(AdminServiceError::InternalError(format!(
            "命令 {} {} 执行失败（{}）: {}",
            program,
            args.join(" "),
            output.status,
            text.trim()
        )));
    }
    Ok(text)
}

fn run_command(program: &str, args: &[&str]) -> Result<String, AdminServiceError> {
    run_command_with_env(program, args, &[])
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
    ) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));

        let proxy_pool_path = token_manager.cache_dir().map(|d| d.join("proxy_pool.json"));

        let balance_cache = Self::load_balance_cache_from(&cache_path);
        let update_config = RuntimeUpdateConfig::from_config(token_manager.config());

        let svc = Self {
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            proxy_pool: ProxyPoolManager::new(proxy_pool_path),
            update_config: Mutex::new(update_config),
            update_check_cache: Mutex::new(None),
            idc_sessions: Arc::new(Mutex::new(HashMap::new())),
            social_sessions: Arc::new(Mutex::new(HashMap::new())),
        };

        // 后台任务：每 5 分钟清理过期的登录会话，防止内存泄漏
        {
            let idc = Arc::clone(&svc.idc_sessions);
            let social = Arc::clone(&svc.social_sessions);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
                loop {
                    interval.tick().await;
                    let now = Utc::now();
                    idc.lock().retain(|_, s| now < s.expires_at);
                    social.lock().retain(|_, s| now < s.expires_at);
                }
            });
        }

        svc
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let default_endpoint = self.token_manager.config().default_endpoint.clone();

        // 一次性快照余额缓存，避免 N 次加锁
        let balance_snapshot: HashMap<u64, CachedBalance> = {
            let cache = self.balance_cache.lock();
            cache.clone()
        };
        let now_ts = Utc::now().timestamp() as f64;

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| {
                let (balance, balance_updated_at) = balance_snapshot
                    .get(&entry.id)
                    .filter(|c| (now_ts - c.cached_at) < BALANCE_CACHE_TTL_SECS as f64)
                    .map(|c| (Some(c.data.clone()), Some(c.cached_at)))
                    .unwrap_or((None, None));

                CredentialStatusItem {
                    id: entry.id,
                    priority: entry.priority,
                    disabled: entry.disabled,
                    failure_count: entry.failure_count,
                    is_current: entry.id == snapshot.current_id,
                    expires_at: entry.expires_at,
                    auth_method: entry.auth_method,
                    has_profile_arn: entry.has_profile_arn,
                    refresh_token_hash: entry.refresh_token_hash,
                    api_key_hash: entry.api_key_hash,
                    masked_api_key: entry.masked_api_key,
                    email: entry.email,
                    success_count: entry.success_count,
                    last_used_at: entry.last_used_at.clone(),
                    has_proxy: entry.has_proxy,
                    proxy_url: entry.proxy_url,
                    refresh_failure_count: entry.refresh_failure_count,
                    disabled_reason: entry.disabled_reason,
                    endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
                    balance,
                    balance_updated_at,
                }
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials,
        }
    }

    /// 一键禁用所有"已超额"的凭据（remaining ≤ 0 或 usage_percentage ≥ 100）
    ///
    /// 数据来源是 `balance_cache`，所以前端在调用前最好先触发一次"查询信息"
    /// 或等待后台调度器完成首次刷新。返回 (禁用数量, 跳过数量, 已超额未禁用名单)。
    pub fn disable_quota_exceeded(&self) -> QuotaExceededResult {
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        let cache_snapshot: HashMap<u64, CachedBalance> = {
            let cache = self.balance_cache.lock();
            cache.clone()
        };
        let now_ts = Utc::now().timestamp() as f64;

        let mut disabled_ids: Vec<u64> = Vec::new();
        let mut skipped_ids: Vec<u64> = Vec::new();
        let mut switched_current = false;

        for entry in snapshot.entries.iter() {
            if entry.disabled {
                continue;
            }
            let cached = match cache_snapshot.get(&entry.id) {
                Some(c) if (now_ts - c.cached_at) < BALANCE_CACHE_TTL_SECS as f64 => c,
                _ => continue,
            };
            let exceeded = cached.data.remaining <= 0.0 || cached.data.usage_percentage >= 100.0;
            if !exceeded {
                continue;
            }
            match self.token_manager.disable_quota_exceeded(entry.id) {
                Ok(()) => {
                    disabled_ids.push(entry.id);
                    if entry.id == current_id {
                        switched_current = true;
                    }
                }
                Err(e) => {
                    tracing::warn!("一键超额：禁用凭据 #{} 失败: {}", entry.id, e);
                    skipped_ids.push(entry.id);
                }
            }
        }

        if switched_current {
            let _ = self.token_manager.switch_to_next();
        }

        QuotaExceededResult {
            disabled_ids,
            skipped_ids,
        }
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    pub fn reset_success_count(&self, id: Option<u64>) -> Result<u32, AdminServiceError> {
        self.token_manager
            .reset_success_count(id)
            .map_err(|e| self.classify_error(e, id.unwrap_or(0)))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        // 允许 remaining 显示为负值：开启超额后实际使用可能超过限额，
        // 直接保留差值便于在 UI 中体现"已欠多少"。
        let remaining = usage_limit - current_usage;
        // usage_percentage 同理保留真实值，超额时 > 100%。
        let usage_percentage = if usage_limit > 0.0 {
            current_usage / usage_limit * 100.0
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
            overage_enabled: usage.overage_enabled(),
            overage_capable: usage.overage_capable(),
            overage_capability_raw: usage
                .subscription_info
                .as_ref()
                .and_then(|s| s.overage_capability.clone()),
        })
    }

    /// 批量刷新所有非禁用凭据的余额（用于后台调度）
    ///
    /// 串行执行以避免对上游产生瞬时高并发，每次成功的查询都会更新内存缓存
    /// 与磁盘缓存。失败的条目不会清空旧缓存，调用方可在下次轮询时重试。
    pub async fn refresh_all_balances(&self) -> (usize, usize) {
        let snapshot = self.token_manager.snapshot();
        let mut success = 0_usize;
        let mut failure = 0_usize;

        for entry in snapshot.entries.into_iter() {
            if entry.disabled {
                continue;
            }
            match self.fetch_balance(entry.id).await {
                Ok(balance) => {
                    {
                        let mut cache = self.balance_cache.lock();
                        cache.insert(
                            entry.id,
                            CachedBalance {
                                cached_at: Utc::now().timestamp() as f64,
                                data: balance,
                            },
                        );
                    }
                    success += 1;
                }
                Err(e) => {
                    tracing::warn!("后台刷新凭据 #{} 余额失败: {}", entry.id, e);
                    failure += 1;
                }
            }
            // 节流，避免上游限流
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }

        if success > 0 {
            self.save_balance_cache();
        }
        (success, failure)
    }

    /// 启动余额后台刷新调度器
    ///
    /// - 启动后立刻执行一次刷新
    /// - 之后按 `interval` 周期循环刷新
    /// - 调用方持有 `Arc<Self>` 即可，任务在后台 tokio runtime 上运行
    pub fn start_balance_refresher(self: &Arc<Self>, interval: std::time::Duration) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            // 启动后稍等片刻，让上游/Token Manager 准备就绪
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            loop {
                let started = std::time::Instant::now();
                let (ok, err) = svc.refresh_all_balances().await;
                tracing::info!(
                    "余额后台刷新完成：成功 {}，失败 {}，耗时 {:.1}s",
                    ok,
                    err,
                    started.elapsed().as_secs_f32()
                );
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// 启动无人值守自动更新调度器。
    ///
    /// 任务始终运行，每分钟唤醒一次：
    /// - `update_auto_apply` 关闭时只是记录"未到点"，不做任何远端调用。
    /// - 开启时，比较当前本地时间与 `update_auto_apply_time`，命中目标分钟
    ///   就触发一次 `apply_image_update`。同一目标版本只会被自动应用一次。
    pub fn start_auto_update_scheduler(self: &Arc<Self>) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            // 给 Docker socket / compose 元数据探测留点准备时间
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;

            // 同一分钟避免重复触发；记录最近一次应用过的"日期 + 版本"
            let mut last_run_marker: Option<String> = None;
            let mut last_applied_version: Option<String> = None;

            loop {
                let runtime = svc.update_config.lock().clone();
                if runtime.auto_apply {
                    let target = parse_auto_apply_time(&runtime.auto_apply_time).ok();
                    if let Some((target_hour, target_minute)) = target {
                        let now = chrono::Local::now();
                        let date_minute_marker = format!(
                            "{}-{:02}:{:02}",
                            now.format("%Y-%m-%d"),
                            now.hour(),
                            now.minute()
                        );

                        let hit = now.hour() == target_hour && now.minute() == target_minute;
                        let already_ran_this_minute = last_run_marker.as_deref()
                            == Some(date_minute_marker.as_str());

                        if hit && !already_ran_this_minute {
                            last_run_marker = Some(date_minute_marker);
                            let info = svc.check_update(true).await;
                            if info.has_update
                                && !info.latest_version.is_empty()
                                && last_applied_version.as_deref()
                                    != Some(info.latest_version.as_str())
                            {
                                tracing::info!(
                                    "自动更新：到达计划时间 {}，发现新版本 {}（当前 {}），开始应用",
                                    runtime.auto_apply_time,
                                    info.latest_version,
                                    info.current_version
                                );
                                match svc.apply_image_update() {
                                    Ok(res) => {
                                        tracing::info!("自动更新完成：{}", res.message);
                                        last_applied_version = Some(info.latest_version);
                                    }
                                    Err(e) => {
                                        tracing::warn!("自动更新失败：{}", e);
                                    }
                                }
                            } else {
                                tracing::info!(
                                    "自动更新：到达计划时间 {}，但当前已是最新版本（{}）",
                                    runtime.auto_apply_time,
                                    info.current_version
                                );
                            }
                        }
                    } else {
                        tracing::warn!(
                            "自动更新时间配置无效：{}，跳过本轮检查",
                            runtime.auto_apply_time
                        );
                    }
                }

                // 30 秒粒度足以可靠命中目标分钟，又不会在系统时间漂移下错过
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 校验端点名：未指定则默认合法，指定则必须已注册
        if let Some(ref name) = req.endpoint {
            if !self.known_endpoints.contains(name) {
                let mut known: Vec<&str> =
                    self.known_endpoints.iter().map(|s| s.as_str()).collect();
                known.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知端点 \"{}\"，已注册端点: {:?}",
                    name, known
                )));
            }
        }

        // 构建凭据对象
        let email = req.email.clone();
        let new_cred = KiroCredentials {
            id: None,
            access_token: req.access_token,
            refresh_token: req.refresh_token,
            profile_arn: req.profile_arn,
            expires_at: req.expires_at,
            auth_method: Some(req.auth_method),
            provider: req.provider,
            client_id: req.client_id,
            client_secret: req.client_secret,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 更新凭据的可编辑字段（email、proxy 等）
    pub fn update_credential(
        &self,
        id: u64,
        req: UpdateCredentialRequest,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .update_credential(
                id,
                req.email.map(|v| if v.is_empty() { None } else { Some(v) }),
                req.proxy_url
                    .map(|v| if v.is_empty() { None } else { Some(v) }),
                req.proxy_username
                    .map(|v| if v.is_empty() { None } else { Some(v) }),
                req.proxy_password
                    .map(|v| if v.is_empty() { None } else { Some(v) }),
            )
            .map_err(|e| self.classify_error(e, id))
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 从磁盘加载最新配置并应用更新，再写回磁盘。
    ///
    /// 每次读最新文件再写，避免多次调用之间字段互相覆盖。
    fn update_config_file(&self, updater: impl FnOnce(&mut Config)) {
        let base = self.token_manager.config();
        let Some(path) = base.config_path() else {
            return;
        };
        match Config::load(path) {
            Ok(mut fresh) => {
                updater(&mut fresh);
                if let Err(e) = fresh.save() {
                    tracing::warn!("保存配置文件失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("读取配置文件失败（跳过持久化）: {}", e),
        }
    }

    /// 获取全局代理 URL
    pub fn get_global_proxy(&self) -> Option<String> {
        self.token_manager.proxy().map(|p| p.url.clone())
    }

    /// 设置全局代理 URL（None 表示清除）并持久化到配置文件
    pub fn set_global_proxy(&self, url: Option<String>) -> Result<(), AdminServiceError> {
        if let Some(ref u) = url {
            let valid_prefix = u.starts_with("http://")
                || u.starts_with("https://")
                || u.starts_with("socks5://")
                || u.starts_with("socks4://");
            if !valid_prefix {
                return Err(AdminServiceError::InvalidCredential(
                    "代理 URL 格式无效，需以 http://、https://、socks5:// 或 socks4:// 开头"
                        .to_string(),
                ));
            }
        }

        let proxy = url.as_deref().map(ProxyConfig::new);
        self.token_manager.set_global_proxy(proxy);

        // 从磁盘加载最新 config 再写，避免覆盖其他字段的并发修改
        let url_for_save = url;
        self.update_config_file(move |c| c.proxy_url = url_for_save);
        Ok(())
    }

    /// 持久化新的 Admin API Key 到配置文件（内存中的 key 由 handler 层负责更新）
    pub fn persist_admin_key(&self, new_key: &str) {
        let key = new_key.to_string();
        self.update_config_file(move |c| c.admin_api_key = Some(key));
    }

    /// 持久化新的业务 API Key 到配置文件
    pub fn persist_api_key(&self, new_key: &str) {
        let key = new_key.to_string();
        self.update_config_file(move |c| c.api_key = Some(key));
    }

    /// 获取在线更新配置（GitHub Token 只返回是否已配置）
    pub fn get_update_config(&self) -> UpdateConfigResponse {
        self.update_config.lock().response()
    }

    /// 更新在线更新配置。
    pub fn set_update_config(
        &self,
        req: SetUpdateConfigRequest,
    ) -> Result<UpdateConfigResponse, AdminServiceError> {
        if let Some(image) = &req.image {
            let trimmed = image.trim();
            if !trimmed.is_empty() {
                validate_image_ref(trimmed)?;
            }
        }

        // 在写入运行时之前先校验时间格式，并规范化成两位补零的 HH:MM
        let normalized_time = match req.auto_apply_time.as_deref() {
            Some(value) => Some(normalize_auto_apply_time(value)?),
            None => None,
        };

        {
            let mut runtime = self.update_config.lock();
            if let Some(image) = &req.image {
                runtime.image = image.trim().to_string();
            }
            if let Some(auto_apply) = req.auto_apply {
                runtime.auto_apply = auto_apply;
            }
            if let Some(time) = &normalized_time {
                runtime.auto_apply_time = time.clone();
            }
        }

        self.update_config_file(move |c| {
            if let Some(image) = req.image {
                c.update_image = image.trim().to_string();
            }
            if let Some(auto_apply) = req.auto_apply {
                c.update_auto_apply = auto_apply;
            }
            if let Some(time) = normalized_time {
                c.update_auto_apply_time = time;
            }
        });

        Ok(self.get_update_config())
    }

    /// 拉取配置中的镜像（公开镜像，直接 docker pull，无需登录）。
    pub fn pull_update_image(&self) -> Result<ImageUpdateResponse, AdminServiceError> {
        let config = self.update_config.lock().clone();
        let image = config.image.trim().to_string();
        validate_image_ref(&image)?;

        let output = run_command("docker", &["pull", &image])?;

        Ok(ImageUpdateResponse {
            success: true,
            message: "镜像拉取完成".to_string(),
            image,
            output: Some(output),
            applied: false,
            need_restart: false,
        })
    }

    /// 拉取镜像并通过 docker compose 重建服务。
    ///
    /// 容器内调用时需要满足：
    /// 1. 已挂载 `/var/run/docker.sock`（默认 docker-compose.yml 已挂载）。
    /// 2. 容器是由 `docker compose` 启动的（compose 自带的标签会写入容器）。
    /// 3. 容器在宿主机上的 compose 文件仍然存在于 compose 标签所记录的位置。
    pub fn apply_image_update(&self) -> Result<ImageUpdateResponse, AdminServiceError> {
        let image = self.update_config.lock().image.trim().to_string();
        validate_image_ref(&image)?;

        let ctx = ComposeContext::detect()?;

        let mut output = String::new();

        // 在 pull 新镜像之前，把当前正在运行的镜像打到 `kiro-rs:rollback` 这个
        // 本地备份 tag 上。即使后续远端 latest 被覆盖、本地原始 tag 被新镜像
        // 取代，备份镜像也仍然可用，可在断网情况下回退。
        let previous_image_ref = match detect_running_image() {
            Ok((image_ref, image_id)) => match tag_rollback_image(&image_id) {
                Ok(tag_out) => {
                    output.push_str(&tag_out);
                    Some(image_ref)
                }
                Err(e) => {
                    output.push_str(&format!("warning: 备份当前镜像失败: {}\n", e));
                    None
                }
            },
            Err(e) => {
                output.push_str(&format!("warning: 读取当前镜像信息失败: {}\n", e));
                None
            }
        };

        output.push_str(&run_command("docker", &["pull", image.as_str()])?);
        output.push_str(&ctx.compose_up(image.as_str())?);

        // 仅当成功应用了新镜像后再持久化 previous_image，避免回退指向并未真正
        // 切换到的版本。
        if let Some(prev) = previous_image_ref.clone() {
            self.update_config.lock().previous_image = Some(prev.clone());
            self.update_config_file(move |c| {
                c.update_previous_image = Some(prev);
            });
        }

        Ok(ImageUpdateResponse {
            success: true,
            message: "镜像已更新并应用，容器将由 Docker Compose 重建".to_string(),
            image,
            output: Some(output),
            applied: true,
            need_restart: true,
        })
    }

    /// 把容器回退到上一次更新前的镜像版本。
    ///
    /// 回退使用本地备份 tag `kiro-rs:rollback`，不会再访问镜像仓库，因此即使
    /// 上游 latest 已经被覆盖、网络中断，也能恢复到旧版本。
    pub fn rollback_image_update(&self) -> Result<ImageUpdateResponse, AdminServiceError> {
        let runtime = self.update_config.lock().clone();
        let previous_image = runtime
            .previous_image
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                AdminServiceError::InvalidCredential(
                    "尚未记录可回退的镜像版本，请先执行一次在线更新".to_string(),
                )
            })?
            .to_string();

        if !rollback_image_present() {
            return Err(AdminServiceError::InternalError(format!(
                "本地未找到备份镜像 {}，可能已被 docker image prune 清理。无法离线回退。",
                ROLLBACK_IMAGE_TAG
            )));
        }

        let ctx = ComposeContext::detect()?;

        let mut output = String::new();
        // 直接用本地备份 tag 跑 compose up，不再 pull。
        output.push_str(&ctx.compose_up(ROLLBACK_IMAGE_TAG)?);

        // 把当前镜像配置同步成回退后的版本，方便用户在 UI 上看清现状。
        self.update_config.lock().image = previous_image.clone();
        let to_persist = previous_image.clone();
        self.update_config_file(move |c| {
            c.update_image = to_persist;
        });

        Ok(ImageUpdateResponse {
            success: true,
            message: format!("已回退到镜像 {}", previous_image),
            image: previous_image,
            output: Some(output),
            applied: true,
            need_restart: true,
        })
    }

    /// 检查 GitHub Releases 上是否存在新版本。
    ///
    /// `force=false` 时优先返回 30 分钟内的缓存结果；`force=true` 时强制查询
    /// 远端。查询失败但有旧缓存时，返回旧缓存并附带 warning。
    pub async fn check_update(&self, force: bool) -> UpdateCheckInfo {
        if !force {
            if let Some(cached) = self.update_check_cache.lock().clone() {
                let age = Utc::now()
                    .signed_duration_since(cached.cached_at)
                    .num_seconds();
                if age < UPDATE_CHECK_TTL_SECS {
                    let mut info = cached.info.clone();
                    info.cached = true;
                    return info;
                }
            }
        }

        match self.fetch_latest_release().await {
            Ok(info) => {
                self.update_check_cache.lock().replace(CachedUpdateCheck {
                    cached_at: Utc::now(),
                    info: info.clone(),
                });
                info
            }
            Err(err) => {
                let warning = format!("检查更新失败：{}", err);
                if let Some(cached) = self.update_check_cache.lock().clone() {
                    let mut info = cached.info.clone();
                    info.cached = true;
                    info.warning = Some(warning);
                    return info;
                }
                UpdateCheckInfo {
                    current_version: env!("CARGO_PKG_VERSION").to_string(),
                    latest_version: String::new(),
                    has_update: false,
                    build_type: BUILD_TYPE.to_string(),
                    release_name: None,
                    release_notes: None,
                    release_url: None,
                    published_at: None,
                    checked_at: Utc::now().to_rfc3339(),
                    cached: false,
                    warning: Some(warning),
                }
            }
        }
    }

    async fn fetch_latest_release(&self) -> Result<UpdateCheckInfo, AdminServiceError> {
        let configured_image = self.update_config.lock().image.clone();
        // 用户清空 image 时降级到默认镜像，行为对齐 Config::load 的清洗逻辑
        let image = if configured_image.trim().is_empty() {
            "zyphrzero/kiro-rs:latest".to_string()
        } else {
            configured_image
        };
        let (owner, repo) = dockerhub_owner_repo(image.trim()).ok_or_else(|| {
            AdminServiceError::InternalError(format!(
                "无法从镜像 {} 解析出 Docker Hub owner/repo",
                image
            ))
        })?;

        // page_size=100 在我们目标仓库已足够覆盖所有版本 tag；增量发布也不会
        // 一次性产生上百个版本。`page=1` 即可拿到最新批次。
        let url = format!(
            "https://hub.docker.com/v2/repositories/{}/{}/tags?page_size=100&page=1",
            owner, repo
        );
        let resp = reqwest::Client::new()
            .get(&url)
            .header("Accept", "application/json")
            .header("User-Agent", "kiro-rs-update-checker")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| {
                AdminServiceError::InternalError(format!("请求 Docker Hub API 失败: {}", e))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            // Docker Hub 仓库不存在或私有时返回 404；这是配置错误而非服务故障，
            // 给用户更具体的提示，让他们知道该去看 updateImage 配置而不是日志。
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(AdminServiceError::InternalError(format!(
                    "Docker Hub 上未找到镜像 {}/{}（可能仓库还未发布或为私有）。请确认 updateImage 指向的仓库存在并已公开。",
                    owner, repo
                )));
            }
            return Err(AdminServiceError::InternalError(format!(
                "Docker Hub API 返回 {}: {}",
                status,
                body.chars().take(200).collect::<String>()
            )));
        }

        let payload: DockerHubTagsResponse = resp.json().await.map_err(|e| {
            AdminServiceError::InternalError(format!("解析 Docker Hub 响应失败: {}", e))
        })?;

        // 过滤出语义化版本 tag（排除 latest / rolling / dev 之类），按版本号选最大
        let latest_tag = payload
            .results
            .into_iter()
            .filter(|t| is_semver_tag(&t.name))
            .max_by(|a, b| parse_semver_core(&a.name).cmp(&parse_semver_core(&b.name)));

        let current = env!("CARGO_PKG_VERSION").to_string();
        let (latest_version, published_at) = match latest_tag {
            Some(tag) => (
                tag.name.trim().trim_start_matches('v').to_string(),
                Some(tag.last_updated).filter(|v| !v.is_empty()),
            ),
            None => (String::new(), None),
        };
        let has_update =
            !latest_version.is_empty() && compare_semver(&current, &latest_version).is_lt();

        let release_url = if !latest_version.is_empty() {
            Some(format!(
                "https://hub.docker.com/r/{}/{}/tags",
                owner, repo
            ))
        } else {
            None
        };

        // 拉取 GitHub Release 上的 changelog；失败不影响主流程，前端能拿到
        // 版本号就够展示红点了。published_at 优先用 GitHub Release 给出的，
        // 因为它是发布动作的时间，比 Docker Hub 推送时间更接近"用户视角的版本时间"。
        let mut release_name: Option<String> = None;
        let mut release_notes: Option<String> = None;
        let mut release_html_url: Option<String> = None;
        let mut release_published_at: Option<String> = None;
        if !latest_version.is_empty() {
            match Self::fetch_github_release(&latest_version).await {
                Ok(release) => {
                    release_name = Some(release.name).filter(|v| !v.is_empty());
                    release_notes = Some(release.body).filter(|v| !v.is_empty());
                    release_html_url = Some(release.html_url).filter(|v| !v.is_empty());
                    release_published_at = Some(release.published_at).filter(|v| !v.is_empty());
                }
                Err(e) => {
                    tracing::debug!("获取 GitHub Release 信息失败（不影响检查更新）：{}", e);
                }
            }
        }

        Ok(UpdateCheckInfo {
            current_version: current,
            latest_version,
            has_update,
            build_type: BUILD_TYPE.to_string(),
            release_name,
            release_notes,
            // 优先用 GitHub Release 页面 URL（带 changelog），回退到 Docker Hub tag 列表
            release_url: release_html_url.or(release_url),
            // GitHub published_at 更精确，未拿到就回退到 Docker Hub last_updated
            published_at: release_published_at.or(published_at),
            checked_at: Utc::now().to_rfc3339(),
            cached: false,
            warning: None,
        })
    }

    /// 拉 GitHub Releases 上指定版本的发布信息（用于展示 changelog）。
    /// 调用方负责处理失败 —— 这条不能阻塞主版本检查流程。
    async fn fetch_github_release(version: &str) -> Result<GitHubRelease, AdminServiceError> {
        let tag = format!("v{}", version.trim());
        let url = format!(
            "https://api.github.com/repos/{}/releases/tags/{}",
            GITHUB_RELEASES_REPO, tag
        );
        let resp = reqwest::Client::new()
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "kiro-rs-update-checker")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| AdminServiceError::InternalError(format!("请求 GitHub API 失败: {}", e)))?;

        if !resp.status().is_success() {
            return Err(AdminServiceError::InternalError(format!(
                "GitHub API 返回 {}",
                resp.status()
            )));
        }

        resp.json::<GitHubRelease>()
            .await
            .map_err(|e| AdminServiceError::InternalError(format!("解析 GitHub release 失败: {}", e)))
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 更新指定凭据的 refreshToken（仅限已禁用凭据）
    pub fn update_refresh_token(
        &self,
        id: u64,
        req: UpdateRefreshTokenRequest,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .update_refresh_token(id, req.refresh_token, req.access_token, req.expires_at)
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("不存在") {
                    AdminServiceError::NotFound { id }
                } else if msg.contains("只能为已禁用")
                    || msg.contains("refreshToken 重复")
                    || msg.contains("已被截断")
                    || msg.contains("refreshToken 为空")
                    || msg.contains("缺少 refreshToken")
                {
                    AdminServiceError::InvalidCredential(msg)
                } else {
                    AdminServiceError::InternalError(msg)
                }
            })
    }

    /// 一键开启所有"可开启超额且当前未开启"凭据的超额
    /// 数据来源是 balance_cache（5 分钟有效）；若缓存缺失或 capable 状态未知则乐观尝试，
    /// 由上游 setUserPreference 接口本身决定是否成功（不支持的订阅会返回 4xx 失败）。
    pub async fn enable_overage_for_all_capable(&self) -> EnableOverageAllResult {
        let snapshot = self.token_manager.snapshot();
        let cache_snapshot: HashMap<u64, CachedBalance> = {
            let cache = self.balance_cache.lock();
            cache.clone()
        };
        let now_ts = Utc::now().timestamp() as f64;

        // 选出需要操作的 ID 列表
        let mut targets: Vec<u64> = Vec::new();
        let mut skipped: Vec<u64> = Vec::new();
        for entry in snapshot.entries.iter() {
            if entry.disabled {
                skipped.push(entry.id);
                continue;
            }
            let cached = cache_snapshot.get(&entry.id).filter(|c| {
                (now_ts - c.cached_at) < BALANCE_CACHE_TTL_SECS as f64
            });

            match cached {
                // 缓存命中：明确不可开启，跳过
                Some(c) if c.data.overage_capable == Some(false) => {
                    skipped.push(entry.id);
                    continue;
                }
                // 缓存命中：明确已开启，跳过
                Some(c) if c.data.overage_enabled == Some(true) => {
                    skipped.push(entry.id);
                    continue;
                }
                // 其它（缓存缺失 / 状态未知 / 明确可开启未开启）— 乐观尝试
                _ => targets.push(entry.id),
            }
        }

        let mut enabled_ids: Vec<u64> = Vec::new();
        let mut failed_ids: Vec<u64> = Vec::new();
        let mut failure_messages: Vec<String> = Vec::new();

        for id in targets {
            match self.token_manager.set_user_preference_for(id, "ENABLED").await {
                Ok(()) => {
                    enabled_ids.push(id);
                    // 失效本地缓存
                    let mut cache = self.balance_cache.lock();
                    cache.remove(&id);
                }
                Err(e) => {
                    tracing::warn!("一键开启超额：凭据 #{} 失败: {}", id, e);
                    failed_ids.push(id);
                    failure_messages.push(e.to_string());
                }
            }
            // 节流
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }

        if !enabled_ids.is_empty() {
            self.save_balance_cache();
        }

        EnableOverageAllResult {
            enabled_ids,
            skipped_ids: skipped,
            failed_ids,
            failure_messages,
        }
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    /// 设置凭据的"超额"开关（ENABLED / DISABLED）
    /// 成功后会主动失效本地余额缓存，让下次列表刷新展示最新 overage 状态
    pub async fn set_overage(&self, id: u64, enabled: bool) -> Result<(), AdminServiceError> {
        let status = if enabled { "ENABLED" } else { "DISABLED" };
        self.token_manager
            .set_user_preference_for(id, status)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        // 让本地缓存的 overage 状态失效（下次刷新时重新拉）
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        // 异步触发一次新的余额查询（不阻塞响应）
        let svc_handle = self.token_manager.clone();
        tokio::spawn(async move {
            if let Err(e) = svc_handle.get_usage_limits_for(id).await {
                tracing::warn!("超额状态变更后预热余额失败 #{}: {}", id, e);
            }
        });

        Ok(())
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 丢弃超过 TTL 的条目
                if (now - v.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }

    // ============ 代理池管理 ============

    /// 获取代理池列表（含凭据引用计数）
    pub fn get_proxy_pool(&self) -> ProxyPoolResponse {
        let proxies = self.proxy_pool.list();
        let credentials = {
            let snapshot = self.token_manager.snapshot();
            snapshot.entries
        };

        let pool: Vec<ProxyPoolEntry> = proxies
            .into_iter()
            .map(|p| {
                let count = credentials
                    .iter()
                    .filter(|c| c.proxy_url.as_deref().map(|u| u == p.url).unwrap_or(false))
                    .count() as u32;
                ProxyPoolEntry {
                    id: p.id,
                    url: p.url,
                    label: p.label,
                    enabled: p.enabled,
                    credential_count: count,
                }
            })
            .collect();

        ProxyPoolResponse {
            total: pool.len(),
            proxies: pool,
        }
    }

    /// 添加代理到池中
    pub fn add_proxy(
        &self,
        url: String,
        label: Option<String>,
    ) -> Result<ProxyPoolEntry, AdminServiceError> {
        let entry = self
            .proxy_pool
            .add(url, label)
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))?;
        Ok(ProxyPoolEntry {
            id: entry.id,
            url: entry.url,
            label: entry.label,
            enabled: entry.enabled,
            credential_count: 0,
        })
    }

    /// 批量添加代理
    pub fn batch_add_proxies(
        &self,
        req: BatchAddProxyRequest,
    ) -> (Vec<ProxyPoolEntry>, Vec<String>) {
        let (added, errors) = self.proxy_pool.batch_add(req.urls);
        let result = added
            .into_iter()
            .map(|e| ProxyPoolEntry {
                id: e.id,
                url: e.url,
                label: e.label,
                enabled: e.enabled,
                credential_count: 0,
            })
            .collect();
        (result, errors)
    }

    /// 删除代理池中的代理
    pub fn delete_proxy(&self, id: u64) -> Result<(), AdminServiceError> {
        self.proxy_pool.delete(id).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("不存在") {
                AdminServiceError::NotFound { id }
            } else {
                AdminServiceError::InternalError(msg)
            }
        })
    }

    /// 设置代理启用/禁用状态
    pub fn set_proxy_enabled(&self, id: u64, enabled: bool) -> Result<(), AdminServiceError> {
        self.proxy_pool
            .set_enabled(id, enabled)
            .map_err(|_| AdminServiceError::NotFound { id })
    }

    /// 将代理池中的代理分配给指定凭据
    pub fn assign_proxy_to_credential(
        &self,
        credential_id: u64,
        req: AssignProxyRequest,
    ) -> Result<(), AdminServiceError> {
        let proxy_url = match req.proxy_id {
            Some(proxy_id) => {
                let url = match self.proxy_pool.get_url(proxy_id) {
                    GetUrlResult::Ok(url) => url,
                    GetUrlResult::NotFound => {
                        return Err(AdminServiceError::NotFound { id: proxy_id });
                    }
                    GetUrlResult::Disabled => {
                        return Err(AdminServiceError::InvalidCredential(format!(
                            "代理 #{} 已被禁用，请先启用后再分配",
                            proxy_id
                        )));
                    }
                };
                Some(url)
            }
            None => None, // 清除代理
        };

        self.token_manager
            .update_credential(
                credential_id,
                None,            // email 不修改
                Some(proxy_url), // 设置或清除 proxy_url（Some(None) = 清除，Some(Some(url)) = 设置）
                None,            // proxy_username 不修改
                None,            // proxy_password 不修改
            )
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("不存在") {
                    AdminServiceError::NotFound { id: credential_id }
                } else {
                    AdminServiceError::InternalError(msg)
                }
            })
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. API Key 凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API Key 凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 3. 上游明确指出凭据缺少或携带了错误的 Profile ARN，属于导入凭据不完整/无效。
        if msg.contains("Invalid profileArn") {
            return AdminServiceError::InvalidCredential(
                "凭据缺少或包含无效 profileArn，无法查询余额；请重新登录获取 profileArn，或导入包含 profileArn 的完整凭据"
                    .to_string(),
            );
        }

        // 3. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error = msg.contains("获取使用额度失败") ||
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误格式）
            msg.contains("error sending request") ||
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out") ||
            msg.contains("proxy") ||
            msg.contains("SOCKS") ||
            msg.contains("dns") ||
            msg.contains("DNS");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 4. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    // ── Social 登录（Portal PKCE OAuth）────────────────────────────────────────

    /// 发起 Social 登录，返回 portal URL 供用户在浏览器打开
    ///
    /// 模式选择：
    /// - `callback_base_url` 为 Some → 远程模式：redirect_uri 使用服务端公网地址，不启动本地端口
    /// - `callback_base_url` 为 None  → 本地模式：启动本地 TCP 回调服务器（浏览器与服务端须同机）
    pub async fn start_social_login(
        &self,
        req: StartSocialLoginRequest,
    ) -> Result<StartSocialLoginResponse, AdminServiceError> {
        let global_proxy = self.token_manager.proxy();
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let auth_endpoint = req
            .auth_endpoint
            .unwrap_or_else(|| social::KIRO_AUTH_ENDPOINT.to_string());

        let (code_verifier, code_challenge) = social::generate_pkce();
        let state = uuid::Uuid::new_v4().to_string();

        let (tx, rx) = tokio::sync::oneshot::channel::<social::OAuthCallbackData>();

        // 启动本地 TCP 回调服务器（本地模式）
        // 远程访问时用户须从浏览器地址栏复制回调 URL，通过 complete_social_login 接口手动完成
        let (port, server_handle) = social::start_callback_server(tx)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let redirect_uri = format!("http://127.0.0.1:{}", port);
        let portal_url = social::build_portal_url(&state, &code_challenge, &redirect_uri);

        let expires_at = Utc::now() + Duration::minutes(10);
        let session_id = uuid::Uuid::new_v4().to_string();

        let cred_template = KiroCredentials {
            auth_method: Some("social".to_string()),
            priority: req.priority,
            email: req.email,
            proxy_url: req.proxy_url,
            ..Default::default()
        };

        let session = SocialAuthSession {
            auth_endpoint,
            state,
            code_verifier,
            redirect_uri,
            expires_at,
            callback_rx: tokio::sync::Mutex::new(rx),
            cred_template,
            proxy,
            _server_handle: server_handle,
            relogin_target_id: None,
        };

        self.social_sessions
            .lock()
            .insert(session_id.clone(), session);

        Ok(StartSocialLoginResponse {
            session_id,
            portal_url,
            expires_at: expires_at.to_rfc3339(),
        })
    }

    /// 轮询一次 Social 登录状态
    pub async fn poll_social_login(
        &self,
        session_id: &str,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        use tokio::sync::oneshot::error::TryRecvError;

        // 一次加锁同时完成：过期检查 + 非阻塞回调接收，消除 TOCTOU
        enum PollOutcome {
            Expired,
            Closed,
            Pending,
            Received(social::OAuthCallbackData),
        }

        let outcome = {
            let sessions = self.social_sessions.lock();
            let Some(session) = sessions.get(session_id) else {
                return Err(AdminServiceError::NotFound { id: 0 });
            };

            if Utc::now() >= session.expires_at {
                PollOutcome::Expired
            } else {
                match session.callback_rx.try_lock() {
                    Ok(mut rx) => match rx.try_recv() {
                        Ok(data) => PollOutcome::Received(data),
                        Err(TryRecvError::Empty) => PollOutcome::Pending,
                        Err(TryRecvError::Closed) => PollOutcome::Closed,
                    },
                    Err(_) => PollOutcome::Pending,
                }
            }
        };

        match outcome {
            PollOutcome::Pending => return Ok(PollIdcLoginResponse::Pending),
            PollOutcome::Expired => {
                self.social_sessions.lock().remove(session_id);
                return Ok(PollIdcLoginResponse::Expired);
            }
            PollOutcome::Closed => {
                self.social_sessions.lock().remove(session_id);
                return Err(AdminServiceError::InternalError(
                    "Social 登录回调服务器已关闭，请重新发起登录".to_string(),
                ));
            }
            PollOutcome::Received(callback) => {
                self.do_complete_social_login(session_id, callback).await
            }
        }
    }

    /// 内部：完成 Social 登录的 token 兑换和凭据创建（供轮询回调和手动完成共用）
    ///
    /// 调用前须确认 session 存在且未过期。会在内部做 state CSRF 校验。
    async fn do_complete_social_login(
        &self,
        session_id: &str,
        callback: social::OAuthCallbackData,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        // 先做 CSRF 校验（不移除 session，校验失败时保持 session 可继续轮询）
        {
            let sessions = self.social_sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or(AdminServiceError::NotFound { id: 0 })?;
            if callback.state != s.state {
                tracing::warn!(
                    "Social 登录 state 不匹配（期望 {}, 收到 {}），已拒绝",
                    s.state,
                    callback.state
                );
                return Err(AdminServiceError::InternalError(
                    "OAuth state 不匹配，请重新发起登录".to_string(),
                ));
            }
        }

        // 移除 session（含 code_verifier 等敏感数据）
        let session = self
            .social_sessions
            .lock()
            .remove(session_id)
            .ok_or(AdminServiceError::NotFound { id: 0 })?;

        let config = self.token_manager.config();

        // 构建完整的 redirect_uri（与 IDE 行为一致）
        let full_redirect_uri = if callback.login_option.is_empty() {
            format!("{}{}", session.redirect_uri, callback.path)
        } else {
            format!(
                "{}{}?login_option={}",
                session.redirect_uri,
                callback.path,
                urlencoding::encode(&callback.login_option),
            )
        };

        let token = social::exchange_code_for_token(
            &session.auth_endpoint,
            &callback.code,
            &session.code_verifier,
            &full_redirect_uri,
            config,
            session.proxy.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        // 重新登录模式：更新已有凭据而非创建新凭据
        if let Some(target_id) = session.relogin_target_id {
            let refresh_token = token.refresh_token.ok_or_else(|| {
                AdminServiceError::InternalError(
                    "Social 登录未返回 refreshToken，无法更新凭据".to_string(),
                )
            })?;
            self.do_relogin_update(target_id, refresh_token)
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
            tracing::info!("Social 重新登录成功，凭据 #{} Token 已更新", target_id);
            return Ok(PollIdcLoginResponse::Success {
                credential_id: target_id,
            });
        }

        let mut new_cred = session.cred_template;
        new_cred.access_token = Some(token.access_token);
        new_cred.refresh_token = token.refresh_token;
        new_cred.expires_at = token.expires_at.or_else(|| {
            token
                .expires_in
                .map(|secs| (Utc::now() + Duration::seconds(secs)).to_rfc3339())
        });
        if let Some(arn) = token.profile_arn {
            new_cred.profile_arn = Some(arn);
        }

        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        tracing::info!("Social 登录成功，已添加凭据 #{}", credential_id);
        Ok(PollIdcLoginResponse::Success { credential_id })
    }

    /// 手动完成 Social 登录：远程访问时从浏览器地址栏粘贴的回调 URL 中提取参数，直接完成 token 兑换
    pub async fn complete_social_login(
        &self,
        session_id: &str,
        code: String,
        state: String,
        login_option: String,
        path: String,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        // 过期检查
        {
            let sessions = self.social_sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or(AdminServiceError::NotFound { id: 0 })?;
            if Utc::now() >= s.expires_at {
                return Ok(PollIdcLoginResponse::Expired);
            }
        }

        let callback = social::OAuthCallbackData {
            code,
            login_option,
            path,
            state,
        };
        self.do_complete_social_login(session_id, callback).await
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据")
        {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    // ── IdC 设备授权登录 ──────────────────────────────────────────────────────

    /// 发起 IdC 设备授权，返回验证码和 URL
    pub async fn start_idc_login(
        &self,
        req: StartIdcLoginRequest,
    ) -> Result<StartIdcLoginResponse, AdminServiceError> {
        let config = self.token_manager.config();
        let global_proxy = self.token_manager.proxy();

        // 代理：优先用请求级，否则回退全局
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let start_url = req.start_url.as_deref().unwrap_or(BUILDER_ID_START_URL);

        // 1. 注册 OIDC 客户端
        let reg = idc::register_client(&req.region, config, proxy.as_ref())
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        // 2. 发起设备授权
        let device = idc::start_device_authorization(
            &req.region,
            start_url,
            &reg.client_id,
            &reg.client_secret,
            config,
            proxy.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let expires_at = Utc::now() + Duration::seconds(device.expires_in);
        let session_id = Uuid::new_v4().to_string();

        // 构建登录成功后写入的凭据模板
        let cred_template = KiroCredentials {
            auth_method: Some("idc".to_string()),
            client_id: Some(reg.client_id.clone()),
            client_secret: Some(reg.client_secret.clone()),
            region: Some(req.region.clone()),
            priority: req.priority,
            email: req.email,
            proxy_url: req.proxy_url,
            ..Default::default()
        };

        let session = IdcAuthSession {
            region: req.region,
            client_id: reg.client_id,
            client_secret: reg.client_secret,
            device_code: device.device_code,
            expires_at,
            poll_interval: device.interval.max(5),
            cred_template,
            proxy,
            relogin_target_id: None,
        };

        let poll_interval = session.poll_interval;
        self.idc_sessions.lock().insert(session_id.clone(), session);

        Ok(StartIdcLoginResponse {
            session_id,
            user_code: device.user_code,
            verification_uri: device.verification_uri,
            verification_uri_complete: device.verification_uri_complete,
            expires_at: expires_at.to_rfc3339(),
            poll_interval,
        })
    }

    /// 轮询一次 IdC 登录状态
    pub async fn poll_idc_login(
        &self,
        session_id: &str,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        let (
            region,
            client_id,
            client_secret,
            device_code,
            _expires_at,
            proxy,
            cred_template,
            relogin_target_id,
        ) = {
            let sessions = self.idc_sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or_else(|| AdminServiceError::NotFound { id: 0 })?;

            if Utc::now() >= s.expires_at {
                return Ok(PollIdcLoginResponse::Expired);
            }

            (
                s.region.clone(),
                s.client_id.clone(),
                s.client_secret.clone(),
                s.device_code.clone(),
                s.expires_at,
                s.proxy.clone(),
                s.cred_template.clone(),
                s.relogin_target_id,
            )
        };

        let config = self.token_manager.config();

        match idc::poll_token(
            &region,
            &client_id,
            &client_secret,
            &device_code,
            config,
            proxy.as_ref(),
        )
        .await
        {
            idc::PollResult::Pending => Ok(PollIdcLoginResponse::Pending),
            idc::PollResult::Expired => {
                self.idc_sessions.lock().remove(session_id);
                Ok(PollIdcLoginResponse::Expired)
            }
            idc::PollResult::Error(e) => Err(AdminServiceError::InternalError(e.to_string())),
            idc::PollResult::Success(token) => {
                self.idc_sessions.lock().remove(session_id);

                // 重新登录模式：更新已有凭据而非创建新凭据
                if let Some(target_id) = relogin_target_id {
                    if let Some(refresh_token) = token.refresh_token {
                        self.do_relogin_update(target_id, refresh_token)
                            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
                    }
                    tracing::info!("IdC 重新登录成功，凭据 #{} Token 已更新", target_id);
                    return Ok(PollIdcLoginResponse::Success {
                        credential_id: target_id,
                    });
                }

                // 写入凭据
                let mut new_cred = cred_template;
                new_cred.access_token = Some(token.access_token);
                new_cred.refresh_token = token.refresh_token;
                if let Some(secs) = token.expires_in {
                    new_cred.expires_at = Some((Utc::now() + Duration::seconds(secs)).to_rfc3339());
                }

                let credential_id = self
                    .token_manager
                    .add_credential(new_cred)
                    .await
                    .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

                tracing::info!("IdC 设备授权登录成功，已添加凭据 #{}", credential_id);
                Ok(PollIdcLoginResponse::Success { credential_id })
            }
        }
    }

    /// 内部：重新登录完成后更新已有凭据的 Token（禁用→更新→重置→启用）
    fn do_relogin_update(&self, target_id: u64, refresh_token: String) -> anyhow::Result<()> {
        // 先禁用（update_refresh_token 要求凭据处于禁用状态）
        self.token_manager.set_disabled(target_id, true)?;
        // 更新 refreshToken（同时清空 accessToken 和 expiresAt，系统会在下次使用时自动刷新）
        self.token_manager
            .update_refresh_token(target_id, refresh_token, None, None)?;
        // 重置失败计数并重新启用
        self.token_manager.reset_and_enable(target_id)?;
        Ok(())
    }

    /// 发起 Social 重新登录（更新已有凭据的 Token 而非创建新凭据）
    pub async fn start_social_relogin(
        &self,
        target_id: u64,
        req: StartSocialLoginRequest,
    ) -> Result<StartSocialLoginResponse, AdminServiceError> {
        // 验证目标凭据存在
        {
            let snapshot = self.token_manager.snapshot();
            if !snapshot.entries.iter().any(|e| e.id == target_id) {
                return Err(AdminServiceError::NotFound { id: target_id });
            }
        }

        let global_proxy = self.token_manager.proxy();
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let auth_endpoint = req
            .auth_endpoint
            .unwrap_or_else(|| social::KIRO_AUTH_ENDPOINT.to_string());

        let (code_verifier, code_challenge) = social::generate_pkce();
        let state = uuid::Uuid::new_v4().to_string();

        let (tx, rx) = tokio::sync::oneshot::channel::<social::OAuthCallbackData>();

        let (port, server_handle) = social::start_callback_server(tx)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let redirect_uri = format!("http://127.0.0.1:{}", port);
        let portal_url = social::build_portal_url(&state, &code_challenge, &redirect_uri);

        let expires_at = Utc::now() + Duration::minutes(10);
        let session_id = uuid::Uuid::new_v4().to_string();

        let session = SocialAuthSession {
            auth_endpoint,
            state,
            code_verifier,
            redirect_uri,
            expires_at,
            callback_rx: tokio::sync::Mutex::new(rx),
            cred_template: KiroCredentials::default(),
            proxy,
            _server_handle: server_handle,
            relogin_target_id: Some(target_id),
        };

        self.social_sessions
            .lock()
            .insert(session_id.clone(), session);

        Ok(StartSocialLoginResponse {
            session_id,
            portal_url,
            expires_at: expires_at.to_rfc3339(),
        })
    }

    /// 发起 IdC 重新登录（更新已有凭据的 Token 而非创建新凭据）
    pub async fn start_idc_relogin(
        &self,
        target_id: u64,
        req: StartIdcLoginRequest,
    ) -> Result<StartIdcLoginResponse, AdminServiceError> {
        // 验证目标凭据存在
        {
            let snapshot = self.token_manager.snapshot();
            if !snapshot.entries.iter().any(|e| e.id == target_id) {
                return Err(AdminServiceError::NotFound { id: target_id });
            }
        }

        let config = self.token_manager.config();
        let global_proxy = self.token_manager.proxy();

        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let start_url = req.start_url.as_deref().unwrap_or(BUILDER_ID_START_URL);

        let reg = idc::register_client(&req.region, config, proxy.as_ref())
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let device = idc::start_device_authorization(
            &req.region,
            start_url,
            &reg.client_id,
            &reg.client_secret,
            config,
            proxy.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let expires_at = Utc::now() + Duration::seconds(device.expires_in);
        let session_id = Uuid::new_v4().to_string();

        let session = IdcAuthSession {
            region: req.region,
            client_id: reg.client_id,
            client_secret: reg.client_secret,
            device_code: device.device_code,
            expires_at,
            poll_interval: device.interval.max(5),
            cred_template: KiroCredentials::default(),
            proxy,
            relogin_target_id: Some(target_id),
        };

        let poll_interval = session.poll_interval;
        self.idc_sessions.lock().insert(session_id.clone(), session);

        Ok(StartIdcLoginResponse {
            session_id,
            user_code: device.user_code,
            verification_uri: device.verification_uri,
            verification_uri_complete: device.verification_uri_complete,
            expires_at: expires_at.to_rfc3339(),
            poll_interval,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_image_refs() {
        assert!(validate_image_ref("zyphrzero/kiro-rs:latest").is_ok());
        assert!(validate_image_ref(" docker.io/owner/kiro-rs:v1 ").is_ok());
        assert!(validate_image_ref("registry-1.docker.io/owner/kiro-rs").is_ok());

        assert!(validate_image_ref("").is_err());
        assert!(validate_image_ref("ghcr.io/owner/kiro-rs:latest").is_err());
        assert!(validate_image_ref("docker.io/owner/").is_err());
        assert!(validate_image_ref("docker.io//kiro-rs:latest").is_err());
        assert!(validate_image_ref("docker.io/owner/kiro rs:latest").is_err());
        // 单段、缺少 owner 视为非法
        assert!(validate_image_ref("kiro-rs").is_err());
    }
}
