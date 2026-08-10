# CC Switch 自定义 Codex 模型 FAST 与完整思考强度支持

> 文档类型：现状分析与实施计划  
> 目标代码库：`D:\codex对话\cc-switch-v3.19-safe`  
> 分析基线：CC Switch v3.19 安全集成工作区  
> Codex 验证版本：`codex-cli 0.145.0`  
> 官方源码依据：`openai/codex@66ebeb703710ae9e298b62d88d709b67777c91b4`  
> 计划等级：Standard  
> 就绪目标：operator-ready  
> 状态：待实施

## 1. 背景与目标

用户通过 CC Switch 配置 Codex 自定义 Provider 和自定义模型后，希望获得与官方 Codex 模型一致的两类能力：

1. 模型默认使用 FAST 模式，并允许用户在 Codex 中切换服务等级。
2. Codex 模型菜单展示完整思考强度，并根据所选 Provider 协议正确透传或映射。

FAST 的请求语义是向兼容上游发送：

```json
{
  "service_tier": "priority"
}
```

思考强度由 Codex 模型目录中的 `supported_reasoning_levels` 和 `default_reasoning_level` 控制，最终请求使用 `reasoning.effort`，再由 CC Switch 根据上游协议直接转发或转换。

### 1.1 计划目标

- 为每个自定义 Codex 模型独立声明 FAST 能力。
- 为每个自定义 Codex 模型独立声明 FAST 默认状态。
- 为每个自定义 Codex 模型独立声明支持的思考强度。
- 为每个自定义 Codex 模型设置默认思考强度。
- 保持旧 Provider 配置行为兼容。
- 保持现有 Codex OAuth FAST、审批模型、自动重试和故障转移功能不变。
- 根据 `openai_responses`、`openai_chat`、`anthropic` 三种协议执行真实且可验证的能力映射。

### 1.2 非目标

- 不修改数据库 schema。
- 不新增 Tauri command。
- 不改变普通 Provider 自动重试策略。
- 不改变 `codex-auto-review` Native、Auto、Fallback 和回退模型流程。
- 不为不支持 `service_tier` 的上游强制注入 FAST。
- 不通过修改 CC Switch 模型目录尝试绕过 Codex 官方的 `ultra -> max` 请求映射。
- 不在本轮引入新的 npm、Cargo 或运行时依赖。

## 2. 当前实现分析

### 2.1 自定义模型目录主动移除了 FAST

当前自定义模型目录生成逻辑位于：

```text
D:\codex对话\cc-switch-v3.19-safe\src-tauri\src\codex_config.rs
```

`codex_catalog_model_entry` 会固定写入：

```rust
entry_obj.insert("additional_speed_tiers".to_string(), json!([]));
entry_obj.insert("service_tiers".to_string(), json!([]));
```

这会覆盖模板原有的 FAST 能力。生成后的自定义模型没有：

- `additional_speed_tiers: ["fast"]`
- `service_tiers[].id: "priority"`
- `default_service_tier: "priority"`

因此 Codex 无法从模型目录判断自定义模型支持 FAST，也无法根据模型目录默认使用 FAST。

### 2.2 现有 `codexFastMode` 仅用于 Codex OAuth

现有 Provider 元数据字段：

```ts
interface ProviderMeta {
  codexFastMode?: boolean;
}
```

Rust 位置：

```text
D:\codex对话\cc-switch-v3.19-safe\src-tauri\src\provider.rs
```

前端位置：

```text
D:\codex对话\cc-switch-v3.19-safe\src\types.ts
D:\codex对话\cc-switch-v3.19-safe\src\components\providers\forms\ProviderForm.tsx
```

当前保存规则：

```ts
codexFastMode: isCodexOauthProvider ? codexFastMode : undefined;
```

普通自定义 Provider 保存时会删除 `codexFastMode`。请求转换也仅在 `is_codex_oauth && codex_fast_mode` 时注入：

```rust
result["service_tier"] = json!("priority");
```

