//! DeepSeek 客户端配置目录与配置文件辅助
//!
//! DeepSeek harness 使用 `~/.deepseek` 作为默认配置目录，
//! 配置文件为 `config.json`（camelCase 平铺：baseUrl / apiKey / model）。

use std::path::PathBuf;

/// 获取 DeepSeek 配置目录
///
/// 解析顺序：
///   1. CCS 设置 `deepseek_config_dir`（显式覆盖）
///   2. 默认 `~/.deepseek`
pub fn get_deepseek_dir() -> PathBuf {
    if let Some(override_dir) = crate::settings::get_deepseek_override_dir() {
        return override_dir;
    }
    crate::config::get_home_dir().join(".deepseek")
}

/// 获取 DeepSeek 配置文件路径（config.json）
#[allow(dead_code)]
pub fn get_deepseek_config_path() -> PathBuf {
    get_deepseek_dir().join("config.json")
}
