import { test as base, expect, type Page } from "@playwright/test";
import { createHmac } from "node:crypto";
import { BOT_TOKEN } from "../playwright.config";

/** Build initData exactly as Telegram does (HMAC-SHA256, key = HMAC("WebAppData", bot_token)). */
export function signInitData(userId: number, authDate = Math.floor(Date.now() / 1000), token = BOT_TOKEN, extra: Record<string, string> = {}): string {
  const fields: Record<string, string> = {
    auth_date: String(authDate),
    query_id: `AAE${userId}${authDate}${Math.random().toString(36).slice(2)}`,
    user: JSON.stringify({ id: userId, first_name: "E2E", username: `e2e${userId}` }),
    ...extra,
  };
  const dcs = Object.keys(fields)
    .sort()
    .map((k) => `${k}=${fields[k]}`)
    .join("\n");
  const secret = createHmac("sha256", "WebAppData").update(token).digest();
  const hash = createHmac("sha256", secret).update(dcs).digest("hex");
  return new URLSearchParams({ ...fields, hash }).toString();
}

export async function openAs(page: Page, initData: string | null) {
  // Block the external SDK; inject the WebApp object Telegram would provide.
  await page.route("https://telegram.org/js/telegram-web-app.js", (r) => r.fulfill({ status: 200, contentType: "application/javascript", body: "" }));
  if (initData !== null) {
    await page.addInitScript((d) => {
      (window as unknown as { Telegram: unknown }).Telegram = { WebApp: { initData: d, ready() {}, expand() {} } };
    }, initData);
  }
  await page.goto("/");
}

let nextUser = 700_000 + Math.floor(Math.random() * 100_000);
export const test = base.extend<{ userId: number; mockMint: string }>({
  userId: async ({}, use) => {
    await use(nextUser++);
  },
  mockMint: async ({ request }, use) => {
    const h = await (await request.get("/v1/health")).json();
    await use(h.mock_demo_mint as string);
  },
});
export { expect };

export async function onboard(page: Page, userId: number) {
  await openAs(page, signInitData(userId));
  await expect(page.getByTestId("mode-banner")).toContainText("MOCK MODE");
  await page.getByRole("button", { name: "Settings" }).click();
  await page.getByRole("button", { name: "Create test trading wallet" }).click();
  await expect(page.getByTestId("wallet-address")).toBeVisible();
  await page.getByRole("button", { name: "Get test SOL" }).click();
  await expect(page.getByRole("status")).toContainText("Test SOL requested");
}
