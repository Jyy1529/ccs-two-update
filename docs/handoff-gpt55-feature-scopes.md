# 交接说明书：Provider 功能范围设置 + DeepSeek/Pi 数据持久化（收尾）

> **执行者**：GPT-5.6
> **审核者**：Claude（完成后做 code review 与问题修复）
> **仓库**：`D:\AI_Projects\ccs-two-update`（Tauri 2 + React/TS + Rust，包管理 pnpm）
> **日期**：2026-08-15
> **原则**：本文档自包含。除本文档与仓库代码外，不依赖任何会话上下文。

---

## 1. 背景与总目标

本仓库是 CC Switch 3.19.2 的二次开发版。本轮迭代包含两个特性，**大部分已实现并通过 `cargo check`**，剩余收尾工作交由你完成：

**特性 A：DeepSeek / Pi 的 MCP 与 Skills 启用状态持久化**
此前 DeepSeek、Pi 两个新应用在 MCP/Skills 面板勾选后不落库（无数据库列）。现已添加 schema v17 迁移与 DAO 读写、以及 MCP 文件同步（写各自配置目录下独立的 `mcp.json`）。

**特性 B：三个 Provider 功能做成「开关 + 应用生效范围」**
- 本地代理自动重试（localProxyRetry）——通用功能，默认生效应用 `["claude", "codex"]`，全部应用可选。
- 前端/后端子代理角色路由（agentRoleRouting）——**Codex 专属实现**，默认且仅可选 `["codex"]`。
- 审批模型路由（autoReviewRouting）——**Codex 专属实现**，默认且仅可选 `["codex"]`。

行为定义：功能开关关闭、或当前应用不在 apps 列表内时——①对应配置块在该应用的 Provider 表单中**不渲染**；②后端代理链路对该应用**不应用**该功能。设置保存后角色路由立即 reconcile（关闭即清除已生成的 `cc-switch-frontend.toml`/`cc-switch-backend.toml`）。

---

## 2. 数据契约（Rust 与 TS 必须保持一致）

### 2.1 settings.json 新字段（camelCase 序列化）

```jsonc
{
  "providerFeatureScopes": {
    "localProxyRetry":  { "enabled": true, "apps": ["claude", "codex"] },
    "agentRoleRouting": { "enabled": true, "apps": ["codex"] },
    "autoReviewRouting":{ "enabled": true, "apps": ["codex"] }
  }
}
```

- 字段缺失（旧配置）时按上述默认值处理；`providerRetryEnabled=false`（旧全局开关）仍一票否决自动重试。
- Rust 侧定义在 `src-tauri/src/settings.rs`：`FeatureScope` / `ProviderFeatureScopes` / `AppSettings.provider_feature_scopes`，读取函数 `get_provider_feature_scopes()` / `provider_retry_allowed_for(&AppType)` / `agent_role_routing_allowed()` / `auto_review_routing_allowed()`。
- TS 侧定义在 `src/types.ts`（`FeatureScope` / `ProviderFeatureScopes`，`Settings.providerFeatureScopes?`），默认值与工具在 `src/lib/featureScopes.ts`（`DEFAULT_PROVIDER_FEATURE_SCOPES` / `resolveProviderFeatureScopes` / `featureScopeAllows` / `useProviderFeatureScopes`）。

### 2.2 数据库 schema v17

`src-tauri/src/database/`：`mod.rs` 的 `SCHEMA_VERSION = 17`；`schema.rs` 的 `migrate_v16_to_v17` 为 `mcp_servers` 与 `skills` 两表各加 `enabled_deepseek` / `enabled_pi`（`BOOLEAN NOT NULL DEFAULT 0`），CREATE TABLE 初始定义同步含列。

### 2.3 DeepSeek / Pi 的 MCP 文件

写入独立文件（不写 config.json，避免被 Provider 切换整体覆盖）：
- `~/.deepseek/mcp.json`、`~/.pi/mcp.json`
- 结构 `{"mcpServers": {"<id>": <统一 spec JSON>}}`，保留文件中其他顶层键。
- 实现在 `src-tauri/src/mcp/simple_json.rs`（含 2 个单测）。

---

