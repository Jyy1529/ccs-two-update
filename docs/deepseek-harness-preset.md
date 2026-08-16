# DeepSeek Harness 配置预设

## 概述

DeepSeek Harness 支持通过 CC Switch 进行 Codex 角色路由，适用于：
- DeepSeek V3 官方 API
- DeepSeek Chat Completions 格式
- 需要本地路由转换的场景

## Provider 配置模板

### 1. DeepSeek 原生 Responses 直连（推荐）

适用于 3.19.1+ 版本，无需本地路由。

```json
{
  "id": "deepseek-v3",
  "name": "DeepSeek V3",
  "appType": "codex",
  "baseUrl": "https://api.deepseek.com/v1",
  "apiKey": "YOUR_DEEPSEEK_API_KEY",
  "models": [
    {
      "id": "deepseek-chat",
      "name": "DeepSeek V3",
      "contextWindow": 64000,
      "maxOutputTokens": 8000
    },
    {
      "id": "deepseek-reasoner",
      "name": "DeepSeek R1",
      "contextWindow": 64000,
      "maxOutputTokens": 8000
    }
  ],
  "meta": {
    "codexAgentRoleRouting": {
      "enabled": false
    }
  }
}
```

### 2. DeepSeek Chat Completions 格式（需本地路由）

适用于需要 Chat Completions → Responses 转换的场景。

```json
{
  "id": "deepseek-local-route",
  "name": "DeepSeek (Local Route)",
  "appType": "codex",
  "baseUrl": "http://127.0.0.1:15777/deepseek/v1",
  "apiKey": "YOUR_DEEPSEEK_API_KEY",
  "models": [
    {
      "id": "deepseek-chat",
      "name": "DeepSeek V3",
      "contextWindow": 64000,
      "maxOutputTokens": 8000
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
  "id": "deepseek-with-roles",
  "name": "DeepSeek (With Role Routing)",
  "appType": "codex",
  "baseUrl": "https://api.deepseek.com/v1",
  "apiKey": "YOUR_DEEPSEEK_API_KEY",
  "models": [
    {
      "id": "deepseek-chat",
      "name": "DeepSeek V3",
      "contextWindow": 64000,
      "maxOutputTokens": 8000
    }
  ],
  "meta": {
    "codexAgentRoleRouting": {
      "enabled": true,
      "frontend": {
        "providerId": "cc-switch-frontend-local",
        "override": {
          "model": "deepseek-chat",
          "instructions": "You are the CC Switch frontend specialist. Focus on user-facing interface work."
        }
      },
      "backend": {
        "override": {
          "model": "deepseek-reasoner",
          "instructions": "You are the CC Switch backend specialist. Focus on services, APIs, and backend logic."
        }
      }
    }
  }
}
```

## 前置条件

1. **本地代理监听回环地址**
   - 必须配置为 `127.0.0.1` 或 `::1`
   - 不能使用 `0.0.0.0`（安全风险）

2. **角色配置文件路径**
   - Windows: `%USERPROFILE%\.codex\agents\`
   - macOS/Linux: `~/.codex/agents/`

3. **自动生成的文件**
   - `cc-switch-frontend.toml` - 前端角色配置
   - `cc-switch-backend.toml` - 后端角色配置

## 使用流程

### 1. 创建 Provider

在 CC Switch 中添加 DeepSeek Provider，配置角色路由。

### 2. 启用角色路由

```javascript
// 通过 API 启用
await fetch('/api/providers/deepseek-with-roles/enable-role-routing', {
  method: 'POST'
});
```

### 3. 验证配置

检查生成的 TOML 文件：

```bash
# Windows
type %USERPROFILE%\.codex\agents\cc-switch-frontend.toml
type %USERPROFILE%\.codex\agents\cc-switch-backend.toml

# macOS/Linux
cat ~/.codex/agents/cc-switch-frontend.toml
cat ~/.codex/agents/cc-switch-backend.toml
```

## 路由令牌验证

CC Switch 使用 HMAC-SHA256 验证路由请求：

1. **生成令牌**
   - 输入: `owner_provider_id` + `route` + `routing_version`
   - 算法: HMAC-SHA256
   - 编码: Base64 URL-safe

2. **HTTP Headers**
   ```
   x-cc-switch-role-route: frontend
   x-cc-switch-role-owner: deepseek-with-roles
   x-cc-switch-role-token: <HMAC-SHA256-token>
   ```

3. **验证流程**
   - Codex 代理接收请求
   - 读取 headers 并验证令牌
   - 令牌有效 → 路由到指定 Provider
   - 令牌无效 → 拒绝请求

## 故障排查

### 问题 1: 配置文件未生成

**症状**: `~/.codex/agents/` 目录为空

**原因**:
- 角色路由未启用
- 父目录不存在
- 权限不足

**解决**:
```bash
# 检查目录
ls -la ~/.codex/agents/

# 手动创建目录
mkdir -p ~/.codex/agents/

# 检查权限
chmod 700 ~/.codex/agents/
```

### 问题 2: 监听地址验证失败

**症状**: 日志显示 "listen address is not loopback"

**原因**: 本地代理监听在 `0.0.0.0`

**解决**:
```javascript
// 修改代理配置
await fetch('/api/proxy/config', {
  method: 'PUT',
  body: JSON.stringify({
    listen_address: '127.0.0.1',  // 改为回环地址
    listen_port: 15777
  })
});
```

### 问题 3: 路由令牌验证失败

**症状**: 请求被拒绝，日志显示 token verification failed

**原因**:
- 令牌过期
- 配置版本不匹配
- HMAC 密钥不同

**解决**:
1. 重启 CC Switch（刷新 HMAC 密钥）
2. 重新生成配置文件
3. 检查 Codex 配置是否同步

## 性能优化

### 1. 模型选择

- **Frontend**: 使用 `deepseek-chat`（更快响应）
- **Backend**: 使用 `deepseek-reasoner`（更强推理）

### 2. 超时配置

```json
{
  "streaming_first_byte_timeout": 30,
  "streaming_idle_timeout": 60,
  "non_streaming_timeout": 120
}
```

### 3. 熔断保护

```json
{
  "circuit_failure_threshold": 5,
  "circuit_success_threshold": 2,
  "circuit_timeout_seconds": 60
}
```

## 安全注意事项

1. **API Key 保护**
   - 不要硬编码在配置文件中
   - 使用环境变量或密钥管理服务

2. **回环地址要求**
   - 必须使用 `127.0.0.1` 或 `::1`
   - 防止外部访问本地代理

3. **令牌验证**
   - 每个请求都会验证 HMAC 令牌
   - 令牌包含路由配置的版本哈希

## 参考链接

- [DeepSeek 官方文档](https://platform.deepseek.com/docs)
- [CC Switch 代理路由指南](./fix-and-enhancement-plan.md)
- [Codex 配置参考](https://docs.codex.ai/configuration)
