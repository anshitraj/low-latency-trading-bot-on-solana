#!/usr/bin/env bash
# Local PostgreSQL for development/tests WITHOUT Docker, inside WSL2 Ubuntu.
#   scripts/postgres-wsl.sh up     install (first time), start, create trader db, keep WSL alive
#   scripts/postgres-wsl.sh down   release the keepalive (WSL may then stop the distro)
#   scripts/postgres-wsl.sh reset-e2e   drop/create the trader_e2e database
# Result: DATABASE_URL=postgres://trader:trader@127.0.0.1:5433/trader
#
# Why port 5433: a native Windows PostgreSQL service may already own 5432.
# Why a keepalive: WSL terminates an idle distro ~60 s after the last wsl.exe
# client exits, which kills Postgres mid-test ("os error 10053"). A hidden
# `wsl.exe -- sleep infinity` keeps the distro running until `down`.
# Dev credentials only — never reuse for a deployed database.
set -euo pipefail
DISTRO="${WSL_DISTRO:-Ubuntu}"
cmd="${1:-up}"

keepalive_running() {
  wsl -d "$DISTRO" -u root -- pgrep -f "sleep infinity # trader-pg-keepalive" >/dev/null 2>&1
}

case "$cmd" in
  up)
    wsl -d "$DISTRO" -u root -- bash -s <<'INNER'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
command -v psql >/dev/null || { apt-get update -qq && apt-get install -y -qq postgresql >/dev/null; }
conf=$(ls /etc/postgresql/*/main/postgresql.conf | head -1)
if ! grep -qE '^port = 5433' "$conf"; then
  sed -i -E 's/^#?port = [0-9]+/port = 5433/' "$conf"
  service postgresql restart >/dev/null
fi
service postgresql start >/dev/null
for i in $(seq 1 20); do su postgres -c "pg_isready -q -p 5433" && break; sleep 0.5; done
su postgres -c 'psql -q -p 5433' <<'SQL'
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'trader') THEN
    CREATE ROLE trader LOGIN CREATEDB PASSWORD 'trader';
  ELSE
    ALTER ROLE trader WITH LOGIN CREATEDB PASSWORD 'trader';
  END IF;
END $$;
SQL
su postgres -c "psql -p 5433 -tAc \"SELECT 1 FROM pg_database WHERE datname='trader'\"" | grep -q 1 || su postgres -c "createdb -p 5433 -O trader trader"
su postgres -c "psql -p 5433 -tAc 'select version()'"
INNER
    if ! keepalive_running; then
      powershell.exe -NoProfile -Command "Start-Process wsl.exe -ArgumentList '-d','$DISTRO','-u','root','--','bash','-c','\"exec -a ''sleep infinity # trader-pg-keepalive'' sleep infinity\"' -WindowStyle Hidden" >/dev/null
      sleep 2
    fi
    keepalive_running && echo "keepalive: running (stop with: scripts/postgres-wsl.sh down)" || echo "warning: keepalive not detected; WSL may stop the distro when idle"
    echo "DATABASE_URL=postgres://trader:trader@127.0.0.1:5433/trader"
    ;;
  down)
    wsl -d "$DISTRO" -u root -- pkill -f "sleep infinity # trader-pg-keepalive" || true
    echo "keepalive stopped"
    ;;
  reset-e2e)
    wsl -d "$DISTRO" -u root -- bash -c "su postgres -c 'dropdb -p 5433 --if-exists trader_e2e' && su postgres -c 'createdb -p 5433 -O trader trader_e2e'"
    echo "E2E_DATABASE_URL=postgres://trader:trader@127.0.0.1:5433/trader_e2e"
    ;;
  *) echo "usage: $0 up|down|reset-e2e" >&2; exit 2 ;;
esac