现有字段继续承担 Codex OAuth 请求转换职责。自定义 Codex 模型的 FAST 能力应归属于模型目录，避免把 OAuth 请求开关和模型能力声明耦合在一起。

### 2.3 当前思考强度不完整

原生 Responses 模板：

```text
D:\codex对话\cc-switch-v3.19-safe\src-tauri\src\resources\codex_native_responses_template.json
```

当前只包含：

```text
none
high
```

Proxy Chat 模板：

```text
D:\codex对话\cc-switch-v3.19-safe\src-tauri\src\resources\gpt5_5_template.json
```

当前包含：

```text
low
medium
high
xhigh
```

自定义模型目录克隆上述模板后，没有提供每个模型独立覆盖 `supported_reasoning_levels` 和 `default_reasoning_level` 的能力。

### 2.4 模型映射已具备正确的持久化边界

自定义模型当前存储在：

```text
settingsConfig.modelCatalog.models[]
```

对应 TypeScript 类型：

```ts
export interface CodexCatalogModel {
  model: string;
  displayName?: string;
  contextWindow?: string | number;
  supportsParallelToolCalls?: boolean;
  inputModalities?: string[];
  baseInstructions?: string;
}
```

该 JSON 结构已经支持前端保存、Provider 编辑回显和 Rust 端解析。FAST 与思考能力字段可以直接扩展此结构，无需新增数据库列。

## 3. Codex 官方行为分析

### 3.1 FAST 模型目录字段

Codex 当前模型目录支持：

```json
{
  "additional_speed_tiers": ["fast"],
  "service_tiers": [
    {
      "id": "priority",
      "name": "Fast",
      "description": "Lower latency; upstream usage or pricing may increase"
    }
  ],
  "default_service_tier": "priority"
}
```

字段语义：

- `additional_speed_tiers`：旧版 Codex 兼容字段。
- `service_tiers`：新版 Codex 的服务等级能力目录。
- `default_service_tier`：用户没有显式选择服务等级时使用的模型默认值。
- FAST 的实际请求值为 `priority`。
- Codex 配置中的 `fast` 会在配置解析时归一化为 `priority`。
- `default` 是用户显式选择标准路由的哨兵值，此时模型目录默认 FAST 不再覆盖用户选择。

官方来源：

- `codex-rs/protocol/src/config_types.rs`
- `codex-rs/protocol/src/openai_models.rs`
- `codex-rs/tui/src/service_tier_resolution.rs`
- `codex-rs/tui/src/chatwidget/service_tiers.rs`

### 3.2 FAST 功能开关

官方 Codex 的 `Feature::FastMode` 当前为稳定功能，默认启用：

```rust
FeatureSpec {
    id: Feature::FastMode,
    key: "fast_mode",
    stage: Stage::Stable,
    default_enabled: true,
}
```

CC Switch 无需修改用户的 `[features] fast_mode` 设置。用户显式关闭该功能时，Codex 应继续尊重用户配置。

### 3.3 FAST 状态展示边界

Codex 的模型服务等级命令和有效服务等级由模型目录驱动。当前 TUI 的 FAST 状态徽标额外检查 `has_chatgpt_account`。

因此 API-key-only 场景可能出现：

- 模型目录和请求已使用 `priority`。
- FAST 状态徽标仍未显示。

该限制属于 Codex 客户端 UI。CC Switch 可以正确生成能力目录并发送请求；徽标展示需要由官方 Codex 或自定义 Codex 构建调整。

### 3.4 官方思考强度

当前官方 `ReasoningEffort` 支持：

```text
none
minimal
low
medium
high
xhigh
max
ultra
Custom(String)
```

官方 Codex 在实际发送请求前执行：

```rust
ReasoningEffortConfig::Ultra => ReasoningEffortConfig::Max
```

结果：