## 3. 已完成清单（勿重复实现，审核时会 diff 对照）

### 后端（`cargo check` 已通过）

| 文件 | 内容 |
|---|---|
| `src-tauri/src/database/mod.rs` | SCHEMA_VERSION=17 |
| `src-tauri/src/database/schema.rs` | v16→v17 迁移 + CREATE TABLE 加列 + v15→v16 测试断言改为 `SCHEMA_VERSION` |
| `src-tauri/src/database/dao/mcp.rs` | SELECT 常量、row 映射、INSERT、`update_mcp_server_app_enabled` 列映射全部接入 deepseek/pi |
| `src-tauri/src/database/dao/skills.rs` | 两处 SELECT+row 映射（新列追加在末尾，索引 17/18）、`save_skill` INSERT、`update_skill_apps` UPDATE |
| `src-tauri/src/mcp/simple_json.rs`（新） | DeepSeek/Pi 的 mcp.json 读写 + 单测 |
| `src-tauri/src/mcp/mod.rs` | 模块声明与 4 个函数导出 |
| `src-tauri/src/services/mcp.rs` | `sync_server_to_app_no_config` / `remove_server_from_app` 接线 DeepSeek/Pi |
| `src-tauri/src/settings.rs` | `FeatureScope`/`ProviderFeatureScopes` 结构、默认值函数、`provider_feature_scopes` 字段（struct+default）、4 个读取函数 |
| `src-tauri/src/commands/settings.rs` | `merge_settings_for_save` 保留 None、保存后 `feature_scopes_changed` 时 await `reconcile_current_codex_agent_roles` |
| `src-tauri/src/proxy/handler_context.rs` | 重试开关改为 `provider_retry_allowed_for(&app_type)` |
| `src-tauri/src/proxy/codex_auto_review.rs` | `configured_mode` 在 `!auto_review_routing_allowed()` 时返回 `Native` |
| `src-tauri/src/services/codex_agent_roles.rs` | `reconcile_current_codex_agent_roles_locked` 开头与 `current_codex_role_route_requires_proxy` 加 `agent_role_routing_allowed()` gate |

### 前端

| 文件 | 内容 |
|---|---|
| `src/types.ts` | `FeatureScope` / `ProviderFeatureScopes` / `Settings.providerFeatureScopes?` |
| `src/lib/featureScopes.ts`（新） | 默认值 + resolve + allows + `useProviderFeatureScopes()`（基于 `useSettingsQuery`，加载中返回默认值） |
| `src/components/settings/ProviderFeatureScopeSettings.tsx`（新） | 设置页区块：三行卡片（图标+标题+描述+Switch+应用 chips），开启时若 apps 为空则回填默认 apps；角色路由/审批路由仅显示 codex chip + `codexOnlyHint` 说明 |
| `src/components/settings/SettingsPage.tsx` | 在 `AppVisibilitySettings` 之后挂载新组件（含 import） |
| `src/components/providers/forms/ProviderForm.tsx` | import featureScopes 工具；`supportsLocalProxyRetry` 改为 scope 判断；新增 `supportsAgentRoleRouting`（`appId==="codex" && allows`），传参处替换原 `appId === "codex"` 条件 |
| `src/components/providers/forms/CodexFormFields.tsx` | import featureScopes 工具；组件内 `supportsAutoReviewRouting`；审批路由块渲染条件由 `appId === "codex"` 改为该变量 |
| `src/components/providers/forms/GrokBuildProviderForm.tsx` | `retryAdvancedOptions` 改为 scope 判断（`featureScopeAllows(featureScopes.localProxyRetry, "grokbuild")`），**⚠️ import 尚未添加，当前 typecheck 必挂——见任务 T1** |

---

## 4. 待办任务（按顺序执行）

### T1【必做·修编译】GrokBuildProviderForm 补 import

`src/components/providers/forms/GrokBuildProviderForm.tsx` 顶部 import 区添加：

```ts
import {
  featureScopeAllows,
  useProviderFeatureScopes,
} from "@/lib/featureScopes";
```

组件体内已引用 `useProviderFeatureScopes()`（约 468 行处）。加完后 `pnpm typecheck` 应对该文件无错。

