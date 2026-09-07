import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import type { ComponentProps, ReactNode } from "react";
import {
  DndContext,
  KeyboardSensor,
  PointerSensor,
  closestCenter,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import { sortableKeyboardCoordinates } from "@dnd-kit/sortable";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ProviderGroupSections } from "@/components/providers/ProviderGroupSections";
import { providerGroupsApi } from "@/lib/api/providerGroups";
import { createTestQueryClient } from "../utils/testQueryClient";
import type { BalanceQueryResult, Provider, ProviderGroup } from "@/types";
import { toast } from "sonner";
import { I18nextProvider } from "react-i18next";
import { initializeSafetyI18n, safetyI18n } from "../utils/safetyTestUtils";

vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() } }));

const groups: ProviderGroup[] = ["First", "Second"].map((name, sortIndex) => ({
  id: name,
  name,
  appType: "codex",
  kind: "manual",
  normalizedBaseUrl: null,
  sortIndex,
  collapsed: true,
  keyPoolEnabled: false,
  keyPoolStrategy: "failover",
  keyPoolMaxRetries: 0,
  keyPoolCooldownMs: 1000,
  balanceTemplateId: null,
  createdAt: 1,
  updatedAt: 1,
}));
const provider: Provider = { id: "key-a", name: "Key A", settingsConfig: {} };

function DragSurface({ children }: { children: ReactNode }) {
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 8 } }),
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
    }),
  );
  return (
    <DndContext sensors={sensors} collisionDetection={closestCenter}>
      {children}
    </DndContext>
  );
}

function mockFolderGeometry() {
  class TestPointerEvent extends MouseEvent {
    readonly pointerId: number;
    readonly isPrimary: boolean;
    constructor(type: string, init: PointerEventInit = {}) {
      super(type, init);
      this.pointerId = init.pointerId ?? 1;
      this.isPrimary = init.isPrimary ?? true;
    }
  }
  vi.stubGlobal("PointerEvent", TestPointerEvent);
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
    function (this: HTMLElement) {
      const folder = this.closest("section[aria-label]");
      const folders = Array.from(
        document.querySelectorAll("section[aria-label]"),
      );
      const index = folder ? folders.indexOf(folder) : 0;
      return new DOMRect(
        0,
        Math.max(0, index) * 100,
        600,
        this === folder ? 80 : 28,
      );
    },
  );
}

async function startFolderPointerDrag(name = "First") {
  const handle = screen.getByRole("button", { name: `Drag folder ${name}` });
  fireEvent.pointerDown(handle, { button: 0, clientX: 20, clientY: 14 });
  fireEvent.pointerMove(document, { clientX: 20, clientY: 30 });
  await waitFor(() => expect(handle).toHaveAttribute("aria-pressed", "true"));
  return handle;
}

function setup(
  overrides: Partial<ComponentProps<typeof ProviderGroupSections>> = {},
) {
  const client = createTestQueryClient();
  const props: ComponentProps<typeof ProviderGroupSections> = {
    appId: "codex",
    groups,
    providers: [provider],
    allProviders: overrides.providers ?? [provider],
    autoGrouping: false,
    autoGroupingPending: false,
    onAutoGroupingChange: vi.fn(),
    onToggleCollapsed: vi.fn(),
    renderProvider: (item, actions, balance) => (
      <div data-testid={`provider-card-${item.id}`}>
        <span>{item.name}</span>
        <div>{actions}</div>
        {balance}
      </div>
    ),
    ...overrides,
  };
  const tree = (changes: Partial<typeof props> = {}) => (
    <QueryClientProvider client={client}>
      <I18nextProvider i18n={safetyI18n}>
        <DragSurface key={changes.appId ?? props.appId}>
          <ProviderGroupSections {...props} {...changes} />
        </DragSurface>
      </I18nextProvider>
    </QueryClientProvider>
  );
  const rendered = render(tree());
  return {
    rerender: (changes: Partial<typeof props>) =>
      rendered.rerender(tree(changes)),
  };
}

