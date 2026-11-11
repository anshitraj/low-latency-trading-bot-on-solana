// Exact base-unit formatting with BigInt. No floating point for amounts.

export function formatUnits(raw: string | number | bigint | null | undefined, decimals: number, maxFrac = decimals): string {
  if (raw === null || raw === undefined || raw === "") return "—";
  let v = BigInt(raw);
  const neg = v < 0n;
  if (neg) v = -v;
  const scale = 10n ** BigInt(decimals);
  const int = v / scale;
  let frac = (v % scale).toString().padStart(decimals, "0").slice(0, maxFrac).replace(/0+$/, "");
  const intStr = int.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return `${neg ? "-" : ""}${intStr}${frac ? "." + frac : ""}`;
}

export const sol = (lamports: string | number | bigint | null | undefined, maxFrac = 6) => formatUnits(lamports, 9, maxFrac);

/** Validates a user-entered SOL amount string (sent to the server as-is; the server parses exactly). */
export function isValidSolInput(s: string): boolean {
  return /^(?:\d+(?:\.\d{1,9})?|\.\d{1,9})$/.test(s.trim()) && !/^0*(?:\.0*)?$/.test(s.trim());
}

export function isLikelyMint(s: string): boolean {
  return /^[1-9A-HJ-NP-Za-km-z]{32,44}$/.test(s.trim());
}

export function short(s: string, n = 4): string {
  return s.length <= n * 2 + 1 ? s : `${s.slice(0, n)}…${s.slice(-n)}`;
}

export function bps(b: number | null | undefined): string {
  if (b === null || b === undefined) return "unknown";
  return `${(b / 100).toFixed(2)}%`;
}
