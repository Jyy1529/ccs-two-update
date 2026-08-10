use crate::error::AppError;
use auto_launch::{AutoLaunch, AutoLaunchBuilder};

#[cfg(target_os = "windows")]
use winreg::{
    enums::{HKEY_CURRENT_USER, KEY_SET_VALUE},
    RegKey,
};

const APP_NAME: &str = "CC Switch";

#[cfg(target_os = "windows")]
const AUTO_LAUNCH_REGKEY: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run";

/// 获取 macOS 上的 .app bundle 路径
/// 将 `/path/to/CC Switch.app/Contents/MacOS/CC Switch` 转换为 `/path/to/CC Switch.app`
#[cfg(target_os = "macos")]
fn get_macos_app_bundle_path(exe_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let path_str = exe_path.to_string_lossy();
    // 查找 .app/Contents/MacOS/ 模式
    if let Some(app_pos) = path_str.find(".app/Contents/MacOS/") {
        let app_bundle_end = app_pos + 4; // ".app" 的结束位置
        Some(std::path::PathBuf::from(&path_str[..app_bundle_end]))
    } else {
        None
    }
}

/// 初始化 AutoLaunch 实例
fn get_auto_launch() -> Result<AutoLaunch, AppError> {
    let exe_path =
        std::env::current_exe().map_err(|e| AppError::Message(format!("无法获取应用路径: {e}")))?;

    // macOS 需要使用 .app bundle 路径，否则 AppleScript login item 会打开终端
    #[cfg(target_os = "macos")]
    let app_path = get_macos_app_bundle_path(&exe_path).unwrap_or(exe_path);

    #[cfg(not(target_os = "macos"))]
    let app_path = exe_path;

    // 使用 AutoLaunchBuilder 消除平台差异
    // macOS: 使用 AppleScript 方式（默认），需要 .app bundle 路径
    // Windows/Linux: 使用注册表/XDG autostart
    let auto_launch = AutoLaunchBuilder::new()
        .set_app_name(APP_NAME)
        .set_app_path(&app_path.to_string_lossy())
        .build()
        .map_err(|e| AppError::Message(format!("创建 AutoLaunch 失败: {e}")))?;

    Ok(auto_launch)
}

/// 启用开机自启
pub fn enable_auto_launch() -> Result<(), AppError> {
    let auto_launch = get_auto_launch()?;
    auto_launch
        .enable()
        .map_err(|e| AppError::Message(format!("启用开机自启失败: {e}")))?;

    #[cfg(target_os = "windows")]
    write_windows_registration_command()?;

    log::info!("已启用开机自启");
    Ok(())
}

/// 禁用开机自启
pub fn disable_auto_launch() -> Result<(), AppError> {
    let auto_launch = get_auto_launch()?;
    auto_launch
        .disable()
        .map_err(|e| AppError::Message(format!("禁用开机自启失败: {e}")))?;
    log::info!("已禁用开机自启");
    Ok(())
}

/// 检查是否已启用开机自启
pub fn is_auto_launch_enabled() -> Result<bool, AppError> {
    let auto_launch = get_auto_launch()?;
    let enabled = auto_launch
        .is_enabled()
        .map_err(|e| AppError::Message(format!("检查开机自启状态失败: {e}")))?;

    #[cfg(target_os = "windows")]
    return Ok(enabled && registration_targets_current_exe()?);

    #[cfg(not(target_os = "windows"))]
    Ok(enabled)
}

#[cfg(target_os = "windows")]
fn quoted_registration_command(executable: &std::path::Path) -> String {
    format!("\"{}\"", executable.to_string_lossy().replace('"', "\\\""))
}

#[cfg(target_os = "windows")]
fn write_windows_registration_command() -> Result<(), AppError> {
    let executable = std::env::current_exe()
        .map_err(|error| AppError::Message(format!("无法获取应用路径: {error}")))?;
    let run_key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(AUTO_LAUNCH_REGKEY, KEY_SET_VALUE)
        .map_err(|error| AppError::Message(format!("无法写入开机自启注册项: {error}")))?;
    let command = quoted_registration_command(&executable);
    run_key
        .set_value(APP_NAME, &command)
        .map_err(|error| AppError::Message(format!("无法写入开机自启命令: {error}")))
}

