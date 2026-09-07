import { fireEvent, screen } from "@testing-library/react";
import {
  afterAll,
  beforeAll,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import { ProfileSwitcher } from "@/components/profiles/ProfileSwitcher";
import {
  initializeSafetyI18n,
  managementFixture,
  renderSafetyUi,
  safetyI18n,
} from "../utils/safetyTestUtils";

const actions = vi.hoisted(() => ({
  apply: vi.fn(),
  clear: vi.fn(),
  create: vi.fn(),
}));
vi.mock("@/lib/query/profiles", () => ({
  useProfilesQuery: () => ({
    data: {
      currentIds: {
        claude: "current",
        claudeDesktop: "current",
        codex: "current",
      },
      profiles: ["current", "other"].map((id) => ({
        id,
        name: `Project ${id}`,
        payload: { providers: {}, mcp: {}, skills: {}, prompts: {} },
      })),
    },
  }),
  useApplyProfileMutation: () => ({ mutate: actions.apply }),
  useClearProfileMutation: () => ({ mutate: actions.clear }),
  useCreateProfileMutation: () => ({
    mutate: actions.create,
    isPending: false,
  }),
}));
vi.mock("@/components/profiles/ProfileManageDialog", () => ({
  ProfileManageDialog: () => null,
}));

beforeEach(async () => {
  await initializeSafetyI18n();
  vi.clearAllMocks();
});
const scrollDescriptor = Object.getOwnPropertyDescriptor(
  HTMLElement.prototype,
  "scrollIntoView",
);
beforeAll(() =>
  Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
    configurable: true,
    value: vi.fn(),
  }),
);
afterAll(() => {
  if (scrollDescriptor)
    Object.defineProperty(
      HTMLElement.prototype,
      "scrollIntoView",
      scrollDescriptor,
    );
  else Reflect.deleteProperty(HTMLElement.prototype, "scrollIntoView");
});

describe("profile application follows local app management", () => {
  it.each(["claude", "claude-desktop", "codex"] as const)(
    "disables %s profile application and clearing but retains record management",
    (appId) => {
      renderSafetyUi(
        <ProfileSwitcher activeApp={appId} />,
        managementFixture({ [appId]: { enabled: false, phase: "unmanaged" } }),
      );
      fireEvent.click(screen.getByRole("combobox"));
      const option = screen.getByRole("option", { name: /Project other/ });
      expect(option).toHaveAttribute("aria-disabled", "true");
      expect(
        screen.getByRole("option", { name: safetyI18n.t("profiles.none") }),
      ).toHaveAttribute("aria-disabled", "true");
      expect(
        screen.getByRole("option", { name: safetyI18n.t("profiles.manage") }),
      ).not.toHaveAttribute("aria-disabled", "true");
      fireEvent.click(option);
      expect(actions.apply).not.toHaveBeenCalled();
      expect(actions.clear).not.toHaveBeenCalled();
    },
  );

  it("leaves managed Codex operable when Claude is stopped", () => {
    renderSafetyUi(
      <ProfileSwitcher activeApp="codex" />,
      managementFixture({ claude: { enabled: false, phase: "unmanaged" } }),
    );
    fireEvent.click(screen.getByRole("combobox"));
    fireEvent.click(screen.getByRole("option", { name: /Project other/ }));
    expect(actions.apply).toHaveBeenCalledWith({ id: "other", scope: "codex" });
  });
});