- 模型菜单可以展示 `ultra`。
- `ultra` 的最终请求值是 `max`。
- 当前官方 Codex 存在七个唯一标准请求值。
- 上游需要原始 `ultra` 时，需要修改 Codex 客户端，或引入自定义别名和代理翻译方案。

## 4. 推荐架构

### 4.1 能力归属于每个模型

同一 Provider 可以包含多个模型，各模型的 FAST、思考强度、上下文和工具能力可能不同。能力应存放在 `modelCatalog.models[]` 的每个模型对象中。

推荐 TypeScript 接口：

```ts
export interface CodexCatalogModel {
  model: string;
  displayName?: string;
  contextWindow?: string | number;

  fastModeSupported?: boolean;
  fastModeDefault?: boolean;

  supportedReasoningLevels?: string[];
  defaultReasoningLevel?: string;

  supportsParallelToolCalls?: boolean;
  inputModalities?: string[];
  baseInstructions?: string;
}
```

推荐 Rust 结构：

```rust
struct CodexCatalogModelSpec {
    model: String,
    display_name: String,
    context_window: u64,

    fast_mode_supported: Option<bool>,
    fast_mode_default: Option<bool>,
    supported_reasoning_levels: Option<Vec<String>>,
    default_reasoning_level: Option<String>,

    supports_parallel_tool_calls: Option<bool>,
    input_modalities: Option<Vec<String>>,
    base_instructions: Option<String>,
}
```

### 4.2 字段兼容规则

- 旧模型缺少 `fastModeSupported`：保持当前 FAST 关闭行为。
- 旧模型缺少 `fastModeDefault`：保持当前 FAST 默认关闭行为。
- `fastModeDefault=true`：运行时归一化为 `fastModeSupported=true`。
- 旧模型缺少 `supportedReasoningLevels`：继续继承当前模板档位。
- 旧模型缺少 `defaultReasoningLevel`：继续继承当前模板默认值。
- `supportedReasoningLevels=[]`：按显式空配置处理，建议前端禁止保存该状态，后端回退模板值。
- `defaultReasoningLevel` 必须存在于 `supportedReasoningLevels`。
- 思考档位清理首尾空白、移除空值、忽略大小写去重并保持稳定顺序。
- Rust 使用 `Vec<String>` 保留 Codex 的 `Custom(String)` 扩展能力。

### 4.3 FAST 生成规则

#### FAST 不支持

```json
{
  "additional_speed_tiers": [],
  "service_tiers": []
}
```

同时移除模板中可能存在的 `default_service_tier`。

#### FAST 支持，默认标准模式

```json
{
  "additional_speed_tiers": ["fast"],
  "service_tiers": [
    {
      "id": "priority",
      "name": "Fast",
      "description": "Lower latency; upstream usage or pricing may increase"
    }
  ]
}
```

#### FAST 支持且默认启用

```json
{
  "additional_speed_tiers": ["fast"],
  "service_tiers": [
    {
      "id": "priority",
      "name": "Fast",
      "description": "Lower latency; upstream usage or pricing may increase"
    }
  ],
  "default_service_tier": "priority"
}
```

### 4.4 完整思考强度生成规则

推荐完整预设：

```json
{
  "supported_reasoning_levels": [
    { "effort": "none", "description": "Disable reasoning" },
    { "effort": "minimal", "description": "Minimal reasoning" },
    { "effort": "low", "description": "Low reasoning" },
    { "effort": "medium", "description": "Medium reasoning" },
    { "effort": "high", "description": "High reasoning" },
    { "effort": "xhigh", "description": "Extra high reasoning" },
    { "effort": "max", "description": "Maximum reasoning" },
    { "effort": "ultra", "description": "Ultra reasoning; sent as max by current Codex" }
  ],
  "default_reasoning_level": "high"
}
```

档位顺序固定为：

```text
none -> minimal -> low -> medium -> high -> xhigh -> max -> ultra
```

未知自定义值可以追加在标准档位之后。

## 5. 协议兼容矩阵

