//! Kiro 托管门户登录（Hosted browser sign-in）
//!
//! 移植自 Kiro-Go PR #134：实现与 Kiro IDE 相同的 https://app.kiro.dev/signin 登录流程。
//! 与 Builder ID / IAM Identity Center（AWS SSO OIDC）不同，该门户在单个 PKCE 授权码流程下
//! 同时联合了 Google、GitHub 以及企业身份提供方（如 Microsoft 365 / Entra ID / Azure AD）。
//! 这是企业 Azure 租户账号（既非 AWS Builder ID 也非 AWS IAM Identity Center）登录 Kiro 的唯一方式。
//!
//! 流程有两条腿，由绑定在固定回环端口上的一次性监听器统一捕获：
//!
//!   - Social（Google/GitHub）：门户通过其 Cognito 后端认证，直接把授权码重定向回回环地址，
//!     然后在 Kiro social token 端点换取 token。
//!
//!   - 企业 / 外部 IdP（Azure AD）：门户检测到邮箱属于外部 IdP，重定向到 /signin/callback 并
//!     携带 IdP 描述符（issuer_url、client_id、scopes）而非授权码。随后我们直接对该 IdP 驱动
//!     第二个 OIDC 授权码 + PKCE 流程（回环重定向到 /oauth/callback），并在 IdP token 端点换取
//!     授权码。得到的 access token 是 IdP 签发、面向 CodeWhisperer 的 token，作为运行时 bearer
//!     使用，并在 IdP token 端点刷新（见 token_manager::refresh_external_idp_token）。
//!
//! 登录通过 admin 面板以 Start/Poll/Cancel 会话模式暴露：start_kiro_sso_login 绑定监听器并返回
//! 登录 URL；操作者在**同一主机**上用浏览器打开它（重定向目标是 127.0.0.1:3128）；
//! poll_kiro_sso_auth 在监听器捕获授权码前返回 pending，捕获后换取并返回凭据。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use anyhow::{anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;

/// Kiro 托管登录页（在浏览器中打开）
const KIRO_SIGN_IN_BASE_URL: &str = "https://app.kiro.dev/signin";
/// 门户校验并在登录成功后重定向回的固定回环地址。主机为 "localhost"（门户期望值），
/// 而监听器绑定的是 127.0.0.1 / [::1] 字面量；浏览器把 "localhost" 解析为任一回环地址
/// 是二者的桥梁，因此操作者主机必须把 localhost 解析到回环地址。
const KIRO_REDIRECT_URI: &str = "http://localhost:3128";
/// AWS IAM Identity Center 直连流程专用的回环 redirect base。AWS SSO OIDC 的 RegisterClient
/// 强制要求 public client 使用「loopback interface」——即 IP 字面量 127.0.0.1，`localhost` 会被
/// 拒绝（error=invalid_redirect_uri: "Requested client type must use loopback interface for
/// redirect"）。redirect_uri 在 RegisterClient / /authorize / /token 三处必须完全一致。
const KIRO_IDC_REDIRECT_URI: &str = "http://127.0.0.1:3128";
/// KIRO_REDIRECT_URI 中嵌入的回环端口。
const KIRO_REDIRECT_PORT: u16 = 3128;
/// 门户期望的 Kiro IDE 客户端标记。
const KIRO_REDIRECT_FROM: &str = "KiroIDE";
/// 企业（外部 IdP）腿把授权码重定向回的回环路径。与门户的 /signin/callback 区分开，
/// 便于监听器辨别两条腿。
const KIRO_OAUTH_CALLBACK_PATH: &str = "/oauth/callback";
/// Cognito 支持的 social 授权码换取端点。注意这与 social 刷新端点（/refreshToken）不同：
/// Kiro IDE 在 /oauth/token 换取登录码，在 /refreshToken 刷新。不要合并。
const KIRO_SOCIAL_TOKEN_URL: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/token";
/// 监听器等待用户完成登录的超时时间。
const KIRO_SSO_LOGIN_TIMEOUT: StdDuration = StdDuration::from_secs(600);

/// 允许的外部 IdP issuer / 端点主机后缀。issuer 来自可被攻击者影响的门户回调查询参数，
/// 因此限制为已知企业 IdP 主机（Microsoft Entra / Azure AD）。这是抵御 SSRF、开放重定向、
/// 强制认证滥用（通过伪造 /signin/callback）的主要控制手段。前导点把每个后缀锚定到真实的
/// 子域边界，使 "evil-microsoftonline.com" 无法匹配。要接入更多企业 IdP 请扩展此列表。
const ALLOWED_EXTERNAL_IDP_ISSUER_SUFFIXES: &[&str] = &[
    ".microsoftonline.com",
    ".microsoftonline.us",
    ".microsoftonline.cn",
    // AWS IAM Identity Center（原 AWS SSO）。access portal / OIDC 主机形如
    // d-xxxxxxxxxx.awsapps.com；awsapps.com 是 AWS 控制的根域，前导点锚定子域边界，
    // 使 "evil-awsapps.com" 无法匹配。与上面的 Microsoft 根域同属可信厂商多租户域。
    ".awsapps.com",
];

/// 登录会话的解析结果，供 admin 处理器在换取授权码后使用。
#[derive(Debug, Clone, Default)]
pub struct KiroSsoResult {
    pub access_token: String,
    pub refresh_token: String,
    pub auth_method: String, // "external_idp" | "social" | "idc"
    pub provider: String,    // "AzureAD" | "Kiro SSO" | "IAM Identity Center"
    pub client_id: Option<String>,
    /// IdC（AWS SSO OIDC）刷新所需的动态注册 client secret；仅 auth_method = "idc" 时非空。
    pub client_secret: Option<String>,
    pub token_endpoint: Option<String>,
    pub issuer_url: Option<String>,
    pub scopes: Option<String>,
    pub profile_arn: Option<String>,
    pub region: String,
    pub expires_in: i64,
    pub email: Option<String>,
}

/// poll 的返回状态。
pub enum KiroSsoPollStatus {
    Pending,
    Completed(Box<KiroSsoResult>),
}

/// 回环监听器交付的原始捕获结果：social 授权码、企业（外部 IdP）授权码及其 leg-2 上下文，或错误。
#[derive(Clone, Default)]
struct KiroSsoCapture {
    kind: String, // "social" | "external_idp"
    code: String,
    /// 非空时表示终态错误（例如授权错误、描述符无效）
    err: Option<String>,
    token_endpoint: String,
    issuer_url: String,
    client_id: String,
    scopes: String,
    redirect_uri: String,
    code_verifier: String,
    // --- idc（AWS IAM Identity Center 授权码流程）专用：leg-2 换取时需要的 client secret 与 region ---
    idc_client_secret: String,
    idc_region: String,
}

