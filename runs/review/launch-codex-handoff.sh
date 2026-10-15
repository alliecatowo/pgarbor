#!/usr/bin/env bash
# Waits for the three Claude workflows to finish, then hands the project to Codex (gpt-6.1-sol).
T=/tmp/claude-1000/-home-allie-develop-pgarbor/000fed8a-3882-41eb-8a7c-1d7e77c94b31/tasks
C=/home/allie/.claude/plugins/cache/openai-codex/codex/1.0.6/scripts/codex-companion.mjs
cd /home/allie/develop/pgarbor
until [ -s $T/wyb99k5tr.output ] && [ -s $T/whxiik9x9.output ] && [ -s $T/wbgpjiexd.output ]; do sleep 30; done
# let the test audit finish too (max 1h)
for i in $(seq 1 120); do node $C status task-mv9zg6g9-ejanm0 --json | grep -qE '"status": *"(completed|failed|cancelled)"' && break; sleep 30; done
node $C task --background --write --model gpt-6.1-sol "$(cat runs/review/codex-handoff-01.md)"
