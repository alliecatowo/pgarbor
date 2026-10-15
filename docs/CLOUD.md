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
- `codex login` is interactive and is not run by the script.
- Not tested end-to-end in a cloud VM. Lint-checked only (`bash -n`, shellcheck).

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