### T2【必做】i18n 文案（4 个语言文件）

`src/i18n/locales/{zh,zh-TW,en,ja}.json`，在顶层 `settings` 对象内新增 `featureScopes` 子对象（各语言翻译到位，不要留英文占位在中文文件里）：

```jsonc
"featureScopes": {
  "title": "…",                     // zh: "Provider 功能生效范围"
  "description": "…",               // zh: "控制以下功能启用与生效的应用；未勾选的应用不显示对应配置，也不参与该功能。"
  "codexOnlyHint": "…",             // zh: "当前仅 Codex 支持"
  "localProxyRetry": {
    "title": "…",                   // zh: "本地代理自动重试"
    "description": "…"              // zh: "本地代理请求失败时按 Provider 重试策略自动重试。"
  },
  "agentRoleRouting": {
    "title": "…",                   // zh: "前端/后端子代理模型路由"
    "description": "…"              // zh: "前端角色可使用独立 Codex Provider，后端角色跟随当前 Provider。"
  },
  "autoReviewRouting": {
    "title": "…",                   // zh: "审批模型路由"
    "description": "…"              // zh: "控制 Codex 审批请求使用原生、自动或回退模型。"
  }
}
```

注意：这些 key 已被 `ProviderFeatureScopeSettings.tsx` 引用（组件内含 defaultValue 兜底，但正式文案必须落到语言文件）。`apps.deepseek` / `apps.pi` 四语言已存在，勿重复添加。修改 JSON 后运行 `npx prettier --write "src/i18n/locales/*.json"`。

### T3【必做】前端测试适配

行为变化：**gemini 默认不再显示自动重试块**（旧逻辑 claude/codex/gemini 硬编码，新默认 scope 为 claude/codex）。

1. 运行 `npx vitest run tests/components/ProviderForm.retryPolicy.test.tsx tests/components/ProviderRetryPolicyConfig.test.tsx tests/components/GrokBuildProviderForm.test.tsx tests/components/CodexFormFields.autoReview.test.tsx tests/components/CodexAgentRoleRoutingConfig.test.tsx`。
2. 失败用例两类处理：
   - 若断言「gemini/grokbuild 表单显示重试块」→ 改为在测试的 MSW settings 数据中提供 `providerFeatureScopes`（把对应 app 加进 `localProxyRetry.apps`），或改断言为不显示——**优先前者**（保持测试覆盖原有 UI 行为）。MSW settings 状态在 `tests/msw/state.ts` 的 `settingsState`。
   - 若组件测试因缺少 QueryClientProvider 报错 → 查看同目录其他测试的 render wrapper 惯例（多数已有 wrapper）。`useProviderFeatureScopes` 在无数据时返回默认值，claude/codex 用例不应受影响。
3. 新增一个组件测试 `tests/components/ProviderFeatureScopeSettings.test.tsx`：
   - 渲染后三行功能卡片均存在；
   - 关闭「本地代理自动重试」开关 → `onChange` 收到 `providerFeatureScopes.localProxyRetry.enabled === false`；
   - 点击 claude chip 取消 → apps 不含 "claude"；
   - 角色路由行仅有 codex 一个 chip。
   参考同目录 `AppVisibilitySettings` 相关测试或 `McpFormModal.test.tsx` 的写法（vitest + @testing-library/react，i18n mock 返回 key）。

### T4【必做】Rust 测试与 schema 迁移测试

1. `src-tauri/src/database/schema.rs` 测试模块内仿照 `migrate_v14_to_v15` 的既有测试（文件约 3150-3210 行）新增 `migrate_v16_to_v17_adds_deepseek_pi_columns`：建表→`set_user_version(16)`→`apply_schema_migrations_on_conn`→断言 `has_column` 4 个新列 + 既有行新列值为 0。
2. `src-tauri/src/settings.rs` 测试模块新增：
   - 旧 JSON（无 `providerFeatureScopes`）反序列化后 `provider_feature_scopes` 为 `None`，`get` 类函数返回默认范围（claude/codex 重试允许、gemini 不允许、role/review 仅 codex）；
   - `FeatureScope::allows` 对 `enabled=false` 恒 false；
   - 序列化含 camelCase 键 `localProxyRetry`。
   注意测试隔离：`settings.rs` 既有测试用 `CC_SWITCH_TEST_HOME` 模式，纯结构测试直接构造 `AppSettings`/`serde_json` 即可，不要碰全局 `settings_store()`（`provider_retry_allowed_for` 这类读全局的函数用现有 TempHome 模式测试或跳过）。
