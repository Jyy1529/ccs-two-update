# CC Switch Codex 前端子代理独立 Provider 路由

> 文档类型：已实施架构、配置说明、使用说明与验证台账  
> 工作区：`D:\codex对话\cc-switch-auto-review`  
> Codex 客户端契约：`codex-cli 0.145.0`，源码 commit `25af12f7e61572b0bc18ddb1008be543b91519b0`  
> 文档更新：2026-08-02  
> 当前真值面：源码、定向测试与隔离 Codex 0.145.0 角色发现/spawn 已验证；真实 CC Switch B→A 出站路由、全量测试及安装包验证待最终门禁完成

---

## 1. 功能目标

CC Switch 为两个固定 Codex Agent Role 提供 Provider 级模型路由：

```text
根 Agent / cc-switch-backend
  → 当前 Codex Provider A

cc-switch-frontend
  → 前端专用 Codex Provider B
  → Provider B 自己的自动重试
  → 可重试错误耗尽后回退 Provider A
  → Provider A 自己的默认模型与自动重试
```

该架构同时保留以下行为：

- 根 Agent、后端角色和普通请求继续使用当前 Provider A。
- 前端角色可选择另一个现有 Codex Provider B，并自由填写实际上游模型。
- B 或 A 成功都保持 CC Switch 全局当前 Provider、托盘状态和 live config 指向 A。
- `codex-auto-review` 继续使用既有 Native、Auto、Fallback、回退模型和模型不可用处理。
- Provider 自动重试、Codex Repair、Provider Transfer、媒体降级、thinking/budget 整流继续按原实现工作。
- `model_catalog_json`、bundled catalog、Codex Common Config 和 `default_subagent_model` 保持原样。

旧文档采用的“修改全局 `model_catalog_json` 并设置 `default_subagent_model`”路线已经废止。当前实现使用 Agent Role 原生 `model_provider` 覆盖和 CC Switch 内部定向路由。

---

## 2. 用户入口与使用方法

### 2.1 配置入口

1. 在 CC Switch 中进入 Codex Provider A 的新增或编辑表单。
2. 展开“高级选项”。
3. 展开默认折叠的“前端/后端子代理模型路由”。
4. 打开“启用 Agent Role 路由”。
5. 配置前端角色：
   - Provider：跟随当前 Provider A，或选择另一个 Codex Provider B。
   - “新增 Provider”：在保留 A 表单状态的同时打开嵌套 Codex Provider 新增表单；保存后自动选中 B。
   - 前端上游模型：跟随 B 默认模型、从候选中选择，或自由输入任意上游模型 ID。
   - Codex 能力模型：默认留空并跟随主 Agent；也可显式填写共享 Catalog 中的已知模型。
   - reasoning effort：默认跟随主 Agent，可显式选择 `none`、`minimal`、`low`、`medium`、`high`、`xhigh`、`max` 或 `ultra`。
6. 配置后端角色：
   - Provider 固定为配置拥有者 A。
   - 模型与 reasoning effort 均可跟随主 Agent或显式覆盖。
7. 保存 Provider A。

角色路由启用后，CC Switch 自动启用 Codex 本地代理接管。新增或更新成功后，界面显示“代理已自动启用”和“新建 Codex 任务或重启 Codex”的提示。保存失败时保留表单，不显示成功提示。

### 2.2 在 Codex 中使用

创建新 Codex 任务或重启 Codex，使角色索引发现托管文件，然后按任务选择角色：

```text
前端实现、React、样式、交互、视觉验收
  → spawn_agent(agent_type="cc-switch-frontend")

后端、Rust、代理、数据库、协议处理
  → spawn_agent(agent_type="cc-switch-backend")
```

V1 使用 `fork_context=false`。V2 使用 `fork_turns="none"` 或正整数。完整历史 fork 的 Codex 既有角色限制保持不变。

---

## 3. ProviderMeta 契约

配置保存在现有 `providers.meta` JSON，不增加数据库列，不增加公开 Tauri command。

