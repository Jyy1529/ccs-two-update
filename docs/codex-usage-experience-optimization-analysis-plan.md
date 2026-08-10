# Codex 0.145.0 单代理、审批与压缩体验优化分析及计划

> - 文档类型：现状分析、推荐配置与实施计划
> - 分析对象：`C:\Users\JASOY\.codex\config.toml`
> - Codex 验证版本：`codex-cli 0.145.0`
> - 官方源码基线：`openai/codex` 的 `rust-v0.145.0` tag
> - 社区材料核对日期：2026-08-01
> - 计划等级：Standard
> - 就绪目标：operator-ready
> - 真值平面：live-codex
> - 状态：待用户实施

## 1. 目标与范围

本计划优化当前自定义 Provider 下的 Codex 使用体验，目标包括：

1. 保留 `approval_policy = "on-request"` 与 `approvals_reviewer = "auto_review"`，让需要审批的操作继续由内部 Guardian 自动审查。
2. 同时关闭 Codex V1、V2 多代理工具，并阻止模型元数据重新选择 V2。
3. 保留 `on-request` 审批策略与 `workspace-write` 沙箱边界。
4. 保留 FAST 默认状态和当前稳定的高思考强度默认值。
5. 让长会话优先使用远程压缩，让缺少远程压缩能力的自定义 Provider 使用更保真的本地压缩 Prompt。
6. 避免固定压缩阈值造成过早压缩、上下文浪费或模型切换后的阈值错配。

本文中的“严格单代理”指普通编码任务不开放 V1/V2 多代理工具。Guardian 自动审批审核属于独立的内部审批会话，继续保留。

本计划仅生成配置建议，不直接修改 `config.toml`。FAST 与完整思考强度在 CC Switch 模型目录和协议转换层的实现计划继续由以下专项文档承载：

```text
D:\codex对话\cc-switch-auto-review\docs\codex-custom-model-fast-reasoning-plan.md
```

### 1.1 非目标

- 不修改 CC Switch 源码。
- 不修改 Codex CLI 源码。
- 不关闭审批、安全检查、Guardian 自动审查或沙箱。
- 不引入新的代理框架、记忆框架或运行时依赖。
- 不把社区帖子中的推断直接当作 Codex 0.145.0 的官方契约。
- 不为生产配置固定 `model_auto_compact_token_limit = 175000`。

## 2. 当前状态

### 2.1 当前关键配置

本机配置当前包含：

```toml
model_provider = "custom"
model = "gpt-5.6"
approval_policy = "on-request"
approvals_reviewer = "auto_review"
model_reasoning_effort = "high"
service_tier = "fast"
sandbox_mode = "workspace-write"

[features]
personality = true
fast_mode = true
```

当前审批行为符合最新偏好：

- `approval_policy = "on-request"` 继续决定哪些操作进入审批流程。
- `approvals_reviewer = "auto_review"` 继续把审批请求交给内部 Guardian 审核会话。

当前待优化项为：

- `multi_agent` 当前处于 stable 且有效值为 `true`，配置中缺少 `[agents] enabled = false` 这一明确禁用覆盖。

### 2.2 当前 feature 状态

`codex features list` 在 0.145.0 中显示：

```text
enable_fanout                        removed            false
fast_mode                            stable             true
guardian_approval                    stable             true
multi_agent                          stable             true
multi_agent_mode                     removed            false
multi_agent_v2                       stable             false
remote_compaction_v2                 stable             true
```

这些状态说明：

- V1 多代理当前已启用。
- V2 当前已关闭，但模型元数据仍可能参与版本选择。
- `enable_fanout` 和 `multi_agent_mode` 已进入 removed 状态，只保留兼容解析。
- `remote_compaction_v2` 已是 stable 且当前有效值为 `true`。
- `fast_mode` 已是 stable 且当前有效值为 `true`。
- `guardian_approval` 提供内部自动审批审核能力，`approvals_reviewer = "auto_review"` 选择该审核路径。

## 3. 两篇社区材料的分析

### 3.1 长任务、保留推理与压缩

来源：

