//! gh-proxy-manager —— 管理 Arch Linux 上的 GitHub 下载加速。
//!
//! 分层：
//! - [`paths`] / [`state`]：磁盘布局与状态探测
//! - [`mirrors`] / [`gitcfg`]：纯逻辑（规范化、列表、insteadOf 计划）
//! - [`accelerator`]：GitHub 链接识别/规范化、云端节点测速与完整性校验
//! - [`apply`] / [`exec`]：生成 root 脚本、提权执行、回读校验
//! - [`probe`]：真实 release 文件测速
//! - [`cli`] / [`gui`]：两个前端，共用以上全部逻辑

pub mod accelerator;
pub mod apply;
pub mod cli;
pub mod error;
pub mod exec;
pub mod gitcfg;
pub mod logging;
pub mod mirrors;
pub mod paths;
pub mod probe;
pub mod shell;
pub mod sources;
pub mod state;

#[cfg(feature = "gui")]
pub mod gui;

pub use error::{Error, Result};
