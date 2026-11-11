import type { NextConfig } from "next";

// Static export: served by trader-api (MINIAPP_DIR) on the same origin as /v1.
const config: NextConfig = {
  output: "export",
  images: { unoptimized: true },
  poweredByHeader: false,
  reactStrictMode: true,
};

export default config;