/// 企业描述符到达 /signin/callback 时捕获、在 IdP 把授权码重定向回来时消费的每次尝试状态。
#[derive(Clone)]
struct KiroLeg2 {
    /// "external_idp"（Azure AD 等）或 "idc"（AWS IAM Identity Center）
    kind: String,
    state: String,
    verifier: String,
    token_endpoint: String,
    issuer_url: String,
    client_id: String,
    scopes: String,
    redirect_uri: String,
    /// 仅 idc：动态注册得到的 client secret（授权码换取 + 后续刷新都需要）
    client_secret: String,
    /// 仅 idc：AWS SSO OIDC region（端点/刷新用）
    region: String,
}

/// 单次托管门户登录尝试的瞬态状态。
struct KiroSsoSession {
    id: String,
    verifier: String, // social 腿 PKCE verifier（在 social 授权码换取时发送）
    state: String,    // 门户在 social 重定向上回显的反 CSRF state
    region: String,
    proxy: Option<ProxyConfig>,
    tls_backend: TlsBackend,
    expires_at: Instant,

    /// 关闭监听器的信号发送端（drop / send 即触发 axum graceful shutdown）
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    /// 回环 serve 任务句柄（含广播任务）。close() 时强制 abort，立即 drop TcpListener
    /// 释放端口——不能只靠 graceful shutdown：回调页是 HTTP keep-alive 长连接，优雅关闭
    /// 会一直等这条连接排空，导致 serve future 永不返回、127.0.0.1:3128 被永久占用。
    serve_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// 捕获结果通道（一次性）
    result_rx: Mutex<Option<oneshot::Receiver<KiroSsoCapture>>>,
    result_tx: Mutex<Option<oneshot::Sender<KiroSsoCapture>>>,

    leg2: Mutex<Option<KiroLeg2>>,
}

/// 会话内部状态，随 axum handler 通过共享指针传递。
#[derive(Clone)]
struct ListenerState {
    session: Arc<KiroSsoSession>,
}

/// 全局会话注册表。
static KIRO_SSO_SESSIONS: RwLock<Option<HashMap<String, Arc<KiroSsoSession>>>> = RwLock::new(None);

fn sessions_insert(session: Arc<KiroSsoSession>) {
    let mut guard = KIRO_SSO_SESSIONS.write();
    guard.get_or_insert_with(HashMap::new).insert(session.id.clone(), session);
}

fn sessions_get(id: &str) -> Option<Arc<KiroSsoSession>> {
    KIRO_SSO_SESSIONS.read().as_ref().and_then(|m| m.get(id).cloned())
}

fn sessions_remove(id: &str) {
    if let Some(m) = KIRO_SSO_SESSIONS.write().as_mut() {
        m.remove(id);
    }
}

/// 关闭并清空所有在途会话。回调端口固定（127.0.0.1:3128），同一时刻只能有一个登录，
/// 所以每次 start 前调用此函数：即使用户放弃了上一次登录、没点取消或取消未送达，
/// 也能强制回收端口，避免第二次登录报“无法绑定”。
fn sessions_clear_all() {
    let old: Vec<Arc<KiroSsoSession>> = {
        let mut guard = KIRO_SSO_SESSIONS.write();
        match guard.as_mut() {
            Some(m) => m.drain().map(|(_, s)| s).collect(),
            None => Vec::new(),
        }
    };
    for s in old {
        s.close();
    }
}

/// 生成 PKCE code verifier（32 字节随机 → base64url，无填充）。
fn generate_code_verifier() -> String {
    let mut bytes = [0u8; 32];
    for b in bytes.iter_mut() {
        *b = fastrand::u8(..);
    }
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 生成 PKCE code challenge（SHA-256(verifier) → base64url，无填充）。
fn generate_code_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// 生成账号 ID（UUID v4）。
pub fn generate_account_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 校验 raw_url 是否为 https 且主机为非 IP、在白名单内的企业 IdP 主机。用于在发现前把关
/// issuer，以及把关两个已发现端点（浏览器被 302 到的 authorize URL 和授权码换取的 token 端点）。
/// 凭据导入 / 刷新路径用它防止 SSRF / refresh token 外泄。
pub fn validate_external_idp_endpoint(raw_url: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(raw_url.trim())
        .map_err(|e| anyhow!("无效的外部 IdP URL: {}", e))?;
    if !parsed.scheme().eq_ignore_ascii_case("https") {
        bail!("外部 IdP URL 必须是 https");
    }
    let host = parsed
        .host_str()
        .map(|h| h.to_lowercase())
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow!("外部 IdP URL 没有主机名"))?;
    // 拒绝 IP 字面量主机；只有命名的、白名单内的主机可通过。
    if host.parse::<std::net::IpAddr>().is_ok() {
        bail!("外部 IdP 主机不能是 IP 字面量");
    }
    for suffix in ALLOWED_EXTERNAL_IDP_ISSUER_SUFFIXES {
        if host.ends_with(suffix) {
            return Ok(());
        }
    }
    bail!("外部 IdP 主机 {:?} 不在白名单内", host)
}

/// 从 access token JWT 的 payload（不验证签名）提取账号邮箱。Azure AD v2.0 token 常常省略
/// "email" claim，把登录名放在 "preferred_username"/"upn"，因此作为回退。
pub fn extract_email_from_jwt(access_token: &str) -> Option<String> {
    let parts: Vec<&str> = access_token.trim().split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(parts[1]))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    for key in ["email", "preferred_username", "upn", "unique_name"] {
        if let Some(v) = claims.get(key).and_then(|v| v.as_str()) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// 门户回环回调的绑定地址。默认仅回环（127.0.0.1 + 尽力而为的 [::1]），这是安全默认：
/// 只有同一主机能到达瞬态回调。设置 KIRO_SSO_CALLBACK_BIND 覆盖绑定主机；当代理运行在容器中、
/// 操作者浏览器通过非回环接口访问已发布端口时需要（例如 KIRO_SSO_CALLBACK_BIND=0.0.0.0）。
fn kiro_callback_bind_addrs() -> Vec<String> {
    if let Ok(bind) = std::env::var("KIRO_SSO_CALLBACK_BIND") {
        let bind = bind.trim();
        if !bind.is_empty() {
            return vec![format!("{}:{}", bind, KIRO_REDIRECT_PORT)];
        }
    }
    vec![
        format!("127.0.0.1:{}", KIRO_REDIRECT_PORT),
        format!("[::1]:{}", KIRO_REDIRECT_PORT),
    ]
}

/// 生成 PKCE 码、绑定回环监听器，返回会话 ID 和操作者需打开的托管登录 URL。
pub async fn start_kiro_sso_login(
    region: Option<String>,
    proxy: Option<ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<(String, String)> {
    let region = region
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "us-east-1".to_string());

    // 回收所有在途会话，释放固定回调端口。防止上一次被放弃/取消未生效的登录占着
    // 127.0.0.1:3128，导致本次绑定失败。abort 立即释放，随后短暂等待确保端口就绪。
    sessions_clear_all();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let verifier = generate_code_verifier();
    let challenge = generate_code_challenge(&verifier);
    let state = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();

    let (result_tx, result_rx) = oneshot::channel::<KiroSsoCapture>();

    let session = Arc::new(KiroSsoSession {
        id: session_id.clone(),
        verifier,
        state: state.clone(),
        region,
        proxy,
        tls_backend,
        expires_at: Instant::now() + KIRO_SSO_LOGIN_TIMEOUT,
        shutdown: Mutex::new(None),
        serve_tasks: Mutex::new(Vec::new()),
        result_rx: Mutex::new(Some(result_rx)),
        result_tx: Mutex::new(Some(result_tx)),
        leg2: Mutex::new(None),
    });

    start_listener(session.clone()).await?;

    // 构造登录 URL
    let mut url = url::Url::parse(KIRO_SIGN_IN_BASE_URL).unwrap();
    url.query_pairs_mut()
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("redirect_uri", KIRO_REDIRECT_URI)
        .append_pair("redirect_from", KIRO_REDIRECT_FROM);
    let sign_in_url = url.to_string();

    sessions_insert(session.clone());

    // 到期自毁：释放回环监听器并丢弃会话，即使操作者放弃登录、前端停止轮询。
    // 否则被放弃的登录会占用 127.0.0.1:3128 直到进程重启，阻塞后续所有 SSO 登录
    // （重定向端口固定，同一时间只能有一个登录使用）。
    let sid = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(KIRO_SSO_LOGIN_TIMEOUT).await;
        if let Some(s) = sessions_get(&sid) {
            s.close();
        }
        sessions_remove(&sid);
    });

    Ok((session_id, sign_in_url))
}

