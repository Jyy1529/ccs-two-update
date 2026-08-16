# Pi Harness 配置预设

## 概述

Pi AI (Inflection AI) 是一个情感智能的个人 AI 助手。通过 CC Switch，可以将 Pi 模型集成到 Codex 角色路由中。

## 访问方式

Pi AI 目前通过第三方聚合器提供 API 访问：

1. **OpenRouter** (推荐)
   - Base URL: `https://openrouter.ai/api/v1`
   - 模型: `inflection/inflection-3-pi`

2. **AIML API**
   - Base URL: `https://api.aimlapi.com/v1`
   - 模型: `inflection/inflection-3-pi`

## Provider 配置模板

### 1. Pi via OpenRouter (推荐)

```json
{
  "id": "pi-openrouter",
  "name": "Pi AI (OpenRouter)",
  "appType": "codex",
  "baseUrl": "https://openrouter.ai/api/v1",
  "apiKey": "YOUR_OPENROUTER_API_KEY",
  "models": [
    {
      "id": "inflection/inflection-3-pi",
      "name": "Inflection 3 Pi",
      "contextWindow": 8000,
      "maxOutputTokens": 1000
    },
    {
      "id": "inflection/inflection-3-productivity",
      "name": "Inflection 3 Productivity",
      "contextWindow": 8000,
      "maxOutputTokens": 1000
    }
  ],
  "meta": {
    "codexAgentRoleRouting": {
      "enabled": false
    }
  }
}
```

### 2. Pi via AIML API

```json
{
  "id": "pi-aiml",
  "name": "Pi AI (AIML)",
  "appType": "codex",
  "baseUrl": "https://api.aimlapi.com/v1",
  "apiKey": "YOUR_AIML_API_KEY",
  "models": [
    {
      "id": "inflection/inflection-3-pi",
      "name": "Inflection 3 Pi",
      "contextWindow": 8000,
      "maxOutputTokens": 1000
    }
  ],
  "meta": {
    "codexAgentRoleRouting": {
      "enabled": false
    }
  }
}
```

## Codex 角色路由配置

### 启用前端/后端代理路由

```json
{
  "id": "pi-with-roles",
  "name": "Pi AI (With Role Routing)",
  "appType": "codex",
  "baseUrl": "https://openrouter.ai/api/v1",
  "apiKey": "YOUR_OPENROUTER_API_KEY",
  "models": [
    {
      "id": "inflection/inflection-3-pi",
      "name": "Inflection 3 Pi",
      "contextWindow": 8000,
      "maxOutputTokens": 1000
    },
    {
      "id": "inflection/inflection-3-productivity",
      "name": "Inflection 3 Productivity",
      "contextWindow": 8000,
      "maxOutputTokens": 1000
    }
  ],
  "meta": {
    "codexAgentRoleRouting": {
      "enabled": true,
      "frontend": {
        "providerId": "cc-switch-frontend-local",
        "override": {
          "model": "inflection/inflection-3-pi",
          "instructions": "You are the CC Switch frontend specialist. Focus on user-facing interface work, leveraging Pi's empathetic and human-centered approach."
        }
      },
      "backend": {
        "override": {
          "model": "inflection/inflection-3-productivity",
          "instructions": "You are the CC Switch backend specialist. Focus on services, APIs, and backend logic with productivity-focused problem solving."
        }
      }
    }
  }
}
```

## Pi 模型特性

### Inflection 3 Pi

**特点**:
- 情感智能对话
- 上下文感知
- 擅长客户服务场景
- 富有同理心的响应

**适用场景**:
- 前端用户界面开发
- 用户体验优化
- 文档编写
- 对话式交互设计

**规格**:
- Context Window: 8K tokens
- Max Output: 1K tokens
- 价格: 按 token 计费

### Inflection 3 Productivity

**特点**:
- 生产力导向
- 任务执行能力强
- 结构化输出
- 高效问题解决

**适用场景**:
- 后端服务开发
- API 实现
- 数据处理逻辑
- 系统架构设计