```ts
type CodexAgentReasoningEffort =
  | "none"
  | "minimal"
  | "low"
  | "medium"
  | "high"
  | "xhigh"
  | "max"
  | "ultra";

interface CodexAgentRoleOverride {
  model?: string;
  reasoningEffort?: CodexAgentReasoningEffort;
}

interface CodexFrontendAgentRoleOverride extends CodexAgentRoleOverride {
  providerId?: string;
  upstreamModel?: string;
}

interface CodexAgentRoleRouting {
  enabled?: boolean;
  frontend?: CodexFrontendAgentRoleOverride;
  backend?: CodexAgentRoleOverride;
}

interface ProviderMeta {
  codexAgentRoleRouting?: CodexAgentRoleRouting;
}
```

兼容规则：

- 旧 Provider 缺少 `codexAgentRoleRouting` 时按关闭处理。
- `enabled` 缺失或值为 `false` 时按关闭处理。
- 空字符串在保存和投影阶段规范化为缺省值。
- 外部导入或数据库恢复形成的失效 `providerId` 继续完整读取；UI 回显失效项，运行时回退当前可用 Provider。

---

## 4. Codex 能力模型与实际上游模型

前端角色将“Codex 客户端能力判断”和“Provider 实际收到的模型”分离：

### 4.1 Codex 能力模型

```text
frontend.model
  → 缺省时继承父 Agent 当前模型
```

Codex 使用该模型的共享 Catalog 元数据判断工具、reasoning、FAST、verbosity 和模态能力。留空能够最大限度继承主 Agent 的完整能力配置。

显式能力模型未出现在父任务 ThreadManager 的共享 Catalog 时仍可保存，UI 显示 fallback 模型元数据警告。角色专属 Catalog 在 Codex 0.145.0 中不独立生效，因此当前实现继续复用父任务共享 Catalog。

### 4.2 Provider B 上游模型

```text
frontend.upstreamModel
  → B settingsConfig.model
  → B config.toml 顶层 model
  → B modelCatalog 第一项
  → 前端 Codex 能力模型/请求模型
```

`frontend.upstreamModel` 只在 CC Switch 转发前改写，可填写任意 Provider B 支持的模型 ID，不参与 Codex 客户端能力解析。

### 4.3 Provider A 回退模型

```text
A settingsConfig.model
  → A config.toml 顶层 model
  → A modelCatalog 第一项
  → 前端 Codex 能力模型/请求模型
```

模型 override 在 Provider 映射、默认模型选择、Responses→Chat/Anthropic 转换和请求 body override 之后仍保持最高优先级，确保 Native Responses、Chat Completions 与 Anthropic 上游收到同一个目标模型。

---

## 5. 托管 Agent Role 文件

CC Switch 只管理两个固定文件：

```text
<CODEX_HOME>\agents\cc-switch-frontend.toml
<CODEX_HOME>\agents\cc-switch-backend.toml
```

每个文件带所有权标记：

```toml
# cc-switch-managed: codex-agent-role-v1
```

同名文件缺少该标记时，CC Switch 拒绝覆盖并返回冲突路径。目录、符号链接、损坏链接及其他非普通文件同样拒绝处理。

前端角色在启用独立路由时包含专属本地 Provider：

```toml
name = "cc-switch-frontend"
model_provider = "cc-switch-frontend-local"

[model_providers.cc-switch-frontend-local]
name = "CC Switch Frontend Route"
base_url = "http://127.0.0.1:<port>/v1"
wire_api = "responses"
supports_websockets = false
request_max_retries = 0
stream_max_retries = 0

[model_providers.cc-switch-frontend-local.http_headers]
x-cc-switch-role-route = "frontend"
x-cc-switch-role-owner = "<Provider A UUID>"
```

`request_max_retries = 0` 与 `stream_max_retries = 0` 阻止 Codex 客户端再次重放完整的 B→A 链。Provider 内重试由 CC Switch 的 Provider 级自动重试独立负责。

`frontend.model` 或 reasoning 缺省时，角色文件省略对应键，使 Codex 原生继承父 Agent。后端文件省略独立 `model_provider`，只投影可选的模型和 reasoning override。