/// 启动 AWS IAM Identity Center（Enterprise SSO）直连登录 —— 不经过 app.kiro.dev 门户。
///
/// 对齐 Kiro Account Manager 参考实现的自包含授权码 + PKCE 流程：
///   1. RegisterClient 动态注册 public client（声明 redirectUris + authorization_code/refresh_token）
///   2. 预填 leg2（kind=idc），绑定回环监听器
///   3. 直接返回 https://oidc.{region}.amazonaws.com/authorize URL 让操作者在浏览器打开
/// 用户在 AWS 登录页输账密 → 回调 /oauth/callback?code=... → 现有 leg-2 消费并在 /token 换取。
///
/// 返回 (session_id, authorize_url)。
pub async fn start_kiro_idc_login(
    start_url: String,
    region: Option<String>,
    proxy: Option<ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<(String, String)> {
    let start_url = start_url.trim().to_string();
    if start_url.is_empty() {
        bail!("请填写 IAM Identity Center 的 Start URL");
    }
    validate_external_idp_endpoint(&start_url)
        .map_err(|e| anyhow!("Start URL 校验失败: {}", e))?;

    let region = region
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "us-east-1".to_string());

    // 回收在途会话，释放固定回调端口（同上）。
    sessions_clear_all();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // 先 RegisterClient（同步），失败则直接报错，不占用会话/端口。
    let client = build_client(proxy.as_ref(), 60, tls_backend)?;
    // AWS 强制 loopback IP 字面量：redirect_uri 用 127.0.0.1（不能用 localhost），
    // RegisterClient / /authorize / /token 三处必须一致。
    let redirect_uri = format!("{}{}", KIRO_IDC_REDIRECT_URI, KIRO_OAUTH_CALLBACK_PATH);
    let reg = idc_register_client(&client, &region, &start_url, &redirect_uri)
        .await
        .map_err(|e| anyhow!("IAM Identity Center 注册客户端失败: {}", e))?;

    let verifier = generate_code_verifier();
    let challenge = generate_code_challenge(&verifier);
    let state2 = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();

    let (result_tx, result_rx) = oneshot::channel::<KiroSsoCapture>();

    let session = Arc::new(KiroSsoSession {
        id: session_id.clone(),
        verifier: verifier.clone(),
        state: String::new(), // idc 不用 social 的 state，用 leg2.state
        region: region.clone(),
        proxy,
        tls_backend,
        expires_at: Instant::now() + KIRO_SSO_LOGIN_TIMEOUT,
        shutdown: Mutex::new(None),
        serve_tasks: Mutex::new(Vec::new()),
        result_rx: Mutex::new(Some(result_rx)),
        result_tx: Mutex::new(Some(result_tx)),
        // 预填 leg2：回调到达 /oauth/callback 时直接消费。
        leg2: Mutex::new(Some(KiroLeg2 {
            kind: "idc".to_string(),
            state: state2.clone(),
            verifier: verifier.clone(),
            token_endpoint: format!("https://oidc.{}.amazonaws.com/token", region),
            issuer_url: start_url.clone(),
            client_id: reg.client_id.clone(),
            scopes: IDC_SCOPES.join(","),
            redirect_uri: redirect_uri.clone(),
            client_secret: reg.client_secret.clone(),
            region: region.clone(),
        })),
    });

    start_listener(session.clone()).await?;

    let authorize_url =
        idc_authorize_url(&region, &reg.client_id, &redirect_uri, &challenge, &state2);

    sessions_insert(session.clone());

    // 到期自毁（同 start_kiro_sso_login）。
    let sid = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(KIRO_SSO_LOGIN_TIMEOUT).await;
        if let Some(s) = sessions_get(&sid) {
            s.close();
        }
        sessions_remove(&sid);
    });

    Ok((session_id, authorize_url))
}

/// 轮询登录状态。捕获授权码前返回 Pending，捕获后换取并返回 Completed。
/// 终态错误（超时、换取失败）以 Err 返回。
pub async fn poll_kiro_sso_auth(session_id: &str) -> anyhow::Result<KiroSsoPollStatus> {
    let session = sessions_get(session_id)
        .ok_or_else(|| anyhow!("会话未找到或已过期"))?;

    // 在独立作用域内非阻塞读取，确保 parking_lot 锁在任何 .await 之前完全释放
    // （否则 !Send 的 guard 跨 await 会让整个 future 变成 !Send，无法作为 axum handler）。
    let recv = {
        let mut rx_guard = session.result_rx.lock();
        match rx_guard.as_mut() {
            Some(rx) => rx.try_recv(),
            // 接收端已被消费（不应发生），视为已关闭
            None => Err(oneshot::error::TryRecvError::Closed),
        }
    };

    match recv {
        Ok(capture) => {
            // 终态：捕获到授权码（或错误）。无论结果如何都拆除监听器并丢弃会话。
            session.close();
            sessions_remove(session_id);
            if let Some(err) = capture.err {
                bail!("{}", err);
            }
            let result = session.exchange(capture).await?;
            Ok(KiroSsoPollStatus::Completed(Box::new(result)))
        }
        Err(oneshot::error::TryRecvError::Empty) => {
            if Instant::now() > session.expires_at {
                session.close();
                sessions_remove(session_id);
                bail!("SSO 登录超时（{} 秒）", KIRO_SSO_LOGIN_TIMEOUT.as_secs());
            }
            Ok(KiroSsoPollStatus::Pending)
        }
        Err(oneshot::error::TryRecvError::Closed) => {
            session.close();
            sessions_remove(session_id);
            bail!("SSO 登录会话意外关闭");
        }
    }
}

