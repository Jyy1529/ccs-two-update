//! Pi 客户端配置目录与配置文件辅助
//!
//! Pi coding agent 使用 `~/.pi` 作为默认配置目录（agent 子目录存放
//! models.json 等），CC Switch 管理其中的 provider 配置。

use std::path::PathBuf;

/// 获取 Pi 配置目录
///
/// 解析顺序：
///   1. CCS 设置 `pi_config_dir`（显式覆盖）
///   2. 默认 `~/.pi`
pub fn get_pi_dir() -> PathBuf {
    if let Some(override_dir) = crate::settings::get_pi_override_dir() {
        return override_dir;
    }
    crate::config::get_home_dir().join(".pi")
}

/// 获取 Pi 配置文件路径（config.json）
#[allow(dead_code)]
pub fn get_pi_config_path() -> PathBuf {
    get_pi_dir().join("config.json")
}
