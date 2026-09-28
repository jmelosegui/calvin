<p align="center"><img src="assets/logo.svg" width="96" alt="calvin logo: a robot head with one orange eye"></p>

# calvin

**Learn how you actually use AI coding agents.** One binary, fully local.

> Named after Dr Susan Calvin, the robopsychologist in Isaac Asimov's *I, Robot*. Her job was
> working out why a robot behaved the way it did. calvin does the same for you and your agents.

calvin reads the session logs your AI harness already writes to disk (Claude Code first),
loads them into a local SQLite database, and shows you:

- **Prompts you keep typing**, which should become skills or `CLAUDE.md` rules
- **Skills that never trigger**, and prompts a skill should have caught
- **Cost and cache hit rate** per day, project and model (estimated at API list price)
- **Friction**: denied tool calls, interruptions, "no, that's wrong" replies

> **Status:** early development. Works with Claude Code on Windows; no releases yet.

## Local only

Your session logs contain prompts, source code and often confidential details. calvin:

- never sends data anywhere: no telemetry, no accounts, no cloud
- makes one network call on its own: while running, it asks GitHub once a day whether a
  newer release exists, so it can tell you. That request carries nothing about you or your
  usage. Turn it off with `[updates] check = false` in `config.toml`
- sends your data only when you ask it to: **Ask for a plan** on the Opportunities page
  passes the report (findings, numbers, and evidence such as repeated prompts and rejected
  commands) to the AI tool you choose. **Preview what is sent** shows exactly what calvin
  sends; **Copy as prompt** lets you paste it yourself instead
- never writes to your harness folders or skill folders
- serves its dashboard on `127.0.0.1` only

## Install

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/jmelosegui/calvin/main/docs/install.ps1 | iex
```

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/jmelosegui/calvin/main/docs/install.sh | sh
```

The scripts download the latest release for your platform, verify its checksum, and put
`calvin` in `%LOCALAPPDATA%\calvinin` (Windows) or `~/.local/bin` (macOS, Linux). Run them
again to update; a running calvin is stopped and restarted for you.

Prebuilt for Windows x64, Linux x64/arm64 (static) and macOS x64/arm64. Anything else, or
from source (Rust 1.95+):

```sh
cargo install --git https://github.com/jmelosegui/calvin
```

There are no other dependencies. The dashboard, its fonts and SQLite are all built into the
one binary.

## Usage

```sh
calvin start   # import your history, keep following your sessions, open the dashboard
calvin stop    # stop it
```

That's it. `calvin start` runs in the background and serves the dashboard at
`http://127.0.0.1:1982`. Close the terminal and it keeps running until `calvin stop`.

Also available:

```sh
calvin status                  # is it running, on which URL, how fresh the data is
calvin open                    # reopen the dashboard in your browser
calvin                         # import and print a 30-day summary in the terminal
calvin report skills           # skills the model ran
calvin report commands         # slash commands you typed
calvin report prompts          # prompts you keep typing
calvin report friction         # rejected / denied tool calls, interruptions, errors
calvin report cache            # prompt-cache hit rate per model
calvin report cost --by project --since 4w
calvin doctor                  # what was detected, where, and log retention warnings
```

## How it works

```
                                  ┌──────────── calvin start ────────────┐
~/.claude/projects/**/*.jsonl ─┐  │                                      │
(more harnesses via adapters) ─┼──► catch up + follow ──► SQLite ──► dashboard ──► 127.0.0.1:1982
skill folders ─────────────────┘  │                                      │
                                  └──────────── calvin stop ─────────────┘
```

- **One process:** it imports, watches and serves the dashboard together. One start, one stop.
- **Backfill:** your whole history is imported on first run, not just what happens from now on.
- **No hooks:** new data is picked up by watching the log files, so calvin can't slow down
  or break your sessions.
- **Adapters:** each harness is a small parser. Claude Code is first; others are welcome.

## Supported harnesses

| Harness | Status |
|---|---|
| Claude Code | planned for v0.1 |
| Codex CLI, Gemini CLI, Copilot CLI, Cursor, Aider | contributions welcome |

## Configuration

Optional. Everything works with defaults. `config.toml` lives in your OS config directory.

```toml
[skills]
extra_paths = ["~/other/skills"]  # more places where skills are installed

[advisor]                         # who writes plans on the Opportunities page
provider = "claude-code"          # or "command"

[advisor.claude-code]
model = "claude-sonnet-5"
max_budget_usd = 1.0

[advisor.command]                 # any tool: prompt on stdin, Markdown on stdout
name = "My tool"
program = "my-tool"
args = []

[prices.overrides]
# "model-id" = { input = 0.0, output = 0.0 }   # USD per million tokens
```

## Contributing

The most useful contribution is a new harness adapter. Adapters ship with **hand-written,
anonymised fixtures**. Never commit or attach real transcripts; use `calvin export --redact`
when filing issues. Details in `CONTRIBUTING.md` (coming).

## License

Licensed under the [MIT License](LICENSE).

The bundled fonts are under the SIL Open Font License; see [web/fonts/OFL.txt](web/fonts/OFL.txt).
