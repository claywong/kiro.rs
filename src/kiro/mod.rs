//! Kiro API 客户端模块

pub mod auth;
pub mod create_response;
pub mod credential_dedup;
pub mod endpoint;
pub mod error;
pub mod kiro_version;
pub mod machine_id;
pub mod model;
pub mod parser;
pub mod provider;
pub mod token_manager;
// 本地新增：定向单凭据调用，单独成行避免与上游模块声明的增删相撞。
pub mod provider_pinned;
// 本地新增：CreateResponse 回退调用辅助。
pub mod provider_create_response;
#[cfg(test)]
mod provider_affinity_tests;
#[cfg(test)]
mod provider_network_error_tests;
#[cfg(test)]
mod provider_create_response_tests;
