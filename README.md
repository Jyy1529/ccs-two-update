# CCS Two Update

CCS Two Update 是面向多种 AI 编程工具的本地配置与代理管理应用，重点增强 Codex 本地代理、审批模型路由、子代理角色路由、供应商重试与迁移，以及 Windows 端 Codex Desktop 修复能力。

本仓库用于保存项目源码、功能测试和版本归档，由 Jyy1529 独立维护。

## 项目信息

| 项目 | 内容 |
| --- | --- |
| 仓库 | [Jyy1529/ccs-two-update](https://github.com/Jyy1529/ccs-two-update) |
| 归档版本 | 3.18.0、3.19.0、3.19.2 |
| 桌面框架 | Tauri 2 + React + TypeScript + Rust |
| 当前推荐版本 | CCS Two Update 3.20.0 |
| 主要平台 | Windows、macOS、Linux；Codex Desktop 自动修复面向 Windows |

## 二次开发目标

本次改造围绕以下目标展开：

1. 提升第三方 Codex Provider 在审批请求、协议转换和模型不可用场景下的兼容性。
2. 为前端与后端子代理提供独立、可控的 Provider 和模型路由。
3. 在故障转移之前增加当前 Provider 内部的精细化自动重试。
4. 降低 Provider 配置迁移、复制和跨应用复用的操作成本。
5. 为 Windows Codex Desktop 增加健康检测和一键修复入口。
6. 补齐代理链路、配置边界和关键交互的自动化测试。

## 已完成的二次开发

### 1. Codex 审批模型路由

新增 Codex 审批请求识别和模型路由逻辑，支持三种模式：

- `native`：保持请求原始模型和原始行为。
- `auto`：审批请求优先使用 `codex-auto-review`。
- `fallback`：直接使用用户配置的回退模型。

自动模式会在 `codex-auto-review` 不可用时识别对应错误，并切换到配置的回退模型。相关逻辑接入本地代理转发、错误映射和请求重试链路，避免普通模型请求受到审批策略影响。

主要实现：

- `src-tauri/src/proxy/codex_auto_review.rs`
- `src-tauri/src/proxy/provider_retry.rs`
- `src/components/providers/forms/CodexFormFields.tsx`
- `tests/components/CodexFormFields.autoReview.test.tsx`

### 2. Codex 前端与后端子代理角色路由

新增 CC Switch 托管的 Codex Agent Role 配置：

- 前端角色可选择独立的 Codex Provider 和模型。
- 后端角色继续使用配置拥有者 Provider。
- 前端独立 Provider 先执行自身重试策略，耗尽后回退到拥有者 Provider。
- 路由过程保持全局当前 Provider 不变。
- 自动生成和维护 `cc-switch-frontend.toml`、`cc-switch-backend.toml`。
- 使用带签名的内部路由信息识别角色、配置拥有者和路由目标。
- 角色路由启用时要求 Codex 本地代理监听回环地址。
- 删除被引用 Provider、关闭依赖中的代理功能时提供阻断和错误说明。

主要实现：

- `src-tauri/src/services/codex_agent_roles.rs`
- `src/components/providers/forms/CodexAgentRoleRoutingConfig.tsx`
- `tests/components/CodexAgentRoleRoutingConfig.test.tsx`

### 3. Provider 自动重试策略

在普通本地代理模型请求中增加 Provider 级重试策略：

- 提供全局重试开关和单 Provider 开关。
- 支持限流、过载、服务端错误和网络错误分类。
- 支持自定义错误文案匹配。
- 支持 `1-100` 次有限重试。
- 启用重试时，`0` 表示持续重试，直至成功、错误不再匹配或请求取消。
- 支持 `1-60000` 毫秒重试间隔。
- 有限重试耗尽后继续进入原有故障转移流程。
- 审批模型请求使用独立策略，避免与普通请求规则相互干扰。

主要实现：

- `src-tauri/src/proxy/provider_retry.rs`
- `src/components/providers/forms/ProviderRetryPolicyConfig.tsx`
- `tests/components/ProviderForm.retryPolicy.test.tsx`
- `tests/components/ProviderRetryPolicyConfig.test.tsx`

### 4. Provider 配置迁移与复制

新增 Provider Transfer 工作流，用于预览和执行 Provider 配置迁移：

- 在操作前生成迁移预览。
- 复制可移植的 Provider 字段并为目标环境构建配置。
- 处理目标名称冲突和重复导入。
- 返回明确的迁移状态、结果和错误信息。
- 在 Provider 操作菜单中增加迁移入口。
- 增加删除失败和依赖关系错误的前端提示。

主要实现：

- `src-tauri/src/services/provider/transfer.rs`
- `src/components/providers/ProviderTransferDialog.tsx`
- `tests/components/ProviderTransferDialog.test.tsx`
- `tests/components/ProviderActions.transfer.test.tsx`
- `tests/lib/providerDeleteError.test.ts`

### 5. Windows Codex Desktop 检测与修复

新增 Codex Desktop 健康检测和修复入口：

- 检测 Codex Desktop 相关运行状态和修复状态。
- 在设置页面展示健康状态、异常信息和修复动作。
- 通过内置 PowerShell bootstrap 启动修复流程。
- 固定修复运行时提交和 SHA-256 校验值，减少运行时来源漂移。
- 支持静默检测开关和 Windows 无窗口执行。

主要实现：

- `src-tauri/src/services/codex_repair.rs`
- `src-tauri/src/commands/codex_repair.rs`
- `src-tauri/resources/codex_repair_bootstrap.ps1`
- `src/components/settings/CodexRepairSettings.tsx`
- `tests/components/CodexRepairSettings.test.tsx`

### 6. 本地代理和协议兼容性增强

围绕 Codex、Claude 和第三方中转 Provider 扩展代理链路：

- 加强 OpenAI Responses、Chat Completions 和 Anthropic Messages 的协议转换。
- 改进流式响应、SSE、内容编码和上游错误映射。
- 增加请求级覆盖配置、最大输出 Token、自定义 User-Agent 等高级选项。
- 扩展模型获取、模型上下文窗口元数据和模型名称转换。
- 调整 Provider 路由、故障转移和代理生命周期协作。
- 强化配置同步、导入导出、WebDAV 与 S3 同步边界处理。

主要实现集中在：

- `src-tauri/src/proxy/forwarder.rs`
- `src-tauri/src/proxy/handler_context.rs`
- `src-tauri/src/proxy/provider_router.rs`
- `src-tauri/src/proxy/server.rs`
- `src-tauri/src/services/proxy.rs`
- `src-tauri/resources/model_context_windows.json`

### 7. 界面、国际化与测试

前端同步增加对应设置和交互，并更新简体中文、繁体中文、英文和日文文案。新增或扩展的测试覆盖以下边界：

- 审批模型自动路由和回退。
- 子代理角色 Provider 路由。
- Provider 重试次数、延迟和触发条件校验。
- Provider 迁移预览和执行结果。
- Codex Desktop 修复状态展示。
- Provider 删除依赖错误。
- 高级配置折叠、全屏面板和代理设置交互。
- 应用初始化、设置对话框和 Provider 表单集成。

### 8. DeepSeek Harness 与 Pi Coding Agent

- 本地环境检查支持官方 `dsh` / `@deepseek-ai/dsh` 和 `pi` / `@earendil-works/pi-coding-agent` CLI。
- DeepSeek Harness 使用 `DSH_HOME` 或 `~/.dsh/settings.yaml`、`.credentials.yaml`。
- Pi 指 [`pi.dev`](https://pi.dev) coding agent，使用官方图标以及 `PI_CODING_AGENT_DIR` 或 `~/.pi/agent/models.json`、`settings.json`；要求 Node.js `>=22.19.0`。
- Pi 官方明确 `No MCP`，DeepSeek Harness 也没有旧版通用 `mcp.json` 契约，因此二者不显示 MCP 开关、不生成伪配置文件；Skills 同步仍受支持。
- Provider 投影会保留无关设置和已有密钥，遇到损坏的用户配置时拒绝覆盖。

v3.20.0 在上一版基础上补充 DeepSeek Harness 与 Pi Coding Agent 的完整管理能力，并完善多账号、重试、代理、同步和 Windows 发布流程。

## 关键配置行为

| 配置 | 行为 |
| --- | --- |
| `codexAutoReviewMode` | 控制审批请求使用原生、自动或回退模型 |
| `codexAutoReviewFallbackModel` | 配置审批模型不可用时使用的模型 |
| `localProxyRetryPolicy` | 配置单 Provider 的重试次数、延迟、错误类型和文案 |
| `providerRetryEnabled` | 控制 Provider 自动重试的全局开关 |
| `codexAgentRoleRouting` | 配置前端与后端子代理的 Provider 和模型路由 |
| `codexRepairDetectionEnabled` | 控制 Windows Codex Desktop 健康检测 |
| `enableFailoverToggle` | 控制主界面的独立故障转移入口 |

## 分支说明

| 分支 | 内容 |
| --- | --- |
| `main` | 当前 3.20.0 版本，也是 GitHub 默认分支 |
| `archive/v3.19-safe-2026-08-10` | 3.19.0 二次开发归档，用于版本追溯和升级比较 |
| `archive/auto-review-2026-08-10` | 早期自动审查开发快照，保留用于追溯和差异比较 |

## 安装包和安装顺序

仓库按开发演进顺序记录 Windows 发布包。安装包只包含正式 Release 中的 NSIS、MSI 和便携包；debug 可执行文件、Rust 构建辅助程序和旧的 3.17.0 包不属于发布资产。

| 顺序 | 版本 | 分支 | NSIS 安装程序 | MSI 安装程序 |
| ---: | --- | --- | --- | --- |
| 1 | 3.18.0 | `archive/auto-review-2026-08-10` | `src-tauri/target/release/bundle/nsis/CC Switch_3.18.0_x64-setup.exe` | `src-tauri/target/release/bundle/msi/CC Switch_3.18.0_x64_en-US.msi` |
| 2 | 3.19.0 | `archive/v3.19-safe-2026-08-10` | `src-tauri/target/release/bundle/nsis/CC Switch_3.19.0_x64-setup.exe` | `src-tauri/target/release/bundle/msi/CC Switch_3.19.0_x64_en-US.msi` |
| 3 | 3.19.2 | `main` | `src-tauri/target/release/bundle/nsis/CC Switch_3.19.2_x64-setup.exe` | `src-tauri/target/release/bundle/msi/CC Switch_3.19.2_x64_en-US.msi` |
| 4 | 3.19.3 | `main` | GitHub Release asset `CC-Switch-v3.19.3-Windows-Setup.exe` | GitHub Release asset `CC-Switch-v3.19.3-Windows.msi` |
| 5 | 3.19.4 | `main` | GitHub Release asset `CC-Switch-v3.19.4-Windows-Setup.exe` | GitHub Release asset `CC-Switch-v3.19.4-Windows.msi` |
| 6 | 3.20.0 | `main` | GitHub Release asset `CC-Switch-v3.20.0-Windows-Setup.exe` | GitHub Release asset `CC-Switch-v3.20.0-Windows.msi` |

从旧版本逐级升级时，按 `3.18.0 -> 3.19.0 -> 3.19.2 -> 3.19.3 -> 3.19.4 -> 3.20.0` 安装。新设备直接安装 `3.20.0` 即可。NSIS 适合常规交互式安装，MSI 适合企业部署和脚本化安装；同一版本选择其中一种安装包即可，便携包无需安装。

`3.20.0` 是本仓库的 Windows 手动发布：安装器未进行 Authenticode 签名，未生成 Tauri updater `.sig` 或 `latest.json`，应用内自动更新通道暂不可用。请从本仓库的 [GitHub Release](https://github.com/Jyy1529/ccs-two-update/releases/tag/v3.20.0) 手动下载，并使用 `SHA256SUMS.txt` 核对文件完整性。

## 开发环境

需要安装：

- Node.js
- pnpm
- Rust 工具链
- Tauri 2 所需系统依赖

安装依赖：

```bash
pnpm install
```

启动开发模式：

```bash
pnpm dev
```

构建应用：

```bash
pnpm build
```

## 验证命令

前端类型检查：

```bash
pnpm typecheck
```

前端格式检查：

```bash
pnpm format:check
```

前端单元测试：

```bash
pnpm test:unit
```

Rust 格式和静态检查：

```bash
cd src-tauri
cargo fmt --check
cargo clippy --all-targets --all-features
```

Rust 测试：

```bash
cd src-tauri
cargo test
```

## 使用和安全说明

- 使用 Provider 迁移、配置同步或 Codex 修复前，先备份本地配置。
- 子代理角色路由依赖 Codex 本地代理接管，监听地址建议保持为 `127.0.0.1` 或 `::1`。
- 无限重试会持续占用当前请求链路，应结合取消能力和上游服务状态使用。
- API Key、访问令牌、个人配置、SQLite 数据库和 `.tmp` 运行状态不应提交到仓库。
- `archive/auto-review-2026-08-10` 和 `archive/v3.19-safe-2026-08-10` 用于历史追溯，日常开发以 `main` 为准。

## 贡献者与许可证

- 当前项目维护者及贡献者：[@Jyy1529](https://github.com/Jyy1529)
- 许可证以仓库中的 [LICENSE](LICENSE) 为准。
- 第三方组件说明见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