- [Tibo 所说提升 GPT 5.6 Sol 性能的两个选项是什么？](https://linux.do/t/topic/2685872/1)

帖子基于 OpenAI 的 ARC-AGI-3 文章整理了三个实践点：

- 使用 Responses API。
- 保留 reasoning 状态。
- 使用 compaction。

帖子对“跨多个上下文窗口工作”的解释与 Codex 长任务运行方式一致：同一个用户 turn 可以持续执行模型采样、工具调用和后续推理；上下文接近限制时执行 mid-turn compaction，压缩成功后继续当前 turn。大型任务因此可以经历多轮“工作窗口 -> 压缩 -> 继续工作”。

对当前配置最有价值的结论是：

1. 保留 Responses API 路径。自定义 Provider 需要支持相应请求和压缩能力，CC Switch 的协议转换层负责保持兼容。
2. 保留 reasoning。上游需要接受并正确处理 Codex 发送的 reasoning 字段。
3. 让模型目录提供默认压缩阈值。不同模型的上下文窗口、输出预算和安全余量不同，统一固定阈值会降低适配性。
4. ARC-AGI-3 文中出现的是 `175,000` 字符滚动截断条件，Codex 的 `model_auto_compact_token_limit` 字段使用 token 语义。两者单位不同，不能直接等值迁移。
5. `remote_compaction_v2` 在本机 0.145.0 已默认开启，重复写入配置只会增加未来维护成本。

生产配置建议保持以下字段缺省：

```toml
# 保持未配置
# model_auto_compact_token_limit = 175000
# model_auto_compact_token_limit_scope = "total"
```

### 3.2 本地压缩的状态失真问题

来源：

- [Codex 默认压缩 Prompt 到底有多坑？缓解 5.6 Sol 上下文本地压缩幻觉问题](https://linux.do/t/topic/2686152/)

帖子指出默认本地压缩 Prompt 容易产生以下状态失真：

- 当前需求与已废弃需求混合。
- assistant 提议被升级为用户已接受决策。
- 计划和预测被写成已完成事实。
- 精确路径、版本、数值、错误和验证状态因追求 concise 而丢失。
- 压缩后重复执行已经完成的步骤。

帖子提供的 faithful prompt 采用更严格的事实分层：

- 区分 current、superseded、rejected、tentative、failed 和 unresolved。
- 区分用户指令、assistant 提议、引用材料、直接观察、推断和验证结果。
- 保留重要的精确路径、版本、标识符、数值、数量、关系和状态变化。
- 禁止把提议、意图、预测或下一步建议写成已接受决策或已验证状态。
- 让摘要长度服从保真需求，取消固定 concise 目标。

该方案适合公益站、自定义中转和缺少远程压缩能力的 Provider。它比额外引入一套记忆或 harness 框架更轻量，也更适合当前单代理目标。

## 4. Codex 0.145.0 官方源码结论

### 4.1 审批接收方

`codex-rs/protocol/src/config_types.rs` 中的 `ApprovalsReviewer` 只有两个实际枚举值：

```text
user
auto_review
```

`guardian_subagent` 是 `auto_review` 的旧兼容别名。官方注释明确说明：

- 默认值为 `user`。
- `auto_review` 使用经过专门提示的内部子代理收集上下文并执行风险判断。

要保留当前自动审查行为，应使用：

```toml
approval_policy = "on-request"
approvals_reviewer = "auto_review"
```

两个字段承担不同职责：

| 字段                 | 职责                     | 推荐值        |
| -------------------- | ------------------------ | ------------- |
| `approval_policy`    | 决定哪些操作需要升级审批 | `on-request`  |
| `approvals_reviewer` | 决定升级后的审批交给谁   | `auto_review` |

该组合保留审批和沙箱安全边界，并由内部 Guardian 审核会话自动给出 allow、deny、timeout 或 abort 结果。`user` 仍可用于需要纯人工审批的配置，本计划采用 `auto_review`。

Guardian 审核会话与普通多代理工具相互独立。`build_guardian_review_session_config` 会为审批创建专用只读会话，并在该会话内主动关闭 `Feature::Collab` 和 `Feature::MultiAgentV2`。因此 `[agents] enabled = false` 可以关闭普通 V1/V2 委派工具，同时继续保留 `approvals_reviewer = "auto_review"` 的内部审批审核。

### 4.2 多代理版本选择优先级

`codex-rs/core/src/config/mod.rs` 的核心选择顺序为：

```rust
if features.multi_agent_v2 {
    MultiAgentVersion::V2
} else if !agents_enabled {
    MultiAgentVersion::Disabled
} else {
    model metadata or feature fallback
}
```

官方测试 `codex-rs/core/tests/suite/model_runtime_selectors.rs` 已覆盖以下行为：当远程模型元数据声明 V2，同时 `agents_enabled = false` 且 V2 feature 关闭时，请求工具列表中没有 V1、V2 namespace 或下列多代理工具：

```text
spawn_agent
send_message
wait_agent
list_agents
```

因此，严格单代理配置的关键开关为：

```toml
[agents]
enabled = false
```

同时关闭显式 V2 feature 可以封闭优先级最高的入口：

```toml
[features.multi_agent_v2]
enabled = false
```

关闭 V1 feature 可以封闭 feature fallback：

```toml
[features]
multi_agent = false
```

### 4.3 两个线程上限字段的语义

两个同名字段的计算方式不同：

| 路径                                                           | 语义                                                 | 设置为 `1` 的结果                      |
| -------------------------------------------------------------- | ---------------------------------------------------- | -------------------------------------- |
| `[features.multi_agent_v2].max_concurrent_threads_per_session` | V2 会话总并发线程数，包含根 Agent                    | `saturating_sub(1)` 后子代理容量为 `0` |
| `[agents].max_concurrent_threads_per_session`                  | spawned agent 线程上限；作为 V2 缺省来源时会先加 `1` | V1 意外启用时仍可能允许 `1` 个子代理   |

由此得到三层保护：

1. `[agents] enabled = false`：主要权限级禁用覆盖，阻止模型元数据重新启用多代理。
2. `[features] multi_agent = false` 与 `[features.multi_agent_v2] enabled = false`：关闭 V1/V2 feature 入口。
3. `[features.multi_agent_v2] max_concurrent_threads_per_session = 1`：V2 意外启用后的容量后备限制。

`[agents].max_concurrent_threads_per_session = 1` 可以保留为保守上限；它本身不构成零子代理保证。

### 4.4 `enable_fanout` 与提示文本

`enable_fanout = false` 在 0.145.0 中属于 removed/no-op 兼容键。推荐配置省略该字段，减少误导。

`multi_agent_mode_hint_text` 是模型可见提示文本。它可以表达使用偏好，无法移除工具或形成权限边界。V2 已关闭且 `[agents] enabled = false` 时，继续注入长提示只会增加提示词长度。推荐配置省略该字段，把约束交给运行时开关。

### 4.5 压缩 Prompt 的读取与作用范围

`codex-rs/config/src/config_toml.rs` 声明：

```rust
pub experimental_compact_prompt_file: Option<AbsolutePathBuf>
```

`codex-rs/core/src/config/mod.rs` 会读取非空文件，并按以下优先级形成 `compact_prompt`：

```text
运行时 compact_prompt override
    -> experimental_compact_prompt_file 的文件内容
    -> Codex 内置 SUMMARIZATION_PROMPT
```

`codex-rs/core/src/compact.rs` 的 inline/local auto-compaction 使用该 prompt。远程 compaction v2 使用专用 Responses 压缩请求，不读取这份文本。因此：

- 支持远程压缩的 Provider 继续使用远程 compaction v2。
- 缺少远程压缩能力的 Provider 在 inline/local 路径使用 faithful prompt。
- 配置该文件不会替换远程 compaction v2 的服务端压缩协议。
- 文件必须存在、非空并可读取；应使用绝对路径。

## 5. 推荐配置

### 5.1 严格单代理稳定配置

将以下字段合并到现有 `config.toml`。现有 `[features]` 表应直接增加 `multi_agent = false`，同一文件中不能创建第二个重复的 `[features]` 表。

```toml
model_provider = "custom"
model = "gpt-5.6"

approval_policy = "on-request"
approvals_reviewer = "auto_review"

model_reasoning_effort = "high"
service_tier = "fast"
sandbox_mode = "workspace-write"

[agents]
enabled = false
max_concurrent_threads_per_session = 1

[features]
personality = true
fast_mode = true
multi_agent = false

[features.multi_agent_v2]
enabled = false
max_concurrent_threads_per_session = 1
```

体验结果：

- 普通命令、读写和测试全部由根 Agent 独立完成。
- 需要审批的操作继续进入内部 Guardian 自动审查。
- FAST 保持默认开启。
- 日常默认思考强度保持 `high`。
- V1/V2 多代理工具从请求工具面移除。
- 模型元数据声明 V2 时仍由 `[agents] enabled = false` 生成 Disabled override。

### 5.2 单代理 + 本地压缩增强配置

在 5.1 的基础上新增：

```toml
experimental_compact_prompt_file = 'C:\Users\JASOY\.codex\prompts\generic-faithful-v2.md.txt'
```

TOML 单引号 literal string 可以直接容纳 Windows 反斜杠。使用双引号时需要写成：

```toml
experimental_compact_prompt_file = "C:\\Users\\JASOY\\.codex\\prompts\\generic-faithful-v2.md.txt"
```

推荐的 `generic-faithful-v2.md.txt` 内容为：

```text
Create a faithful context-compaction summary of the conversation for a model that will receive the summary instead of the discarded context.

Preserve all material information needed to understand the user's task and its current state, including objectives, requirements, constraints, corrections, decisions and their rationale, work completed or in progress, unfinished work, relevant evidence, failures, uncertainties, active external work, and surviving artifacts or references.

Reconcile earlier information with later evidence and corrections. Keep current, superseded, rejected, tentative, failed, and unresolved information distinct. Preserve the source and status of important claims, especially the distinction between user instructions, assistant proposals, quoted or relayed material, direct observations, inferences, and verified results.

Do not turn a proposal, intention, prediction, or suggested next step into an accepted decision, governing requirement, or verified current state. Preserve separately what was proposed, whether it was accepted, rejected, superseded, or left unanswered, and what work was actually started or completed. Do not label a next action as required unless that status is supported by the governing instructions and conversation state.

Preserve exact names, identifiers, paths, versions, values, counts, relationships, and state transitions when they materially affect understanding or future work. Do not invent facts, completion, decisions, authorization, verification, or certainty. Do not omit material information merely because it occurred early or in the middle of the conversation.

Avoid repetition, routine narration, raw logs, and detail that can be safely reconstructed without changing the interpretation of the task. Use as much detail as necessary for fidelity; there is no target length or required output structure.
```

该文件内容来自主题 2686152。实施时应保留来源链接，后续 Codex 升级后重新对照官方默认 prompt 与 compaction 实现。

### 5.3 保持缺省的字段

推荐让下列字段保持未配置：

```toml
# remote_compaction_v2 已是 stable/default true
# [features]
# remote_compaction_v2 = true

# 让模型目录决定压缩阈值
# model_auto_compact_token_limit = 175000
# model_auto_compact_token_limit_scope = "total"

# removed/no-op
# enable_fanout = false

# 运行时开关已经形成约束
# multi_agent_mode_hint_text = "..."
```

### 5.4 FAST 与思考强度建议

当前全局体验建议保持：

```toml
service_tier = "fast"
model_reasoning_effort = "high"

[features]
fast_mode = true
```

使用原则：

- `high` 适合作为日常默认值，兼顾响应时间、成本和复杂工程任务质量。
- 复杂架构、长链调试或高风险变更可在单次会话中切换到模型目录实际支持的更高强度。
- 自定义模型可见的完整思考强度集合由模型目录的 `supported_reasoning_levels` 决定。
- FAST 的真实生效还依赖模型目录的 service tier 声明和 CC Switch 对上游协议的正确透传或映射。
- 全局 `config.toml` 只能选择已暴露能力，无法单独补齐自定义模型目录缺失的能力元数据。

完整的 CC Switch 实现边界和测试要求见：

```text
docs/codex-custom-model-fast-reasoning-plan.md
```

## 6. 方案比较

| 方案                       | 审批子代理        | 多代理工具              | 压缩策略               | 维护成本 | 结论                     |
| -------------------------- | ----------------- | ----------------------- | ---------------------- | -------- | ------------------------ |
| 仅写提示文本禁止子代理     | 仍取决于 reviewer | 工具仍可能存在          | 不变                   | 中       | 提示层约束，强度不足     |
| 只关闭 `multi_agent`       | 取决于 reviewer   | 模型元数据仍可能选择 V2 | 不变                   | 低       | 缺少 V2 和模型元数据保护 |
| `[agents]` + V1/V2 双关闭  | 自动审查          | 工具面移除              | 保持默认               | 低       | 推荐的单代理基线         |
| 推荐基线 + faithful prompt | 自动审查          | 工具面移除              | 远程优先，本地保真增强 | 低       | 推荐的完整体验方案       |
| 引入外部记忆/harness       | 可配置            | 依赖框架                | 额外记忆链路           | 高       | 当前范围无需引入         |

## 7. 实施计划

### 7.1 成功标准

| AC  | 验收条件                            | 证据                                                                              |
| --- | ----------------------------------- | --------------------------------------------------------------------------------- |
| AC1 | 审批请求进入 Guardian 自动审查      | 新会话触发一次受控审批，请求或日志出现 Guardian assessment 与自动审查结果         |
| AC2 | 请求工具面无 V1/V2 多代理工具       | 捕获自定义 Provider 的首轮 Responses 请求并检查 `tools` 列表                      |
| AC3 | 模型元数据声明 V2 时仍保持 Disabled | 使用当前自定义模型完成首轮请求，工具列表无 collaboration namespace                |
| AC4 | FAST 默认保持开启                   | 请求中 service tier 与 CC Switch 上游映射符合专项计划定义                         |
| AC5 | 默认思考强度为 `high`               | 首轮请求的 reasoning effort 和 UI 回显一致                                        |
| AC6 | 远程压缩保持默认启用                | `codex features list` 显示 `remote_compaction_v2 stable true`                     |
| AC7 | 本地压缩使用 faithful prompt        | 不支持远程压缩的测试 Provider 完成一次受控 compaction，摘要保留状态分层和精确路径 |
| AC8 | 生产配置没有固定 175000 阈值        | 配置审查确认两个 auto-compact limit 字段缺省                                      |
| AC9 | 旧 FAST/思考强度计划保持完整        | 文件 SHA-256 与实施前记录一致                                                     |

### 7.2 阶段 A：建立基线

1. 记录 `codex --version` 和 `codex features list`。
2. 备份 `C:\Users\JASOY\.codex\config.toml` 到带时间戳的副本。
3. 记录当前有效模型、Provider、FAST、思考强度、审批策略和沙箱模式。
4. 记录当前首轮 Responses 请求中的工具列表。
5. 确认 CC Switch 当前 Provider 支持的协议和远程 compaction 能力。

检查点：基线文件和请求证据齐全后再改配置。

### 7.3 阶段 B：应用严格单代理配置

1. 保持 `approvals_reviewer = "auto_review"`。
2. 保持 `approval_policy = "on-request"`。
3. 新增 `[agents] enabled = false`。
4. 在现有 `[features]` 中新增 `multi_agent = false`。
5. 新增 `[features.multi_agent_v2] enabled = false`。
6. 为 V2 设置 `max_concurrent_threads_per_session = 1` 作为容量后备限制。
7. 保留 `service_tier = "fast"`、`fast_mode = true`、`model_reasoning_effort = "high"` 和 `sandbox_mode = "workspace-write"`。
8. 移除计划中的 `enable_fanout` 与长 `multi_agent_mode_hint_text`，避免无效配置和额外提示词。

检查点：解析配置并启动一个全新 Codex 会话；旧会话不作为配置生效证据。

### 7.4 阶段 C：增加本地压缩保真 Prompt

1. 新建 `C:\Users\JASOY\.codex\prompts\generic-faithful-v2.md.txt`。
2. 写入 5.2 节的完整 faithful prompt。
3. 在顶层配置增加 `experimental_compact_prompt_file` 绝对路径。
4. 保持 `remote_compaction_v2` 和两个 auto-compact limit 字段缺省。
5. 重新启动新会话，确认配置文件可读且无启动警告。

检查点：远程 Provider 和本地 compaction Provider 各执行一次定向验证。

### 7.5 阶段 D：运行时验收

1. 执行 `codex features list`，确认 V1 false、V2 false、remote compaction true、FAST true。
2. 捕获第一轮自定义 Provider 请求，检查多代理工具完全缺失。
3. 使用受控的越权请求触发一次审批，确认请求进入 Guardian 自动审查并产生结构化审核结果。
4. 检查同一首轮请求中的 service tier 和 reasoning effort。
5. 用一个专门测试会话触发本地压缩，检查摘要是否区分：当前需求、已废弃需求、提议、已完成工作、失败、未决事项。
6. 检查摘要是否保留测试用的精确路径、版本、数值和验证结果。
7. 执行一个长任务冒烟，确认 compaction 后继续同一任务且不重复已完成步骤。

检查点：AC1-AC8 全部有证据后进入日常使用。

### 7.6 阶段 E：升级后的复核

每次 Codex CLI 升级后执行：

1. 查看 `codex features list` 的 stage 和默认值变化。
2. 核对 `ApprovalsReviewer`、`AgentsToml`、`multi_agent_version_override` 和 compaction 代码路径。
3. 检查 removed/no-op 字段是否已经转为解析错误。
4. 捕获一次请求工具列表，验证多代理工具仍然缺失。
5. 复测 faithful prompt 的读取和本地 compaction 输出。
6. 对照新的模型目录确认 FAST 和思考强度能力。

## 8. 依赖关系

```text
A-1 基线与备份
  -> B-1 审批路由和单代理开关
  -> B-2 新会话配置解析验证

A-1
  -> C-1 faithful prompt 文件
  -> C-2 本地/远程压缩路径验证

B-2 + C-2
  -> D-1 请求工具面、审批、FAST、reasoning 验收
  -> D-2 长任务 compaction 冒烟
  -> operator-ready
```

## 9. Failure-Mode Forecast

| 风险                                  | 影响                               | 预防与验证                                                    |
| ------------------------------------- | ---------------------------------- | ------------------------------------------------------------- |
| 重复声明 `[features]`                 | TOML 解析失败或启动失败            | 把键合并进现有表，启动前做语法检查                            |
| 上层 managed config 显式启用 V2       | 本地用户配置可能被更高优先级覆盖   | 以实际请求工具列表作为最终证据                                |
| 只设置线程上限                        | V1 仍可能拥有一个子代理槽位        | 以 `agents.enabled=false` 和 V1/V2 feature false 作为主要开关 |
| 只写禁止委派提示                      | 模型仍看到多代理工具               | 检查请求 `tools`，不把自然语言提示当权限证明                  |
| 自定义 Provider 不支持远程 compaction | 长会话进入 inline/local 路径       | 配置 faithful prompt 并执行本地压缩测试                       |
| faithful prompt 文件路径错误或为空    | 配置加载失败或继续使用默认 prompt  | 使用绝对路径、非空文件并检查启动日志                          |
| 固定 175000 阈值                      | 过早压缩、上下文浪费、模型切换错配 | 生产配置保持阈值缺省                                          |
| 旧会话缓存旧配置                      | 验收结论失真                       | 每次配置变更后使用新会话验证                                  |
| 自定义模型目录缺少 FAST/思考能力声明  | UI 选项或请求字段缺失              | 按 FAST/思考强度专项计划验证模型目录与协议转换                |
| 本地压缩摘要仍发生状态升级            | 长任务继续方向错误                 | 用冲突需求和未接受提议构造回归场景                            |

## 10. Path Map

### 10.1 审批路径

```text
config.toml approvals_reviewer
  -> ApprovalsReviewer::AutoReview
  -> approval escalation
  -> dedicated GuardianReviewSession
  -> read-only + Collab/MultiAgentV2 disabled
  -> allow/deny/timeout/abort
  -> 受控审批冒烟与 assessment 日志验证
```

### 10.2 多代理路径

```text
config.toml [agents]/[features]
  -> Config::multi_agent_version_override
  -> model runtime selector
  -> Responses request tools
  -> CC Switch 请求捕获
  -> 无 V1/V2 namespace 和 spawn/send/wait/list 工具
```

### 10.3 压缩路径

```text
Provider remote-compaction capability
  -> remote compaction v2 或 inline/local compaction
  -> remote Responses compaction / compact_prompt
  -> replacement history
  -> 同一 turn 继续执行
  -> 长任务状态保真验证
```

### 10.4 FAST 与思考强度路径

```text
config.toml 默认值 + 自定义模型目录能力
  -> Codex request service_tier/reasoning.effort
  -> CC Switch 协议转换
  -> 自定义上游
  -> 请求捕获与模型行为验证
```

## 11. Evidence Ledger

| Claim ID | Readiness target | Truth plane      | Ref/path                                  | Evidence                                                                        | Result       | Date       | Owner | Residual risk                  |
| -------- | ---------------- | ---------------- | ----------------------------------------- | ------------------------------------------------------------------------------- | ------------ | ---------- | ----- | ------------------------------ |
| E1       | docs-truth-ready | source-repo      | `guardian/review.rs`、`review_session.rs` | `on-request + auto_review` 路由到专用 Guardian 会话，审核会话主动关闭 Collab/V2 | VERIFIED     | 2026-08-01 | Codex | 后续版本可能调整 Guardian 实现 |
| E2       | docs-truth-ready | source-repo      | `config/mod.rs`                           | V2 feature > agents disabled > model metadata 的选择顺序                        | VERIFIED     | 2026-08-01 | Codex | managed config 可能改变有效值  |
| E3       | docs-truth-ready | source-repo      | `model_runtime_selectors.rs`              | agents disabled 后请求工具面无多代理工具                                        | VERIFIED     | 2026-08-01 | Codex | 本机运行仍需抓包验证           |
| E4       | docs-truth-ready | live-codex       | `codex features list`                     | V1 true、V2 false、remote compaction true、FAST true                            | VERIFIED     | 2026-08-01 | Codex | 升级后默认值可能变化           |
| E5       | docs-truth-ready | source-repo      | `compact.rs` / `config/mod.rs`            | 文件 prompt 用于 inline/local compaction                                        | VERIFIED     | 2026-08-01 | Codex | Provider 能力决定实际路径      |
| E6       | docs-truth-ready | external-blocked | Linux.do 2685872                          | 帖子正文与长 turn/压缩分析                                                      | VERIFIED     | 2026-08-01 | Codex | 社区分析不构成官方契约         |
| E7       | docs-truth-ready | external-blocked | Linux.do 2686152                          | faithful prompt 与使用方式                                                      | VERIFIED     | 2026-08-01 | Codex | 输出质量仍需本机回归           |
| E8       | operator-ready   | live-codex       | `config.toml` + 请求捕获                  | 用户应用配置后的运行时结果                                                      | NOT_VERIFIED | 2026-08-01 | 用户  | 本计划未修改用户配置           |

## 12. 回滚计划

1. 关闭所有使用旧配置的 Codex 会话。
2. 用阶段 A 的备份恢复 `config.toml`。
3. 启动全新会话并检查 `codex features list`。
4. 如仅需回滚本地压缩增强，删除 `experimental_compact_prompt_file` 配置行即可；Prompt 文件可以保留作为审计材料。
5. 如需临时改为纯人工审批，设置 `approvals_reviewer = "user"`；恢复自动审查时改回 `auto_review`。
6. 如需重新启用多代理，先移除 `[agents] enabled = false`，再明确选择 V1 或 V2；启用后重新评估线程上限和 token 成本。

## 13. 最终建议

采用 5.2 节的“严格单代理 + 本地压缩增强”配置：

- `approval_policy = "on-request"` 与 `approvals_reviewer = "auto_review"` 保留当前自动审批审核行为。
- `[agents] enabled = false` 提供模型元数据之上的 Disabled override。
- V1/V2 feature 双关闭封闭显式入口。
- V2 总线程上限 `1` 提供容量后备限制。
- `service_tier = "fast"` 与 `model_reasoning_effort = "high"` 保持当前高效默认体验。
- `remote_compaction_v2` 和 auto-compact token limit 保持缺省，跟随 Codex 0.145.0 稳定默认值和模型目录。
- faithful prompt 只增强 inline/local compaction 路径，改善自定义中转的长会话状态保真。

最终验收以运行时请求工具列表、审批界面、压缩输出和 CC Switch 请求捕获为准。

## 14. 参考资料

- [Linux.do 主题 2685872](https://linux.do/t/topic/2685872/1)
- [Linux.do 主题 2686152](https://linux.do/t/topic/2686152/)
- [OpenAI Codex GitHub 仓库](https://github.com/openai/codex)
- `openai/codex@rust-v0.145.0/codex-rs/protocol/src/config_types.rs`
- `openai/codex@rust-v0.145.0/codex-rs/config/src/config_toml.rs`
- `openai/codex@rust-v0.145.0/codex-rs/core/src/config/mod.rs`
- `openai/codex@rust-v0.145.0/codex-rs/core/src/compact.rs`
- `openai/codex@rust-v0.145.0/codex-rs/core/tests/suite/model_runtime_selectors.rs`
