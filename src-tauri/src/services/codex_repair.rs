use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub const CODEX_REPAIR_RUNTIME_COMMIT: &str = "cb6712f7e17f1c4082c0ad9a39ce225fb895d922";
#[cfg(test)]
pub const CODEX_REPAIR_RUNTIME_SHA256: &str =
    "2000F589B4B2A01DC0C6E2FDFC1A0B7AEBF885CF2F4EFF9E37A85D7D4425ED9B";
pub const CODEX_REPAIR_BOOTSTRAP: &str = include_str!("../../resources/codex_repair_bootstrap.ps1");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexRepairState {
    Unsupported,
    NotInstalled,
    Healthy,
    NeedsRepair,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRepairStatus {
    pub state: CodexRepairState,
    pub platform_supported: bool,
    pub codex_installed: bool,
    pub runtime_installed: bool,
    pub repair_running: bool,
    pub last_repair_error: Option<String>,
    pub package_version: Option<String>,
    pub warnings: Vec<String>,
    pub checked_at: String,
    pub runtime_commit: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRepairLaunchResult {
    pub started: bool,
    pub runtime_installed: bool,
}

#[derive(Debug, Deserialize)]
struct HealthSnapshot {
    ok: Option<bool>,
    warnings: Option<Value>,
    package: Option<HealthPackage>,
}

#[derive(Debug, Deserialize)]
struct HealthPackage {
    version: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairRunSnapshot {
    state: Option<String>,
    message: Option<String>,
}

pub fn classify_repair_state(
    platform_supported: bool,
    codex_installed: bool,
    health_ok: Option<bool>,
) -> CodexRepairState {
    if !platform_supported {
        return CodexRepairState::Unsupported;
    }
    if !codex_installed {
        return CodexRepairState::NotInstalled;
    }
    match health_ok {
        Some(true) => CodexRepairState::Healthy,
        Some(false) => CodexRepairState::NeedsRepair,
        None => CodexRepairState::Unknown,
    }
}

fn codex_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

fn health_snapshot_path(home: &Path) -> PathBuf {
    home.join("state").join("codex-repatch-health.json")
}

fn repair_status_path(home: &Path) -> PathBuf {
    home.join("state")
        .join("cc-switch-codex-repair-status.json")
}

fn read_repair_status(home: &Path) -> (bool, Option<String>) {
    let path = repair_status_path(home);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (false, None);
    };
    let Ok(snapshot) = serde_json::from_str::<RepairRunSnapshot>(&text) else {
        return (
            false,
            Some("repair status marker is invalid JSON".to_string()),
        );
    };
    let is_recent = std::fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed.as_secs() <= 6 * 60 * 60);

    match snapshot.state.as_deref() {
        Some("running") if is_recent => (true, None),
        Some("failed") => (false, snapshot.message),
        _ => (false, None),
    }
}

fn write_repair_status(home: &Path, state: &str, message: Option<&str>) -> Result<(), String> {
    let state_dir = home.join("state");
    std::fs::create_dir_all(&state_dir).map_err(|error| error.to_string())?;
    let snapshot = serde_json::json!({
        "state": state,
        "updatedAt": chrono::Utc::now().to_rfc3339(),
        "message": message,
    });
    std::fs::write(
        repair_status_path(home),
        serde_json::to_vec(&snapshot).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn runtime_installed(home: &Path) -> bool {
    let runtime_root = home.join("skills").join("codex-windows-fast-patch");
    let runner_exists = runtime_root
        .join("scripts")
        .join("repatch-codex-windows.ps1")
        .is_file();
    let version_matches = std::fs::read_to_string(runtime_root.join(".skill-version"))
        .ok()
        .is_some_and(|version| version.trim() == CODEX_REPAIR_RUNTIME_COMMIT);
    runner_exists && version_matches
}

fn parse_warnings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(key, value)| value.as_str().map(|text| format!("{key}: {text}")))
            .collect(),
        _ => Vec::new(),
    }
}