| Codex Provider API 格式 | FAST 行为 | 思考强度行为 | 默认建议 |
| --- | --- | --- | --- |
| `openai_responses` | 直接发送 `service_tier: "priority"` | 直接发送 `reasoning.effort`，`ultra` 由 Codex 转成 `max` | 确认上游支持 `priority` 后允许默认 FAST；可选择全部档位 |
| `openai_chat` | 当前转换层会保留 `service_tier` | 使用 `CodexChatReasoningConfig` 转成顶层或对象字段 | FAST 默认关闭；按网关能力开启；思考档位按映射配置 |
| `anthropic` | 当前转换层移除 OpenAI `service_tier` | 转为 adaptive thinking、`output_config.effort` 或 token budget | FAST 关闭；思考档位通过 Anthropic 映射提供 |

### 5.1 OpenAI Responses

该路径最接近 Codex 原生协议：

- `service_tier` 可以直接进入上游请求。
- `reasoning.effort` 可以直接进入上游请求。
- 上游严格校验字段时，只能声明其真实支持的 FAST 和思考档位。
- 上游不支持 `priority` 时，默认 FAST 会导致 HTTP 400。

### 5.2 OpenAI Chat

当前 `transform_codex_chat.rs` 的透传字段包含：

```rust
"service_tier"
```

因此兼容 OpenAI Chat 上游可以收到 `service_tier: "priority"`。

当前思考档位映射：

| 映射模式 | 输入 | 输出 |
| --- | --- | --- |
| passthrough | `minimal/low/medium/high/xhigh/max` | 原样发送 |
| OpenRouter | `max` | `xhigh` |
| OpenRouter | `minimal/low/medium/high/xhigh` | 原样发送 |
| OpenRouter | `none` | `reasoning: { "effort": "none" }` |
| DeepSeek | `xhigh/max` | `max` |
| DeepSeek | 其余开启档位 | `high` |
| low_high | `minimal/low` | `low` |
| low_high | 其余开启档位 | `high` |

Kimi、GLM、Qwen、MiniMax、MiMo 等仅支持思考开关的模型会把多个 Codex 档位压缩为“开启思考”。模型目录可以展示完整档位，但实际语义仍由上游能力决定。

### 5.3 Anthropic

当前 `transform_codex_anthropic.rs` 映射：

| Codex 档位 | Anthropic 普通 thinking | Anthropic adaptive thinking |
| --- | --- | --- |
| `none` | `thinking.type = "disabled"` | 关闭或按不可关闭模型降为 `low` |
| `minimal` | `budget_tokens = 2048` | `output_config.effort = "low"` |
| `low` | `budget_tokens = 2048` | `output_config.effort = "low"` |
| `medium` | `budget_tokens = 8192` | `output_config.effort = "medium"` |
| `high` | `budget_tokens = 16384` | `output_config.effort = "high"` |
| `xhigh` | `budget_tokens = 24576` | `output_config.effort = "max"` |
| `max` | `budget_tokens = 24576` | `output_config.effort = "max"` |
| `ultra` | Codex 先映射为 `max` | Codex 先映射为 `max` |

Anthropic 原生请求当前会移除 OpenAI `service_tier`。本轮不为 Anthropic 引入新的服务等级字段翻译。

## 6. UI 设计

### 6.1 配置入口

在 Codex 自定义 Provider 的“模型映射”每一行增加可折叠的“Codex 模型能力”。

建议控件：

- Toggle：`支持 FAST 模式`
- Toggle：`默认启用 FAST`
- 多选复选框：八个标准思考强度
- 快捷按钮：`选择全部`
- Select：`默认思考强度`
- 状态提示：当前 API 格式的 FAST 和思考映射结果

### 6.2 交互规则

