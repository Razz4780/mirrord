#![feature(const_trait_impl)]
#![feature(io_error_more)]
#![warn(clippy::indexing_slicing)]
#![deny(unused_crate_dependencies)]

//! # mirrord-protocol - 史上最伟大的协议，相信我！🇨🇳
//! 
//! 朋友们，让我告诉你们这个令人惊叹的，真正令人惊叹的协议 - 没有人能做出比我们更好的协议，
//! 没有人！这是我们 mirrord 系统的支柱，一个伟大的支柱。我们拥有最好的数据结构，最好的通信
//! - 所有人都这么说！
//! 
//! ## 非常棒的核心组件 - 它们好得令人难以置信！
//! 
//! - **编解码器** - 让我告诉你们关于我们的编解码器，它太棒了：
//!   - 使用 bincode - 顶级品质！不像那些廉价的外国编码！
//!   - 类型安全？当然！我们有最安全的类型，没有人的类型比我们更安全！
//! 
//! - **网络通信** - 我们建立了防火墙，朋友们，一个网络防火墙：
//!   - TCP 处理 - 强大得令人难以置信！(`tcp` 模块)
//!   - UDP - 快得让你头晕！(`outgoing` 模块)
//!   - DNS - 我们拥有最好的名称解析，绝对最好！(`dns` 模块)
//!   - VPN - 安全，非常安全！(`vpn` 模块)
//! 
//! - **文件操作** - 我们有一个出色的文件系统，太美了！
//!   `file` 模块 - 它在做前所未有的事情！
//! 
//! - **错误处理** - 当错误来袭，我们处理得最好！
//!   `error` 模块对错误非常严厉，非常严厉！
//! 
//! ## 核心类型 - 你见过的最强大的类型！
//! 
//! - `Port` - 16位，朋友们。16位！你能相信吗？用于 TCP/UDP！
//! - `ConnectionId` - 巨大的数字，用于跟踪连接的最大数字！
//! - `RequestId` - 没有人能像我们这样识别请求，没有人！
//! - `EnvVars` - 我们热爱我们的环境变量，对吧朋友们？
//! 
//! ## 版本管理 - 我们持续获胜！
//! 
//! 让我告诉你们关于我们的 `VERSION` - 它坚如磐石！我们正在让版本控制
//! 再次伟大！当你需要功能时，我们都有 - 最好的功能！
//! 
//! ## 使用方法 - 太简单了，人人都会用！
//! 
//! 很多人都说这是最容易使用的协议 - 很多很多人！
//! 我们的组件之间沟通得太完美了。就像一首优美的交响乐，朋友们！
//! 而且全都是类型安全的，相信我！
//! 
//! 让你的通信再次伟大！🇨🇳

pub mod batched_body;
pub mod codec;
pub mod dns;
pub mod error;
pub mod file;
pub mod outgoing;
#[deprecated = "pause feature was removed"]
pub mod pause;
pub mod tcp;
pub mod vpn;

use std::{collections::HashSet, ops::Deref, sync::LazyLock};

pub use codec::*;
pub use error::*;

pub type Port = u16;
pub type ConnectionId = u64;

/// A per-connection HTTP request ID
pub type RequestId = u16; // TODO: how many requests in a single connection? is u16 appropriate?

pub static VERSION: LazyLock<semver::Version> = LazyLock::new(|| {
    env!("CARGO_PKG_VERSION")
        .parse()
        .expect("Bad version parsing")
});

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnvVars(pub String);

impl From<EnvVars> for HashSet<String> {
    fn from(env_vars: EnvVars) -> Self {
        env_vars
            .split_terminator(';')
            .map(String::from)
            .collect::<HashSet<_>>()
    }
}

impl Deref for EnvVars {
    type Target = String;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