fn read_health(home: &Path) -> (Option<bool>, Option<String>, Vec<String>) {
    let Ok(text) = std::fs::read_to_string(health_snapshot_path(home)) else {
        return (None, None, Vec::new());
    };
    let Ok(snapshot) = serde_json::from_str::<HealthSnapshot>(&text) else {
        return (
            None,
            None,
            vec!["health snapshot is invalid JSON".to_string()],
        );
    };
    let package_version = snapshot
        .package
        .as_ref()
        .and_then(|package| package.version.clone());
    let mut warnings = parse_warnings(snapshot.warnings.as_ref());
    if snapshot
        .package
        .as_ref()
        .and_then(|package| package.status.as_deref())
        .is_some_and(|status| status != "Ok")
    {
        warnings.push("Codex Desktop package status is not Ok".to_string());
    }
    (snapshot.ok, package_version, warnings)
}

#[cfg(target_os = "windows")]
fn query_codex_package() -> (bool, Option<String>) {
    let powershell = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
    let output = Command::new(powershell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-AppxPackage -Name OpenAI.Codex | Select-Object -First 1 Version | ConvertTo-Json -Compress",
        ])
        .output();
    let Ok(output) = output else {
        return (false, None);
    };
    let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else {
        return (false, None);
    };
    let version = value
        .get("Version")
        .and_then(Value::as_str)
        .map(str::to_owned);
    (version.is_some(), version)
}

#[cfg(not(target_os = "windows"))]
fn query_codex_package() -> (bool, Option<String>) {
    (false, None)
}

pub fn detect() -> CodexRepairStatus {
    let platform_supported = cfg!(target_os = "windows");
    let home = codex_home();
    let (package_installed, package_version) = query_codex_package();
    let (mut health_ok, health_version, mut warnings) = read_health(&home);
    let (repair_running, last_repair_error) = read_repair_status(&home);
    if let (Some(current), Some(snapshot)) = (&package_version, &health_version) {
        if current != snapshot {
            warnings.push(format!(
                "health snapshot is for Codex Desktop {snapshot}; current version is {current}"
            ));
            health_ok = Some(false);
        }
    }
    let codex_installed = package_installed || health_version.is_some();
    let runtime_is_installed = runtime_installed(&home);
    if !runtime_is_installed && platform_supported {
        warnings.push("Fast Patch runtime is not installed".to_string());
    }
    CodexRepairStatus {
        state: classify_repair_state(platform_supported, codex_installed, health_ok),
        platform_supported,
        codex_installed,
        runtime_installed: runtime_is_installed,
        repair_running,
        last_repair_error,
        package_version: package_version.or(health_version),
        warnings,
        checked_at: chrono::Utc::now().to_rfc3339(),
        runtime_commit: CODEX_REPAIR_RUNTIME_COMMIT.to_string(),
    }
}

fn powershell_path() -> &'static str {
    r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
}

fn powershell_encoded_command(script: &str) -> String {
    let utf16_le = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    STANDARD.encode(utf16_le)
}

fn elevated_repair_command(script: &str) -> String {
    let encoded_script = powershell_encoded_command(script);
    format!(
        "$ErrorActionPreference = 'Stop'; Start-Process -FilePath '{powershell}' -Verb RunAs -WindowStyle Normal -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-EncodedCommand','{encoded_script}') -PassThru | Out-Null",
        powershell = powershell_path(),
    )
}