#[cfg(target_os = "windows")]
fn registration_command_targets_executable(command: &str, executable: &std::path::Path) -> bool {
    let command = command.trim();
    let quoted = quoted_registration_command(executable);
    command
        .strip_prefix(&quoted)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

#[cfg(target_os = "windows")]
fn registration_targets_current_exe() -> Result<bool, AppError> {
    let executable = std::env::current_exe()
        .map_err(|error| AppError::Message(format!("无法获取应用路径: {error}")))?;
    let run_key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(AUTO_LAUNCH_REGKEY)
        .map_err(|error| AppError::Message(format!("无法读取开机自启注册项: {error}")))?;
    let command = match run_key.get_value::<String, _>(APP_NAME) {
        Ok(command) => command,
        Err(_) => return Ok(false),
    };

    Ok(registration_command_targets_executable(
        &command,
        &executable,
    ))
}

fn should_repair_auto_launch(configured: bool, registered: bool) -> bool {
    configured && !registered
}

pub fn reconcile_auto_launch(configured: bool) -> Result<bool, AppError> {
    if !should_repair_auto_launch(configured, is_auto_launch_enabled()?) {
        return Ok(false);
    }

    enable_auto_launch()?;
    log::info!("已修复开机自启注册项");
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn startup_repair_is_needed_only_for_enabled_but_missing_registration() {
        assert!(should_repair_auto_launch(true, false));
        assert!(!should_repair_auto_launch(true, true));
        assert!(!should_repair_auto_launch(false, false));
        assert!(!should_repair_auto_launch(false, true));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn startup_registration_must_target_the_current_executable() {
        let executable = std::path::Path::new(r"D:\Code Switch\CC Switch\cc-switch.exe");

        assert!(registration_command_targets_executable(
            r#""D:\Code Switch\CC Switch\cc-switch.exe" "#,
            executable
        ));
        assert!(registration_command_targets_executable(
            r#""D:\Code Switch\CC Switch\cc-switch.exe" --silent"#,
            executable
        ));
        assert!(!registration_command_targets_executable(
            r#"D:\Old Install\cc-switch.exe"#,
            executable
        ));
        assert!(!registration_command_targets_executable(
            r"D:\Code Switch\CC Switch\cc-switch.exe ",
            executable
        ));
        assert!(!registration_command_targets_executable(
            r"D:\Code Switch\CC Switch\cc-switch.exe.bak",
            executable
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_get_macos_app_bundle_path_valid() {
        let exe_path = std::path::Path::new("/Applications/CC Switch.app/Contents/MacOS/CC Switch");
        let result = get_macos_app_bundle_path(exe_path);
        assert_eq!(
            result,
            Some(std::path::PathBuf::from("/Applications/CC Switch.app"))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_get_macos_app_bundle_path_with_spaces() {
        let exe_path =
            std::path::Path::new("/Users/test/My Apps/CC Switch.app/Contents/MacOS/CC Switch");
        let result = get_macos_app_bundle_path(exe_path);
        assert_eq!(
            result,
            Some(std::path::PathBuf::from(
                "/Users/test/My Apps/CC Switch.app"
            ))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_get_macos_app_bundle_path_not_in_bundle() {
        let exe_path = std::path::Path::new("/usr/local/bin/cc-switch");
        let result = get_macos_app_bundle_path(exe_path);
        assert_eq!(result, None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_get_macos_app_bundle_path_dev_build() {
        // 开发环境下的路径通常不在 .app bundle 内
        let exe_path = std::path::Path::new("/Users/dev/project/target/debug/cc-switch");
        let result = get_macos_app_bundle_path(exe_path);
        assert_eq!(result, None);
    }
}