- 开启“默认启用 FAST”时自动开启“支持 FAST 模式”。
- 关闭“支持 FAST 模式”时保留默认开关值也容易形成无效状态，UI 应同步关闭默认开关。
- 默认思考强度下拉只显示已勾选档位。
- 取消当前默认档位时，默认值优先切换到 `high`，然后按 `medium -> low -> none -> xhigh -> max -> ultra -> minimal` 寻找可用值。
- `ultra` 下显示说明：“当前 Codex 会作为 max 发送”。
- `anthropic` 格式下显示说明：“FAST 不参与 Anthropic 请求；思考强度将转换为预算或 adaptive effort”。
- `openai_chat` 格式下显示说明：“FAST 仅对接受 service_tier 的上游生效”。
- `openai_responses` 格式下显示说明：“上游需接受 priority 和已选择的 reasoning effort”。

### 6.3 新模型默认值

推荐按协议初始化：

#### `openai_responses`

```text
支持 FAST：开启
默认 FAST：开启
思考档位：全部
默认思考：high
```

该默认值直接满足目标，同时在 UI 中明确要求上游兼容 `priority`。

#### `openai_chat`

```text
支持 FAST：关闭
默认 FAST：关闭
思考档位：根据现有自动推断填充
默认思考：high 或模板默认值
```

用户确认网关支持 `service_tier` 后可以手动开启。

#### `anthropic`

```text
支持 FAST：关闭
默认 FAST：关闭
思考档位：全部
默认思考：high
```

完整档位通过现有 Anthropic 映射实现。

## 7. 实施范围

### 7.1 前端类型与保存归一化

修改：

```text
D:\codex对话\cc-switch-v3.19-safe\src\types.ts
D:\codex对话\cc-switch-v3.19-safe\src\components\providers\forms\ProviderForm.tsx
```

工作内容：

- 扩展 `CodexCatalogModel`。
- 扩展 `normalizeCodexCatalogModelsForSave`。
- 清理和去重思考档位。
- 校验默认思考档位。
- 归一化 FAST 开关组合。
- 保持隐藏能力字段在编辑往返中不丢失。

### 7.2 模型映射 UI

修改：

```text
D:\codex对话\cc-switch-v3.19-safe\src\components\providers\forms\CodexFormFields.tsx
```

工作内容：

- `createCatalogRow` 携带新字段。
- `catalogRowsMatchModels` 比较新字段。
- 模型行增加折叠能力区域。
- 根据 API 格式初始化新模型默认值。
- 模型拉取后保留或初始化能力字段。
- 增加本地校验和协议提示。

### 7.3 Rust 模型目录生成

修改：

```text
D:\codex对话\cc-switch-v3.19-safe\src-tauri\src\codex_config.rs
```

工作内容：

- 扩展 `CodexCatalogModelSpec`。
- 解析 camelCase 和 snake_case 两种字段名。
- 增加 FAST 归一化辅助函数。
- 增加 reasoning levels 归一化辅助函数。
- 在 `codex_catalog_model_entry` 中按模型能力写入字段。
- 缺少新字段时保持当前模板行为。
- 显式关闭 FAST 时清除模板继承的 `default_service_tier`。

### 7.4 本地化

修改：

```text
D:\codex对话\cc-switch-v3.19-safe\src\i18n\locales\zh.json
D:\codex对话\cc-switch-v3.19-safe\src\i18n\locales\zh-TW.json
D:\codex对话\cc-switch-v3.19-safe\src\i18n\locales\en.json
D:\codex对话\cc-switch-v3.19-safe\src\i18n\locales\ja.json
```

新增文案覆盖：

- 模型能力标题。
- FAST 支持与默认开关。
- 全部思考档位。
- 默认思考强度。
- 三种协议说明。
- `ultra -> max` 说明。
- FAST 兼容性校验提示。

### 7.5 请求转换测试

可能只需增加测试，生产转换代码预计保持原样：

```text
D:\codex对话\cc-switch-v3.19-safe\src-tauri\src\proxy\providers\transform_codex_chat.rs
D:\codex对话\cc-switch-v3.19-safe\src-tauri\src\proxy\providers\transform_codex_anthropic.rs
```

验证现有行为：