角色文件成对快照、写入和回滚。关闭配置时，CC Switch 将自有文件移出 `.toml` 自动发现范围并保留内容：

```text
cc-switch-frontend.toml.disabled
cc-switch-backend.toml.disabled
```

---

## 6. 官方 Provider 的动态认证

前端角色文件的 `requires_openai_auth` 根据固定 B→A 链动态生成：

- B 或回退 A 包含内置 Codex 官方 Provider时写入 `true`，使 Codex 客户端提供原生 ChatGPT Authorization。
- B 与 A 均为第三方 Provider时写入 `false`，避免引入官方登录依赖。
- 配置的 B 已失效时，按实际回退 A 判断认证要求。

代理对内置 Codex 官方 Provider执行以下边界：

- 原生 Authorization 透传到固定官方上游。
- 缺少 Authorization 时返回明确的登录提示。
- 收到接管占位符 `PROXY_MANAGED` 时提示新建任务或重启 Codex。
- 官方 Provider 的认证失败、HTTP 401 和 403 归类为不可重试，避免静默切换账户或污染健康状态。
- 第三方 Provider继续使用各自配置的 API Key/OAuth 认证策略。

---

## 7. 本地代理定向路由

角色请求携带内部 Header：

```text
x-cc-switch-role-route: frontend
x-cc-switch-role-owner: <Provider A UUID>
```

代理只在 Codex Chat、Responses 和 Responses Compact 入口解析该 Header。解析成功后构造一次性计划：

```rust
struct ProviderRoutePlan {
    attempts: Vec<ProviderRouteAttempt>,
    use_failover_timeouts: bool,
    sync_logical_target: bool,
    bypass_single_provider_circuit_breaker: bool,
}

struct ProviderRouteAttempt {
    provider: Provider,
    outbound_model_override: Option<String>,
}
```

固定路由规则：

```text
B 有效且 B != A  → [B, A]
B 缺失或失效     → [A]
B == A            → [A]
owner A 失效      → [当前 Codex Provider]
owner 路由已关闭  → 普通 Provider 路由
```

角色链独立于全局故障转移开关和全局 Provider 队列：

- 全局故障转移关闭时仍可执行 B→A。
- 全局队列中的 C、D 不参与角色请求。
- B 仅在现有可重试错误分类下进入 A。
- 普通不可重试 400、明确客户端错误和客户端取消直接返回，A 不发送请求。
- B→A 使用现有非流式、首包和流空闲超时；配置值 `0` 继续表示关闭对应超时。

`sync_logical_target=false` 使 B/A 的实际请求统计、用量和健康度正常归因，同时保持：

- 当前 Provider A 不变。
- 托盘和 UI 当前状态不变。
- live config 不切换到 B。
- `current_providers`、`active_targets` 和 `failover_count` 不变。

主请求成功、媒体降级成功、thinking 签名整流成功和 thinking budget 整流成功使用同一逻辑成功收尾规则。

---

## 8. Provider 自动重试与回退

B 和 A 分别读取自身 `localProxyRetryPolicy`：

```text
B attempt
  → B 自己的最大次数、固定间隔和触发条件
  → 中间同 Provider 重试不记熔断失败
  → 最终耗尽后只记一次 B 健康失败
  → 错误可重试时进入 A

A attempt
  → A 自己的最大次数、固定间隔和触发条件
  → 成功后保持逻辑当前 Provider=A
```

HTTP 429、503、配置的 5xx/网络错误、首包失败和输出前流读取失败可按既有分类触发。已输出正文或工具调用后继续原流，不重放完整请求。

`codex-auto-review` 在角色 Header 解析和 Provider 重试策略入口之前排除。审批请求即使携带角色 Header，也继续进入现有审批路由。

---

## 9. Header 安全边界

内部控制 Header 执行严格校验：

- route 与 owner 必须成对出现。
- 重复 Header、逗号合并多值、空值、非 UTF-8、未知 role 和非法编码返回本地 `400 InvalidRequest`。
- 普通无角色 Header 请求继续原路由。
- `x-openai-subagent: collab_spawn` 只表示普通子代理，本功能不依赖该 Header。

