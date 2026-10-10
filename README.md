# app-claude

SenClaw Space App (background) that runs kanban cards assigned to **`claude-code`** in the user's own
[Claude Code](https://code.claude.com) CLI, and verifies the result before the card closes.

```
kanban card (assignee claude-code) ─► SenClaw MCPDispatcher ─► app-claude (app-worker protocol)
                                       worktree senclaw/card-<id>      │ spawn `claude -p --output-format stream-json …`
                                       dispatch token for /mcp/*       │ Stop hook → POST /hooks/stop (verify gate)
                                                                       │ post-run verify ─ reviewer session ─ fix rounds (--resume)
                                       card Done | Blocked  ◄──────────┘ outcome
```

## How it plugs into SenClaw

The manifest declares a worker:

```json
"workers": [{ "assignee": "claude-code", "aliases": ["claude"], "path": "/api/worker" }]
```

The daemon's dispatcher hands every claimed item with that assignee to the app (`app_space_sdk::worker`):
`POST /api/worker/runs` with the job (working directory — the card's worktree —, the task, and MCP servers: the
daemon's `/mcp/core` and `/mcp/kanban` bridge with a job-scoped token), then polls `GET /api/worker/runs/{id}` until
the app reports an outcome, and closes the card with it. The app never calls the kanban API itself.

The app must run in the **background** (`runtime.mode: "background"`): the daemon starts it at boot and restarts it
if it dies. A run lost to a restart fails the card with a reason.

## Verification

The model's report can only lower the outcome (`blocked`); it never closes a card by itself.

| Layer | What runs | Effect |
|---|---|---|
| Stop hook | verify command, when Claude tries to finish | failing → Claude keeps working with the output (max `maxStopBlocks`) |
| Post-run verify | verify command again, by the app | failing → feedback round |
| Review | second Claude Code session, `plan` mode, read-only tools, over `git diff <base>` | `changes_requested` → feedback round; no verdict = not a pass |
| Fix rounds | `--resume <session>` with the failures | after `maxFixRounds` the card is **Blocked** with the findings |

No verify command and no worktree diff to review → the card is **Blocked** as `review-required`.

## Settings

`$SENCLAW_HOME/space-app-data/claude/settings.json` (edit in the app's page): `binary`, `model`, `permissionMode`
(default `acceptEdits`), `allowedTools` (default Bash, Read, Edit, Write; the SenClaw MCP servers are added),
`maxTurns`, `maxBudgetUsd`, `verifyCommand` (only ever read from here — never from card text), `verifyTimeoutSecs`,
`maxStopBlocks`, `review`, `maxFixRounds`. SenClaw's tools act for the chat the daemon pins in the job's token.

## Trust boundary

The worker is **not sandboxed**: it runs as you, and with `Bash` allowed (the default) it can do anything you can,
including reading `~/.senclaw/api_token`. The worktree protects your checkout, not the machine. Treat "a card
assigned to `claude-code` reaches Ready" as "this text gets a shell on my machine". A session's process group (and
the verify command's) is killed when it ends, when the daemon cancels the run, and when the app stops.

The app answers only the daemon (its access token) except `/health` and `/hooks/*` (the Stop hook, authenticated by a
per-run token). It never reads, stores, or forwards Claude credentials: it runs your own signed-in `claude`.

## Build

```bash
cargo test
scripts/pack.sh      # release/app-claude-<version>-<platform>.zip
```

Depends on `app-space-sdk` from the sibling `../senclaw` checkout (the `worker` module).