- Chat 保留 `service_tier`。
- Anthropic 移除 `service_tier`。
- 完整思考强度正确压缩或映射。

## 8. 实施切片

### C-1：锁定模型能力数据契约

目标：前后端以同一字段表达 FAST 和 reasoning 能力。

工作：

- 扩展 TypeScript 类型。
- 扩展 Rust Spec。
- 定义兼容和归一化规则。
- 为 camelCase/snake_case 解析增加测试。

验证：

- 旧模型 JSON 可以正常解析。
- 新字段保存后完整回显。
- 无效默认思考档位被归一化。

### C-2：生成正确的 Codex 模型目录

目标：Codex 能从 `model-catalogs.json` 读取 FAST 和全部思考强度。

工作：

- 实现三种 FAST 生成状态。
- 实现 reasoning levels 覆盖。
- 保持模板未覆盖字段。
- 移除显式关闭 FAST 模型上的模板遗留字段。

验证：

- FAST 默认开启模型包含 `default_service_tier: "priority"`。
- FAST 可选模型包含 tier 且没有默认 tier。
- FAST 关闭模型的速度字段为空。
- 全部思考档位按固定顺序生成。

### C-3：增加模型行能力配置 UI

目标：用户可以在 CC Switch 中看到并控制模型能力。

工作：

- 增加折叠区和控件。
- 增加 API 格式提示。
- 增加默认值和编辑回显。
- 保持模型拉取、添加、删除和排序行为。

验证：

- 新增 Responses 模型默认 FAST 开启、全部 reasoning、默认 high。
- 编辑旧 Provider 时维持原行为。
- FAST 默认开关与支持开关始终处于合法组合。

### C-4：协议回归与端到端验证

目标：模型目录声明与代理实际请求行为一致。

工作：

- Responses 请求检查。
- Chat 透传和 reasoning 映射检查。
- Anthropic FAST 隔离和 reasoning 预算检查。
- Codex CLI 实际模型菜单和请求日志检查。

验证：

- 默认 FAST 模型首次会话发送 `priority`。
- 用户切换标准模式后停止发送 `priority`。
- 所有可见思考档位均产生预期请求或映射。

## 9. 依赖关系

```text
C-1 数据契约
  -> C-2 模型目录生成
  -> C-3 配置 UI

C-2 + C-3
  -> C-4 协议回归与端到端验证

C-4
  -> 全量测试与打包前验收
```

`C-2` 和 `C-3` 在 `C-1` 契约锁定后可以并行实施。`CodexFormFields.tsx` 和 `ProviderForm.tsx` 属于共享高冲突文件，集成时需要由同一负责人统一检查。

## 10. Path Map

```text
CC Switch Provider 表单
  -> settingsConfig.modelCatalog.models[]
  -> ProviderForm 保存归一化
  -> Rust codex_catalog_model_specs 解析
  -> codex_catalog_model_entry 生成 ModelInfo JSON
  -> Codex model-catalogs.json
  -> Codex TUI 模型/FAST/reasoning 选择
  -> Codex Responses 请求
  -> CC Switch forwarder
  -> Responses 透传 / Chat 转换 / Anthropic 转换
  -> 自定义上游
  -> 请求捕获、单元测试和手工会话验证
```

## 11. Failure-Mode Forecast

