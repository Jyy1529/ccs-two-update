# CCS 前端/后端代理路由修复与 Pi/DeepSeek Harness 支持方案

## 问题分析

根据项目截图和代码分析，当前存在以下问题：

### 1. 前端/后端代理角色路由功能分析

**现状**：
- 项目已实现 `codexAgentRoleRouting` 功能（`src-tauri/src/services/codex_agent_roles.rs`）
- 支持前端角色独立 Provider 和模型路由
- 后端角色继续使用配置拥有者 Provider
- 生成并维护 `cc-switch-frontend.toml` 和 `cc-switch-backend.toml`

**潜在问题**：
1. **配置文件路径错误**：角色路由文件可能未正确写入 Codex 配置目录
2. **权限问题**：Windows 环境下可能存在文件写入权限限制
3. **配置同步延迟**：前端配置更改后，后端服务未及时重载配置
4. **路由令牌验证失败**：HMAC 签名验证可能因密钥不一致而失败
5. **本地代理监听地址错误**：角色路由要求 Codex 本地代理监听回环地址

### 2. DeepSeek Harness 集成需求

根据搜索结果，DeepSeek 在 2026 年 8 月 13 日发布了 **DeepSeek Harness v0.1**：
- 基于 Cordis 插件系统构建
- 提供模型、工具、技能、会话、沙箱、存储、循环、调度和 UI 能力
- 使用插件化架构，所有功能通过插件提供

**集成方向**：
1. **OpenAI 兼容 API**：DeepSeek API 完全兼容 OpenAI 格式（Chat Completions）
2. **协议转换**：项目已支持 Codex Responses ↔ Chat Completions 转换
3. **Provider 配置**：通过 Universal Provider 或 Codex Provider 支持

### 3. Pi Harness 集成需求

根据搜索结果，Pi 是一个轻量级 agent harness（pi.dev）：
- 极简主义设计，仅内置 4 个工具
- 通过 TypeScript 扩展系统实现功能扩展
- 支持统一 LLM API，兼容多家供应商
- AI SDK 7 已集成 HarnessAgent API 支持 Pi

**集成方向**：
1. **Provider 桥接**：通过 CC Switch 的 Provider 系统为 Pi 提供模型访问
2. **协议适配**：Pi 使用标准 Chat Completions 或 Anthropic Messages 格式
3. **配置导出**：支持将 CC Switch Provider 导出为 Pi 配置格式

---

## 修复方案

### 方案 1：前端/后端代理路由功能修复

#### 1.1 配置文件路径验证与修复

**问题定位**：
```rust
// src-tauri/src/services/codex_agent_roles.rs
pub const FRONTEND_ROLE_FILE_NAME: &str = "cc-switch-frontend.toml";
pub const BACKEND_ROLE_FILE_NAME: &str = "cc-switch-backend.toml";
```

**修复步骤**：