/// 手动提交回调 URL（无 SSH 隧道场景）。
///
/// 远程服务器部署时，用户本机可能无法建立 `localhost:3128` 的隧道，浏览器在 AWS 授权后会跳到
/// `http://127.0.0.1:3128/oauth/callback?code=...&state=...` 打不开——但地址栏里的完整 URL 有效。
/// 用户把整条 URL 粘贴进来，这里解析出 code + state，用 leg2.state 做反 CSRF 校验后投递捕获，
/// 随后由现有 poll 循环完成 /token 交换与落库。
///
/// 只对 idc 会话开放（社交/门户流程本就依赖浏览器直接命中回环，不走此路）。
pub fn submit_kiro_idc_callback(session_id: &str, callback_url: &str) -> anyhow::Result<()> {
    let session = sessions_get(session_id)
        .ok_or_else(|| anyhow!("会话未找到或已过期，请重新发起登录"))?;

    // 从粘贴的 URL 解析 query。允许用户粘贴完整 URL；也兼容只粘贴 "code=...&state=..." 的情况。
    let raw = callback_url.trim();
    if raw.is_empty() {
        bail!("请粘贴完整的回调地址");
    }
    let query_str = match url::Url::parse(raw) {
        Ok(u) => u.query().unwrap_or("").to_string(),
        // 不是完整 URL：尝试按 "?a=b&c=d" 或 "a=b&c=d" 解析
        Err(_) => raw.trim_start_matches('?').to_string(),
    };
    let query: HashMap<String, String> = url::form_urlencoded::parse(query_str.as_bytes())
        .into_owned()
        .collect();
    let get = |k: &str| query.get(k).map(|s| s.trim().to_string()).unwrap_or_default();

    let code = get("code");
    let state_q = get("state");
    let err_param = get("error");

    // 取 leg2 上下文（idc 会话在 start 时已预填）。
    let ctx2 = session.leg2.lock().clone();
    let ctx2 = match ctx2 {
        Some(c) if c.kind == "idc" => c,
        Some(_) => bail!("当前会话不支持手动回调（仅 IAM Identity Center 直连登录支持）"),
        None => bail!("会话状态异常（缺少授权上下文），请重新发起登录"),
    };

    if !err_param.is_empty() {
        let desc = get("error_description");
        let msg = format!("AWS 授权错误: {} {}", err_param, desc);
        session.deliver_error(msg.clone());
        bail!("{}", msg);
    }
    if code.is_empty() {
        bail!("回调地址里缺少 code 参数，请确认粘贴完整");
    }
    // 反 CSRF：state 必须与本会话 /authorize 时下发的一致。
    if state_q.is_empty() || state_q != ctx2.state {
        bail!("state 不匹配，可能粘贴了其它会话的回调地址，请重新发起登录并粘贴对应的回调地址");
    }

    session.deliver(KiroSsoCapture {
        kind: ctx2.kind.clone(),
        code,
        err: None,
        token_endpoint: ctx2.token_endpoint,
        issuer_url: ctx2.issuer_url,
        client_id: ctx2.client_id,
        scopes: ctx2.scopes,
        redirect_uri: ctx2.redirect_uri,
        code_verifier: ctx2.verifier,
        idc_client_secret: ctx2.client_secret,
        idc_region: ctx2.region,
        ..Default::default()
    });
    Ok(())
}

/// 立即拆除进行中的会话（操作者在 admin 面板取消），无需等待到期即释放回环端口。
/// 对未知或已完成的会话是空操作。
pub fn cancel_kiro_sso_login(session_id: &str) {
    if let Some(session) = sessions_get(session_id) {
        session.close();
    }
    sessions_remove(session_id);
}

impl KiroSsoSession {
    /// 关闭回环监听器。可安全多次调用。
    ///
    /// 先发 graceful shutdown 信号，再强制 abort 所有 serve 任务。abort 是关键：
    /// 回调页是 HTTP keep-alive 长连接，graceful shutdown 会一直等这条连接排空，
    /// serve future 永不返回、TcpListener 永不 drop，导致 127.0.0.1:3128 被永久占用，
    /// 后续 SSO 登录全部报“无法绑定”。abort 立即取消任务、drop listener、释放端口。
    fn close(&self) {
        if let Some(tx) = self.shutdown.lock().take() {
            let _ = tx.send(());
        }
        let tasks = std::mem::take(&mut *self.serve_tasks.lock());
        for t in tasks {
            t.abort();
        }
    }

    /// 把（唯一的）捕获结果投递到结果通道。
    fn deliver(&self, capture: KiroSsoCapture) {
        if let Some(tx) = self.result_tx.lock().take() {
            let _ = tx.send(capture);
        }
    }

    /// 把捕获的授权码换取为 token 并组装解析后的凭据。
    async fn exchange(&self, capture: KiroSsoCapture) -> anyhow::Result<KiroSsoResult> {
        let client = build_client(self.proxy.as_ref(), 60, self.tls_backend)?;

        // idc（AWS IAM Identity Center，授权码 + PKCE）：用回调拿到的授权码在
        // https://oidc.{region}.amazonaws.com/token 换取 token（grantType=authorization_code）。
        if capture.kind == "idc" {
            let (access, refresh, expires_in) = exchange_idc_code(
                &client,
                &capture.token_endpoint,
                &capture.client_id,
                &capture.idc_client_secret,
                &capture.code,
                &capture.code_verifier,
                &capture.redirect_uri,
            )
            .await
            .map_err(|e| anyhow!("IAM Identity Center token 换取失败: {}", e))?;

            return Ok(KiroSsoResult {
                email: extract_email_from_jwt(&access),
                access_token: access,
                refresh_token: refresh,
                auth_method: "idc".to_string(),
                provider: "IAM Identity Center".to_string(),
                client_id: Some(capture.client_id),
                client_secret: Some(capture.idc_client_secret),
                token_endpoint: None,
                issuer_url: Some(capture.issuer_url),
                scopes: None,
                // IdC 凭据不需要 profileArn（发送反而会导致 403，见 token_manager 注释）
                profile_arn: None,
                region: capture.idc_region,
                expires_in,
            });
        }

        if capture.kind == "external_idp" {
            let (access, refresh, expires_in) = exchange_external_idp_code(
                &client,
                &capture.token_endpoint,
                &capture.client_id,
                &capture.code,
                &capture.code_verifier,
                &capture.redirect_uri,
                &capture.scopes,
            )
            .await
            .map_err(|e| anyhow!("企业 SSO token 换取失败: {}", e))?;

            return Ok(KiroSsoResult {
                email: extract_email_from_jwt(&access),
                access_token: access,
                refresh_token: refresh,
                auth_method: "external_idp".to_string(),
                provider: "AzureAD".to_string(),
                client_id: Some(capture.client_id),
                client_secret: None,
                token_endpoint: Some(capture.token_endpoint),
                issuer_url: Some(capture.issuer_url),
                scopes: Some(capture.scopes),
                profile_arn: None,
                region: self.region.clone(),
                expires_in,
            });
        }

        // Social 腿
        let (access, refresh, expires_in, profile_arn) =
            exchange_social_code(&client, &capture.code, &self.verifier)
                .await
                .map_err(|e| anyhow!("SSO token 换取失败: {}", e))?;

        Ok(KiroSsoResult {
            email: extract_email_from_jwt(&access),
            access_token: access,
            refresh_token: refresh,
            auth_method: "social".to_string(),
            provider: "Kiro SSO".to_string(),
            client_id: None,
            client_secret: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            profile_arn,
            region: self.region.clone(),
            expires_in,
        })
    }
}