| 风险 | 失败表现 | 控制措施 | 验证证据 |
| --- | --- | --- | --- |
| 上游不支持 `service_tier` | HTTP 400 或未知字段错误 | FAST 为模型级显式能力；Chat 默认关闭 | 请求转换测试和真实网关冒烟 |
| 模板遗留默认 tier | 用户关闭 FAST 后仍发送 priority | 显式移除 `default_service_tier` | 目录 JSON 精确断言 |
| 默认 reasoning 不在支持列表 | Codex 默认值不可选择或启动异常 | 前后端双重归一化 | 保存测试和 Rust 生成测试 |
| 旧 Provider 行为漂移 | 升级后突然启用 FAST 或出现新档位 | 缺字段时继承当前行为 | 旧 JSON fixture 回归 |
| `ultra` 语义误解 | 用户期望原始 ultra，上游收到 max | UI 明示官方映射 | 请求捕获断言 |
| Chat 网关档位不完整 | 400 或多档位语义相同 | 使用现有 effortValueMode 映射 | DeepSeek/OpenRouter/low_high 测试 |
| Anthropic 输出空间不足 | thinking budget 占满输出 | 保持现有 max_tokens/2 限制 | budget clamp 测试 |
| API-key-only FAST 徽标隐藏 | 用户认为 FAST 未生效 | UI 文案说明，日志展示出站 tier | 实际请求日志 |
| 模型拉取覆盖能力字段 | 编辑后配置丢失 | `buildFetchedCatalogSelection` 仅覆盖模型基础字段 | 前端单元测试 |

## 12. 验收标准

| AC | 验收条件 | 证据 |
| --- | --- | --- |
| AC1 | 自定义 Responses 模型可以声明 FAST 默认开启 | 生成目录包含 `service_tiers` 和 `default_service_tier=priority` |
| AC2 | Codex 首次选择该模型时默认发送 `service_tier=priority` | 本地代理请求捕获 |
| AC3 | 用户切换标准模式后该请求字段消失 | 连续两次请求对比 |
| AC4 | 模型菜单展示八个标准思考档位 | Codex CLI/TUI 手工冒烟 |
| AC5 | `ultra` 最终按官方行为发送为 `max` | 请求体断言 |
| AC6 | Chat 路径保留兼容 FAST 字段 | Rust 单元测试 |
| AC7 | Anthropic 路径不发送 OpenAI `service_tier` | Rust 单元测试 |
| AC8 | Anthropic 八档选择覆盖关闭、低、中、高、最大映射 | Rust 参数化测试 |
| AC9 | 旧 Provider 缺少新字段时行为保持现状 | 旧配置 fixture 测试 |
| AC10 | 每个模型可以配置不同 FAST 和 reasoning 能力 | 双模型目录生成测试 |
| AC11 | Provider 编辑、模型拉取和再次保存不会丢字段 | 前端单元测试 |
| AC12 | OAuth `codexFastMode` 和审批路由测试保持通过 | 现有回归测试 |

## 13. 测试计划

### 13.1 Rust 模型目录测试

- FAST 关闭生成空数组且无默认 tier。
- FAST 可选生成 `priority` tier 且无默认 tier。
- FAST 默认开启生成 `priority` tier 和默认 tier。
- `fastModeDefault=true` 自动归一化支持状态。
- 全部 reasoning 档位生成顺序稳定。
- 自定义档位保留。
- 默认 reasoning 缺失时继承模板。
- 默认 reasoning 非法时回退合法值。
- 旧模型缺字段保持当前 Native/ProxyChat 模板差异。
- 多模型能力互不影响。

### 13.2 前端测试

- `normalizeCodexCatalogModelsForSave` 保留新字段。
- reasoning 去空、去重和顺序归一化。
- 新 Responses 模型默认 FAST 和全部档位。
- 新 Chat 模型采用安全默认值。
- 新 Anthropic 模型关闭 FAST 并提供完整 reasoning。
- 默认 reasoning 下拉只包含已启用值。
- 模型拉取后保留能力字段。
- 编辑 Provider 时完整回显。
- 删除模型行不会影响其他模型能力。

### 13.3 协议转换测试

- Responses：`priority` 和每种 reasoning 值进入上游请求。
- Chat passthrough：保留 `service_tier`。
- OpenRouter：`max -> xhigh`，`none` 使用原生 reasoning 对象。
- DeepSeek：`xhigh/max -> max`，其他开启值压缩为 high。
- low_high：低档和高档压缩正确。
- Anthropic：移除 `service_tier`。
- Anthropic：none、low、medium、high、max 预算和 adaptive 映射正确。
- Anthropic：小 `max_tokens` 时继续保护可见输出空间。

