import { createInstance, type TFunction } from "i18next";
import { describe, expect, it } from "vitest";
import en from "@/i18n/locales/en.json";
import ja from "@/i18n/locales/ja.json";
import zhTW from "@/i18n/locales/zh-TW.json";
import zh from "@/i18n/locales/zh.json";
import {
  translateCodexAgentRoleDeleteError,
  translateCodexAgentRoleProxyError,
} from "@/utils/errorUtils";

const resources = {
  zh: { translation: zh },
  "zh-TW": { translation: zhTW },
  en: { translation: en },
  ja: { translation: ja },
};

async function translator(language: keyof typeof resources) {
  const instance = createInstance();
  await instance.init({
    lng: language,
    fallbackLng: language,
    resources,
    interpolation: { escapeValue: false },
  });
  return instance.t.bind(instance) as TFunction;
}

describe("translateCodexAgentRoleDeleteError", () => {
  it.each([
    ["zh", "删除此 Provider 前，请先关闭它的子代理角色路由。"],
    ["zh-TW", "刪除此 Provider 前，請先關閉它的子代理角色路由。"],
    ["en", "Disable agent role routing on this provider before deleting it."],
    [
      "ja",
      "この Provider を削除する前に、サブエージェント役割ルーティングを無効化してください。",
    ],
  ] as const)("renders the owner guard in %s", async (language, expected) => {
    const t = await translator(language);
    expect(
      translateCodexAgentRoleDeleteError(
        "codex_agent_role_owner_delete_blocked:",
        t,
      ),
    ).toBe(expected);
  });

  it.each([
    ["zh", "此 Provider 正被以下前端角色配置引用：Provider A。"],
    ["zh-TW", "此 Provider 正被以下前端角色設定引用：Provider A。"],
    [
      "en",
      "This provider is used by frontend roles configured on: Provider A.",
    ],
    [
      "ja",
      "この Provider は次のフロントエンド役割設定から参照されています：Provider A。",
    ],
  ] as const)("renders one target owner in %s", async (language, expected) => {
    const t = await translator(language);
    expect(
      translateCodexAgentRoleDeleteError(
        'codex_agent_role_target_delete_blocked:["Provider A"]',
        t,
      ),
    ).toBe(expected);
  });

  it("renders multiple target owners from the structured payload", async () => {
    const t = await translator("en");
    expect(
      translateCodexAgentRoleDeleteError(
        'codex_agent_role_target_delete_blocked:["Provider A","Provider B"]',
        t,
      ),
    ).toBe(
      "This provider is used by frontend roles configured on: Provider A, Provider B.",
    );
  });
});

describe("translateCodexAgentRoleProxyError", () => {
  it.each([
    [
      "zh",
      "前端子代理独立 Provider 路由要求 Codex 本地代理监听回环地址（127.0.0.1 或 ::1）。请修改监听地址后重试。",
    ],
    [
      "zh-TW",
      "前端子代理獨立 Provider 路由要求 Codex 本機代理監聽迴圈位址（127.0.0.1 或 ::1）。請修改監聽位址後重試。",
    ],
    [
      "en",
      "Frontend subagent provider routing requires the Codex local proxy to listen on a loopback address (127.0.0.1 or ::1). Change the listen address and try again.",
    ],
    [
      "ja",
      "フロントエンドサブエージェントの独立 Provider ルーティングでは、Codex ローカルプロキシがループバックアドレス（127.0.0.1 または ::1）をリッスンする必要があります。リッスンアドレスを変更して再試行してください。",
    ],
  ] as const)("renders the loopback guard in %s", async (language, expected) => {
    const t = await translator(language);
    expect(
      translateCodexAgentRoleProxyError(
        "codex_agent_role_loopback_required: listen address must be loopback",
        t,
      ),
    ).toBe(expected);
  });

  it("returns null for unrelated errors", async () => {
    const t = await translator("en");
    expect(translateCodexAgentRoleProxyError("network timeout", t)).toBeNull();
  });
});