/// 绑定回环监听 socket，开启 SO_REUSEADDR 并带短重试。
///
/// SO_REUSEADDR 让新监听器能立即复用处于 TIME_WAIT 的地址；重试则覆盖前一个登录刚
/// 被 abort、内核尚未完全释放 socket 的极短窗口。二者共同确保连续两次 SSO 登录不会
/// 因为“无法绑定 127.0.0.1:3128”而失败。
async fn bind_reuse(sa: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..5 {
        let socket = if sa.is_ipv4() {
            tokio::net::TcpSocket::new_v4()
        } else {
            tokio::net::TcpSocket::new_v6()
        }?;
        // 允许复用 TIME_WAIT 地址
        socket.set_reuseaddr(true)?;
        match socket.bind(sa).and_then(|_| socket.listen(1024)) {
            Ok(listener) => return Ok(listener),
            Err(e) => {
                last_err = Some(e);
                // 端口刚释放的极短窗口，退避后重试
                tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AddrInUse, "bind retries exhausted")
    }))
}

async fn start_listener(session: Arc<KiroSsoSession>) -> anyhow::Result<()> {
    use axum::Router;
    use axum::routing::any;

    let addrs = kiro_callback_bind_addrs();
    let state = ListenerState { session: session.clone() };

    let make_router = || {
        Router::new()
            .route("/", any(handle_callback))
            .route("/{*path}", any(handle_callback))
            .with_state(state.clone())
    };

    // 绑定第一个（必需）地址
    let primary: SocketAddr = addrs[0]
        .parse()
        .map_err(|e| anyhow!("无法解析回调绑定地址 {}: {}", addrs[0], e))?;
    let listener = bind_reuse(primary).await.map_err(|e| {
        anyhow!(
            "无法绑定 {} 作为 SSO 回调（端口是否已被占用？）: {}",
            addrs[0],
            e
        )
    })?;

    // 单个 shutdown 广播给所有子监听器
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    *session.shutdown.lock() = Some(shutdown_tx);

    // 收集所有后台任务句柄，close() 时强制 abort 立即释放端口
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // 用一个共享的 Notify 传播关闭
    let notify = Arc::new(tokio::sync::Notify::new());
    let notify_primary = notify.clone();
    tasks.push(tokio::spawn(async move {
        let _ = shutdown_rx.await;
        notify.notify_waiters();
    }));

    let router = make_router();
    let n = notify_primary.clone();
    tasks.push(tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { n.notified().await })
            .await;
    }));

    // 尽力而为绑定其余地址（如 IPv6 回环）
    for addr in addrs.iter().skip(1) {
        if let Ok(sa) = addr.parse::<SocketAddr>() {
            if let Ok(l) = bind_reuse(sa).await {
                let router = make_router();
                let n = notify_primary.clone();
                tasks.push(tokio::spawn(async move {
                    let _ = axum::serve(l, router)
                        .with_graceful_shutdown(async move { n.notified().await })
                        .await;
                }));
            } else {
                tracing::debug!("[KiroSSO] 次要回调绑定 {} 跳过", addr);
            }
        }
    }

    *session.serve_tasks.lock() = tasks;

    Ok(())
}

/// 回环回调页 HTML。
fn kiro_callback_page(ok: bool) -> String {
    let msg = if ok {
        "Kiro 登录完成。可以关闭此标签页并返回 admin 面板。"
    } else {
        "Kiro 登录失败。请返回 admin 面板重试。"
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Kiro Sign-In</title></head><body style=\"font-family:sans-serif;padding:2rem\"><p>{}</p></body></html>",
        msg
    )
}