1. **验证 Codex 配置目录路径**
   - 检查 `~/.codex/` 或 Windows `%USERPROFILE%\.codex\` 目录是否存在
   - 确认当前用户对该目录具有写入权限

2. **添加配置写入日志**
   ```rust
   // 增强错误日志，记录完整路径和权限信息
   log::info!("[CodexRoleRoute] Writing role config to: {:?}", role_file_path);
   log::info!("[CodexRoleRoute] Parent directory writable: {}", parent_dir.metadata()?.permissions().readonly());
   ```

3. **实现配置文件原子写入**
   ```rust
   // 使用临时文件 + 原子重命名避免写入中断
   let temp_file = role_file_path.with_extension("tmp");
   fs::write(&temp_file, content)?;
   fs::rename(&temp_file, &role_file_path)?;
   ```

4. **Windows 路径特殊处理**
   ```rust
   #[cfg(windows)]
   fn ensure_codex_config_dir() -> Result<PathBuf, AppError> {
       let home = std::env::var("USERPROFILE")
           .or_else(|_| std::env::var("HOME"))?;
       let codex_dir = PathBuf::from(home).join(".codex");
       if !codex_dir.exists() {
           fs::create_dir_all(&codex_dir)?;
       }
       Ok(codex_dir)
   }
   ```

#### 1.2 路由令牌验证机制加固

**问题定位**：
```rust
// src-tauri/src/services/codex_agent_roles.rs:98
pub fn create_codex_role_route_token(
    owner_provider_id: &str,
    route: &str,
    routing: &CodexAgentRoleRouting,
) -> String
```

**修复步骤**：

1. **持久化路由密钥**
   ```rust
   // 将密钥从 Lazy 静态变量改为配置文件持久化
   fn get_or_create_route_secret() -> Result<[u8; 32], AppError> {
       let secret_path = get_config_dir()?.join("role_route_secret.bin");
       if secret_path.exists() {
           let bytes = fs::read(&secret_path)?;
           if bytes.len() == 32 {
               let mut secret = [0u8; 32];
               secret.copy_from_slice(&bytes);
               return Ok(secret);
           }
       }
       // 生成新密钥并持久化
       let mut secret = [0u8; 32];
       secret[..16].copy_from_slice(Uuid::new_v4().as_bytes());
       secret[16..].copy_from_slice(Uuid::new_v4().as_bytes());
       fs::write(&secret_path, &secret)?;
       Ok(secret)
   }
   ```

2. **增加令牌过期机制**
   ```rust
   // 令牌中嵌入时间戳，防止无限期使用旧令牌
   pub fn create_codex_role_route_token_with_ttl(
       owner_provider_id: &str,
       route: &str,
       routing: &CodexAgentRoleRouting,
       ttl_hours: u64,
   ) -> String {
       let expiry = SystemTime::now()
           .duration_since(UNIX_EPOCH)
           .unwrap()
           .as_secs() + (ttl_hours * 3600);
       let payload = format!("{route}:{owner_provider_id}:{routing_hash}:{expiry}");
       // ... HMAC 签名
   }
   ```

#### 1.3 本地代理监听地址检测

**问题定位**：
项目要求角色路由启用时 Codex 本地代理监听回环地址

**修复步骤**：

1. **启动前置检查**
   ```rust
   pub async fn validate_codex_role_routing_requirements(
       state: &AppState,
       provider_id: &str,
   ) -> Result<(), AppError> {
       let provider = state.db.get_provider_by_id(provider_id, "codex")?
           .ok_or(AppError::ProviderNotFound(provider_id.to_string()))?;
       
       let routing = provider.meta
           .and_then(|m| m.codex_agent_role_routing)
           .filter(|r| r.is_enabled())
           .ok_or_else(|| AppError::Message("Role routing not enabled".into()))?;
       
       // 检查本地代理监听地址
       let proxy_config = state.db.get_proxy_config_for_app("codex").await?;
       if !is_loopback_address(&proxy_config.listen_address) {
           return Err(AppError::Message(
               "Codex role routing requires local proxy to listen on loopback address (127.0.0.1 or ::1)".into()
           ));
       }
       
       Ok(())
   }
   
   fn is_loopback_address(addr: &str) -> bool {
       addr.starts_with("127.") || addr.starts_with("::1") || addr == "localhost"
   }
   ```

2. **前端启用前验证**
   ```typescript
   // src/components/providers/forms/CodexAgentRoleRoutingConfig.tsx
   async function validateRoleRoutingRequirements() {
       const proxyConfig = await getProxyConfig('codex');
       if (!isLoopbackAddress(proxyConfig.listenAddress)) {
           showError(
               'Codex 角色路由要求本地代理监听回环地址',
               '请在代理设置中将监听地址改为 127.0.0.1 或 ::1'
           );
           return false;
       }
       return true;
   }
   ```

#### 1.4 配置重载通知机制

**问题定位**：
前端更改配置后，Codex 本地代理未及时重载角色路由配置

**修复步骤**：

1. **实现配置热重载**
   ```rust
   // src-tauri/src/services/codex_agent_roles.rs
   pub async fn reload_codex_role_routing(
       state: &AppState,
       provider_id: &str,
   ) -> Result<(), AppError> {
       let _lock = ROLE_COORDINATION_LOCK.lock().await;
       
       // 重新生成配置文件
       project_codex_agent_roles(state, provider_id).await?;
       
       // 发送通知到本地代理
       let proxy_url = format!(
           "http://127.0.0.1:{}/internal/reload-config",
           get_codex_proxy_port()?
       );
       
       let client = reqwest::Client::new();
       client.post(&proxy_url)
           .timeout(Duration::from_secs(5))
           .send()
           .await?;
       
       log::info!("[CodexRoleRoute] Config reloaded for provider: {}", provider_id);
       Ok(())
   }
   ```

2. **前端保存后触发重载**
   ```typescript
   async function saveCodexRoleRouting(providerId: string, config: CodexAgentRoleRouting) {
       await updateProvider(providerId, {
           meta: { codexAgentRoleRouting: config }
       });
       
       // 触发配置重载
       await reloadCodexRoleRouting(providerId);
       
       showSuccess('配置已保存并应用');
   }
   ```

---

### 方案 2：DeepSeek Harness 客户端支持

#### 2.1 DeepSeek Provider 预设模板

**实现路径**：
通过 Universal Provider 系统添加 DeepSeek 专用模板

**代码实现**：

```typescript
// src/config/universalProviderPresets.ts
export const DeepSeekPreset: UniversalProviderPreset = {
  id: 'deepseek-v4',
  name: 'DeepSeek V4',
  providerType: 'openai_compatible',
  baseUrl: 'https://api.deepseek.com/v1',
  websiteUrl: 'https://deepseek.com',
  icon: 'deepseek',
  iconColor: '#0066FF',
  category: 'reasoning',
  apps: {
    claude: true,
    codex: true,
    gemini: false,
  },
  models: {
    claude: {
      model: 'deepseek-v4-pro',
      haikuModel: 'deepseek-v4-flash',
      sonnetModel: 'deepseek-v4-pro',
      opusModel: 'deepseek-v4-pro',
    },
    codex: {
      model: 'deepseek-v4-pro',
      reasoningEffort: 'high',
    },
  },
  meta: {
    apiFormat: 'openai_chat',
    costMultiplier: '0.1', // DeepSeek 成本是 GPT-4 的 1/10
    pricingModelSource: 'request',
  },
};
```

#### 2.2 DeepSeek Harness 配置导出

**实现路径**：
添加 "导出到 DeepSeek Harness" 功能

**代码实现**：

```rust
// src-tauri/src/services/provider/export_deepseek.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct DeepSeekHarnessConfig {
    pub plugins: Vec<DeepSeekPlugin>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeepSeekPlugin {
    pub name: String,
    pub enabled: bool,
    pub config: serde_json::Value,
}

