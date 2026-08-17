# GPT-5.5 Provider 功能范围与 3.19.3 发布前执行报告

> [!WARNING]
> 这是 v3.19.3 发布前的历史执行记录。v3.19.3 后续已经发布，其中记录的 DeepSeek/Pi 独立 `mcp.json` 方案已在 v3.19.4 按官方 CLI 契约纠正；不要将本文的待发布状态或旧 MCP 说明视为当前状态。

- 执行日期：2026-08-16（Asia/Shanghai）
- 仓库：`D:\AI_Projects\ccs-two-update`
- 分支：`main`
- 发布仓库：`https://github.com/Jyy1529/ccs-two-update`
- 依据：`docs/handoff-gpt55-feature-scopes.md`
- 目标版本：`3.19.3` / `v3.19.3`
- 发布状态：功能审查无阻塞项；Windows 资产已构建并完成只读校验，Git 提交、推送、tag 和 GitHub Release 仍待用户确认。

## T1-T6 状态

| 任务 | 状态 | 结果 |
| --- | --- | --- |
| T1 GrokBuild import | 完成 | 已接入 feature-scope gate 与 Provider 表单。 |
| T2 四语言 i18n | 完成 | `zh`、`zh-TW`、`en`、`ja` 均包含完整文案。 |
| T3 前端测试适配 | 完成 | 新增功能范围、DeepSeek/Pi Provider、Universal tab 与关闭范围回归测试。 |
| T4 Rust 测试与迁移 | 完成 | schema v16→v17、默认/禁用 scope、camelCase、live MCP 和角色路由 gate 均有测试。 |
| T5 全量验证 | 完成 | 受限并行 Vitest、Rust library、指定集成测试、typecheck、格式和 Clippy 均通过。 |
| T6 MSW 默认字段 | 完成 | 默认及 reset 状态包含完整 `providerFeatureScopes`。 |

## 最终验证摘要

### 前端

```text
pnpm typecheck                         exit 0
pnpm format:check                      exit 0
pnpm exec vitest run --maxWorkers=2 --minWorkers=1
Test Files  116 passed (116)
Tests       789 passed (789)
Duration    843.24s
```

受限 worker 的全量运行退出码为 0。此前不限制并行度的尝试出现 2 个 `App.test.tsx` 超时/DOM 污染；隔离重跑和本次受限并行全量均通过，判断为测试环境资源竞争，不是稳定产品回归。运行中仅有既有浏览器数据、Node punycode、MSW mock 和 Tauri window 警告。

### Rust

```text
cargo fmt --check                                      exit 0
cargo clippy --all-targets --all-features -- -D warnings  exit 0

cargo test --lib
running 2603 tests
test result: ok. 2598 passed; 0 failed; 5 ignored; 0 measured; 0 filtered out; finished in 29.42s
```

指定集成目标（在 `D:\AI_Projects\ccs-two-update\src-tauri` 执行）均通过：

```text
mcp_commands:       24 passed; 0 failed; finished in 17.46s
provider_service:   36 passed; 0 failed; finished in 23.54s
import_export_sync: 26 passed; 0 failed; finished in  3.63s
provider_commands:  10 passed; 0 failed; finished in  6.33s
合计：96 passed; 0 failed
```

## 本轮审查修复

除 T1-T6 外，功能审查发现并已修复：

1. DeepSeek/Pi Provider 表单不再回退到 Claude `env/config`、Claude presets 或 `CommonConfigEditor`，改用扁平 `baseUrl` / `apiKey` / `model` JSON，并保留未知字段。
2. DeepSeek/Pi 页面隐藏只支持 Claude/Codex/Gemini 的 Universal Provider 入口。
3. Codex Agent Role listener 使用 `IpAddr::is_loopback()` 严格校验，拒绝 wildcard、远端、端口拼接和伪造地址。
4. updater endpoint 改为 fork 自有占位地址，避免二次开发版从上游通道获取并覆盖；本次构建通过临时 config 禁用 updater artifacts。

## Windows 构建

使用隔离 target，避免覆盖 Git 跟踪的旧版 `src-tauri\target` 安装包：

```powershell
$env:CARGO_TARGET_DIR = 'D:\AI_Projects\ccs-two-update\src-tauri\target-package-3.19.3'
pnpm tauri build --bundles msi,nsis `
  --config '{"bundle":{"createUpdaterArtifacts":false}}' --ci --no-sign