/// 回环回调状态机：企业 leg-1 描述符 → 302 到 IdP；企业 leg-2 授权码在 /oauth/callback；否则是 social 授权码。
async fn handle_callback(
    axum::extract::State(state): axum::extract::State<ListenerState>,
    method: axum::http::Method,
    uri: axum::http::Uri,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::{Html, IntoResponse, Redirect};

    let session = &state.session;

    // 只期望浏览器 GET 重定向；拒绝其他方法以缩小本地攻击面。
    if method != axum::http::Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let path = uri.path().to_string();
    let query: HashMap<String, String> = uri
        .query()
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();

    let get = |k: &str| query.get(k).map(|s| s.trim().to_string()).unwrap_or_default();

    // 临时诊断：打印回调 path 与全部参数键（值截断，避免泄露 code/token），用于确认门户对
    // IAM Identity Center start URL 实际回传的描述符结构（login_option 值、是否带 issuer_url 等）。
    {
        let mut kv: Vec<String> = query
            .iter()
            .map(|(k, v)| {
                let vt: String = v.chars().take(32).collect();
                let suffix = if v.chars().count() > 32 { "…" } else { "" };
                format!("{}={}{}", k, vt, suffix)
            })
            .collect();
        kv.sort();
        tracing::warn!("[KiroSSO][DIAG] callback path={} query=[{}]", path, kv.join(" | "));
    }

    // --- 企业 leg-1：外部 IdP 描述符（无 code）---
    // 用 path != /oauth/callback 把关，使伪造的 /oauth/callback?issuer_url=... 无法被路由到这里
    // 从而重置进行中的 leg-2。
    let login_option = get("login_option");
    let issuer_url_q = get("issuer_url");

    // --- AWS IAM Identity Center 腿（login_option=awsidc）---
    // 门户对 IAM Identity Center start URL 回传 login_option=awsidc + issuer_url + idc_region，
    // 不含 client_id。IAM Identity Center 走的是 AWS SSO OIDC 的【授权码 + PKCE】流程
    // （对齐 Kiro Account Manager 参考实现，grantTypes=[authorization_code, refresh_token]）：
    //   1. RegisterClient 动态注册 client（含 redirectUris），得到 clientId/clientSecret
    //   2. 把浏览器 302 到 https://oidc.{region}.amazonaws.com/authorize，用户在 AWS 登录页输账密
    //   3. AWS 回调 /oauth/callback?code=... → 走下面的 leg-2，用 code 在 /token 换 token
    // 必须放在下面 external_idp 分支之前，因为 awsidc 也带 issuer_url，否则会误入 external_idp。
    if path != KIRO_OAUTH_CALLBACK_PATH && login_option.eq_ignore_ascii_case("awsidc") {
        // 单次：一旦 leg-2 在飞行中，忽略后续描述符，防止重置/劫持登录。
        if session.leg2.lock().is_some() {
            return StatusCode::NO_CONTENT.into_response();
        }
        let issuer_url = issuer_url_q;
        // 门户回传的 region 键为 idc_region；回退到会话 region。
        let idc_region = {
            let r = get("idc_region");
            if r.is_empty() { session.region.clone() } else { r }
        };

        if issuer_url.is_empty() {
            session.deliver_error("IAM Identity Center 描述符无效（缺少 issuer_url）".to_string());
            return (StatusCode::BAD_REQUEST, Html(kiro_callback_page(false))).into_response();
        }
        if let Err(e) = validate_external_idp_endpoint(&issuer_url) {
            session.deliver_error(format!("IAM Identity Center issuer 校验失败: {}", e));
            return (StatusCode::BAD_REQUEST, Html(kiro_callback_page(false))).into_response();
        }
        if idc_region.trim().is_empty() {
            session.deliver_error("IAM Identity Center 描述符无效（缺少 region）".to_string());
            return (StatusCode::BAD_REQUEST, Html(kiro_callback_page(false))).into_response();
        }

        let client = match build_client(session.proxy.as_ref(), 60, session.tls_backend) {
            Ok(c) => c,
            Err(e) => {
                session.deliver_error(format!("IAM Identity Center 初始化失败: {}", e));
                return (StatusCode::INTERNAL_SERVER_ERROR, Html(kiro_callback_page(false)))
                    .into_response();
            }
        };

        // 授权码流程用的回调地址（必须在 RegisterClient 的 redirectUris 中声明，并与 /authorize 一致）。
        let redirect_uri = format!("{}{}", KIRO_REDIRECT_URI, KIRO_OAUTH_CALLBACK_PATH);

        // RegisterClient：授权码流程，声明 redirectUris + authorization_code/refresh_token。
        let reg =
            match idc_register_client(&client, &idc_region, &issuer_url, &redirect_uri).await {
                Ok(r) => r,
                Err(e) => {
                    session.deliver_error(format!("IAM Identity Center 注册客户端失败: {}", e));
                    return (StatusCode::BAD_GATEWAY, Html(kiro_callback_page(false)))
                        .into_response();
                }
            };

        let verifier = generate_code_verifier();
        let state2 = uuid::Uuid::new_v4().to_string();
        {
            let mut leg2 = session.leg2.lock();
            if leg2.is_some() {
                return StatusCode::NO_CONTENT.into_response();
            }
            *leg2 = Some(KiroLeg2 {
                kind: "idc".to_string(),
                state: state2.clone(),
                verifier: verifier.clone(),
                token_endpoint: format!("https://oidc.{}.amazonaws.com/token", idc_region),
                issuer_url: issuer_url.clone(),
                client_id: reg.client_id.clone(),
                scopes: IDC_SCOPES.join(","),
                redirect_uri: redirect_uri.clone(),
                client_secret: reg.client_secret.clone(),
                region: idc_region.clone(),
            });
        }

        // 构造 /authorize URL，把浏览器 302 到 AWS 登录页。
        let auth_url = idc_authorize_url(
            &idc_region,
            &reg.client_id,
            &redirect_uri,
            &generate_code_challenge(&verifier),
            &state2,
        );
        tracing::warn!(
            "[KiroSSO][DIAG] awsidc 分支命中，RegisterClient 成功 client_id={} → 302 到 authorize={}",
            reg.client_id,
            auth_url
        );
        return Redirect::temporary(&auth_url).into_response();
    }

    if path != KIRO_OAUTH_CALLBACK_PATH
        && (login_option.eq_ignore_ascii_case("external_idp") || !issuer_url_q.is_empty())
    {
        // 单次：一旦 leg-2 在飞行中，忽略后续描述符，防止散杂或伪造的本地请求重置/劫持登录。
        if session.leg2.lock().is_some() {
            return StatusCode::NO_CONTENT.into_response();
        }
        let issuer_url = issuer_url_q;
        let client_id = get("client_id");
        let scopes = get("scopes");
        let login_hint = get("login_hint");

        if issuer_url.is_empty() {
            session.deliver_error("外部 IdP 描述符无效（缺少 issuer_url）".to_string());
            return (StatusCode::BAD_REQUEST, Html(kiro_callback_page(false))).into_response();
        }
        if let Err(e) = validate_external_idp_endpoint(&issuer_url) {
            session.deliver_error(format!("外部 IdP issuer 校验失败: {}", e));
            return (StatusCode::BAD_REQUEST, Html(kiro_callback_page(false))).into_response();
        }
        if client_id.is_empty() {
            session.deliver_error("外部 IdP 描述符无效（缺少 client_id）".to_string());
            return (StatusCode::BAD_REQUEST, Html(kiro_callback_page(false))).into_response();
        }

        // oidc_discover 会把 issuer 与两个已发现端点都校验过白名单，因此这里的 issuer 不被盲信。
        let (auth_endpoint, token_endpoint) = match oidc_discover(session, &issuer_url).await {
            Ok(v) => v,
            Err(e) => {
                session.deliver_error(format!("{}", e));
                return (StatusCode::BAD_GATEWAY, Html(kiro_callback_page(false))).into_response();
            }
        };

        let verifier = generate_code_verifier();
        let state2 = uuid::Uuid::new_v4().to_string();
        let redirect_uri = format!("{}{}", KIRO_REDIRECT_URI, KIRO_OAUTH_CALLBACK_PATH);

        {
            // 在锁内复查以解决并发描述符竞态：只有第一个设置 leg2 并被重定向。
            let mut leg2 = session.leg2.lock();
            if leg2.is_some() {
                return StatusCode::NO_CONTENT.into_response();
            }
            *leg2 = Some(KiroLeg2 {
                kind: "external_idp".to_string(),
                state: state2.clone(),
                verifier: verifier.clone(),
                token_endpoint,
                issuer_url: issuer_url.clone(),
                client_id: client_id.clone(),
                scopes: scopes.clone(),
                redirect_uri: redirect_uri.clone(),
                client_secret: String::new(),
                region: String::new(),
            });
        }

        let auth_url = external_idp_authorize_url(
            &auth_endpoint,
            &client_id,
            &redirect_uri,
            &scopes,
            &generate_code_challenge(&verifier),
            &state2,
            &login_hint,
        );
        // 把同一浏览器标签重定向到 IdP 登录页。
        return Redirect::temporary(&auth_url).into_response();
    }

    // --- 企业 leg-2：IdP 授权码在 /oauth/callback ---
    if path == KIRO_OAUTH_CALLBACK_PATH {
        let ctx2 = session.leg2.lock().clone();
        let code = get("code");
        let state_q = get("state");
        let err_param = get("error");
        // 忽略与飞行中 leg-2 state 不匹配的回调。
        let ctx2 = match ctx2 {
            Some(c) if !state_q.is_empty() && state_q == c.state => c,
            _ => return StatusCode::NO_CONTENT.into_response(),
        };
        if !err_param.is_empty() {
            let desc = get("error_description");
            session.deliver_error(format!("外部 IdP 授权错误: {} {}", err_param, desc));
            return (StatusCode::OK, Html(kiro_callback_page(false))).into_response();
        }
        if code.is_empty() {
            return StatusCode::NO_CONTENT.into_response();
        }
        session.deliver(KiroSsoCapture {
            kind: ctx2.kind.clone(),
            code,
            err: None,
            token_endpoint: ctx2.token_endpoint,
            issuer_url: ctx2.issuer_url,
            client_id: ctx2.client_id,
            scopes: ctx2.scopes,
            redirect_uri: ctx2.redirect_uri,
            code_verifier: ctx2.verifier,
            idc_client_secret: ctx2.client_secret,
            idc_region: ctx2.region,
            ..Default::default()
        });
        return (StatusCode::OK, Html(kiro_callback_page(true))).into_response();
    }

    // --- Social leg-1：Cognito 授权码 ---
    let code = get("code");
    let err_param = get("error");
    let state_q = get("state");
    // 忽略既无 code 也无 error 的杂散命中，以及 state 不匹配的回调——不消费一次性通道。
    if code.is_empty() && err_param.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }
    if session.state.is_empty() || state_q != session.state {
        return StatusCode::NO_CONTENT.into_response();
    }
    if !err_param.is_empty() {
        let desc = get("error_description");
        session.deliver_error(format!("SSO 授权错误: {} {}", err_param, desc));
        return (StatusCode::OK, Html(kiro_callback_page(false))).into_response();
    }
    session.deliver(KiroSsoCapture {
        kind: "social".to_string(),
        code,
        err: None,
        token_endpoint: String::new(),
        issuer_url: String::new(),
        client_id: String::new(),
        scopes: String::new(),
        redirect_uri: String::new(),
        code_verifier: String::new(),
        ..Default::default()
    });
    (StatusCode::OK, Html(kiro_callback_page(true))).into_response()
}

