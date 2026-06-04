//! Engram 引擎库 crate。
//!
//! 把引擎的各功能模块作为库公开，供二进制入口 `main.rs` 与集成测试共享：
//! - [`model`]：核心数据结构与分层参数；
//! - [`activation`]：衰减与加固的懒计算；
//! - [`consolidate`]：升降级状态机（纯算法）；
//! - [`commands`]：write / recall / list 三命令的可测纯逻辑内核；
//! - [`render`]：热索引渲染与 JSON 目录加载（后者现仅服务 `import`）；
//! - [`store`]：redb 持久化存储层（本切片新增的 IO 边界）；
//! - [`session`]：会话巩固的水位线 / pending 标记 / 增量切片支撑层
//!   （增量复盘 + 异常退出的补偿巩固）。
//!
//! 设计文档参考：§3 总体架构、§13 交付形态（存储选定 redb）。

pub mod activation;
pub mod commands;
pub mod consolidate;
pub mod model;
pub mod render;
pub mod session;
pub mod store;