pub fn launch_repair() -> Result<CodexRepairLaunchResult, String> {
    if !cfg!(target_os = "windows") {
        return Err("Codex Desktop repair is supported on Windows only".to_string());
    }
    let home = codex_home();
    let (repair_running, _) = read_repair_status(&home);
    if repair_running {
        return Err("Codex Desktop repair is already running".to_string());
    }

    write_repair_status(
        &home,
        "running",
        Some("Waiting for the administrator repair window"),
    )?;
    let command = elevated_repair_command(CODEX_REPAIR_BOOTSTRAP);
    let mut launcher = Command::new(powershell_path());
    launcher.args(["-NoProfile", "-Command", &command]);
    #[cfg(target_os = "windows")]
    launcher.creation_flags(CREATE_NO_WINDOW);
    let status = launcher.status().map_err(|error| {
        let message = format!("failed to request administrator repair: {error}");
        let _ = write_repair_status(&home, "failed", Some(&message));
        message
    })?;
    if !status.success() {
        let _ = write_repair_status(
            &home,
            "failed",
            Some("The administrator repair process was not started"),
        );
        return Err(format!(
            "administrator repair was not started (exit code: {})",
            status
                .code()
                .map_or_else(|| "unknown".to_string(), |code| code.to_string())
        ));
    }
    Ok(CodexRepairLaunchResult {
        started: true,
        runtime_installed: runtime_installed(&home),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_supported_codex_health_states() {
        assert_eq!(
            classify_repair_state(false, false, None),
            CodexRepairState::Unsupported
        );
        assert_eq!(
            classify_repair_state(true, false, None),
            CodexRepairState::NotInstalled
        );
        assert_eq!(
            classify_repair_state(true, true, Some(true)),
            CodexRepairState::Healthy
        );
        assert_eq!(
            classify_repair_state(true, true, Some(false)),
            CodexRepairState::NeedsRepair
        );
        assert_eq!(
            classify_repair_state(true, true, None),
            CodexRepairState::Unknown
        );
    }

    #[test]
    fn bootstrap_is_pinned_and_verifies_download_before_expansion() {
        assert!(CODEX_REPAIR_BOOTSTRAP.contains(CODEX_REPAIR_RUNTIME_COMMIT));
        assert!(CODEX_REPAIR_BOOTSTRAP.contains(CODEX_REPAIR_RUNTIME_SHA256));
        assert!(CODEX_REPAIR_BOOTSTRAP.contains("Get-FileHash"));
        assert!(CODEX_REPAIR_BOOTSTRAP.contains("Copy-Item"));
        assert!(CODEX_REPAIR_BOOTSTRAP.contains(
            "[System.IO.File]::WriteAllText($runtimeVersionPath, $runtimeCommit, $utf8NoBom)"
        ));
        assert!(CODEX_REPAIR_BOOTSTRAP.contains("backups\\config"));
        assert!(CODEX_REPAIR_BOOTSTRAP.contains("$verifiedRunner"));
        assert!(CODEX_REPAIR_BOOTSTRAP.contains("icacls.exe"));
        assert!(CODEX_REPAIR_BOOTSTRAP.contains("Launching verified Fast Patch runner"));
        assert!(!CODEX_REPAIR_BOOTSTRAP.contains("-File $runner"));
    }

    #[test]
    fn runtime_requires_the_pinned_commit() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("skills").join("codex-windows-fast-patch");
        std::fs::create_dir_all(runtime.join("scripts")).unwrap();
        std::fs::write(runtime.join("scripts/repatch-codex-windows.ps1"), "exit 0").unwrap();

        std::fs::write(runtime.join(".skill-version"), "old-commit").unwrap();
        assert!(!runtime_installed(temp.path()));

        std::fs::write(runtime.join(".skill-version"), CODEX_REPAIR_RUNTIME_COMMIT).unwrap();
        assert!(runtime_installed(temp.path()));
    }

    #[test]
    fn elevation_command_embeds_the_script_and_waits_for_process_creation() {
        let command = elevated_repair_command("Write-Output 'repair'");

        assert!(command.contains("$ErrorActionPreference = 'Stop'"));
        assert!(command.contains("'-EncodedCommand'"));
        assert!(command.contains(&powershell_encoded_command("Write-Output 'repair'")));
        assert!(command.contains("-PassThru"));
        assert!(command.contains("-WindowStyle Normal"));
        assert!(!command.contains("-WindowStyle','Hidden"));
        assert!(!command.contains("codex-repair-bootstrap.ps1"));
    }

    #[test]
    fn bootstrap_persists_repair_progress_and_runner_output() {
        assert!(CODEX_REPAIR_BOOTSTRAP.contains("Write-RepairStatus 'running'"));
        assert!(CODEX_REPAIR_BOOTSTRAP.contains("Write-RepairStatus 'succeeded'"));
        assert!(CODEX_REPAIR_BOOTSTRAP.contains("Write-RepairStatus 'failed'"));
        assert!(CODEX_REPAIR_BOOTSTRAP.contains("Add-Content -LiteralPath $logPath"));
    }

    #[test]
    fn repair_status_reads_running_and_failed_markers() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        write_repair_status(home, "running", Some("starting")).unwrap();
        assert_eq!(read_repair_status(home), (true, None));

        write_repair_status(home, "failed", Some("runner failed")).unwrap();
        assert_eq!(
            read_repair_status(home),
            (false, Some("runner failed".to_string()))
        );
    }
}
