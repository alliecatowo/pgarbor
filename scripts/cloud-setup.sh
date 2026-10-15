#!/usr/bin/env bash
# PGArbor cloud setup for Claude Code on the web (cloud environment "Setup script" field,
# or a SessionStart hook). Idempotent: every step checks first, so rerunning is cheap.
#
# Usage:  bash scripts/cloud-setup.sh
# Env:    BUILD=1            also run `cargo build --workspace --locked` (slow; warm-up only)
#         STRICT=1           exit 1 if a required step failed (default: exit 0, read the summary)
#         ORACLE_PORTS="..." oracle ports (default "54319 54318": 54319 = harness, 54318 = dev port)
#         SKIP_PG_SOURCE=1   skip the PostgreSQL source clone
#
# Snapshots keep files only: running Postgres/containers do NOT survive into a new session.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

# Harness oracle identity (harness/pgarbor_harness/config.py); the password is a throwaway test value.
PG_IMAGE="docker.io/library/postgres@sha256:74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336"
PG_PASS="oracle"
PG_WANT="18.6"
PG_SRC_DIR="${PGARBOR_PG_SOURCE:-$HOME/develop/reference/postgres-18}"
ORACLE_PORTS="${ORACLE_PORTS:-54319 54318}"
STATE_DIR="$HOME/.local/state/pgarbor-setup"
mkdir -p "$STATE_DIR"

REQUIRED_FAILED=0
SUMMARY=()
say()  { printf '\n== %s\n' "$*"; }
ok()   { SUMMARY+=("[ok]   $*"); printf '   ok: %s\n' "$*"; }
warn() { SUMMARY+=("[warn] $*"); printf '   WARN: %s\n' "$*"; }
fail() { SUMMARY+=("[FAIL] $*"); printf '   FAIL: %s\n' "$*"; REQUIRED_FAILED=1; }

# --- privileges: root, or passwordless sudo; otherwise apt steps are skipped ----------------
HAVE_ROOT=0; SUDO=""
if [ "$(id -u)" -eq 0 ]; then HAVE_ROOT=1
elif command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then HAVE_ROOT=1; SUDO="sudo"
fi

APT_UPDATED=0
apt_install() {  # apt_install <pkg>...  (needs HAVE_ROOT)
  [ "$HAVE_ROOT" = 1 ] && command -v apt-get >/dev/null 2>&1 || return 1
  if [ "$APT_UPDATED" = 0 ]; then
    $SUDO apt-get update -qq || return 1
    APT_UPDATED=1
  fi
  DEBIAN_FRONTEND=noninteractive $SUDO apt-get install -y -qq "$@"
}

# --- 1. Rust: the toolchain pinned in rust-toolchain.toml, plus rustfmt and clippy ----------
say "Rust toolchain"
RUST_CHANNEL="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)"
if ! command -v rustup >/dev/null 2>&1; then
  if curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none; then
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
fi
if command -v rustup >/dev/null 2>&1; then
  if rustup toolchain install "$RUST_CHANNEL" --profile minimal -c rustfmt -c clippy >/dev/null 2>&1; then
    ok "rust $RUST_CHANNEL (rustfmt, clippy): $(rustc +"$RUST_CHANNEL" --version 2>/dev/null || echo '?')"
  else
    fail "rustup could not install $RUST_CHANNEL"
  fi
else
  fail "rustup missing and install from sh.rustup.rs failed"
fi

# --- 2. uv + harness dependencies ------------------------------------------------------------
say "uv and harness dependencies"
if ! command -v uv >/dev/null 2>&1; then
  python3 -m pip install --user -q --break-system-packages uv >/dev/null 2>&1 || true
fi
if command -v uv >/dev/null 2>&1; then
  if (cd harness && uv sync -q); then ok "uv $(uv --version | awk '{print $2}'), harness venv synced"
  else fail "uv sync failed in harness/"; fi
else
  fail "uv not available"
fi

# --- 3. Node LTS (20/21/22 are pre-installed; 22 is first on PATH) ---------------------------
say "Node"
if ! command -v node >/dev/null 2>&1; then apt_install nodejs npm || warn "node missing and apt install failed"; fi
if command -v node >/dev/null 2>&1; then
  NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]')"
  ok "node $(node -v)"
  [ "$NODE_MAJOR" -ge 20 ] || warn "node $NODE_MAJOR is older than LTS 20"
