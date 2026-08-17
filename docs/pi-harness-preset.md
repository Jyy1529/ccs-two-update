# Pi Coding Agent 配置说明

## 产品边界

本项目中的 `Pi` 指 [`pi.dev`](https://pi.dev) coding agent，不是 Inflection AI 的个人助手产品。

- 官方仓库：[`earendil-works/pi`](https://github.com/earendil-works/pi)
- npm 包：[`@earendil-works/pi-coding-agent`](https://www.npmjs.com/package/@earendil-works/pi-coding-agent)
- CLI：`pi`
- Node.js：`>=22.19.0`
- 官方图标：[`https://pi.dev/logo-auto.svg`](https://pi.dev/logo-auto.svg)

官方全局安装命令：

```bash
npm install -g --ignore-scripts @earendil-works/pi-coding-agent
```

CC Switch 的安装和升级入口使用同一包名，并在 npm/pnpm 路径保留 `--ignore-scripts`。

## 官方配置位置

Pi 使用 `PI_CODING_AGENT_DIR` 作为配置目录覆盖变量。未设置时默认目录为：

```text
~/.pi/agent
```

CC Switch 管理的官方文件是：

```text
~/.pi/agent/models.json
~/.pi/agent/settings.json
```

不是旧文档曾描述的单一 `config.json`，也不是 `~/.pi/mcp.json`。

## CC Switch Provider 配置

Provider 表单保存一份紧凑的兼容配置，Rust 后端再投影为 Pi 的两个官方 JSON 文件：

```json
{
  "baseUrl": "https://gateway.example.com/v1",
  "apiKey": "YOUR_API_KEY",
  "model": "your-model-id",
  "api": "openai-completions"
}
```

字段说明：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `baseUrl` | 是 | Provider API 根地址 |
| `apiKey` | 否 | 留空时不会清除同一受管 Provider 已保存的密钥 |
| `model` | 是 | 写入 `settings.json.defaultModel` 并确保存在于模型列表 |
| `api` | 否 | 默认 `openai-completions`，也可按上游能力使用 Pi 支持的其他 API 类型 |

写入时，CC Switch 使用 `cc-switch-<provider-id>` 作为受管 Provider ID，并更新：

```json
{
  "defaultProvider": "cc-switch-example",
  "defaultModel": "your-model-id"
}
```

已有的其他 Provider、主题设置、未知字段、请求头、兼容性选项和模型元数据都会保留。两个配置文件会在首次写入前全部解析，损坏的用户配置不会被静默覆盖。

## MCP 与 Skills

Pi 官方 README 明确标注 `No MCP`。因此：

- MCP 表单不展示 Pi 开关。
- 历史数据库中的 `pi: true` 会被归一化为 `false`。
- CC Switch 不创建 `~/.pi/mcp.json`。
- Skills 同步仍支持 Pi，默认目录基于 `~/.pi/agent`。

## 环境检查

设置页的本地环境检查会执行 `pi --version`，并通过 npm 注册表查询最新版。若安装后仍无法运行，优先确认：

```bash
node --version
pi --version
```

Node.js 版本低于 `22.19.0` 时应先升级 Node.js。

## 参考链接

- [Pi 官网](https://pi.dev)
- [Pi GitHub](https://github.com/earendil-works/pi)
- [Pi npm 包](https://www.npmjs.com/package/@earendil-works/pi-coding-agent)
- [Pi 官方图标](https://pi.dev/logo-auto.svg)
