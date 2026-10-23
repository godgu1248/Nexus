//! Nexus（枢纽）—— 团队协作网站的库根。
//!
//! 为什么要有 lib 而不是纯 bin：
//! 1. M1 起需求要求"service 关键业务集成测试"，集成测试在 `tests/` 里只能测库，不能测 bin；
//! 2. `shared` 里的校验/可见性/审计在 M0 还没有调用方，放进 bin crate 会被 `dead_code` 报警，
//!    而 lib 里的 `pub` 项不会。用 `#![allow(dead_code)]` 掩盖只会把真正的死代码一起埋掉。
//!
//! `main.rs` 只做装配（读配置 → 建池 → 起服务），业务代码一律留在库里的模块。

pub mod admin;
pub mod app;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod forum;
pub mod group;
pub mod kb;
pub mod media;
pub mod notice;
pub mod public;
pub mod shared;
pub mod state;
pub mod task;