**规格**:
- Context Window: 8K tokens
- Max Output: 1K tokens
- 价格: 按 token 计费

## 前置条件

### 1. 获取 API Key

**OpenRouter**:
1. 访问 https://openrouter.ai/
2. 注册账户
3. 在 Dashboard 中创建 API Key
4. 充值余额（按需付费）

**AIML API**:
1. 访问 https://aimlapi.com/
2. 注册账户
3. 获取 API Key
4. 充值余额

### 2. 本地代理配置

- 监听地址: `127.0.0.1`（必须是回环地址）
- 监听端口: 默认 `15777`

### 3. 角色配置文件

- Windows: `%USERPROFILE%\.codex\agents\`
- macOS/Linux: `~/.codex/agents/`

## 使用流程

### 1. 配置 Provider

在 CC Switch 中添加 Pi Provider：

```javascript
// 通过 API 创建
const response = await fetch('/api/providers', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({
    id: 'pi-with-roles',
    name: 'Pi AI (With Role Routing)',
    appType: 'codex',
    baseUrl: 'https://openrouter.ai/api/v1',
    apiKey: process.env.OPENROUTER_API_KEY,
    models: [/* ... */],
    meta: { codexAgentRoleRouting: { enabled: true, /* ... */ } }
  })
});
```

### 2. 激活角色路由

```javascript
// 启用角色路由
await fetch('/api/providers/pi-with-roles/reconcile-roles', {
  method: 'POST'
});
```

### 3. 验证配置

检查生成的配置文件：

```bash
# Windows
type %USERPROFILE%\.codex\agents\cc-switch-frontend.toml
type %USERPROFILE%\.codex\agents\cc-switch-backend.toml

# macOS/Linux
cat ~/.codex/agents/cc-switch-frontend.toml
cat ~/.codex/agents/cc-switch-backend.toml
```

示例 `cc-switch-frontend.toml`:

```toml
# cc-switch-managed: codex-agent-role-v1

name = "cc-switch-frontend"

[model_provider]
id = "cc-switch-frontend-local"
base_url = "http://127.0.0.1:15777/v1"

[model_provider.models.inflection-3-pi]
id = "inflection/inflection-3-pi"
context_window = 8000
max_output_tokens = 1000

model = "inflection/inflection-3-pi"
developer_instructions = "You are the CC Switch frontend specialist..."
```

### 4. 测试路由

使用 Codex CLI 测试：

```bash
# 启动 Codex 并指定角色
codex --agent cc-switch-frontend

# 在对话中验证路由生效
> 帮我创建一个用户友好的登录表单
```

## 路由工作流程

```
┌─────────────┐
│ Codex Agent │
└──────┬──────┘
       │
       │ 1. 读取 cc-switch-frontend.toml
       │
       ▼
┌─────────────────────┐
│ CC Switch Proxy     │
│ 127.0.0.1:15777     │
└──────┬──────────────┘
       │
       │ 2. 验证路由令牌
       │ x-cc-switch-role-route: frontend
       │ x-cc-switch-role-owner: pi-with-roles
       │ x-cc-switch-role-token: <HMAC>
       │
       ▼
┌─────────────────────┐
│ OpenRouter API      │
│ openrouter.ai/api   │
└──────┬──────────────┘
       │
       │ 3. 转发到 Pi 模型
       │ model: inflection/inflection-3-pi
       │
       ▼
┌─────────────────────┐
│ Inflection AI       │
│ Pi Model Response   │
└─────────────────────┘
```

## 故障排查

### 问题 1: API Key 验证失败

**症状**: 401 Unauthorized

**解决**:
```bash
# 检查 API Key 是否有效
curl -H "Authorization: Bearer $OPENROUTER_API_KEY" \
  https://openrouter.ai/api/v1/models