impl KiroSsoSession {
    /// 投递一个携带具体错误信息的终态结果，使 poll 返回该错误。
    fn deliver_error(&self, msg: String) {
        tracing::warn!("[KiroSSO] {}", msg);
        self.deliver(KiroSsoCapture {
            err: Some(msg),
            ..Default::default()
        });
    }
}

/// 为 issuer_url 获取 OIDC 发现文档，返回其 authorization / token 端点。issuer 与两个已发现端点
/// 都要通过 IdP 主机白名单校验；不跟随重定向（使发现主机无法把请求跳到内部目标）；错误里不回显响应体。
async fn oidc_discover(
    session: &KiroSsoSession,
    issuer_url: &str,
) -> anyhow::Result<(String, String)> {
    validate_external_idp_endpoint(issuer_url)?;
    let doc_url = format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim().trim_end_matches('/')
    );

    // 不跟随重定向：白名单内的 issuer 主机必须直接应答，3xx（可能指向内部/链路本地目标）视为失败。
    let client = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none());
    let client = match session.proxy.as_ref() {
        Some(p) if !p.url.trim().is_empty() && p.url != "direct" => {
            let mut proxy = reqwest::Proxy::all(&p.url)?;
            if let (Some(u), Some(pw)) = (&p.username, &p.password) {
                proxy = proxy.basic_auth(u, pw);
            }
            client.proxy(proxy)
        }
        _ => client,
    };
    let client = client.build()?;

    let resp = client
        .get(&doc_url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| anyhow!("OIDC 发现请求失败: {}", e))?;
    if !resp.status().is_success() {
        bail!("OIDC 发现失败（status {}）", resp.status());
    }
    let doc: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("解析 OIDC 发现文档失败: {}", e))?;
    let auth_endpoint = doc
        .get("authorization_endpoint")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let token_endpoint = doc
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if auth_endpoint.is_empty() || token_endpoint.is_empty() {
        bail!("OIDC 发现文档缺少 authorization_endpoint 或 token_endpoint");
    }
    // 两个端点也必须 https + 白名单。
    validate_external_idp_endpoint(&auth_endpoint)
        .map_err(|e| anyhow!("已发现的 authorization_endpoint 被拒: {}", e))?;
    validate_external_idp_endpoint(&token_endpoint)
        .map_err(|e| anyhow!("已发现的 token_endpoint 被拒: {}", e))?;
    Ok((auth_endpoint, token_endpoint))
}

/// 构造 IdP 授权码 + PKCE URL（企业腿浏览器被重定向到这里）。scopes 从门户原样透传（已是空格分隔列表）。
fn external_idp_authorize_url(
    auth_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &str,
    challenge: &str,
    state: &str,
    login_hint: &str,
) -> String {
    let mut url = match url::Url::parse(auth_endpoint) {
        Ok(u) => u,
        Err(_) => return auth_endpoint.to_string(),
    };
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", scopes)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("response_mode", "query")
            .append_pair("state", state);
        if !login_hint.trim().is_empty() {
            q.append_pair("login_hint", login_hint);
        }
    }
    url.to_string()
}

#[derive(Serialize)]
struct SocialExchangePayload<'a> {
    code: &'a str,
    code_verifier: &'a str,
    redirect_uri: &'a str,
}