两个内部 Header 在所有上游协议前无条件移除，并加入 `localProxyRequestOverrides` 的保护名单。Provider Header override 无法恢复或伪造内部控制 Header。

---

## 10. Provider 生命周期与保护

角色文件同步覆盖：

- 新建或更新当前 Codex Provider。
- 普通 Provider 切换、托盘切换、深链启用和 Profile 切换。
- 本地代理启动、热切换、端口或配置变化。
- Codex takeover 开启。
- CC Switch 启动自愈和 Codex 配置目录变化。
- 普通自动故障转移热切换。
- 应用退出清理。

投影失败时恢复上一组有效角色文件，并恢复可恢复的代理运行/接管状态。角色路由有效时，代理设置页阻止直接停止 Codex 代理或关闭 takeover，提示先关闭角色路由。

删除与复制规则：

- owner A 自身启用角色路由时阻止直接删除。
- B 被任何前端角色引用时阻止删除，并列出引用它的 Provider 和角色。
- Universal Provider 删除或取消 Codex 子 Provider时执行同一引用检查。
- Universal Codex 重同步保留现有 `codexAgentRoleRouting`。
- Provider Transfer 不复制角色路由。
- Provider Duplicate 深拷贝角色设置，新 Provider 自身成为新的配置拥有者。

官方和第三方 Codex Provider均支持。Claude、Gemini、Grok Build、OpenCode、OpenClaw、Hermes 的应用表单保持原样。通过 Responses/Chat 兼容接口暴露的 Gemini 模型可放在 Codex Provider中使用；Gemini Native `generateContent` 协议桥属于后续范围。

---

## 11. 配置保持边界

本功能直接复用父任务 ThreadManager 的共享模型 Catalog，并保持以下配置原样：

- Provider 的 `settingsConfig.modelCatalog`。
- live `model_catalog_json` 指针。
- bundled model catalog。
- Codex Common Config。
- `[agents] default_subagent_model` 及其他全局子代理默认值。
- Guardian / `codex-auto-review` 配置。
- FAST、reasoning、verbosity、工具和协议字段的现有生成与转换逻辑。

当前实现只选择已有 Codex Provider并改写每个 attempt 的上游模型，不为第三方模型推断新的 FAST 或能力元数据。

---

## 12. 源码验证状态

### 12.1 已验证：source-repo / task-worktree

截至 2026-08-02，当前工作树已有以下证据：

```text
cargo check --tests
  → PASS

pnpm typecheck
  → PASS（角色路由核心实现后的既有记录；最终集成仍需重跑）

pnpm exec vitest run \
  tests/components/AddProviderDialog.test.tsx \
  tests/components/EditProviderDialog.test.tsx
  → 2 files passed, 13 tests passed
```

Rust 定向测试已覆盖：

- ProviderMeta 序列化、缺字段兼容和 reasoning wire value。
- 托管文件生成、父模型继承、显式模型/reasoning、所有权冲突和文件回滚。
- 官方链动态 `requires_openai_auth` 与第三方链关闭认证。
- Header 严格校验、私有 Header 剥离和 override 保护。
- B→A 固定计划、B 缺失、B=A、owner 缺失和 owner 路由关闭。
- Chat/Anthropic/Responses 模型 override。
- B/A 独立 Provider 重试预算。
- owner/B 删除保护。
- `codex-auto-review` 排除 Provider 自动重试。

前端定向测试已覆盖：

- 角色卡片默认折叠、默认关闭和非 Codex 表单隔离。
- B 候选 Provider与模型、自定义上游模型、未知能力模型警告和失效 B 回显。
- 嵌套新增 B 时保留 A 表单并自动选中。
- 新增/更新启用路由后显示代理与重启提示。
- Provider 开关关闭、非 Codex 和保存失败时不显示提示；保存失败保留表单。

### 12.2 待验证：integration-worktree

最终集成需要重新运行：

