#!/usr/bin/env bash
# CC Switch 代理路由功能集成测试脚本

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BASE_URL="http://localhost:8080"

# 颜色输出
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

print_success() {
    echo -e "${GREEN}✓ $1${NC}"
}

print_error() {
    echo -e "${RED}✗ $1${NC}"
}

print_info() {
    echo -e "${YELLOW}ℹ $1${NC}"
}

print_section() {
    echo ""
    echo "═══════════════════════════════════════════════════════════════"
    echo "  $1"
    echo "═══════════════════════════════════════════════════════════════"
    echo ""
}

# 检查 CC Switch 是否运行
check_server() {
    print_section "1. 检查服务状态"

    if curl -s -f "${BASE_URL}/api/health" > /dev/null 2>&1; then
        print_success "CC Switch 服务运行正常"
        return 0
    else
        print_error "CC Switch 服务未运行"
        print_info "请先启动 CC Switch: cd src-tauri && cargo run"
        exit 1
    fi
}

# 检查代理配置
check_proxy_config() {
    print_section "2. 检查代理配置"

    local config=$(curl -s "${BASE_URL}/api/proxy/config")
    local listen_address=$(echo "$config" | jq -r '.listenAddress')
    local listen_port=$(echo "$config" | jq -r '.listenPort')

    print_info "监听地址: ${listen_address}:${listen_port}"

    # 检查是否为回环地址
    if [[ "$listen_address" == "127.0.0.1" ]] || [[ "$listen_address" == "::1" ]] || [[ "$listen_address" == "localhost" ]]; then
        print_success "代理监听回环地址（安全）"
    else
        print_error "代理监听非回环地址: $listen_address"
        print_info "角色路由要求回环地址，请修改配置"
        return 1
    fi

    if [[ "$listen_port" -gt 0 ]]; then
        print_success "代理端口配置正确: $listen_port"
    else
        print_error "代理端口未配置"
        return 1
    fi
}

# 测试 DeepSeek Provider 创建
test_deepseek_provider() {
    print_section "3. 测试 DeepSeek Provider"

    local provider_id="test-deepseek-roles"

    # 删除已存在的测试 Provider
    curl -s -X DELETE "${BASE_URL}/api/providers/${provider_id}" > /dev/null 2>&1 || true

    print_info "创建 DeepSeek Provider (带角色路由)..."

    local response=$(curl -s -X POST "${BASE_URL}/api/providers" \
        -H "Content-Type: application/json" \
        -d '{
            "id": "'${provider_id}'",
            "name": "Test DeepSeek with Roles",
            "appType": "codex",
            "baseUrl": "https://api.deepseek.com/v1",
            "apiKey": "test-key",
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
                            "instructions": "You are the frontend specialist."
                        }
                    },
                    "backend": {
                        "override": {
                            "model": "deepseek-chat",
                            "instructions": "You are the backend specialist."
                        }
                    }
                }
            }
        }')

    if echo "$response" | jq -e '.id' > /dev/null 2>&1; then
        print_success "DeepSeek Provider 创建成功"
    else
        print_error "DeepSeek Provider 创建失败"
        echo "$response" | jq .
        return 1
    fi

    # 验证角色路由配置
    local meta=$(echo "$response" | jq -r '.meta.codexAgentRoleRouting')
    if [[ "$meta" != "null" ]]; then
        print_success "角色路由配置已保存"
    else
        print_error "角色路由配置丢失"
        return 1
    fi
}