# 验证余额充足
# 访问 OpenRouter Dashboard 检查账户状态
```

### 问题 2: 模型不可用

**症状**: 404 Model Not Found

**原因**:
- 模型 ID 拼写错误
- 模型在该聚合器上不可用

**解决**:
```javascript
// 列出可用模型
const response = await fetch('https://openrouter.ai/api/v1/models', {
  headers: {
    'Authorization': `Bearer ${OPENROUTER_API_KEY}`
  }
});
const models = await response.json();
console.log(models.data.filter(m => m.id.includes('inflection')));
```

### 问题 3: Context Window 超限

**症状**: 413 Request Entity Too Large

**原因**: 输入超过 8K tokens

**解决**:
```json
{
  "meta": {
    "codexAgentRoleRouting": {
      "frontend": {
        "override": {
          "model": "inflection/inflection-3-pi",
          "contextWindow": 8000,  // 确保不超过限制
          "instructions": "Keep responses concise..."
        }
      }
    }
  }
}
```

### 问题 4: 配置文件未生成

**症状**: `~/.codex/agents/` 目录为空

**解决**:
```bash
# 检查 CC Switch 日志
tail -f ~/.cc-switch/logs/app.log | grep CodexRoleRoute

# 手动触发协调
curl -X POST http://localhost:8080/api/providers/pi-with-roles/reconcile-roles

# 验证父目录存在
mkdir -p ~/.codex/agents/
chmod 700 ~/.codex/agents/
```

## 性能优化

### 1. 模型选择策略

```json
{
  "frontend": {
    "model": "inflection/inflection-3-pi",
    "rationale": "情感智能适合用户界面，响应速度快"
  },
  "backend": {
    "model": "inflection/inflection-3-productivity",
    "rationale": "生产力导向适合后端逻辑，输出结构化"
  }
}
```

### 2. 超时配置

```json
{
  "streaming_first_byte_timeout": 20,  // Pi 响应较快
  "streaming_idle_timeout": 40,
  "non_streaming_timeout": 90
}
```

### 3. 重试策略

```json
{
  "max_retries": 3,
  "auto_failover_enabled": true,
  "circuit_failure_threshold": 5
}
```

## 成本估算

### OpenRouter 定价（示例）

- **Inflection 3 Pi**: $0.10/M input tokens, $0.30/M output tokens
- **Inflection 3 Productivity**: $0.15/M input tokens, $0.45/M output tokens

### 使用场景成本分析

**前端开发任务** (1000 请求/天):
- 平均输入: 2K tokens
- 平均输出: 500 tokens
- 日成本: (2K × 1000 × $0.10/M) + (500 × 1000 × $0.30/M) = $0.35/天

**后端开发任务** (500 请求/天):
- 平均输入: 4K tokens
- 平均输出: 800 tokens
- 日成本: (4K × 500 × $0.15/M) + (800 × 500 × $0.45/M) = $0.48/天

**总计**: ~$0.83/天 = $25/月

## 安全注意事项

1. **API Key 保护**
   - 使用环境变量存储
   - 不要提交到版本控制
   - 定期轮换 Key

2. **回环地址要求**
   - 本地代理必须监听 `127.0.0.1`
   - 防止外部网络访问

3. **路由令牌验证**
   - 每个请求都验证 HMAC-SHA256 令牌
   - 令牌绑定到特定的 Provider 和路由配置

4. **速率限制**
   - OpenRouter 有默认速率限制
   - 实现客户端限流避免 429 错误

## 与 DeepSeek 的比较

| 特性 | Pi (Inflection) | DeepSeek |
|------|-----------------|----------|
| Context Window | 8K | 64K |
| Max Output | 1K | 8K |
| 特点 | 情感智能 | 推理能力强 |
| 价格 | 中等 | 低 |
| 适用场景 | 用户交互 | 复杂逻辑 |
| API 访问 | 聚合器 | 官方直连 |

## 参考链接

- [OpenRouter 文档](https://openrouter.ai/docs)
- [AIML API 文档](https://aimlapi.com/docs)
- [Inflection AI 官网](https://inflection.ai/)
- [CC Switch 代理路由指南](./fix-and-enhancement-plan.md)
- [DeepSeek Harness 预设](./deepseek-harness-preset.md)
