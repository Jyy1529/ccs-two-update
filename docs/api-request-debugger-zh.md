# API 请求调试与 Codex 配置保护诊断

## 使用入口

在供应商卡片的操作栏点击 **API 请求调试**（终端方框图标）。目前支持 Claude、Claude Desktop、Codex、Gemini、OpenCode 和 OpenClaw 的 API Key 供应商。原生 OAuth、云厂商签名认证和 OMO 配置不使用此入口。

1. 选择接口路径。默认采用供应商配置中的 API 格式。
2. 从模型下拉框选择已有模型，或点击刷新按钮获取供应商模型列表。接口未开放模型列表时，可以手动输入模型 ID。
3. 提示词默认是 `搜索一下今日科技和ai热点`，可以修改；流式响应默认开启。
4. 查看或复制 cURL，选择 Bash / zsh 或 PowerShell 7.3+ 格式。
5. 点击发送请求，在“完整响应”查看 HTTP 状态、原始 JSON/SSE、响应头和本次发送的请求快照。

API Key 自动来自所点击供应商。预览默认遮盖 Key，复制的 cURL 包含真实 Key。请求不修改供应商配置，也不参与连通性检测或故障转移。应用内发送使用全局代理设置，终端执行 cURL 则使用终端环境的代理设置。

Codex 优先读取当前供应商表中的 `experimental_bearer_token`，没有时才使用旧 `auth` 或环境字段，避免历史 Key 覆盖供应商自己的凭据。

## 接口格式

| 下拉选项               | 路径                                     | 鉴权                                              | 提示词字段                                    |
| ---------------------- | ---------------------------------------- | ------------------------------------------------- | --------------------------------------------- |
| Chat Completions       | `/v1/chat/completions`                   | `Authorization: Bearer …`                         | `messages[].content`                          |
| Responses              | `/v1/responses`                          | `Authorization: Bearer …`                         | `input`                                       |
| Anthropic Messages     | `/v1/messages`                           | `x-api-key`，另带 `anthropic-version: 2023-06-01` | `messages[].content`，并提供必填 `max_tokens` |
| Gemini GenerateContent | `/v1beta/models/{model}:generateContent` | `x-goog-api-key`                                  | `contents[].parts[].text`                     |

Gemini 开启流式后使用 `:streamGenerateContent?alt=sse`，请求体不添加 OpenAI 的 `stream` 字段。其他三个接口使用 `stream: true/false`。

模型刷新使用供应商原有的协议，与当前选中的测试接口独立。Gemini 原生模型列表支持分页，过滤仅用于向量嵌入的模型，并保留供应商已配置的模型。Anthropic 模型列表附带所需的版本请求头。第三方供应商是否支持下拉中的各个接口，以其实际响应为准。

自动处理已有 `/v1`、已知完整接口路径和网关前缀，避免 `/v1/v1`。启用供应商的“完整 URL”设置时，自定义端点原样保留，已有不带版本的完整端点不被强行加入 `/v1`。已有供应商自定义请求头保留，保留的 HTTP 传输控制字段会明确报错。

提示词本身不保证模型联网。是否能获得当天新闻，取决于供应商的模型能力与工具支持；此功能不会默认为第三方接口添加未经确认支持的联网工具。

## 响应与取消

- SSE 保留所有事件行与 `[DONE]`，不只提取最后的回答文本。HTTP 4xx/5xx 同样保留完整响应体。
- 请求只发送一次；不自动重试或跟随重定向。
- 超时默认 120 秒，可设为 1–600 秒。可以取消正在执行的请求，保留已经接收的内容。
- 超时、取消、网络中断或非 UTF-8 响应都会明确标明未完整接收。
- 请求体上限 1 MiB，响应上限 32 MiB。达到响应上限时停止接收并明确提示内容不完整，不把截断结果当作完整响应。
- 没有请求历史数据库或自动持久化；关闭弹窗后不保留 Key、提示词或响应。

## 官方依据（2026-09-08 查阅）

- [OpenAI Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)
- [OpenAI Responses](https://developers.openai.com/api/reference/resources/responses/methods/create)
- [Anthropic Messages](https://docs.anthropic.com/en/api/messages/create)
- [Google Gemini GenerateContent / StreamGenerateContent](https://ai.google.dev/api/generate-content)
- [Google Gemini Models](https://ai.google.dev/api/models)

## 本次 Codex 热切换报错的诊断

报错 `Configuration has external/user-owned changes. Review the complete connection change in Configuration protection` 表示配置保护检测到外部修改或用户所有的字段，拒绝自动覆盖。外层“热切换失败 / 写入 Codex 配置失败”是传播这次拒绝的调用错误；`&#x20;` 是显示出来的空格转义，不是故障原因。

首次诊断时的只读核对得到以下证据：

- 正在运行的程序是 `D:\Code Switch\CC Switch\cc-switch.exe`，产品版本 **3.20.1**，包含 `get_config_guard_state`、`preview_config_change`、`apply_config_change` 等配置保护命令。
- 当时工作区的 `package.json`、`src-tauri/Cargo.toml` 和 Tauri 配置版本是 **3.20.0**，没有上述配置保护实现。后续已确认共同基线，并合入 `Jyy1529/ccs-two-update` 的 **3.20.1** 源码；本次请求调试版本统一为 **3.20.2**，包含原有配置保护模块。
- `C:\Users\JASOY\.cc-switch\local-state\config-guard.json` 的历史在北京时间 **2026-09-08 19:15:33** 记录了 Codex `config.toml` 的 `/model_reasoning_effort` 冲突。
- **19:15:44** 又记录了一次成功应用。在本次读取时，该字段的基线和当前文件都为 `high`，待确认项目中没有 Codex。
- 读取时 Codex 的整文件保护标记为 `false`。证据支持“字段修改冲突”，不支持把它解释成 Windows 写权限不足、API Key 错误或模型接口不通。

不能仅凭这些记录确定是谁改了推理强度，也不能断言保护规则存在误判。修改 Codex 推理强度的程序、编辑器或 Codex 设置都可能造成基线差异。后续同类冲突应在运行程序的“配置保护 / Configuration protection”中查看完整连接变更，核对字段与目标供应商后应用所需值。

诊断和请求调试功能不改写当前 Codex 配置、认证文件或保护状态。3.20.2 在完整的 3.20.1 基线上集成新功能，保留配置保护对外部修改的拦截；这次诊断不等于确认或修复了保护规则误判。安装包与正在运行的程序分开交付，升级须由用户执行。
