import { expect, onboard, openAs, signInitData, test } from "./fixtures";

test("outside Telegram shows open-in-Telegram and never authenticates", async ({ page }) => {
  await openAs(page, null);
  await expect(page.getByRole("heading", { name: "Open inside Telegram" })).toBeVisible();
});

test("forged / expired initData is rejected server-side", async ({ page, userId }) => {
  await openAs(page, signInitData(userId, undefined, "999:WRONG-TOKEN"));
  await expect(page.getByRole("heading", { name: "Could not sign in" })).toBeVisible();
  await expect(page.getByTestId("error")).toContainText("invalid_init_data");
});

test("expired auth_date is rejected", async ({ page, userId }) => {
  await openAs(page, signInitData(userId, Math.floor(Date.now() / 1000) - 3600));
  await expect(page.getByTestId("error")).toContainText("expired");
});

test("invalid mint shows an error and loads nothing", async ({ page, userId }) => {
  await openAs(page, signInitData(userId));
  await page.getByLabel("Token mint address").fill("not-a-mint");
  await page.getByRole("button", { name: "Load" }).click();
  await expect(page.getByTestId("error")).toContainText("not a valid Solana mint");
});

test("first run -> buy with confirmation sheet -> on-chain finalized -> sell -> portfolio", async ({ page, userId, mockMint }) => {
  await onboard(page, userId);
  await page.getByRole("button", { name: "Trade" }).click();
  await page.getByLabel("Token mint address").fill(mockMint);
  await page.getByRole("button", { name: "Load" }).click();
  await expect(page.getByText("MOCK pool (not real market data)")).toBeVisible();
  await page.getByRole("button", { name: "Buy 0.05", exact: true }).click();
  const sheet = page.getByRole("dialog", { name: "Confirm buy" });
  await expect(sheet).toBeVisible();
  await expect(page.getByTestId("max-spend")).toContainText("SOL");
  await expect(sheet).toContainText("MOCK pool (not real)");
  await sheet.getByRole("button", { name: "Confirm trade" }).click();
  await expect(page.getByTestId("order-status")).toContainText("filled (finalized)", { timeout: 30_000 });
  await expect(page.getByText("Tokens received")).toBeVisible();

  await page.getByRole("button", { name: "Sell 50%" }).click();
  const sellSheet = page.getByRole("dialog", { name: "Confirm sell" });
  await expect(sellSheet).toBeVisible();
  await sellSheet.getByRole("button", { name: "Confirm trade" }).click();
  await expect(page.getByTestId("order-status")).toContainText("filled (finalized)", { timeout: 30_000 });

  await page.getByRole("button", { name: "Portfolio" }).click();
  await expect(page.getByTestId("position")).toHaveCount(1);
  await expect(page.getByText("weighted average", { exact: false })).toBeVisible();
  await page.getByRole("button", { name: "Orders" }).click();
  await expect(page.getByTestId("order-row")).toHaveCount(2);
});

test("cancel in confirmation sheet never signs", async ({ page, userId, mockMint }) => {
  await onboard(page, userId);
  await page.getByRole("button", { name: "Trade" }).click();
  await page.getByLabel("Token mint address").fill(mockMint);
  await page.getByRole("button", { name: "Load" }).click();
  await page.getByRole("button", { name: "Buy 0.1", exact: true }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Cancel" }).click();
  await page.getByRole("button", { name: "Orders" }).click();
  await expect(page.getByTestId("order-row")).toContainText("REJECTED");
  await expect(page.getByTestId("order-row")).toContainText("cancelled_by_user");
});

test("kill switch blocks new orders", async ({ page, userId, mockMint }) => {
  await onboard(page, userId);
  await page.getByRole("button", { name: "Pause all trading" }).click();
  await expect(page.getByText("paused", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Trade" }).click();
  await page.getByLabel("Token mint address").fill(mockMint);
  await page.getByRole("button", { name: "Load" }).click();
  await page.getByRole("button", { name: "Buy 0.05", exact: true }).click();
  await expect(page.getByTestId("error")).toContainText("paused");
});

test("not-implemented features are labeled, tracker works", async ({ page, userId }) => {
  await openAs(page, signInitData(userId));
  await page.getByRole("button", { name: "More" }).click();
  await expect(page.getByText("Not available in this build", { exact: false })).toBeVisible();
  await page.getByRole("button", { name: "Track" }).click();
  await expect(page.getByText("Watchlist only", { exact: false })).toBeVisible();
  await page.getByLabel("Wallet address").fill("So11111111111111111111111111111111111111112");
  await page.getByRole("main").getByRole("button", { name: "Track", exact: true }).click();
  await expect(page.getByText("So1111…111112")).toBeVisible();
});
