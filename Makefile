# Common tasks. Windows users: run from Git Bash.
.PHONY: setup dev test test-rust test-web test-pg lint fmt bench localnet smoke-localnet db db-wsl webapp audit secrets clean

setup:            ## install toolchains deps (no network services started)
	cargo fetch
	cd apps/telegram-webapp && pnpm install && pnpm gen:types

dev: webapp       ## run API in MOCK mode with Mini App on http://127.0.0.1:8080
	TRADER_MODE=mock MINIAPP_DIR=apps/telegram-webapp/out ADMIN_TOKEN=dev cargo run -p trader-api

webapp:
	cd apps/telegram-webapp && pnpm build

test: test-rust test-web

test-rust:
	cargo test --workspace

test-web: webapp
	cd apps/telegram-webapp && pnpm typecheck && pnpm exec playwright test

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

bench:            ## offline + mock benchmarks (no SOL, no network)
	cargo run --release -p trader-benchmarks -- hotpath --iterations 20000
	cargo run --release -p trader-benchmarks -- engine --iterations 4000 --concurrency 1,10,50,100
	cargo run --release -p trader-benchmarks -- load --rates 100,500,1000 --seconds 30
	cargo run --release -p trader-benchmarks -- faults --rates 1,5,10 --orders 200

localnet:         ## local validator with official Pump programs (Linux/macOS; Windows: scripts/localnet-wsl.sh)
	scripts/localnet.sh

smoke-localnet:   ## requires `make localnet` running; creates a coin and trades it 10x
	MINT=$$(cargo run -q -p trader-cli -- create-coin | sed -E 's/.*"mint":"([^"]+)".*/\1/'); \
	cargo run --release -p trader-cli -- smoke-pump --mint $$MINT --iterations 10 --report benchmarks/results/localnet-pump-smoke.json

db:               ## Postgres via Docker (optional)
	docker compose up -d postgres

db-wsl:           ## Postgres in WSL2 Ubuntu, no Docker (Windows) -> 127.0.0.1:5433
	scripts/postgres-wsl.sh up

test-pg:          ## PgStore invariants against Postgres
	TEST_DATABASE_URL=$${TEST_DATABASE_URL:-postgres://trader:trader@127.0.0.1:5433/trader} cargo test -p trader-api --test pg

audit:
	cargo audit
	cd apps/telegram-webapp && pnpm audit --prod

secrets:
	scripts/secret-scan.sh

clean:
	cargo clean
	rm -rf apps/telegram-webapp/.next apps/telegram-webapp/out