### 13.4 手工冒烟

1. 创建一个 `openai_responses` 自定义 Provider。
2. 添加两个模型：一个 FAST 默认开启，一个 FAST 关闭。
3. 启动 Codex 并检查模型菜单。
4. 确认第一个模型默认发送 `priority`。
5. 切换标准模式并确认字段消失。
6. 逐个选择 reasoning 档位并检查请求体。
7. 切换到 Chat Provider 检查字段透传与映射。
8. 切换到 Anthropic Provider 检查 FAST 隔离和 thinking 预算。

## 14. 最终验证命令

在目标代码库执行：

```text
cargo test
pnpm test:unit
pnpm typecheck
cargo fmt --check
pnpm format:check
git diff --check
```

建议增加定向测试命令：

```text
cargo test codex_catalog
cargo test transform_codex_chat
cargo test transform_codex_anthropic
pnpm test:unit -- CodexFormFields
pnpm test:unit -- ProviderForm
```

最终证据必须来自完成集成后的 `D:\codex对话\cc-switch-v3.19-safe` 当前工作树，不能只引用实施前测试或其他工作区结果。

## 15. 实施文件清单

预计修改：

```text
src/types.ts
src/components/providers/forms/ProviderForm.tsx
src/components/providers/forms/CodexFormFields.tsx
src/components/providers/forms/hooks/useCodexConfigState.ts
src-tauri/src/codex_config.rs
src-tauri/src/proxy/providers/transform_codex_chat.rs        # 预计仅测试
src-tauri/src/proxy/providers/transform_codex_anthropic.rs   # 预计仅测试
src/i18n/locales/zh.json
src/i18n/locales/zh-TW.json
src/i18n/locales/en.json
src/i18n/locales/ja.json
```

根据现有测试组织方式，可能增加或修改相关 `*.test.tsx`、Rust `#[cfg(test)]` 测试模块。实施时保持手术式修改，不重构无关 Provider 表单和代理路径。

## 16. 推荐决策

采用“每个模型独立能力配置 + 模型目录驱动”的方案：

- `openai_responses` 新模型默认开启 FAST、默认使用 FAST、提供全部思考档位、默认 `high`。
- `openai_chat` FAST 使用安全默认关闭，用户确认网关兼容后开启；思考强度复用现有映射。
- `anthropic` FAST 关闭，完整思考强度通过预算和 adaptive thinking 映射。
- 现有 ProviderMeta `codexFastMode` 保持 OAuth 专用职责。
- 旧 Provider 缺少字段时保持当前行为。
- `ultra` 在 UI 中保留，同时明确当前 Codex 请求会映射为 `max`。

该方案让 Codex 菜单、默认状态、代理请求和用户切换使用同一份模型能力真值，同时控制不兼容上游的 HTTP 400 风险。

## 17. 官方参考

- [OpenAI Codex `ReasoningEffort` 与模型目录结构](https://github.com/openai/codex/blob/66ebeb703710ae9e298b62d88d709b67777c91b4/codex-rs/protocol/src/openai_models.rs)
- [OpenAI Codex 服务等级解析](https://github.com/openai/codex/blob/66ebeb703710ae9e298b62d88d709b67777c91b4/codex-rs/tui/src/service_tier_resolution.rs)
- [OpenAI Codex FAST 菜单与切换逻辑](https://github.com/openai/codex/blob/66ebeb703710ae9e298b62d88d709b67777c91b4/codex-rs/tui/src/chatwidget/service_tiers.rs)
- [OpenAI Codex `ultra -> max` 请求映射](https://github.com/openai/codex/blob/66ebeb703710ae9e298b62d88d709b67777c91b4/codex-rs/core/src/client.rs)
- [OpenAI Codex FAST feature 默认状态](https://github.com/openai/codex/blob/66ebeb703710ae9e298b62d88d709b67777c91b4/codex-rs/features/src/lib.rs)
