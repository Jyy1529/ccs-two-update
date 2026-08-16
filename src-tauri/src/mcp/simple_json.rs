//! DeepSeek / Pi 的 MCP 同步
//!
//! 两个客户端共用同一形态：配置目录下独立的 `mcp.json`，
//! 结构为 `{"mcpServers": {"<id>": <统一 spec JSON>}}`。
//! 使用独立文件（而非 config.json）避免 Provider 切换时整体覆盖丢失 MCP 配置。

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

use crate::config::{read_json_file, write_json_file};
use crate::error::AppError;

const MCP_FILE_NAME: &str = "mcp.json";
const MCP_SERVERS_KEY: &str = "mcpServers";

fn deepseek_mcp_path() -> PathBuf {
    crate::deepseek_config::get_deepseek_dir().join(MCP_FILE_NAME)
}

fn pi_mcp_path() -> PathBuf {
    crate::pi_config::get_pi_dir().join(MCP_FILE_NAME)
}

fn read_servers_map(path: &Path) -> Result<Map<String, Value>, AppError> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let root: Value = read_json_file(path)?;
    let root = root
        .as_object()
        .ok_or_else(|| AppError::Config(format!("{} 根必须是对象", path.display())))?;
    match root.get(MCP_SERVERS_KEY) {
        None => Ok(Map::new()),
        Some(value) => value.as_object().cloned().ok_or_else(|| {
            AppError::Config(format!(
                "{} 的 {} 字段必须是对象",
                path.display(),
                MCP_SERVERS_KEY
            ))
        }),
    }
}

fn write_servers_map(path: &Path, servers: Map<String, Value>) -> Result<(), AppError> {
    // 保留 mcp.json 中除 mcpServers 外的其他键（若用户手动添加过）
    let mut root: Value = if path.exists() {
        read_json_file(path)?
    } else {
        json!({})
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| AppError::Config(format!("{} 根必须是对象", path.display())))?;
    obj.insert(MCP_SERVERS_KEY.to_string(), Value::Object(servers));
    write_json_file(path, &root)
}

fn sync_single_server(path: PathBuf, id: &str, server_spec: &Value) -> Result<(), AppError> {
    let mut servers = read_servers_map(&path)?;
    servers.insert(id.to_string(), server_spec.clone());
    write_servers_map(&path, servers)
}

fn remove_server(path: PathBuf, id: &str) -> Result<(), AppError> {
    if !path.exists() {
        return Ok(());
    }
    let mut servers = read_servers_map(&path)?;
    if servers.remove(id).is_some() {
        write_servers_map(&path, servers)?;
    }
    Ok(())
}

/// 将单个 MCP 服务器同步到 DeepSeek 的 mcp.json
pub fn sync_single_server_to_deepseek(id: &str, server_spec: &Value) -> Result<(), AppError> {
    sync_single_server(deepseek_mcp_path(), id, server_spec)
}

/// 从 DeepSeek 的 mcp.json 中移除单个 MCP 服务器
pub fn remove_server_from_deepseek(id: &str) -> Result<(), AppError> {
    remove_server(deepseek_mcp_path(), id)
}

/// 将单个 MCP 服务器同步到 Pi 的 mcp.json
pub fn sync_single_server_to_pi(id: &str, server_spec: &Value) -> Result<(), AppError> {
    sync_single_server(pi_mcp_path(), id, server_spec)
}

/// 从 Pi 的 mcp.json 中移除单个 MCP 服务器
pub fn remove_server_from_pi(id: &str) -> Result<(), AppError> {
    remove_server(pi_mcp_path(), id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn sync_and_remove_round_trip_preserves_other_keys() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join(MCP_FILE_NAME);
        std::fs::write(&path, r#"{"custom":"kept"}"#).expect("seed file");

        let spec = json!({"type":"stdio","command":"echo"});
        sync_single_server(path.clone(), "s1", &spec).expect("sync");

        let root: Value = read_json_file(&path).expect("read back");
        assert_eq!(root["custom"], "kept");
        assert_eq!(root[MCP_SERVERS_KEY]["s1"], spec);

        remove_server(path.clone(), "s1").expect("remove");
        let root: Value = read_json_file(&path).expect("read back");
        assert_eq!(root["custom"], "kept");
        assert!(root[MCP_SERVERS_KEY]
            .as_object()
            .expect("servers object")
            .is_empty());
    }

    #[test]
    fn remove_from_missing_file_is_noop() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join(MCP_FILE_NAME);
        remove_server(path.clone(), "absent").expect("noop remove");
        assert!(!path.exists());
    }

    #[test]
    fn malformed_root_is_rejected_without_overwriting() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join(MCP_FILE_NAME);
        let original = "[]";
        std::fs::write(&path, original).expect("seed malformed file");

        let error = sync_single_server(path.clone(), "s1", &json!({"type": "stdio"}))
            .expect_err("array root must be rejected");
        assert!(matches!(error, AppError::Config(message) if message.contains("根必须是对象")));
        assert_eq!(std::fs::read_to_string(&path).expect("read file"), original);
    }

    #[test]
    fn malformed_servers_value_is_rejected_without_overwriting() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join(MCP_FILE_NAME);
        let original = r#"{"mcpServers": [] , "custom": "kept"}"#;
        std::fs::write(&path, original).expect("seed malformed file");

        let error = sync_single_server(path.clone(), "s1", &json!({"type": "stdio"}))
            .expect_err("non-object mcpServers must be rejected");
        assert!(matches!(error, AppError::Config(message) if message.contains("mcpServers")));
        assert_eq!(std::fs::read_to_string(&path).expect("read file"), original);
    }
}