```text
cargo test
pnpm test:unit
pnpm typecheck
cargo fmt --check --manifest-path src-tauri/Cargo.toml
pnpm format:check
git diff --check
```

这些命令通过前，文档状态保持“源码定向验证”，不声明全量回归完成。

### 12.3 已验证：隔离 Codex 0.145.0 角色发现

隔离目录：

```text
D:\codex对话\cc-switch-auto-review\.tmp\codex-role-smoke
```

已完成的真实 Codex 0.145.0 证据：

1. Codex 自动发现 `cc-switch-frontend` 与 `cc-switch-backend`。
2. `spawn_agent(agent_type="cc-switch-frontend", fork_turns="none")` 成功创建并执行子代理。
3. 角色级 `model_provider` 生效，请求到达 `/v1/responses`。
4. 捕获请求包含 `x-cc-switch-role-route=frontend` 与 `x-cc-switch-role-owner=provider-a`。
5. 角色文件的 `developer_instructions` 生效。
6. `fork_turns="none"` 未传播父任务历史。
7. 当 `requires_openai_auth=false` 时，捕获请求未携带 `Authorization`。

请求捕获证据：

```text
D:\codex对话\cc-switch-auto-review\.tmp\codex-role-smoke\captured-frontend-0.json
```

### 12.4 待验证：live CC Switch 定向路由

需要在隔离环境或明确授权的真实 Codex 配置目录执行：

1. 根 Agent 和 backend 的出站 Provider为 A。
2. frontend 的出站 Provider为 B，实际上游模型符合配置。
3. B high-demand/503/429/network 按 B 的预算重试，耗尽后使用 A 默认模型。
4. B 普通不可重试 400 时 A 零请求。
5. B 或 A 为内置官方 Provider时，原生 ChatGPT Authorization 正常到达官方上游。
6. B/A 成功后 CC Switch 全局当前 Provider始终为 A。
7. 内部 Header 未到达任何上游。
8. `codex-auto-review` 携带角色 Header 时仍走审批路由。
9. `model_catalog_json`、bundled catalog、Common Config 和 `default_subagent_model` 实施前后无差异。

### 12.5 待验证：package-artifact

全量验证通过后执行：

```text
pnpm tauri build
```

随后核验 EXE/MSI/NSIS 产物路径、版本、SHA-256、卸载规则、隔离目录启动、角色路由 UI 与真实角色投影。本工作树当前清单版本为 `3.18.0`；安装包版本以最终构建清单和产物元数据为准。

---

## 13. 运行日志验收

最终 live smoke 的日志应同时记录：

```text
role=frontend
owner_provider=A
target_provider=B 或 A
capability_model=<父模型或显式 frontend.model>
outbound_model=<B/A 实际模型>
provider_retry=<当前次数>/<上限>
route_fallback=<B→A 结果>
sync_logical_target=false
```

日志中的健康度、请求统计和用量归因到实际 B/A；UI 当前 Provider、托盘、live config 和全局故障转移计数保持 A。

---

## 14. 验收清单

```text
[x] ProviderMeta 前后端契约与旧数据关闭语义
[x] Codex 高级选项中的默认折叠角色路由卡片
[x] 前端 Provider B 选择、嵌套新增、模型候选与自由输入
[x] 前端能力模型和前后端 reasoning override
[x] 固定 cc-switch-frontend / cc-switch-backend 文件投影
[x] 所有权保护、成对写入、禁用改名和失败回滚
[x] B→A 定向链、模型 override 与逻辑 Provider 状态隔离
[x] B/A 独立 Provider 自动重试
[x] codex-auto-review 审批排除
[x] 内部 Header 校验、剥离和 override 保护
[x] Provider 删除与代理关闭保护
[x] 新增/编辑成功提示和失败边界测试
[ ] 三套全量测试与格式门禁
[x] 隔离 Codex 0.145.0 角色发现、spawn、角色 Provider/Header 与 `fork_turns="none"` 证据
[ ] 真实 CC Switch B→A 出站路由与重试回退证据
[ ] 官方 Provider 原生认证 live 证据
[ ] MSI/便携包构建、哈希和隔离启动证据
```