fi
if ! command -v pnpm >/dev/null 2>&1 && command -v npm >/dev/null 2>&1; then
  npm install -g -s pnpm >/dev/null 2>&1 || warn "could not install pnpm (needed by tests/apps)"
fi

# --- 4. PG18.6 oracle ------------------------------------------------------------------------
# Plain Ubuntu 24.04 apt only ships PG16. Preferred: container (docker is pre-installed, and
# Docker Hub is on the Trusted allowlist). Fallback: PGDG apt, which is NOT on the default
# allowlist, so it works only under a Custom allowlist that includes apt.postgresql.org.
say "PG18 oracle (ports: $ORACLE_PORTS)"
RT=""
for c in podman docker; do
  if command -v "$c" >/dev/null 2>&1 && "$c" info >/dev/null 2>&1; then RT="$c"; break; fi
done
if [ -z "$RT" ] && command -v dockerd >/dev/null 2>&1 && [ "$HAVE_ROOT" = 1 ]; then
  $SUDO sh -c "nohup dockerd >'$STATE_DIR/dockerd.log' 2>&1 &"
  for _ in $(seq 1 20); do docker info >/dev/null 2>&1 && break; sleep 1; done
  docker info >/dev/null 2>&1 && RT="docker"
fi

ORACLE_STATUS=""
oracle_container() {  # oracle_container <port>  (sets ORACLE_STATUS)
  local port="$1" name="pgarbor-oracle-$1"
  [ "$port" = 54319 ] && name="pgarbor-harness-pg18"
  if ! $RT inspect -f '{{.State.Running}}' "$name" 2>/dev/null | grep -q true; then
    $RT rm -f "$name" >/dev/null 2>&1 || true
    $RT run -d --name "$name" -p "127.0.0.1:$port:5432" --shm-size=256m \
      -e POSTGRES_PASSWORD="$PG_PASS" -e POSTGRES_HOST_AUTH_METHOD=trust \
      -e "POSTGRES_INITDB_ARGS=--encoding=UTF8 --locale-provider=builtin --locale=C.UTF-8" \
      -e TZ=UTC "$PG_IMAGE" \
      -c TimeZone=UTC -c DateStyle='ISO, MDY' -c IntervalStyle=postgres -c extra_float_digits=1 \
      -c autovacuum=off -c lc_messages=C.UTF-8 -c max_connections=200 \
      -c fsync=off -c synchronous_commit=off -c full_page_writes=off >/dev/null || return 1
  fi
  for _ in $(seq 1 60); do
    $RT exec "$name" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1 && break; sleep 1
  done
  $RT exec "$name" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1 || return 1
  ORACLE_STATUS="$($RT exec "$name" psql -h 127.0.0.1 -U postgres -tAc 'show server_version' 2>/dev/null || echo '?')"
}

PGBIN=/usr/lib/postgresql/18/bin
oracle_native() {  # oracle_native <port>  (PGDG apt + initdb; needs root)
  local port="$1" d="$HOME/.local/share/pgarbor-oracle/pg18-$1"
  if [ ! -x "$PGBIN/pg_ctl" ]; then
    apt_install postgresql-common >/dev/null 2>&1 || return 1
    $SUDO /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y >/dev/null 2>&1 || return 1
    APT_UPDATED=0
    apt_install postgresql-18 postgresql-client-18 >/dev/null 2>&1 || return 1
  fi
  if [ ! -f "$d/PG_VERSION" ]; then
    printf '%s' "$PG_PASS" > "$STATE_DIR/pwfile"; chmod 600 "$STATE_DIR/pwfile"
    "$PGBIN/initdb" -D "$d" -U postgres --pwfile="$STATE_DIR/pwfile" --auth=scram-sha-256 \
      --encoding=UTF8 --locale-provider=builtin --locale=C.UTF-8 \
      >"$STATE_DIR/initdb-$port.log" 2>&1 || return 1
    rm -f "$STATE_DIR/pwfile"
  fi
  if ! "$PGBIN/pg_isready" -q -h 127.0.0.1 -p "$port"; then
    "$PGBIN/pg_ctl" -D "$d" -l "$STATE_DIR/pg-$port.log" -w -t 60 start -o \
      "-p $port -c listen_addresses=127.0.0.1 -c unix_socket_directories=$STATE_DIR -c TimeZone=UTC -c DateStyle=ISO,MDY -c IntervalStyle=postgres -c extra_float_digits=1 -c autovacuum=off -c lc_messages=C.UTF-8 -c fsync=off -c synchronous_commit=off -c full_page_writes=off" \
      >/dev/null || return 1
  fi
  ORACLE_STATUS="$(PGPASSWORD="$PG_PASS" "$PGBIN/psql" -h 127.0.0.1 -p "$port" -U postgres -tAc 'show server_version' 2>/dev/null || echo '?')"
}