pub fn export_provider_to_deepseek_harness(
    provider: &Provider,
) -> Result<DeepSeekHarnessConfig, AppError> {
    let base_url = provider.settings_config
        .pointer("/env/ANTHROPIC_BASE_URL")
        .or_else(|| provider.settings_config.get("baseUrl"))
        .and_then(|v| v.as_str())
        .unwrap_or("https://api.deepseek.com/v1");
    
    let api_key = provider.settings_config
        .pointer("/env/ANTHROPIC_AUTH_TOKEN")
        .or_else(|| provider.settings_config.get("apiKey"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    
    let model = provider.settings_config
        .pointer("/env/ANTHROPIC_MODEL")
        .or_else(|| provider.settings_config.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or("deepseek-v4-pro");
    
    Ok(DeepSeekHarnessConfig {
        plugins: vec![
            DeepSeekPlugin {
                name: "@deepseek/model-provider".into(),
                enabled: true,
                config: serde_json::json!({
                    "baseUrl": base_url,
                    "apiKey": api_key,
                    "defaultModel": model,
                }),
            },
            DeepSeekPlugin {
                name: "@deepseek/coding-agent".into(),
                enabled: true,
                config: serde_json::json!({
                    "model": model,
                    "maxTokens": 8192,
                }),
            },
        ],
    })
}
```

**前端集成**：

```typescript
// src/components/providers/ProviderActions.tsx
async function exportToDeepSeekHarness(providerId: string) {
  const config = await invoke<DeepSeekHarnessConfig>(
    'export_provider_to_deepseek_harness',
    { providerId }
  );
  
  const yaml = jsyaml.dump(config);
  const blob = new Blob([yaml], { type: 'text/yaml' });
  const url = URL.createObjectURL(blob);
  
  const a = document.createElement('a');
  a.href = url;
  a.download = `deepseek-harness-${providerId}.yaml`;
  a.click();
  
  URL.revokeObjectURL(url);
  showSuccess('已导出 DeepSeek Harness 配置');
}
```

---

### 方案 3：Pi Harness 客户端支持

#### 3.1 Pi Provider 预设模板

**实现路径**：
通过 OpenCode Provider 系统添加 Pi 支持（Pi 使用 AI SDK 兼容格式）

**代码实现**：

```typescript
// src/config/piProviderPresets.ts
export const PiAnthropicPreset: OpenCodeProviderPreset = {
  id: 'pi-anthropic',
  name: 'Pi (Anthropic)',
  npm: '@ai-sdk/anthropic',
  icon: 'pi',
  iconColor: '#7C3AED',
  options: {
    apiKey: '{env:ANTHROPIC_API_KEY}',
    baseURL: 'https://api.anthropic.com/v1',
  },
  models: {
    'claude-sonnet-4-20250514': {
      name: 'Claude Sonnet 4',
      limit: { context: 200000, output: 8192 },
    },
  },
};

export const PiOpenAICompatiblePreset: OpenCodeProviderPreset = {
  id: 'pi-openai-compatible',
  name: 'Pi (OpenAI Compatible)',
  npm: '@ai-sdk/openai-compatible',
  icon: 'pi',
  iconColor: '#7C3AED',
  options: {
    apiKey: '{env:API_KEY}',
    baseURL: '{env:BASE_URL}',
  },
  models: {},
};
```

#### 3.2 Pi 配置导出

**实现路径**：
导出 CC Switch Provider 为 Pi 配置格式

**代码实现**：

```rust
// src-tauri/src/services/provider/export_pi.rs
#[derive(Debug, Serialize)]
pub struct PiConfig {
    pub providers: HashMap<String, PiProvider>,
    pub default_provider: String,
}

#[derive(Debug, Serialize)]
pub struct PiProvider {
    pub sdk: String,
    pub options: PiProviderOptions,
    pub models: Vec<PiModel>,
}

#[derive(Debug, Serialize)]
pub struct PiProviderOptions {
    #[serde(rename = "apiKey")]
    pub api_key: String,
    #[serde(rename = "baseURL", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PiModel {
    pub id: String,
    pub name: String,
}

pub fn export_provider_to_pi(
    provider: &Provider,
    app_type: &AppType,
) -> Result<PiConfig, AppError> {
    let (base_url, api_key) = provider.resolve_usage_credentials(app_type);
    
    let sdk = match app_type {
        AppType::Claude | AppType::ClaudeDesktop => "@ai-sdk/anthropic",
        AppType::Codex => "@ai-sdk/openai",
        _ => "@ai-sdk/openai-compatible",
    };
    
    let models = extract_models_from_provider(provider);
    
    let pi_provider = PiProvider {
        sdk: sdk.into(),
        options: PiProviderOptions {
            api_key,
            base_url: (!base_url.is_empty()).then_some(base_url),
        },
        models,
    };
    
    Ok(PiConfig {
        providers: [(provider.id.clone(), pi_provider)].into(),
        default_provider: provider.id.clone(),
    })
}
```

**前端集成**：

```typescript
// src/components/providers/ProviderActions.tsx
async function exportToPiHarness(providerId: string) {
  const config = await invoke<PiConfig>('export_provider_to_pi', {
    providerId,
    appType: 'codex',
  });
  
  const json = JSON.stringify(config, null, 2);
  const blob = new Blob([json], { type: 'application/json' });
  const url = URL.createObjectURL(blob);
  
  const a = document.createElement('a');
  a.href = url;
  a.download = `pi-config-${providerId}.json`;
  a.click();
  
  URL.revokeObjectURL(url);
  showSuccess('已导出 Pi Harness 配置');
}
```

#### 3.3 Pi 扩展包生成

**实现路径**：
生成可直接安装的 Pi 扩展包

**代码实现**：

```typescript
// src/lib/pi/extension-builder.ts
interface PiExtension {
  name: string;
  version: string;
  description: string;
  exports: {
    providers?: PiProviderExport[];
    tools?: PiToolExport[];
    skills?: PiSkillExport[];
  };
}

export function buildPiExtension(provider: Provider): PiExtension {
  return {
    name: `@cc-switch/${provider.id}`,
    version: '1.0.0',
    description: `CC Switch provider: ${provider.name}`,
    exports: {
      providers: [{
        id: provider.id,
        name: provider.name,
        sdk: inferAiSdkPackage(provider),
        options: extractProviderOptions(provider),
        models: extractModels(provider),
      }],
    },
  };
}

// 生成 package.json
export function generatePiExtensionPackage(extension: PiExtension): object {
  return {
    name: extension.name,
    version: extension.version,
    description: extension.description,
    main: './index.js',
    type: 'module',
    peerDependencies: {
      'pi': '^1.0.0',
    },
  };
}
```

---

## 实施计划

### 阶段 1：修复前端/后端代理路由（优先级：高）

**时间**：2-3 天

1. **Day 1**：
   - 实现配置文件路径验证与修复
   - 添加详细日志和错误处理
   - Windows 路径特殊处理

2. **Day 2**：
   - 路由令牌验证机制加固
   - 本地代理监听地址检测
   - 前端启用前验证

3. **Day 3**：
   - 配置重载通知机制
   - 集成测试
   - 文档更新

### 阶段 2：DeepSeek Harness 支持（优先级：中）

**时间**：1-2 天

1. **Day 1**：
   - DeepSeek Provider 预设模板
   - 配置导出功能
   - 前端界面集成

2. **Day 2**：
   - 测试与验证
   - 使用文档编写

### 阶段 3：Pi Harness 支持（优先级：中）

**时间**：2-3 天

1. **Day 1**：
   - Pi Provider 预设模板
   - 基础配置导出

2. **Day 2**：
   - Pi 扩展包生成
   - AI SDK 兼容性测试

3. **Day 3**：
   - 前端界面完善
   - 使用文档编写

---

## 测试计划

### 前端/后端代理路由测试

1. **配置文件写入测试**
   ```bash
   # 验证文件是否正确生成
   ls ~/.codex/cc-switch-frontend.toml
   ls ~/.codex/cc-switch-backend.toml
   
   # 检查文件内容
   cat ~/.codex/cc-switch-frontend.toml
   ```

2. **路由令牌验证测试**
   ```rust
   #[test]
   fn test_role_route_token_persistence() {
       let token1 = create_codex_role_route_token("owner", "frontend", &routing);
       // 重启应用
       let token2 = create_codex_role_route_token("owner", "frontend", &routing);
       assert_eq!(token1, token2, "Token should be consistent across restarts");
   }
   ```

3. **监听地址检测测试**
   ```rust
   #[tokio::test]
   async fn test_loopback_address_validation() {
       assert!(is_loopback_address("127.0.0.1"));
       assert!(is_loopback_address("::1"));
       assert!(is_loopback_address("localhost"));
       assert!(!is_loopback_address("0.0.0.0"));
       assert!(!is_loopback_address("192.168.1.1"));
   }
   ```

### DeepSeek Harness 集成测试

1. **配置导出测试**
   ```typescript
   test('exports DeepSeek Harness config', async () => {
       const config = await exportToDeepSeekHarness('deepseek-provider');
       expect(config.plugins).toHaveLength(2);
       expect(config.plugins[0].name).toBe('@deepseek/model-provider');
   });
   ```

2. **API 兼容性测试**
   ```bash
   # 使用导出的配置测试 DeepSeek API
   curl https://api.deepseek.com/v1/chat/completions \
     -H "Authorization: Bearer $API_KEY" \
     -H "Content-Type: application/json" \
     -d '{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"Hello"}]}'
   ```

### Pi Harness 集成测试

1. **配置导出测试**
   ```typescript
   test('exports Pi config', async () => {
       const config = await exportToPi('anthropic-provider');
       expect(config.providers).toHaveProperty('anthropic-provider');
       expect(config.providers['anthropic-provider'].sdk).toBe('@ai-sdk/anthropic');
   });
   ```

2. **扩展包生成测试**
   ```typescript
   test('generates Pi extension package', () => {
       const extension = buildPiExtension(provider);
       const pkg = generatePiExtensionPackage(extension);
       expect(pkg.name).toMatch(/^@cc-switch\//);
       expect(pkg.peerDependencies).toHaveProperty('pi');
   });
   ```

---

## 风险与缓解

### 风险 1：Windows 文件权限问题

**缓解措施**：
- 实现降级方案：配置文件写入失败时，提供手动配置指引
- 添加权限检测工具，在启用功能前验证文件写入权限
- 提供管理员权限运行选项

### 风险 2：路由令牌密钥丢失

**缓解措施**：
- 实现密钥备份机制
- 密钥丢失时自动重新生成并更新所有相关配置
- 提供密钥导入/导出功能

### 风险 3：DeepSeek/Pi Harness API 变更

**缓解措施**：
- 使用版本化配置导出（在导出文件中记录 CC Switch 版本）
- 实现配置迁移工具，支持旧版本配置升级
- 定期更新 API 兼容性测试

### 风险 4：配置同步延迟

**缓解措施**：
- 实现配置变更通知机制
- 添加强制重载按钮
- 提供配置验证工具，确认生效状态

---

## 成功指标

### 前端/后端代理路由

- ✅ 配置文件写入成功率 > 99%
- ✅ 路由令牌验证失败率 < 1%
- ✅ 配置重载响应时间 < 2 秒
- ✅ Windows/macOS/Linux 三平台功能一致性

### DeepSeek Harness 支持

- ✅ 配置导出无错误
- ✅ DeepSeek API 调用成功率 > 95%
- ✅ 协议转换无损失（请求/响应完整性 100%）

### Pi Harness 支持

- ✅ 配置导出格式正确性 100%
- ✅ AI SDK 兼容性覆盖主流 Provider（Anthropic、OpenAI、OpenRouter）
- ✅ 扩展包可直接安装使用

---

## 参考资料

1. **CC Switch 文档**
   - [Codex DeepSeek 路由指南](https://github.com/farion1231/cc-switch/blob/main/docs/guides/codex-deepseek-routing-guide-en.md)
   - [项目 README](../README.md)

2. **DeepSeek 文档**
   - [DeepSeek API 文档](https://api-docs.deepseek.com/)
   - [DeepSeek Harness 发布公告](https://deepseek.com/harness/en/)

3. **Pi 文档**
   - [Pi 官方网站](https://pi.dev/)
   - [AI SDK Harness Agent API](https://vercel.com/changelog/program-agent-harnesses-with-ai-sdk)

4. **代码引用**
   - `src-tauri/src/services/codex_agent_roles.rs` - 角色路由实现
   - `src-tauri/src/proxy/provider_router.rs` - Provider 路由器
   - `src-tauri/src/provider.rs` - Provider 数据结构

---

## 更新日志

- **2026-08-14**：初始方案创建
  - 分析前端/后端代理路由功能现状
  - 调研 DeepSeek Harness v0.1 集成需求
  - 调研 Pi Harness 集成方向
  - 制定三阶段实施计划
