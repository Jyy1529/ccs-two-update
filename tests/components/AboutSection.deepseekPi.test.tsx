import { render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { AboutSection } from "@/components/settings/AboutSection";

const mocks = vi.hoisted(() => ({
  getToolVersions: vi.fn(async (tools: string[] = []) =>
    tools.map((name) => ({
      name,
      version: null,
      latest_version: "1.0.0",
      error: "not installed",
      installed_but_broken: false,
      env_type: "windows" as const,
      wsl_distro: null,
    })),
  ),
}));

vi.mock("@tauri-apps/api/app", () => ({
  getVersion: vi.fn().mockResolvedValue("3.19.4"),
}));

vi.mock("@/contexts/UpdateContext", () => ({
  useUpdate: () => ({
    hasUpdate: false,
    updateInfo: null,
    checkUpdate: vi.fn().mockResolvedValue(false),
    resetDismiss: vi.fn(),
    isChecking: false,
  }),
}));

vi.mock("@/lib/api", () => ({
  settingsApi: {
    getToolVersions: mocks.getToolVersions,
  },
}));

describe("AboutSection DeepSeek/Pi tools", () => {
  it("shows and probes the official dsh and pi CLIs with the official Pi mark", async () => {
    const { container } = render(<AboutSection isPortable={false} />);

    await waitFor(() => {
      expect(screen.getByText("DeepSeek Harness")).toBeInTheDocument();
      expect(
        screen.getByText("Pi", { selector: "div.truncate" }),
      ).toBeInTheDocument();
      const requestedTools = mocks.getToolVersions.mock.calls.flatMap(
        ([tools]) => tools,
      );
      expect(requestedTools).toEqual(expect.arrayContaining(["dsh", "pi"]));
    });

    const piLogo = container.querySelector(
      '[title="Pi"] svg[viewBox="0 0 800 800"]',
    );
    expect(piLogo).not.toBeNull();
    expect(piLogo?.querySelectorAll("path")).toHaveLength(2);
    expect(piLogo?.querySelector("circle")).toBeNull();
  });
});