if [ -n "$RT" ]; then
  if $RT pull -q "$PG_IMAGE" >/dev/null 2>&1; then
    for p in $ORACLE_PORTS; do
      if oracle_container "$p"; then
        ok "PG via $RT on 127.0.0.1:$p, server_version $ORACLE_STATUS"
      else
        fail "container oracle on $p not healthy (see: $RT logs)"
      fi
    done
  else
    fail "could not pull $PG_IMAGE with $RT"
  fi
else
  for p in $ORACLE_PORTS; do
    if oracle_native "$p"; then
      ok "PG via PGDG apt on 127.0.0.1:$p, server_version $ORACLE_STATUS"
    else
      fail "no container runtime and PGDG apt PG18 failed on $p (apt.postgresql.org not allowlisted?)"
    fi
  done
fi
case "$ORACLE_STATUS" in
  "$PG_WANT"*) ;;
  *) warn "oracle reports '$ORACLE_STATUS', corpus was recorded on PG $PG_WANT: results are not oracle-grade" ;;
esac

# --- 5. PostgreSQL source reference (REL_18_6) ----------------------------------------------
say "PG source reference"
if [ -d "$PG_SRC_DIR/src" ]; then
  ok "PG source present at $PG_SRC_DIR"
elif [ "${SKIP_PG_SOURCE:-0}" = 1 ]; then
  warn "PG source skipped (SKIP_PG_SOURCE=1)"
else
  mkdir -p "$(dirname "$PG_SRC_DIR")"; rm -rf "$PG_SRC_DIR.partial"
  if timeout 240 git clone -q --depth 1 --branch REL_18_6 https://github.com/postgres/postgres.git "$PG_SRC_DIR.partial" \
     && mv "$PG_SRC_DIR.partial" "$PG_SRC_DIR"; then
    ok "PG source REL_18_6 cloned to $PG_SRC_DIR"
  else
    rm -rf "$PG_SRC_DIR.partial"; warn "PG source clone failed (optional for most work)"
  fi
fi

# --- 6. Taskwarrior (optional; apt gives 2.6, the local setup is v3) ------------------------
say "Taskwarrior (optional)"
if command -v task >/dev/null 2>&1; then ok "task $(task --version 2>/dev/null)"
elif apt_install taskwarrior >/dev/null 2>&1; then ok "task $(task --version 2>/dev/null) (apt; not v3)"
else warn "taskwarrior not installed (optional)"; fi

# --- 7. Cargo warm-up ------------------------------------------------------------------------
say "Cargo"
if cargo fetch --locked >"$STATE_DIR/fetch.log" 2>&1; then ok "cargo fetch --locked"
else fail "cargo fetch failed (see $STATE_DIR/fetch.log)"; fi
if [ "${BUILD:-0}" = 1 ]; then
  if cargo build --workspace --locked >"$STATE_DIR/build.log" 2>&1; then ok "cargo build --workspace"
  else warn "cargo build --workspace failed (see $STATE_DIR/build.log)"; fi
else
  warn "workspace build skipped (set BUILD=1 to warm target/)"
fi

# --- summary ---------------------------------------------------------------------------------
say "Summary"
printf '   %s\n' "${SUMMARY[@]}"
# Optional env vars: report NAMES that are unset, never values. Set them in the cloud environment settings.
say "Optional environment variables (unset ones)"
UNSET=()
for v in DATABASE_URL PGARBOR_ORACLE PGARBOR_ORACLE_HOST PGARBOR_ORACLE_PORT PGARBOR_ORACLE_USER \
         PGARBOR_ORACLE_PASSWORD PGARBOR_ORACLE_DB PGARBOR_ORACLE_REVIEW PGARBOR_REAL_BINARY \
         PGARBOR_PG_SOURCE GH_TOKEN GITHUB_TOKEN; do
  [ -n "${!v:-}" ] || UNSET+=("$v")
done
printf '   unset: %s\n' "${UNSET[*]:-(none)}"
printf '   (see docs/CLOUD.md, "Environment variables and secrets")\n'
if [ "$REQUIRED_FAILED" = 1 ] && [ "${STRICT:-0}" = 1 ]; then exit 1; fi
exit 0