```

构建退出码为 0，目标为 Windows `x86_64`。原始产物（保留用于复核）：

| 产物 | 大小 | SHA-256 |
| --- | ---: | --- |
| `src-tauri\target-package-3.19.3\release\bundle\nsis\CC Switch_3.19.3_x64-setup.exe` | 9,997,250 bytes | `383C7BA1EEEC5C8CFA87F44C154C9D8E55A13BA52009DB19BD5C7F7CFEA9BCFF` |
| `src-tauri\target-package-3.19.3\release\bundle\msi\CC Switch_3.19.3_x64_en-US.msi` | 13,541,376 bytes | `7FA4237014808EFDFC733DD7FCD538B82484DCF5361499777DA1A20232A5F559` |

只读安装包检查结果：

- MSI/NSIS `Get-AuthenticodeSignature` 均为 `NotSigned`；本次没有 Windows Authenticode 证书。
- NSIS EXE `FileVersion=3.19.3`、`ProductVersion=3.19.3`、`ProductName=CC Switch`。
- MSI 属性：`ProductName=CC Switch`、`Manufacturer=ccswitch`、`ProductVersion=3.19.3`、`ProductCode={E4BBED18-05B6-4A3C-8107-A2234A158C3A}`、`UpgradeCode={55F90CA4-617A-5350-A7CF-F6F24252866B}`。
- bundle 目录没有 `.sig` 或 `latest.json`。默认 `src-tauri\target` 中被 Git 跟踪的旧版 3.19.2 包未被覆盖。

发布资产已复制到仓库外目录：

```text
D:\AI_Projects\ccs-two-update-release\v3.19.3\
  CC-Switch-v3.19.3-Windows-Setup.exe
  CC-Switch-v3.19.3-Windows.msi
  SHA256SUMS.txt
  release-notes.md
```

复制后重新计算的 hash 与上表一致。

## 签名与 updater 边界

- `TAURI_SIGNING_PRIVATE_KEY` / password 未配置（`gh secret list` 为空），因此本次不生成 Tauri updater artifact、minisign `.sig` 或 `latest.json`。
- `tauri.conf.json` 的默认 `createUpdaterArtifacts` 仍为 `true`；本次仅用 `--config {"bundle":{"createUpdaterArtifacts":false}} --no-sign` 覆盖，不能视为永久关闭配置。
- endpoint 已改为 fork 的 `https://github.com/Jyy1529/ccs-two-update/releases/latest/download/latest.json`，避免查询上游。由于当前 fork 没有 manifest，应用内自动检查/手动“检查更新”按钮可能提示不可用；后续应建立 fork 自有签名通道或显式禁用 updater UX。

## 已知保留项（非本次发布阻塞）

- `import_from_all_apps` 仍固定导入 Claude、Codex、Gemini、Grok Build、OpenCode、Hermes；DeepSeek/Pi 的现有 `mcp.json` 尚未纳入全应用导入。handoff 只要求独立写入/清理，因此列为后续 Opportunity。
- 功能范围变化后的角色路由 reconcile 失败仍记录 warning，设置保存成功；运行时 gate 已立即保护行为，但文件可能短暂残留。
- 真实 MSI/NSIS 安装、升级、卸载和回滚未在本机执行。本机已有 HKCU NSIS 3.18.0、HKLM MSI 3.19.2 和 `D:\Code Switch\CC Switch`，直接安装会覆盖现有环境；应在 Windows Sandbox/VM 验证。
- 未创建自定义 `.test-artifacts`；隔离构建目录和仓库外发布目录按发布审计需要保留。不要删除默认 target 中的旧跟踪产物。

## GitHub 发布前提

当前 `HEAD` 与 `origin/main` 仍为 `e0c2e8e`，功能修改处于未提交工作树；因此尚未创建 `v3.19.3` tag 或 GitHub Release。必须先让 tag 指向包含本报告及源码修改的提交，再上传已核对资产，避免源码 tag 与安装包不一致。

待用户明确确认后，才执行基于最终 `git status --short` 生成的完整文件清单：

```powershell
git add <本次发布文件的明确完整清单>
git commit -m "release: CC Switch 3.19.3 fork"
git push origin main
git tag -a v3.19.3 -m "CC Switch 3.19.3 二次开发版"
git push origin v3.19.3
```

确认并完成上述 Git 操作后，再执行 `gh release create v3.19.3` 上传 MSI、NSIS 和 `SHA256SUMS.txt`，随后用 `gh release view` 检查资产、说明和 URL；最终 commit SHA、tag SHA、Release URL 及上传后 hash 复核结果以发布执行记录为准。

## 变更透明度

本轮最终受影响的源码、配置、测试和文档文件以 `git status --short` 为准；本轮未执行 `git add`、`git commit`、`git push`、`git tag` 或真实安装命令。构建/复制产物路径见上文，均未写入源码仓库的 Git 跟踪资产。
