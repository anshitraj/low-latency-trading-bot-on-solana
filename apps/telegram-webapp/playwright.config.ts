import { defineConfig, devices } from "@playwright/test";
import path from "node:path";

// E2E against the REAL trader-api binary in MOCK mode (simulated chain).
// Telegram auth is real: tests sign initData with the test bot token and the
// server validates it exactly as in production.
export const BOT_TOKEN = "123456:E2E-test-token-not-real";
const PORT = 18080;
const repo = path.resolve(__dirname, "../..");

export default defineConfig({
  testDir: "./e2e",
  timeout: 60_000,
  fullyParallel: false,
  workers: 1,
  reporter: [["list"], ["html", { open: "never" }]],
  use: { baseURL: `http://127.0.0.1:${PORT}`, trace: "retain-on-failure" },
  projects: [
    { name: "desktop", use: { ...devices["Desktop Chrome"] } },
    { name: "mobile", use: { ...devices["Pixel 7"] } },
  ],
  webServer: {
    command: "cargo run -q -p trader-api",
    cwd: repo,
    url: `http://127.0.0.1:${PORT}/v1/health`,
    timeout: 600_000,
    reuseExistingServer: false,
    env: {
      TRADER_MODE: "mock",
      BIND_ADDR: `127.0.0.1:${PORT}`,
      TELEGRAM_BOT_TOKEN: BOT_TOKEN,
      TELEGRAM_WEBHOOK_SECRET: "e2e-webhook-secret-0123456789abcdef",
      TELEGRAM_API_BASE: "http://127.0.0.1:9",
      MINIAPP_DIR: path.resolve(__dirname, "out"),
      MOCK_SLOT_MS: "50",
      OBSERVER_POLL_MS: "30",
      ADMIN_TOKEN: "e2e-admin",
      RUST_LOG: "warn",
    },
  },
});
