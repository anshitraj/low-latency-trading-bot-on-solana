import { expect, onboard, test } from "./fixtures";

// Visual capture for docs; only runs with SCREENSHOTS=1.
test.skip(!process.env.SCREENSHOTS, "set SCREENSHOTS=1 to capture docs screenshots");

test("capture trade flow", async ({ page, userId, mockMint }, info) => {
  test.skip(info.project.name !== "mobile", "mobile only");
  await onboard(page, userId);
  await page.screenshot({ path: "../../docs/screenshots/settings.png" });
  await page.getByRole("button", { name: "Trade" }).click();
  await page.getByLabel("Token mint address").fill(mockMint);
  await page.getByRole("button", { name: "Load" }).click();
  await expect(page.getByText("MOCK pool (not real market data)")).toBeVisible();
  await page.screenshot({ path: "../../docs/screenshots/token.png" });
  await page.getByRole("button", { name: "Buy 0.1", exact: true }).click();
  await expect(page.getByRole("dialog")).toBeVisible();
  await page.screenshot({ path: "../../docs/screenshots/confirm.png" });
  await page.getByRole("dialog").getByRole("button", { name: "Confirm trade" }).click();
  await expect(page.getByTestId("order-status")).toContainText("complete", { timeout: 30_000 });
  await page.screenshot({ path: "../../docs/screenshots/filled.png", fullPage: true });
});