3. 运行并确保通过：
   ```bash
   cd src-tauri
   cargo fmt
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --lib
   cargo test --test mcp_commands --test provider_service --test import_export_sync --test provider_commands
   ```

### T5【必做】全量验证

```bash
pnpm typecheck
pnpm format:check   # 不过则 npx prettier --write 后重跑
pnpm test:unit
```

### T6【可选·加分】MSW settings mock 补默认字段

`tests/msw/state.ts` 的 `settingsState` 增加 `providerFeatureScopes`（用第 2.1 节默认值），并在 `resetProviderState`/settings reset 处同步，避免未来测试踩 undefined。

---

## 5. 已知事实与坑（务必读完再动手）

1. **`model_pricing` 的 4 个 lib 测试失败是预先存在的环境问题**（改动前干净工作树同样失败）：`batch_update_and_delete_are_persisted_to_local_file` 等。**不要**试图修复，也不要计入你的失败清单。
2. `cargo fmt` 会重排我们改过的文件——先跑 fmt 再提交 diff。clippy 要求零 warning（CI 以 `-D warnings` 跑）。
3. 集成测试里 `McpApps` 字面量已统一 `..Default::default()` 兜底；新写测试沿用该风格，避免下次加应用再批量爆炸。
4. `dao/skills.rs` 的 SELECT 新列**特意追加在列表末尾**（索引 17/18），不要"顺手"重排列顺序——row.get 是按索引取的。
5. `useProviderFeatureScopes` 在 settings 未加载时返回默认值：claude/codex 的表单行为与旧版一致，测试无需为 loading 态做特殊处理。
6. `merge_settings_for_save` 已处理 `provider_feature_scopes: None` 时保留现值；前端 handleAutoSave 是浅合并 Partial<Settings>，新组件每次提交完整的三段 scopes（已实现），不要改成只传单段。
7. 角色路由/审批路由是 **Codex 专属**：不要给其他应用的表单塞这两个配置块，也不要在设置 UI 放出其他应用的 chip（诚实原则——选了没有任何效果的选项是欺骗用户）。
8. i18n 中文文件里避免使用「很关键/大概/基本」等口头语，保持与现有文案风格一致（简洁书面语）。
9. Windows 环境，shell 是 Git Bash；路径用正斜杠；`pnpm` 可用。

---

## 6. 交付物（提交给审核者）

1. 全部代码改动（不要求 commit，保留工作区即可）。
2. 一份简短的执行报告 `docs/handoff-gpt55-report.md`，包含：
   - 每个任务 T1–T6 的状态（完成/跳过+原因）；
   - 上述 4 组验证命令的**末尾摘要输出**（test result 行）粘贴；
   - 你新增/修改的测试清单；
   - 你发现并绕过/未解决的问题清单（如有）。

## 7. 审核标准（Claude 将按此验收）

- [ ] `pnpm typecheck`、`pnpm format:check`、`pnpm test:unit` 全绿（App.test.tsx 偶发超时可重跑单文件确认）
- [ ] `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings` 零输出
- [ ] `cargo test --lib` 除 model_pricing 预存在 4 例外全部通过；4 个集成测试文件通过
- [ ] schema v17 迁移测试存在且通过；升级路径（v16 库→v17）不丢已有行
- [ ] 四语言 i18n 完整、无英文占位混入非英文文件
- [ ] 功能语义抽查：默认设置下 gemini 表单无重试块、codex 表单有角色路由与审批路由块；关闭审批路由开关后 codex 表单不再显示审批块；`settings.json` 写入 camelCase 结构
- [ ] 未引入无关重构、未改动本文档第 3 节所列已完成代码的语义

---

*本文档由 Claude 生成于交接时点；第 3 节列出的实现均已在仓库工作区中，可直接 `git diff` 查看。*