describe("ProviderGroupSections", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  beforeEach(async () => {
    await initializeSafetyI18n();
    vi.spyOn(providerGroupsApi, "listBalanceTemplates").mockResolvedValue([]);
    vi.spyOn(providerGroupsApi, "reorder").mockResolvedValue(undefined);
    vi.spyOn(providerGroupsApi, "moveProvider").mockResolvedValue(undefined);
  });

  it("keeps balance queries and offers template settings without any folders", async () => {
    vi.spyOn(providerGroupsApi, "setProviderBalanceTemplate").mockResolvedValue(
      undefined,
    );
    setup({ groups: [] });
    expect(
      screen.getByRole("button", { name: "Query balance for Key A" }),
    ).toBeEnabled();
    fireEvent.click(
      screen.getByRole("button", { name: "Balance settings for Key A" }),
    );
    const dialog = await screen.findByRole("dialog", {
      name: "Balance settings for Key A",
    });
    await waitFor(() =>
      expect(within(dialog).getByLabelText("Balance template")).toBeEnabled(),
    );
    expect(
      within(dialog).getByRole("option", { name: "Built-in detection" }),
    ).toBeInTheDocument();
    expect(
      within(dialog).getByRole("button", { name: "New balance template" }),
    ).toBeEnabled();
    fireEvent.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(providerGroupsApi.setProviderBalanceTemplate).toHaveBeenCalledWith(
        "codex",
        "key-a",
        null,
      ),
    );
  });

  it("persists folder order using pointer events without native HTML drag", async () => {
    mockFolderGeometry();
    const onToggleCollapsed = vi.fn();
    setup({ onToggleCollapsed });
    const handle = await startFolderPointerDrag();
    expect(handle).not.toHaveAttribute("draggable", "true");
    fireEvent.pointerMove(document, { clientX: 20, clientY: 114 });
    fireEvent.pointerUp(document);
    await waitFor(() =>
      expect(providerGroupsApi.reorder).toHaveBeenCalledWith("codex", [
        "Second",
        "First",
      ]),
    );
    expect(onToggleCollapsed).not.toHaveBeenCalled();
    expect(toast.success).toHaveBeenCalledWith("Sort order updated");
  });

  it("supports keyboard folder sorting", async () => {
    mockFolderGeometry();
    setup();
    const user = userEvent.setup();
    const handle = screen.getByRole("button", { name: "Drag folder First" });
    act(() => handle.focus());
    await user.keyboard("[Space]");
    await waitFor(() => expect(handle).toHaveAttribute("aria-pressed", "true"));
    await user.keyboard("[ArrowDown][Space]");
    await waitFor(() =>
      expect(providerGroupsApi.reorder).toHaveBeenCalledWith("codex", [
        "Second",
        "First",
      ]),
    );
  });

  it("does not save a click or an unchanged folder position", async () => {
    mockFolderGeometry();
    setup();
    const handle = screen.getByRole("button", { name: "Drag folder First" });
    fireEvent.pointerDown(handle, { button: 0, clientX: 20, clientY: 14 });
    fireEvent.pointerMove(document, { clientX: 20, clientY: 18 });
    fireEvent.pointerUp(document);
    expect(providerGroupsApi.reorder).not.toHaveBeenCalled();
    await startFolderPointerDrag();
    fireEvent.pointerUp(document);
    expect(providerGroupsApi.reorder).not.toHaveBeenCalled();
    expect(toast.success).not.toHaveBeenCalled();
  });

  it("cancels folder dragging with Escape without saving", async () => {
    mockFolderGeometry();
    setup();
    const handle = await startFolderPointerDrag();
    fireEvent.pointerMove(document, { clientX: 20, clientY: 114 });
    fireEvent.keyDown(document, { code: "Escape", key: "Escape" });
    fireEvent.pointerUp(document);
    await waitFor(() =>
      expect(handle).not.toHaveAttribute("aria-pressed", "true"),
    );
    expect(providerGroupsApi.reorder).not.toHaveBeenCalled();
    expect(toast.success).not.toHaveBeenCalled();
  });

  it("disables drag handles while folder order is being saved", async () => {
    mockFolderGeometry();
    let resolveSave = () => {};
    vi.mocked(providerGroupsApi.reorder).mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );
    setup();
    const handle = await startFolderPointerDrag();
    fireEvent.pointerMove(document, { clientX: 20, clientY: 114 });
    fireEvent.pointerUp(document);
    await waitFor(() => expect(handle).toBeDisabled());
    expect(
      screen.getByRole("button", { name: "Drag folder Second" }),
    ).toBeDisabled();
    await act(async () => resolveSave());
    await waitFor(() => expect(handle).toBeEnabled());
    expect(providerGroupsApi.reorder).toHaveBeenCalledTimes(1);
  });

  it("keeps the saved folder order and reports a failed reorder", async () => {
    mockFolderGeometry();
    vi.mocked(providerGroupsApi.reorder).mockRejectedValue(
      new Error("Order save failed"),
    );
    setup();
    await startFolderPointerDrag();
    fireEvent.pointerMove(document, { clientX: 20, clientY: 114 });
    fireEvent.pointerUp(document);
    await waitFor(() =>
      expect(toast.error).toHaveBeenCalledWith("Order save failed"),
    );
    expect(
      screen
        .getAllByRole("region")
        .map((region) => region.getAttribute("aria-label")),
    ).toEqual(["First", "Second"]);
    expect(toast.success).not.toHaveBeenCalled();
  });

  it("cancels an in-progress folder drag when switching applications", async () => {
    mockFolderGeometry();
    const { rerender } = setup();
    await startFolderPointerDrag();
    rerender({
      appId: "gemini",
      groups: groups.map((group) => ({ ...group, appType: "gemini" })),
    });
    fireEvent.pointerMove(document, { clientX: 20, clientY: 114 });
    fireEvent.pointerUp(document);
    expect(providerGroupsApi.reorder).not.toHaveBeenCalled();
    expect(
      screen.getByRole("button", { name: "Drag folder First" }),
    ).not.toHaveAttribute("aria-pressed", "true");
  });

  it("labels folder ownership and moves the independent provider", async () => {
    setup();
    const card = within(screen.getByTestId("provider-card-key-a"));
    expect(
      card.getByRole("combobox", { name: "Folder for Key A" }),
    ).toBeInTheDocument();
    expect(
      card.getByRole("button", { name: "Query balance for Key A" }),
    ).toBeInTheDocument();
    fireEvent.change(
      screen.getByRole("combobox", { name: "Folder for Key A" }),
      { target: { value: "Second" } },
    );
    await waitFor(() =>
      expect(providerGroupsApi.moveProvider).toHaveBeenCalledWith(
        "codex",
        "key-a",
        "Second",
      ),
    );
    expect(screen.getByText("Key A")).toBeInTheDocument();
  });

  it("supports menu sorting without drag and keeps a failed edit open", async () => {
    vi.spyOn(providerGroupsApi, "update").mockRejectedValue(
      new Error("Test save failure"),
    );
    const user = userEvent.setup();
    setup();
    await user.click(
      screen.getAllByRole("button", { name: "Folder actions" })[0],
    );
    await user.click(screen.getByRole("menuitem", { name: "Move down" }));
    await waitFor(() =>
      expect(providerGroupsApi.reorder).toHaveBeenCalledWith("codex", [
        "Second",
        "First",
      ]),
    );
    await user.click(
      screen.getAllByRole("button", { name: "Folder actions" })[0],
    );
    await user.click(screen.getByRole("menuitem", { name: "Rename folder" }));
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(providerGroupsApi.update).toHaveBeenCalled());
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("shows per-Key results, sums only independent quotas and removes totals on failure", async () => {
    const pool = { ...groups[0], collapsed: false, keyPoolEnabled: true };
    const members = ["one", "two"].map((id) => ({
      id,
      name: id,
      settingsConfig: {},
      meta: { providerGroupId: pool.id, keyPoolEnabled: true },
    }));
    vi.spyOn(providerGroupsApi, "queryGroupBalances").mockResolvedValue(
      members.map((p, index) => ({
        providerId: p.id,
        providerName: p.name,
        status: "success",
        aggregationKey: "template:test",
        currency: "USD",
        data: [{ remaining: 20 + index * 10, unit: "USD" }],
      })),
    );
    vi.spyOn(providerGroupsApi, "queryProviderBalance").mockRejectedValue(
      new Error("Test query failure"),
    );
    setup({ groups: [pool], providers: members });
    fireEvent.click(screen.getByRole("button", { name: "Query balances" }));
    expect(await screen.findByText("Pool balance: 50 USD")).toBeInTheDocument();
    expect(screen.getByText("20 USD")).toBeInTheDocument();
    expect(screen.getByText("30 USD")).toBeInTheDocument();
    fireEvent.click(
      screen.getByRole("button", { name: "Query balance for one" }),
    );
    await waitFor(() =>
      expect(toast.error).toHaveBeenCalledWith("Test query failure"),
    );
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByText("Test query failure")).not.toBeInTheDocument();
    expect(providerGroupsApi.queryProviderBalance).toHaveBeenCalledWith(
      "one",
      "codex",
    );
    expect(screen.queryByText("Pool balance: 50 USD")).not.toBeInTheDocument();
    expect(screen.getByText(/Shown per Key/)).toBeInTheDocument();
  });

  it("ignores late balance results from the previously selected app", async () => {
    let finish!: (results: BalanceQueryResult[]) => void;
    vi.spyOn(providerGroupsApi, "queryGroupBalances").mockReturnValue(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    const pool = { ...groups[0], collapsed: false };
    const view = setup({
      groups: [pool],
      providers: [{ ...provider, meta: { providerGroupId: pool.id } }],
    });
    fireEvent.click(screen.getByRole("button", { name: "Query balances" }));
    view.rerender({
      appId: "claude",
      groups: [],
      providers: [{ ...provider, name: "Other app key" }],
    });
    await act(async () =>
      finish([
        {
          providerId: provider.id,
          providerName: provider.name,
          status: "success",
          data: [{ remaining: 999, unit: "USD" }],
        },
      ]),
    );
    expect(screen.queryByText("999 USD")).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Query balance for Other app key" }),
    ).toBeEnabled();
  });

  it("keeps hidden pool members in the total when the provider list is filtered", async () => {
    const pool = { ...groups[0], collapsed: false, keyPoolEnabled: true };
    const members = ["one", "two"].map((id) => ({
      id,
      name: id,
      settingsConfig: {},
      meta: { providerGroupId: pool.id, keyPoolEnabled: true },
    }));
    vi.spyOn(providerGroupsApi, "queryGroupBalances").mockResolvedValue(
      members.map((p, index) => ({
        providerId: p.id,
        providerName: p.name,
        status: "success",
        aggregationKey: "template:test",
        currency: "USD",
        data: [{ remaining: 20 + index * 10, unit: "USD" }],
      })),
    );
    const view = setup({ groups: [pool], providers: members });
    fireEvent.click(screen.getByRole("button", { name: "Query balances" }));
    expect(await screen.findByText("Pool balance: 50 USD")).toBeInTheDocument();
    view.rerender({ providers: [members[0]] });
    expect(screen.getByText("Pool balance: 50 USD")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Query balance for two" }),
    ).not.toBeInTheDocument();
  });

  it("invalidates previous balances after changing the selected template", async () => {
    const pool = {
      ...groups[0],
      collapsed: false,
      keyPoolEnabled: true,
      balanceTemplateId: "old-template",
    };
    const member = {
      ...provider,
      meta: { providerGroupId: pool.id, keyPoolEnabled: true },
    };
    vi.spyOn(providerGroupsApi, "queryGroupBalances").mockResolvedValue([
      {
        providerId: member.id,
        providerName: member.name,
        status: "success",
        aggregationKey: "template:old-template",
        currency: "USD",
        data: [{ remaining: 20, unit: "USD" }],
      },
    ]);
    vi.spyOn(providerGroupsApi, "update").mockResolvedValue({
      ...pool,
      balanceTemplateId: null,
    });
    setup({ groups: [pool], providers: [member] });
    fireEvent.click(screen.getByRole("button", { name: "Query balances" }));
    expect(await screen.findByText("Pool balance: 20 USD")).toBeInTheDocument();
    fireEvent.change(
      screen.getByRole("combobox", { name: "Balance template for First" }),
      { target: { value: "" } },
    );
    await waitFor(() =>
      expect(providerGroupsApi.update).toHaveBeenCalledWith(
        expect.objectContaining({ balanceTemplateId: null }),
      ),
    );
    await waitFor(() =>
      expect(
        screen.queryByText("Pool balance: 20 USD"),
      ).not.toBeInTheDocument(),
    );
    expect(screen.queryByText("20 USD")).not.toBeInTheDocument();
  });

  it("keeps template controls on the folder header while collapsed", () => {
    setup();
    const folder = within(screen.getByRole("region", { name: "First" }));
    const toggle = folder.getByRole("button", { name: "Toggle folder" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    const header = within(toggle.parentElement!);
    expect(
      header.getByRole("combobox", { name: "Balance template for First" }),
    ).toBeInTheDocument();
    expect(
      header.getByRole("button", { name: "New balance template" }),
    ).toBeInTheDocument();
  });

  it("reports unsupported balance endpoints through a toast without an inline error", async () => {
    vi.spyOn(providerGroupsApi, "queryProviderBalance").mockResolvedValue({
      providerId: provider.id,
      providerName: provider.name,
      status: "failed",
      data: [],
      error: "This endpoint needs a balance template",
    });
    setup();
    fireEvent.click(
      screen.getByRole("button", { name: "Query balance for Key A" }),
    );
    await waitFor(() =>
      expect(toast.error).toHaveBeenCalledWith(
        "This endpoint needs a balance template",
      ),
    );
    expect(
      screen.queryByText("This endpoint needs a balance template"),
    ).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("continues to notify after a folder rename", async () => {
    vi.spyOn(providerGroupsApi, "update").mockResolvedValue({
      ...groups[0],
      name: "Renamed",
    });
    const user = userEvent.setup();
    setup();
    await user.click(
      screen.getAllByRole("button", { name: "Folder actions" })[0],
    );
    await user.click(screen.getByRole("menuitem", { name: "Rename folder" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Folder name" }), {
      target: { value: "Renamed" },
    });
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(toast.success).toHaveBeenCalledWith("Folder updated"),
    );
  });
});
