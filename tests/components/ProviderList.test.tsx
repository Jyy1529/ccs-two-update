import {
  render,
  screen,
  fireEvent,
  waitFor,
  within,
} from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { describe, it, expect, vi, beforeEach } from "vitest";
import { useState, type ComponentProps, type ReactElement } from "react";
import { http, HttpResponse } from "msw";
import type { Provider, ProviderGroup } from "@/types";
import { ProviderList } from "@/components/providers/ProviderList";
import { server } from "../msw/server";
import { createInstance } from "i18next";
import { I18nextProvider } from "react-i18next";
import zh from "@/i18n/locales/zh.json";
import zhTW from "@/i18n/locales/zh-TW.json";
import en from "@/i18n/locales/en.json";
import ja from "@/i18n/locales/ja.json";
import { managementFixture } from "../utils/safetyTestUtils";
import { toast } from "sonner";

vi.mock("sonner", () => ({
  toast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

const TAURI_ENDPOINT = "http://tauri.local";

const useDragSortMock = vi.fn();
const useSortableMock = vi.fn();
const providerCardRenderSpy = vi.fn();

vi.mock("@/hooks/useDragSort", () => ({
  useDragSort: (...args: unknown[]) => useDragSortMock(...args),
}));

vi.mock("@/components/providers/ProviderCard", () => ({
  ProviderCard: (props: any) => {
    providerCardRenderSpy(props);
    const {
      provider,
      onSwitch,
      onEdit,
      onDelete,
      onDuplicate,
      onConfigureUsage,
    } = props;

    return (
      <div data-testid={`provider-card-${provider.id}`}>
        {props.groupActions}
        {props.balanceSummary}
        <button
          data-testid={`switch-${provider.id}`}
          onClick={() => onSwitch(provider)}
        >
          switch
        </button>
        <button
          data-testid={`edit-${provider.id}`}
          onClick={() => onEdit(provider)}
        >
          edit
        </button>
        <button
          data-testid={`duplicate-${provider.id}`}
          onClick={() => onDuplicate(provider)}
        >
          duplicate
        </button>
        <button
          data-testid={`usage-${provider.id}`}
          onClick={() => onConfigureUsage(provider)}
        >
          usage
        </button>
        <button
          data-testid={`delete-${provider.id}`}
          onClick={() => onDelete(provider)}
        >
          delete
        </button>
        <span data-testid={`is-current-${provider.id}`}>
          {props.isCurrent ? "current" : "inactive"}
        </span>
        <span data-testid={`drag-attr-${provider.id}`}>
          {props.dragHandleProps?.attributes?.["data-dnd-id"] ?? "none"}
        </span>
      </div>
    );
  },
}));

vi.mock("@/components/UsageFooter", () => ({
  default: () => <div data-testid="usage-footer" />,
}));

vi.mock("@dnd-kit/sortable", async () => {
  const actual = await vi.importActual<any>("@dnd-kit/sortable");

  return {
    ...actual,
    useSortable: (...args: unknown[]) => useSortableMock(...args),
  };
});

// Mock hooks that use QueryClient
vi.mock("@/hooks/useStreamCheck", () => ({
  useStreamCheck: () => ({
    checkProvider: vi.fn(),
    isChecking: () => false,
  }),
}));

vi.mock("@/lib/query/failover", () => ({
  useAutoFailoverEnabled: () => ({ data: false }),
  useFailoverQueue: () => ({ data: [] }),
  useAddToFailoverQueue: () => ({ mutate: vi.fn() }),
  useRemoveFromFailoverQueue: () => ({ mutate: vi.fn() }),
  useReorderFailoverQueue: () => ({ mutate: vi.fn() }),
}));

function createProvider(overrides: Partial<Provider> = {}): Provider {
  return {
    id: overrides.id ?? "provider-1",
    name: overrides.name ?? "Test Provider",
    settingsConfig: overrides.settingsConfig ?? {},
    category: overrides.category,
    createdAt: overrides.createdAt,
    sortIndex: overrides.sortIndex,
    meta: overrides.meta,
    websiteUrl: overrides.websiteUrl,
  };
}

function renderWithQueryClient(ui: ReactElement) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  queryClient.setQueryData(["appManagement"], managementFixture());

  return render(
    <QueryClientProvider client={queryClient}>{ui}</QueryClientProvider>,
  );
}

function ToolbarProviderList(props: ComponentProps<typeof ProviderList>) {
  const [toolbar, setToolbar] = useState<HTMLDivElement | null>(null);
  return (
    <>
      <header>
        <div ref={setToolbar} data-testid="group-toolbar" />
      </header>
      <main>
        <ProviderList {...props} toolbarContainer={toolbar} />
      </main>
    </>
  );
}

beforeEach(() => {
  useDragSortMock.mockReset();
  useSortableMock.mockReset();
  providerCardRenderSpy.mockClear();

  useSortableMock.mockImplementation(({ id }: { id: string }) => ({
    setNodeRef: vi.fn(),
    setActivatorNodeRef: vi.fn(),
    attributes: { "data-dnd-id": id },
    listeners: { onPointerDown: vi.fn() },
    transform: null,
    transition: null,
    isDragging: false,
  }));

  useDragSortMock.mockReturnValue({
    sortedProviders: [],
    sensors: [],
    handleDragEnd: vi.fn(),
  });
});

describe("ProviderList Component", () => {
  it("should render skeleton placeholders when loading", () => {
    const { container } = renderWithQueryClient(
      <ProviderList
        providers={{}}
        currentProviderId=""
        appId="claude"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
        isLoading
      />,
    );

    const placeholders = container.querySelectorAll(
      ".border-dashed.border-muted-foreground\\/40",
    );
    expect(placeholders).toHaveLength(3);
  });

  it("should show empty state and trigger create callback when no providers exist", () => {
    const handleCreate = vi.fn();
    useDragSortMock.mockReturnValueOnce({
      sortedProviders: [],
      sensors: [],
      handleDragEnd: vi.fn(),
    });

    renderWithQueryClient(
      <ProviderList
        providers={{}}
        currentProviderId=""
        appId="claude"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
        onCreate={handleCreate}
      />,
    );

    const addButton = screen.getByRole("button", {
      name: "provider.addProvider",
    });
    fireEvent.click(addButton);

    expect(handleCreate).toHaveBeenCalledTimes(1);
  });

  it("should render in order returned by useDragSort and pass through action callbacks", () => {
    const providerA = createProvider({ id: "a", name: "A" });
    const providerB = createProvider({ id: "b", name: "B" });

    const handleSwitch = vi.fn();
    const handleEdit = vi.fn();
    const handleDelete = vi.fn();
    const handleDuplicate = vi.fn();
    const handleUsage = vi.fn();
    const handleOpenWebsite = vi.fn();

    useDragSortMock.mockReturnValue({
      sortedProviders: [providerB, providerA],
      sensors: [],
      handleDragEnd: vi.fn(),
    });

    renderWithQueryClient(
      <ProviderList
        providers={{ a: providerA, b: providerB }}
        currentProviderId="b"
        appId="claude"
        onSwitch={handleSwitch}
        onEdit={handleEdit}
        onDelete={handleDelete}
        onDuplicate={handleDuplicate}
        onConfigureUsage={handleUsage}
        onOpenWebsite={handleOpenWebsite}
      />,
    );

    // Verify sort order
    expect(providerCardRenderSpy).toHaveBeenCalledTimes(2);
    expect(providerCardRenderSpy.mock.calls[0][0].provider.id).toBe("b");
    expect(providerCardRenderSpy.mock.calls[1][0].provider.id).toBe("a");

    // Verify current provider marker
    expect(providerCardRenderSpy.mock.calls[0][0].isCurrent).toBe(true);

    // Drag attributes from useSortable
    expect(
      providerCardRenderSpy.mock.calls[0][0].dragHandleProps?.attributes[
        "data-dnd-id"
      ],
    ).toBe("b");
    expect(
      providerCardRenderSpy.mock.calls[1][0].dragHandleProps?.attributes[
        "data-dnd-id"
      ],
    ).toBe("a");

    // Trigger action buttons
    fireEvent.click(screen.getByTestId("switch-b"));
    fireEvent.click(screen.getByTestId("edit-b"));
    fireEvent.click(screen.getByTestId("duplicate-b"));
    fireEvent.click(screen.getByTestId("usage-b"));
    fireEvent.click(screen.getByTestId("delete-a"));

    expect(handleSwitch).toHaveBeenCalledWith(providerB);
    expect(handleEdit).toHaveBeenCalledWith(providerB);
    expect(handleDuplicate).toHaveBeenCalledWith(providerB);
    expect(handleUsage).toHaveBeenCalledWith(providerB);
    expect(handleDelete).toHaveBeenCalledWith(providerA);

    // Verify useDragSort call parameters
    expect(useDragSortMock).toHaveBeenCalledWith(
      { a: providerA, b: providerB },
      "claude",
    );
  });

  it("filters providers without removing the header controls for an empty result", () => {
    const providerAlpha = createProvider({ id: "alpha", name: "Alpha Labs" });
    const providerBeta = createProvider({ id: "beta", name: "Beta Works" });

    useDragSortMock.mockReturnValue({
      sortedProviders: [providerAlpha, providerBeta],
      sensors: [],
      handleDragEnd: vi.fn(),
    });

    renderWithQueryClient(
      <ToolbarProviderList
        providers={{ alpha: providerAlpha, beta: providerBeta }}
        currentProviderId=""
        appId="claude"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
      />,
    );

    fireEvent.keyDown(window, { key: "f", metaKey: true });
    const searchInput = screen.getByPlaceholderText(
      "Search name, notes, or URL...",
    );
    // Initially both providers are rendered
    expect(screen.getByTestId("provider-card-alpha")).toBeInTheDocument();
    expect(screen.getByTestId("provider-card-beta")).toBeInTheDocument();

    fireEvent.change(searchInput, { target: { value: "beta" } });
    expect(screen.queryByTestId("provider-card-alpha")).not.toBeInTheDocument();
    expect(screen.getByTestId("provider-card-beta")).toBeInTheDocument();

    fireEvent.change(searchInput, { target: { value: "gamma" } });
    expect(screen.queryByTestId("provider-card-alpha")).not.toBeInTheDocument();
    expect(screen.queryByTestId("provider-card-beta")).not.toBeInTheDocument();
    expect(
      screen.getByText("No providers match your search."),
    ).toBeInTheDocument();
    expect(
      within(screen.getByTestId("group-toolbar")).getByRole("switch", {
        name: "按 Base URL 自动分组",
      }),
    ).toBeInTheDocument();
  });

  it("does not manufacture a Pi selection summary card", async () => {
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json({
          enabledProviderIds: [],
        }),
      ),
    );

    renderWithQueryClient(
      <ProviderList
        providers={{}}
        currentProviderId=""
        appId="pi"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
        onCreate={vi.fn()}
      />,
    );

    expect(await screen.findByText("pi.empty.title")).toBeInTheDocument();
    expect(providerCardRenderSpy).not.toHaveBeenCalled();
    expect(
      screen.queryByRole("button", { name: "provider.addProvider" }),
    ).not.toBeInTheDocument();
  });

  it("does not expose proxy or failover actions on Pi provider cards", async () => {
    const currentProvider = createProvider({
      id: "current-pi",
      name: "Current Pi",
    });
    const inactiveProvider = createProvider({
      id: "inactive-pi",
      name: "Inactive Pi",
    });
    useDragSortMock.mockReturnValue({
      sortedProviders: [currentProvider, inactiveProvider],
      sensors: [],
      handleDragEnd: vi.fn(),
    });
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json({
          enabledProviderIds: ["current-pi", "inactive-pi"],
        }),
      ),
    );

    renderWithQueryClient(
      <ProviderList
        providers={{
          [currentProvider.id]: currentProvider,
          [inactiveProvider.id]: inactiveProvider,
        }}
        currentProviderId="current-pi"
        appId="pi"
        isProxyRunning
        isProxyTakeover
        activeProviderId="current-pi"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
      />,
    );

    await waitFor(() => {
      const currentCards = providerCardRenderSpy.mock.calls
        .map(([props]) => props)
        .filter((props) => props.provider.id === "current-pi");
      const inactiveCards = providerCardRenderSpy.mock.calls
        .map(([props]) => props)
        .filter((props) => props.provider.id === "inactive-pi");
      expect(currentCards).not.toHaveLength(0);
      expect(inactiveCards).not.toHaveLength(0);
      expect(currentCards.at(-1)).toMatchObject({
        isCurrent: false,
        isRemovalProtected: false,
        isProxyRunning: false,
        isProxyTakeover: false,
        isAutoFailoverEnabled: false,
        activeProviderId: undefined,
        onToggleFailover: undefined,
      });
      expect(inactiveCards.at(-1)).toMatchObject({
        isCurrent: false,
        isProxyRunning: false,
        isProxyTakeover: false,
      });
      expect(currentCards.at(-1)).not.toHaveProperty("piCurrentRoute");
    });
  });

  it("derives Pi membership only from the native provider ID list", async () => {
    const provider = createProvider({
      id: "drifted-pi",
      name: "Saved Pi",
      settingsConfig: { models: [{ id: "saved-model" }] },
    });
    useDragSortMock.mockReturnValue({
      sortedProviders: [provider],
      sensors: [],
      handleDragEnd: vi.fn(),
    });
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json({
          enabledProviderIds: ["drifted-pi"],
        }),
      ),
    );

    renderWithQueryClient(
      <ProviderList
        providers={{ [provider.id]: provider }}
        currentProviderId=""
        appId="pi"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
      />,
    );

    await waitFor(() => {
      const latestCardProps = providerCardRenderSpy.mock.calls
        .map(([props]) => props)
        .filter((props) => props.provider.id === provider.id)
        .at(-1);
      expect(latestCardProps).toMatchObject({
        isCurrent: false,
        isInConfig: true,
        isRemovalProtected: false,
        isStateChangeProtected: false,
      });
    });
  });

  it("sets an inactive Pi provider through the ordinary provider action", async () => {
    const provider = createProvider({
      id: "inactive-pi",
      name: "Inactive Pi",
      settingsConfig: {
        models: [
          { id: "model-a", name: "Model A" },
          { id: "model-b", name: "Model B" },
        ],
      },
    });
    const onSwitch = vi.fn();
    useDragSortMock.mockReturnValue({
      sortedProviders: [provider],
      sensors: [],
      handleDragEnd: vi.fn(),
    });
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json({
          enabledProviderIds: ["other-pi"],
        }),
      ),
    );

    renderWithQueryClient(
      <ProviderList
        providers={{ [provider.id]: provider }}
        currentProviderId=""
        appId="pi"
        onSwitch={onSwitch}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
      />,
    );

    fireEvent.click(await screen.findByTestId("switch-inactive-pi"));
    expect(onSwitch).toHaveBeenCalledWith(provider);
    const latestCardProps = providerCardRenderSpy.mock.calls
      .map(([props]) => props)
      .filter((props) => props.provider.id === "inactive-pi")
      .at(-1);
    expect(latestCardProps).not.toHaveProperty("onSwitchPiModel");
  });

  it("does not use legacy metadata when Pi's authoritative state is unavailable", async () => {
    const provider = createProvider({
      id: "legacy-pi",
      name: "Legacy Pi",
      meta: { liveConfigManaged: true },
    });
    useDragSortMock.mockReturnValue({
      sortedProviders: [provider],
      sensors: [],
      handleDragEnd: vi.fn(),
    });
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json("current state unavailable", { status: 500 }),
      ),
    );

    renderWithQueryClient(
      <ProviderList
        providers={{ [provider.id]: provider }}
        currentProviderId=""
        appId="pi"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
      />,
    );

    await screen.findByRole("alert");
    await waitFor(() => {
      const latestCardProps = providerCardRenderSpy.mock.calls
        .map(([props]) => props)
        .filter((props) => props.provider.id === provider.id)
        .at(-1);
      expect(latestCardProps).toMatchObject({
        isCurrent: false,
        isInConfig: false,
        isStateChangeProtected: true,
      });
    });
  });

  it("keeps provider controls on cards and persists collapse without update notifications", async () => {
    const grouped = createProvider({
      id: "grouped",
      name: "Grouped Provider",
      meta: {
        providerGroupId: "group-1",
        providerGroupSortIndex: 0,
        keyPoolEnabled: true,
      },
    });
    const ungrouped = createProvider({ id: "ungrouped", name: "Ungrouped" });
    const collapsedWrites: boolean[] = [];
    const group: ProviderGroup = {
      id: "group-1",
      appType: "codex",
      name: "AgentRouter",
      kind: "manual",
      normalizedBaseUrl: null,
      sortIndex: 0,
      collapsed: false,
      keyPoolEnabled: true,
      keyPoolStrategy: "failover",
      keyPoolMaxRetries: 1,
      keyPoolCooldownMs: 1000,
      balanceTemplateId: null,
      createdAt: 1,
      updatedAt: 1,
    };
    useDragSortMock.mockReturnValue({
      sortedProviders: [grouped, ungrouped],
      sensors: [],
      handleDragEnd: vi.fn(),
    });
    server.use(
      http.post(`${TAURI_ENDPOINT}/list_provider_groups`, () =>
        HttpResponse.json([group]),
      ),
      http.post(`${TAURI_ENDPOINT}/get_provider_auto_grouping`, () =>
        HttpResponse.json(true),
      ),
      http.post(
        `${TAURI_ENDPOINT}/update_provider_group`,
        async ({ request }) => {
          const body = (await request.json()) as { group: ProviderGroup };
          Object.assign(group, body.group);
          collapsedWrites.push(group.collapsed);
          return HttpResponse.json(group);
        },
      ),
    );

    renderWithQueryClient(
      <ToolbarProviderList
        providers={{ grouped, ungrouped }}
        currentProviderId="grouped"
        appId="codex"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
      />,
    );

    expect((await screen.findAllByText("AgentRouter"))[0]).toBeInTheDocument();
    expect(screen.getByTestId("provider-card-grouped")).toBeInTheDocument();
    expect(screen.getByTestId("provider-card-ungrouped")).toBeInTheDocument();
    expect(useSortableMock).toHaveBeenCalledWith(
      expect.objectContaining({
        id: "provider-group:group-1",
        data: { type: "provider-group", groupId: "group-1", appId: "codex" },
      }),
    );
    const card = within(screen.getByTestId("provider-card-grouped"));
    expect(
      card.getByRole("combobox", { name: "Folder for Grouped Provider" }),
    ).toBeInTheDocument();
    expect(
      card.getByRole("button", { name: "Query balance for Grouped Provider" }),
    ).toBeInTheDocument();
    const toolbar = within(screen.getByTestId("group-toolbar"));
    const groupingSwitch = toolbar.getByRole("switch", {
      name: "按 Base URL 自动分组",
    });
    expect(groupingSwitch).toBeChecked();
    expect(groupingSwitch.parentElement).toHaveClass(
      "h-8",
      "rounded-lg",
      "bg-muted/50",
    );
    expect(groupingSwitch.parentElement?.textContent).toBe("");
    const newFolderButton = toolbar.getByRole("button", { name: "新建文件夹" });
    expect(
      within(screen.getByRole("main")).queryByRole("switch"),
    ).not.toBeInTheDocument();
    expect(
      groupingSwitch.compareDocumentPosition(newFolderButton) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Toggle folder" }));
    await waitFor(() =>
      expect(
        screen.queryByTestId("provider-card-grouped"),
      ).not.toBeInTheDocument(),
    );
    expect(
      screen.getByRole("combobox", {
        name: "Balance template for AgentRouter",
      }),
    ).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Toggle folder" }));
    await screen.findByTestId("provider-card-grouped");
    expect(collapsedWrites).toEqual([true, false]);
    expect(toast.success).not.toHaveBeenCalled();
  });
  it("keeps folder controls available for an app without providers", async () => {
    renderWithQueryClient(
      <ToolbarProviderList
        providers={{}}
        currentProviderId=""
        appId="codex"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
      />,
    );

    const toolbar = within(screen.getByTestId("group-toolbar"));
    expect(
      await toolbar.findByRole("switch", { name: "按 Base URL 自动分组" }),
    ).toBeInTheDocument();
    fireEvent.click(toolbar.getByRole("button", { name: "新建文件夹" }));
    expect(await screen.findByRole("dialog")).toBeInTheDocument();
  });
  it.each([
    ["zh", zh],
    ["zh-TW", zhTW],
    ["en", en],
    ["ja", ja],
  ] as const)(
    "localizes header controls and the folder dialog in %s",
    async (language, translation) => {
      const i18n = createInstance();
      await i18n.init({
        lng: language,
        resources: { [language]: { translation } },
      });
      renderWithQueryClient(
        <I18nextProvider i18n={i18n}>
          <ToolbarProviderList
            providers={{}}
            currentProviderId=""
            appId="codex"
            onSwitch={vi.fn()}
            onEdit={vi.fn()}
            onDelete={vi.fn()}
            onDuplicate={vi.fn()}
            onOpenWebsite={vi.fn()}
          />
        </I18nextProvider>,
      );
      const toolbar = within(screen.getByTestId("group-toolbar"));
      expect(
        await toolbar.findByRole("switch", {
          name: translation.providerGroups.autoGrouping,
        }),
      ).toBeInTheDocument();
      fireEvent.click(
        toolbar.getByRole("button", {
          name: translation.providerGroups.newFolder,
        }),
      );
      expect(
        await screen.findByRole("dialog", {
          name: translation.providerGroups.createTitle,
        }),
      ).toBeInTheDocument();
    },
  );

  it("keeps automatic grouping changes isolated when switching apps", async () => {
    const states: Record<string, boolean> = { codex: true, claude: false };
    server.use(
      http.post(
        `${TAURI_ENDPOINT}/get_provider_auto_grouping`,
        async ({ request }) => {
          const { app } = (await request.json()) as { app: string };
          return HttpResponse.json(states[app]);
        },
      ),
      http.post(
        `${TAURI_ENDPOINT}/set_provider_auto_grouping`,
        async ({ request }) => {
          const { app, enabled } = (await request.json()) as {
            app: string;
            enabled: boolean;
          };
          states[app] = enabled;
          return HttpResponse.json([]);
        },
      ),
    );
    function AppHarness() {
      const [app, setApp] = useState<"codex" | "claude">("codex");
      return (
        <>
          <button onClick={() => setApp(app === "codex" ? "claude" : "codex")}>
            change-app
          </button>
          <ToolbarProviderList
            providers={{}}
            currentProviderId=""
            appId={app}
            onSwitch={vi.fn()}
            onEdit={vi.fn()}
            onDelete={vi.fn()}
            onDuplicate={vi.fn()}
            onOpenWebsite={vi.fn()}
          />
        </>
      );
    }
    renderWithQueryClient(<AppHarness />);
    let control = await screen.findByRole("switch", {
      name: "按 Base URL 自动分组",
    });
    await waitFor(() => expect(control).toBeChecked());
    fireEvent.click(control);
    await waitFor(() => expect(states.codex).toBe(false));
    fireEvent.click(screen.getByText("change-app"));
    control = await screen.findByRole("switch", {
      name: "按 Base URL 自动分组",
    });
    await waitFor(() => expect(control).not.toBeDisabled());
    expect(control).not.toBeChecked();
    fireEvent.click(control);
    await waitFor(() => expect(states.claude).toBe(true));
    fireEvent.click(screen.getByText("change-app"));
    control = await screen.findByRole("switch", {
      name: "按 Base URL 自动分组",
    });
    await waitFor(() => expect(control).not.toBeChecked());
    expect(states).toEqual({ codex: false, claude: true });
  });

  it("keeps Pi provider creation on the page-level add action", async () => {
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json({
          enabledProviderIds: [],
        }),
      ),
    );

    renderWithQueryClient(
      <ProviderList
        providers={{}}
        currentProviderId=""
        appId="pi"
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onDuplicate={vi.fn()}
        onOpenWebsite={vi.fn()}
        onCreate={vi.fn()}
      />,
    );

    await screen.findByText("pi.empty.title");
    expect(
      screen.queryByRole("button", { name: "provider.importCurrent" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "provider.addProvider" }),
    ).not.toBeInTheDocument();
  });
});