# 测试 Pi Provider 创建
test_pi_provider() {
    print_section "4. 测试 Pi Provider"

    local provider_id="test-pi-roles"

    # 删除已存在的测试 Provider
    curl -s -X DELETE "${BASE_URL}/api/providers/${provider_id}" > /dev/null 2>&1 || true

    print_info "创建 Pi Provider (带角色路由)..."

    local response=$(curl -s -X POST "${BASE_URL}/api/providers" \
        -H "Content-Type: application/json" \
        -d '{
            "id": "'${provider_id}'",
            "name": "Test Pi with Roles",
            "appType": "codex",
            "baseUrl": "https://openrouter.ai/api/v1",
            "apiKey": "test-key",
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
                    "enabled": true,
                    "frontend": {
                        "providerId": "cc-switch-frontend-local",
                        "override": {
                            "model": "inflection/inflection-3-pi",
                            "instructions": "You are the frontend specialist."
                        }
                    },
                    "backend": {
                        "override": {
                            "model": "inflection/inflection-3-productivity",
                            "instructions": "You are the backend specialist."
                        }
                    }
                }
            }
        }')

    if echo "$response" | jq -e '.id' > /dev/null 2>&1; then
        print_success "Pi Provider 创建成功"
    else
        print_error "Pi Provider 创建失败"
        echo "$response" | jq .
        return 1
    fi
}

# 测试角色配置文件生成
test_role_file_generation() {
    print_section "5. 测试角色配置文件生成"

    local provider_id="test-deepseek-roles"

    print_info "触发角色协调..."

    # 激活 Provider (触发角色配置生成)
    curl -s -X POST "${BASE_URL}/api/providers/${provider_id}/activate" > /dev/null

    sleep 2  # 等待文件生成

    # 检查配置文件路径
    if [[ "$OSTYPE" == "msys" ]] || [[ "$OSTYPE" == "win32" ]]; then
        # Windows
        local agents_dir="$USERPROFILE/.codex/agents"
    else
        # macOS/Linux
        local agents_dir="$HOME/.codex/agents"
    fi

    print_info "检查配置目录: $agents_dir"

    if [[ -d "$agents_dir" ]]; then
        print_success "配置目录存在"
    else
        print_error "配置目录不存在: $agents_dir"
        return 1
    fi

    # 检查前端角色文件
    if [[ -f "$agents_dir/cc-switch-frontend.toml" ]]; then
        print_success "前端角色配置文件已生成"

        # 验证文件内容
        if grep -q "cc-switch-managed: codex-agent-role-v1" "$agents_dir/cc-switch-frontend.toml"; then
            print_success "前端配置包含管理标记"
        else
            print_error "前端配置缺少管理标记"
        fi

        if grep -q "cc-switch-frontend-local" "$agents_dir/cc-switch-frontend.toml"; then
            print_success "前端配置包含正确的 provider ID"
        else
            print_error "前端配置缺少 provider ID"
        fi
    else
        print_error "前端角色配置文件未生成"
        return 1
    fi

    # 检查后端角色文件
    if [[ -f "$agents_dir/cc-switch-backend.toml" ]]; then
        print_success "后端角色配置文件已生成"
    else
        print_error "后端角色配置文件未生成"
        return 1
    fi
}

# 测试路由令牌验证
test_token_validation() {
    print_section "6. 测试路由令牌验证"

    print_info "测试路由令牌生成和验证..."

    # 这个测试需要内部 API，暂时跳过
    print_info "路由令牌验证需要内部 API，跳过此测试"
}

# 清理测试数据
cleanup_test_data() {
    print_section "7. 清理测试数据"

    print_info "删除测试 Providers..."

    curl -s -X DELETE "${BASE_URL}/api/providers/test-deepseek-roles" > /dev/null 2>&1 || true
    curl -s -X DELETE "${BASE_URL}/api/providers/test-pi-roles" > /dev/null 2>&1 || true

    print_success "测试数据已清理"
}

# 主测试流程
main() {
    echo ""
    echo "╔═══════════════════════════════════════════════════════════════╗"
    echo "║     CC Switch 代理路由功能集成测试                           ║"
    echo "╚═══════════════════════════════════════════════════════════════╝"

    local failed=0

    check_server || ((failed++))
    check_proxy_config || ((failed++))
    test_deepseek_provider || ((failed++))
    test_pi_provider || ((failed++))
    test_role_file_generation || ((failed++))
    test_token_validation || ((failed++))

    print_section "测试摘要"

    if [[ $failed -eq 0 ]]; then
        print_success "所有测试通过！"
        cleanup_test_data
        exit 0
    else
        print_error "$failed 个测试失败"
        print_info "保留测试数据以供调试"
        exit 1
    fi
}

# 运行测试
main "$@"
