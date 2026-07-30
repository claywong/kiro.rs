//! 定向单凭据 API 调用（本地扩展）
//!
//! [`KiroProvider::call_api_with_retry`] 走的是账号池调度：不指定凭据、失败自动
//! 换号。Admin 的「单个凭据模型测试」需要的恰好相反——必须打在指定的那张凭据上，
//! 失败即失败，否则测试结果无法归因（点凭据 #5 的测试却由 #12 应答，等于什么都
//! 没验证）。
//!
//! 因此这里复用 provider 的 endpoint / client / profileArn 装配逻辑，但去掉凭据
//! 选择与故障转移。独立成文件，避免与上游重试路径的改动挤在同一批函数里。
//!
//! @author wangzhong

use crate::kiro::endpoint::RequestContext;
use crate::kiro::machine_id;
use crate::kiro::provider::{KiroCallResult, KiroProvider};
use crate::kiro::token_manager::CallContext;

impl KiroProvider {
    /// 向【指定凭据】发送一次非流式 API 请求，不做凭据选择、不做故障转移。
    ///
    /// 与账号池路径的差异：
    /// - 凭据由 `credential_id` 固定，token 按需刷新（复用只读查询的准备流程）；
    /// - 不参与 RPM 记账、不上报成功/失败统计——这是诊断动作，不应污染调度器的
    ///   健康度与延迟画像，也不应因一次人工测试把凭据判失效；
    /// - 上游返回非 2xx 时直接返回错误，由调用方展示给操作者。
    ///
    /// 返回值中的 `credential_id` 恒等于入参，供前端确认「测的就是这张」。
    pub async fn call_api_pinned(
        &self,
        request_body: &str,
        credential_id: u64,
    ) -> anyhow::Result<KiroCallResult> {
        let (token, credentials) = self
            .token_manager()
            .prepare_request_token(credential_id)
            .await?;

        let mut ctx = CallContext {
            id: credential_id,
            credentials,
            token,
        };
        // Enterprise / IdC 账号需要真实 profileArn，与账号池路径保持一致
        self.ensure_profile_arn(&mut ctx).await?;

        let config = self.token_manager().config();
        let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);
        let endpoint = self.endpoint_for(&ctx.credentials)?;

        let rctx = RequestContext {
            credentials: &ctx.credentials,
            token: &ctx.token,
            machine_id: &machine_id,
            config,
        };

        let url = endpoint.api_url(&rctx);
        let body = endpoint.transform_api_body(request_body, &rctx);

        let base = self
            .client_for(&ctx.credentials)?
            .post(&url)
            .body(body)
            .header("content-type", endpoint.content_type())
            .header("Connection", "close");
        let response = endpoint.decorate_api(base, &rctx).send().await?;

        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            anyhow::bail!("凭据 #{credential_id} 请求失败: {status} {detail}");
        }

        Ok(KiroCallResult {
            response,
            credential_id,
        })
    }
}
