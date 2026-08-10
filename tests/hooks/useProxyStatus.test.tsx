import type { ReactNode } from "react";
import { renderHook, act, waitFor } from "@testing-library/react";
import { QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { useProxyStatus } from "@/hooks/useProxyStatus";
import { createTestQueryClient } from "../utils/testQueryClient";

const toastSuccessMock = vi.fn();
const toastErrorMock = vi.fn();
const invokeMock = vi.fn();

vi.mock("sonner", () => ({
  toast: {
    success: (...args: unknown[]) => toastSuccessMock(...args),
    error: (...args: unknown[]) => toastErrorMock(...args),
  },
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, options?: Record<string, unknown>) => {
      if (key === "proxy.server.started") {
        return `代理服务已启动 - ${options?.address}:${options?.port}`;
      }

      if (key === "providerAdvanced.agentRoleProxyDisableBlocked") {
        return "当前子代理角色路由依赖 Codex 代理接管，请先关闭角色路由。";
      }

      if (key === "providerAdvanced.agentRoleLoopbackRequired") {
        return "前端子代理独立 Provider 路由要求 Codex 本地代理监听回环地址（127.0.0.1 或 ::1）。请修改监听地址后重试。";
      }

      if (typeof options?.defaultValue === "string") {
        return options.defaultValue;
      }

      return key;
    },
  }),
}));

interface WrapperProps {
  children: ReactNode;
}

function createWrapper() {
  const queryClient = createTestQueryClient();

  const wrapper = ({ children }: WrapperProps) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );

  return { wrapper, queryClient };
}

describe("useProxyStatus", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    toastSuccessMock.mockReset();
    toastErrorMock.mockReset();

    invokeMock.mockImplementation((command: string) => {
      if (command === "get_proxy_status") {
        return Promise.resolve({
          running: false,
          address: "127.0.0.1",
          port: 15721,
          active_connections: 0,
          total_requests: 0,
          success_requests: 0,
          failed_requests: 0,
          success_rate: 0,
          uptime_seconds: 0,
          current_provider: null,
          current_provider_id: null,
          last_request_at: null,
          last_error: null,
          failover_count: 0,
        });
      }

      if (command === "get_proxy_takeover_status") {
        return Promise.resolve({
          claude: false,
          codex: false,
          gemini: false,
          grokbuild: false,
          opencode: false,
          openclaw: false,
        });
      }

      if (command === "start_proxy_server") {
        return Promise.resolve({
          address: "127.0.0.1",
          port: 15721,
          started_at: "2026-03-10T00:00:00Z",
        });
      }

      return Promise.resolve(null);
    });
  });

  it("shows interpolated address and port after proxy server starts", async () => {
    const { wrapper } = createWrapper();
    const { result } = renderHook(() => useProxyStatus(), { wrapper });

    await waitFor(() => {
      expect(result.current.isLoading).toBe(false);
    });

    await act(async () => {
      await result.current.startProxyServer();
    });

    expect(toastSuccessMock).toHaveBeenCalledWith(
      "代理服务已启动 - 127.0.0.1:15721",
      { closeButton: true },
    );
  });

  it("shows the localized role-route message when Codex takeover cannot be disabled", async () => {
    invokeMock.mockImplementation((command: string) => {
      if (command === "get_proxy_status") {
        return Promise.resolve({
          running: true,
          address: "127.0.0.1",
          port: 15721,
        });
      }
      if (command === "get_proxy_takeover_status") {
        return Promise.resolve({ codex: true });
      }
      if (command === "set_proxy_takeover_for_app") {
        return Promise.reject(
          new Error(
            "codex_agent_role_proxy_required: disable role routing first",
          ),
        );
      }
      return Promise.resolve(null);
    });
    const { wrapper } = createWrapper();
    const { result } = renderHook(() => useProxyStatus(), { wrapper });

    await waitFor(() => expect(result.current.isLoading).toBe(false));
    await expect(
      result.current.setTakeoverForApp({ appType: "codex", enabled: false }),
    ).rejects.toThrow("codex_agent_role_proxy_required");

    await waitFor(() => {
      expect(toastErrorMock).toHaveBeenCalledWith(
        "当前子代理角色路由依赖 Codex 代理接管，请先关闭角色路由。",
      );
    });
  });

  it("shows the localized loopback requirement when the proxy cannot start", async () => {
    invokeMock.mockImplementation((command: string) => {
      if (command === "get_proxy_status") {
        return Promise.resolve({ running: false });
      }
      if (command === "get_proxy_takeover_status") {
        return Promise.resolve({ codex: false });
      }
      if (command === "start_proxy_server") {
        return Promise.reject(
          new Error(
            "codex_agent_role_loopback_required: listen address must be loopback",
          ),
        );
      }
      return Promise.resolve(null);
    });
    const { wrapper } = createWrapper();
    const { result } = renderHook(() => useProxyStatus(), { wrapper });

    await waitFor(() => expect(result.current.isLoading).toBe(false));
    await expect(result.current.startProxyServer()).rejects.toThrow(
      "codex_agent_role_loopback_required",
    );

    await waitFor(() => {
      expect(toastErrorMock).toHaveBeenCalledWith(
        "前端子代理独立 Provider 路由要求 Codex 本地代理监听回环地址（127.0.0.1 或 ::1）。请修改监听地址后重试。",
      );
    });
  });
});
