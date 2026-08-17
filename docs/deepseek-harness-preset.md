# DeepSeek Harness 配置说明

## 产品边界

DeepSeek Harness 是 DeepSeek 官方 coding agent CLI，与 CC Switch 中给 Codex 创建的 DeepSeek API Provider 是两个不同的配置目标。

- 官方仓库：[`deepseek-ai/deepseek-harness`](https://github.com/deepseek-ai/deepseek-harness)
- npm 包：[`@deepseek-ai/dsh`](https://www.npmjs.com/package/@deepseek-ai/dsh)
- CLI：`dsh`
- 官方站点：[`deepseek.com/harness`](https://deepseek.com/harness)

安装命令：

```bash
npm install -g @deepseek-ai/dsh
```

## 官方配置位置

DeepSeek Harness 使用 `DSH_HOME` 作为配置目录覆盖变量。未设置时默认目录为：

```text
~/.dsh
```

CC Switch 管理的官方文件是：

```text
~/.dsh/settings.yaml
~/.dsh/.credentials.yaml
```

旧实现中的 `~/.deepseek/config.json` 和 `~/.deepseek/mcp.json` 都不是 DeepSeek Harness 官方配置契约。

## CC Switch Provider 配置

Provider 表单保存以下兼容配置，Rust 后端负责投影到官方 YAML：

```json
{
  "baseUrl": "https://api.deepseek.com",
  "apiKey": "YOUR_DEEPSEEK_API_KEY",
  "model": "deepseek-v4-flash"
}
```

默认值：

| 字段 | 默认值 |
| --- | --- |
| `baseUrl` | `https://api.deepseek.com` |
| `model` | `deepseek-v4-flash` |
| 凭据环境变量名 | `DEEPSEEK_API_KEY` |

投影后的核心结构如下：

```yaml
llm-deepseek:
  baseURL: https://api.deepseek.com
  apiKeyEnv: DEEPSEEK_API_KEY
  models:
    - id: deepseek-v4-flash
    - id: deepseek-v4-pro

agent-default-model:
  provider: deepseek-official
  model: deepseek-v4-flash
```

密钥单独写入：

```yaml
DEEPSEEK_API_KEY: YOUR_DEEPSEEK_API_KEY
```

Unix 平台上的 `.credentials.yaml` 权限会收紧到 `0600`。表单中的 API Key 留空时，CC Switch 不会清除已有凭据。

已有 YAML 顶层设置、`llm-deepseek` 未知字段、模型元数据和其他凭据都会保留。损坏 YAML 会直接报错并保持原文件不变。

## 与 Codex DeepSeek Provider 的区别

本文件只描述 AppType `deepseek` 对 DeepSeek Harness CLI 的配置投影。

若要配置 Codex 直连 DeepSeek Responses API，请使用 [Codex DeepSeek 路由指南](./guides/codex-deepseek-routing-guide-zh.md)。不要把 Codex 的 `config.toml`、角色路由或代理端口配置复制到 `settings.yaml`。

## MCP 与 Skills

DeepSeek Harness 有自己的插件能力，但旧版 CC Switch 使用的通用 `~/.deepseek/mcp.json` 没有官方依据。因此：

- MCP 表单不展示 DeepSeek 开关。
- 历史数据库中的 `deepseek: true` 会被归一化为 `false`。
- CC Switch 不创建 `~/.deepseek/mcp.json`。
- Skills 同步仍支持 DeepSeek，默认目录基于 `~/.dsh`。

## 环境检查

设置页的本地环境检查执行 `dsh --version`，并通过 npm 注册表查询 `@deepseek-ai/dsh` 最新版本。未安装时可直接使用设置页安装按钮或上面的官方命令。

## 参考链接

- [DeepSeek Harness GitHub](https://github.com/deepseek-ai/deepseek-harness)
- [DeepSeek Harness npm 包](https://www.npmjs.com/package/@deepseek-ai/dsh)
- [DeepSeek Harness 官网](https://deepseek.com/harness)
- [DeepSeek API Codex 集成](https://api-docs.deepseek.com/quick_start/agent_integrations/codex/)
