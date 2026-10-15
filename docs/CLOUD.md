# PGArbor in Claude Code on the web

## One-liner

Paste into the environment's **Setup script** field (claude.ai/code, environment settings), or run at session start:

```bash
bash scripts/cloud-setup.sh
```

Without a checkout yet, the same script can be fetched first:

```bash
curl -fsSL https://raw.githubusercontent.com/alliecatowo/pgarbor/main/scripts/cloud-setup.sh | bash
```

Options: `BUILD=1` also builds the workspace (slow), `STRICT=1` fails the setup on required-step errors, `SKIP_PG_SOURCE=1` skips the PostgreSQL source clone.

## What the cloud environment provides (per the docs)

- Ubuntu 24.04 x86_64 VM; setup scripts run as root. [Cloud environments](https://code.claude.com/docs/en/cloud-environments#setup-scripts)
- Preinstalled: Python 3 with uv, Node 20/21/22 (22 first on PATH), Rust (rustc and cargo), docker/dockerd/compose, PostgreSQL 16 (not running), gh, git. [Installed tools](https://code.claude.com/docs/en/cloud-environments#installed-tools)
- Network "Trusted" (default) allows crates.io, pypi.org, registry.npmjs.org, github.com and raw/codeload, rustup.rs, docker hub, and Ubuntu archives. apt.postgresql.org and astral.sh are not in the list. "None" breaks installs. [Default allowed domains](https://code.claude.com/docs/en/cloud-environments#default-allowed-domains)
- Setup scripts are cached only if they finish in about five minutes, and the cache keeps files, not running processes. [Environment caching](https://code.claude.com/docs/en/cloud-environments#environment-caching)
- A non-zero setup exit fails session start. Use a SessionStart hook for per-session work. [Setup vs SessionStart](https://code.claude.com/docs/en/cloud-environments#setup-scripts-vs-sessionstart-hooks)

## Caveats

- The PG18.6 oracle is not kept across sessions. The script is idempotent, so rerun it (or use the hook below) each session.
- The container path needs `docker` (preinstalled) and Docker Hub access. Not confirmed here: that `dockerd` starts without help on a fresh VM, and the PG18 image in the VM. The PGDG apt fallback needs apt.postgresql.org on a Custom allowlist.
- The harness shells out to `podman`. In the cloud, use the `dsn` target (see the notes the script prints) or alias podman to docker. Unverified.
- Harness `PG_SERVER_VERSION` expects the Debian build string, so a non-Debian 18.6 build will fail the fingerprint check. The script prints the version it got.
- Timeouts: commands 2 min by default, up to 10 min. SessionStart hooks 600 s. [Time limits](https://code.claude.com/docs/en/cloud-environments#time-limits)
- Codex is not available in the cloud environment.
- Not tested end-to-end in a cloud VM. Lint-checked only (`bash -n`, shellcheck).

## Environment variables and secrets

Names and purpose only. Never put values in the repo or in this file. Set values in the cloud environment settings (claude.ai/code) unless marked "script".

| Name | Required | Purpose | Where to set |
| - | - | - | - |
| `DATABASE_URL` | Fixtures only (`tests/apps/*`) | Connection string for node-pg, drizzle, prisma, sqlx fixtures (`postgres://user@host:port/db`, empty DB). Also read by the `doctor` CLI check. | Cloud environment settings |
| `PGARBOR_ORACLE` | Optional | Set to `1` to un-skip oracle-marked pytest tests (`harness/tests/conftest.py`). | Cloud environment settings |
| `PGARBOR_ORACLE_HOST` | Optional | Oracle host used by harness test helpers (default `127.0.0.1`). | Cloud environment settings |
| `PGARBOR_ORACLE_PORT` | Optional | Oracle port used by harness test helpers (default `54319`). | Cloud environment settings |
| `PGARBOR_ORACLE_USER` | Optional | Oracle user for harness test helpers (default `postgres`). | Cloud environment settings |
| `PGARBOR_ORACLE_PASSWORD` | Optional | Oracle password for harness test helpers. The script's local oracle uses `oracle`. | Cloud environment settings |
| `PGARBOR_ORACLE_DB` | Optional | Oracle database for harness test helpers (default `postgres`). | Cloud environment settings |
| `PGARBOR_ORACLE_REVIEW` | Optional, record-only | Must be `1` to write `tests/expected/**` or regress baselines. Leave unset except for a deliberate, reviewed re-record. | Set per command, never in environment settings |
| `PGARBOR_REAL_BINARY` | Optional | Absolute path to a pgarbor binary for `test_real_pgarbor_target.py`. | Cloud environment settings |
| `PGARBOR_PG_SOURCE` | Optional | PostgreSQL source reference path (default `~/develop/reference/postgres-18`). | Cloud environment settings, or leave to the script default |
| `PGARBOR_DB` | Optional | Default database file for the `pgarbor` CLI. | Cloud environment settings |
| `PGARBOR_REPO` | Optional | Repo root override for the harness and factory (otherwise discovered). | Cloud environment settings |
| `PGARBOR_FAULT_SEEDS`, `PGARBOR_FAULT_SEED`, `PGARBOR_FAULT_CRASH_AT` | Optional | Fault-suite seeds and repro crash points. Set by the factory campaign or by hand. | Per command |
| `GH_TOKEN` / `GITHUB_TOKEN` | Optional, do not set | See GitHub auth below. Leaving them unset lets the GitHub proxy inject credentials. | Not needed |
| `BUILD`, `STRICT`, `ORACLE_PORTS`, `SKIP_PG_SOURCE` | Optional | Setup-script switches (see the header of `scripts/cloud-setup.sh`). | Script: read from the environment when the script runs |

The setup script prints which optional variables are unset (names only).

**Script-set values:** the script exports nothing for the session. Its own `PATH` and `PGBIN` changes live only in that process. The oracle ports and password are hardcoded in the script (`PG_PASS=oracle`, the harness's throwaway test value).

**Git push to github.com/alliecatowo/pgarbor:** per the docs, cloud sessions authenticate GitHub in one of two ways: the Claude GitHub App (installed on the repo) or a token sent with `/web-setup` from your local `gh` login. Credentials stay on Anthropic's side; git and `gh` go through a GitHub proxy that attaches them, and the proxy rejects branch deletions and non-branch pushes. So no GitHub token goes in the environment. Do not set `GH_TOKEN` or `GITHUB_TOKEN` yourself. [Use Claude Code in the cloud](https://code.claude.com/docs/en/claude-code-on-the-web#github-authentication-options)

## Optional SessionStart hook

Add to a repo's `.claude/settings.json` (not changed by this doc). It runs only in cloud sessions:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume",
        "hooks": [
          {
            "type": "command",
            "command": "[ \"$CLAUDE_CODE_REMOTE\" = true ] && bash \"$CLAUDE_PROJECT_DIR/scripts/cloud-setup.sh\" || true",
            "timeout": 600
          }
        ]
      }
    ]
  }
}
```
