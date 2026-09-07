import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

describe("initial interface language", () => {
  beforeEach(() => {
    vi.resetModules();
    localStorage.removeItem("language");
    vi.spyOn(navigator, "language", "get").mockReturnValue("en-US");
  });

  afterEach(() => {
    localStorage.removeItem("language");
    vi.restoreAllMocks();
  });

  it("defaults to simplified Chinese without a saved preference", async () => {
    const { default: i18n } = await import("@/i18n");
    expect(i18n.language).toBe("zh");
    expect(i18n.t("providerGroups.autoGrouping")).toBe("按 Base URL 自动分组");
  });

  it.each(["zh", "zh-TW", "en", "ja"])(
    "preserves the saved %s preference",
    async (language) => {
      localStorage.setItem("language", language);
      const { default: i18n } = await import("@/i18n");
      expect(i18n.language).toBe(language);
    },
  );

  it("uses Chinese when a saved language is unsupported", async () => {
    localStorage.setItem("language", "unsupported");
    const { default: i18n } = await import("@/i18n");
    expect(i18n.language).toBe("zh");
  });
});
