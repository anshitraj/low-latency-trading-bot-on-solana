# ADR 0007 — Mini App as Next.js static export served by the API
Date: 2026-11-11 · Status: accepted

Same origin as /v1 (no CORS), bearer session in memory (no cookies → no CSRF),
no Node server to operate on the VM. Types generated from openapi.yaml.
