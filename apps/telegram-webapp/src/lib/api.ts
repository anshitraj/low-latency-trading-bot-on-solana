// Typed API client. Types are generated from ../../openapi.yaml
// (pnpm gen:types). The session token lives in memory only (never
// localStorage); a reload re-authenticates with fresh Telegram initData.
import type { components } from "./openapi";

export type Order = components["schemas"]["Order"];
export type Quote = components["schemas"]["Quote"];
export type Wallet = components["schemas"]["Wallet"];
export type User = components["schemas"]["User"];
export type UserSettings = components["schemas"]["UserSettings"];
export type OrderState = components["schemas"]["OrderState"];
export type OrderRequest = components["schemas"]["OrderRequest"];

export class ApiError extends Error {
  constructor(public status: number, public code: string, message: string) {
    super(message);
  }
}

let token: string | null = null;

export function setToken(t: string | null) {
  token = t;
}

export async function api<T>(method: string, path: string, body?: unknown, headers?: Record<string, string>): Promise<T> {
  const h: Record<string, string> = { ...(headers ?? {}) };
  if (body !== undefined) h["content-type"] = "application/json";
  if (token) h["authorization"] = `Bearer ${token}`;
  const r = await fetch(path, { method, headers: h, body: body === undefined ? undefined : JSON.stringify(body), cache: "no-store" });
  const text = await r.text();
  const json = text ? JSON.parse(text) : null;
  if (!r.ok) {
    const e = json?.error ?? {};
    throw new ApiError(r.status, e.code ?? "http_error", e.message ?? `HTTP ${r.status}`);
  }
  return json as T;
}

export const TERMINAL: OrderState[] = ["FINALIZED", "FAILED_ONCHAIN", "EXPIRED", "REJECTED"];

export function newIdempotencyKey(): string {
  return `mini:${crypto.randomUUID()}`;
}