/// 用 PKCE verifier 在 Kiro social token 端点换取 Cognito 授权码得到 Kiro token。
/// 请求体匹配 Kiro IDE 客户端 —— {code, code_verifier, redirect_uri}，响应为 camelCase。
async fn exchange_social_code(
    client: &reqwest::Client,
    code: &str,
    code_verifier: &str,
) -> anyhow::Result<(String, String, i64, Option<String>)> {
    let payload = SocialExchangePayload {
        code: code.trim(),
        code_verifier,
        redirect_uri: KIRO_REDIRECT_URI,
    };
    let resp = client
        .post(KIRO_SOCIAL_TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&payload)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let json: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let access_token = json
        .get("accessToken")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if !status.is_success() || access_token.is_empty() {
        bail!("social token 换取失败（status {}）: {}", status, body);
    }
    let refresh_token = json
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let expires_in = json.get("expiresIn").and_then(|v| v.as_i64()).unwrap_or(0);
    let profile_arn = json
        .get("profileArn")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok((access_token, refresh_token, expires_in, profile_arn))
}

/// 用 PKCE verifier 在已发现的 token 端点换取 IdP 授权码得到 IdP token。
/// 标准 OAuth2 authorization_code 授权（public client，PKCE，无 client secret）；
/// 请求 form 编码，响应 snake_case。
async fn exchange_external_idp_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    scopes: &str,
) -> anyhow::Result<(String, String, i64)> {
    // 换取前再次校验 token 端点白名单（纵深防御）。
    validate_external_idp_endpoint(token_endpoint)?;

    let mut form: Vec<(&str, String)> = vec![
        ("client_id", client_id.to_string()),
        ("grant_type", "authorization_code".to_string()),
        ("code", code.trim().to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("code_verifier", code_verifier.to_string()),
    ];
    if !scopes.trim().is_empty() {
        form.push(("scope", scopes.to_string()));
    }

    let resp = client
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let json: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if !status.is_success() || access_token.is_empty() {
        let err = json.get("error").and_then(|v| v.as_str()).unwrap_or_default();
        if !err.is_empty() {
            let desc = json
                .get("error_description")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            bail!("外部 IdP token 换取失败（status {}）: {}: {}", status, err, desc);
        }
        bail!("外部 IdP token 换取失败（status {}）: {}", status, body);
    }
    let refresh_token = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let expires_in = json.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(0);
    Ok((access_token, refresh_token, expires_in))
}

// ============================================================================
// AWS IAM Identity Center（AWS SSO OIDC）授权码 + PKCE 流程
//
// 端点均为 https://oidc.{region}.amazonaws.com，请求/响应 body 为 camelCase JSON
// （与 token_manager::refresh_idc_token 一致）。流程（对齐 Kiro Account Manager 参考实现）：
//   1. RegisterClient  → clientId / clientSecret（public client，动态注册；声明 redirectUris
//                        + grantTypes=[authorization_code, refresh_token] + issuerUrl）
//   2. 浏览器 302 到 /authorize?response_type=code&...&code_challenge=...，用户在 AWS 登录页输账密
//   3. AWS 回调 /oauth/callback?code=... → leg-2 用 code 在 /token 换取
//      （grantType=authorization_code + codeVerifier）→ accessToken / refreshToken / expiresIn
// ============================================================================

/// Kiro 客户端注册用的 client 名称（对齐参考实现 Kiro Account Manager）。
const IDC_CLIENT_NAME: &str = "Kiro Account Manager";
/// IAM Identity Center 登录申请的 scopes（RegisterClient 时声明）。
///
/// 这些 scope 决定了签发的 access token 对 CodeWhisperer / Kiro API 的权限。
/// 逐字取自 Kiro Account Manager 参考实现（src/main/index.ts 的 start-iam-sso-login）。
const IDC_SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];

/// RegisterClient 结果。
struct IdcRegisteredClient {
    client_id: String,
    client_secret: String,
}

/// RegisterClient：动态注册一个 public client，声明授权码流程所需的 redirectUris /
/// grantTypes / issuerUrl，得到 clientId / clientSecret。
async fn idc_register_client(
    client: &reqwest::Client,
    region: &str,
    issuer_url: &str,
    redirect_uri: &str,
) -> anyhow::Result<IdcRegisteredClient> {
    let url = format!("https://oidc.{}.amazonaws.com/client/register", region);
    let body = serde_json::json!({
        "clientName": IDC_CLIENT_NAME,
        "clientType": "public",
        "scopes": IDC_SCOPES,
        "grantTypes": ["authorization_code", "refresh_token"],
        "redirectUris": [redirect_uri],
        "issuerUrl": issuer_url,
    });
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        // 透传 AWS 的 error_description，方便定位（常见：Start URL 无效、region 与实例不符）。
        let err = json.get("error").and_then(|v| v.as_str()).unwrap_or_default();
        let desc = json
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        tracing::warn!(
            "[KiroSSO] RegisterClient 400 region={} issuer_url={} redirect_uri={} → error={} desc={}",
            region,
            issuer_url,
            redirect_uri,
            err,
            desc
        );
        if !desc.is_empty() {
            bail!(
                "AWS 拒绝注册（{}）：{}。请确认 Start URL 正确、且 Region 与该 IAM Identity Center 实例所在区域一致。",
                if err.is_empty() { "invalid_request" } else { err },
                desc
            );
        }
        bail!("RegisterClient 失败（status {}）: {}", status, text);
    }
    let client_id = json
        .get("clientId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let client_secret = json
        .get("clientSecret")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if client_id.is_empty() || client_secret.is_empty() {
        bail!("RegisterClient 响应缺少 clientId/clientSecret");
    }
    Ok(IdcRegisteredClient { client_id, client_secret })
}

/// 构造 AWS SSO OIDC 的 /authorize URL（授权码 + PKCE）。scopes 用逗号分隔（对齐参考实现）。
fn idc_authorize_url(
    region: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    let base = format!("https://oidc.{}.amazonaws.com/authorize", region);
    let mut url = url::Url::parse(&base).unwrap();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scopes", &IDC_SCOPES.join(","))
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    url.to_string()
}

/// 用回调拿到的授权码在 /token 端点换取 token（grantType=authorization_code + PKCE codeVerifier）。
/// 请求/响应均为 camelCase JSON。返回 (accessToken, refreshToken, expiresIn)。
async fn exchange_idc_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> anyhow::Result<(String, String, i64)> {
    let body = serde_json::json!({
        "clientId": client_id,
        "clientSecret": client_secret,
        "grantType": "authorization_code",
        "redirectUri": redirect_uri,
        "code": code.trim(),
        "codeVerifier": code_verifier,
    });
    let resp = client
        .post(token_endpoint)
        .header("content-type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let access_token = json
        .get("accessToken")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if !status.is_success() || access_token.is_empty() {
        let err = json.get("error").and_then(|v| v.as_str()).unwrap_or_default();
        if !err.is_empty() {
            let desc = json
                .get("error_description")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            bail!("IdC token 换取失败（status {}）: {}: {}", status, err, desc);
        }
        bail!("IdC token 换取失败（status {}）: {}", status, text);
    }
    let refresh_token = json
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let expires_in = json.get("expiresIn").and_then(|v| v.as_i64()).unwrap_or(0);
    Ok((access_token, refresh_token, expires_in))
}
