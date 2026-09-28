# What Claude Code stores locally, and what calvin uses

An inventory of the data Claude Code keeps on disk, checked against calvin. Formats are
undocumented and change between versions; this reflects Claude Code 2.1.x (September 2026).

Legend: ✅ used · 🟡 partly · ❌ not yet · 🚫 deliberately ignored

## Files and folders

| Location | What it holds | calvin |
|---|---|---|
| `~/.claude/projects/<folder>/<session>.jsonl` | Full session transcripts (see below) | ✅ imported and archived |
| `…/<session>/subagents/agent-*.jsonl` | Subagent transcripts | 🟡 archived, costed; not shown |
| `~/.claude/history.jsonl` | **Every prompt you typed** (`display`, `pastedContents`, `project`, `sessionId`, `timestamp`), kept far longer than session logs | ❌ |
| `~/.claude/stats-cache.json` | Claude Code's own daily activity and per-model token totals, including periods whose logs were deleted | ❌ |
| `~/.claude.json` → `skillUsage` | Lifetime use count and last use per skill | ❌ |
| `~/.claude.json` → `pluginUsage` | Use count per plugin | ❌ |
| `~/.claude.json` → `mcpServers`, `projects.*.mcpServers` | MCP servers you've configured (user and per project) | ❌ |
| `~/.claude.json` → `projects.*` | Per project: last session's cost, duration, lines added/removed, tokens, hook timings (`lastSessionMetrics`), `allowedTools` | ❌ |
| `~/.claude/settings.json`, `settings.local.json` | Hooks, permissions (allow/deny), model and effort defaults, status line, enabled plugins | ❌ |
| `<repo>/.claude/settings.json` / `settings.local.json` | Project hooks and permissions (shared / personal) | ❌ |
| `<repo>/CLAUDE.md`, `CLAUDE.local.md`, `~/.claude/CLAUDE.md` | Memory / instructions | ❌ presence not checked |
| `~/.claude/agents/`, `<repo>/.claude/agents/` | Custom subagents (with their model) | ❌ |
| `~/.claude/commands/`, `<repo>/.claude/commands/` | Custom slash commands | ❌ |
| `~/.claude/skills/`, plugins, `<repo>/.claude/skills/` | Skills | ✅ |
| `<repo>/.mcp.json` | Project MCP servers | ❌ |
| `~/.claude/plans/` | Saved plans from plan mode | ❌ |
| `~/.claude/file-history/` | File backups behind checkpoints / rewind | 🚫 |
| `~/.claude/paste-cache/`, images in transcripts | Pasted content and images | 🚫 images stripped on import |
| `.credentials.json`, `oauthAccount`, telemetry, caches | Credentials and internals | 🚫 never read |

## Inside session transcripts

### Record types

| Type | What it tells you | calvin |
|---|---|---|
| `user` | Prompts, tool results | ✅ |
| `assistant` | Replies, tool calls, usage, model, effort | ✅ |
| `ai-title`, `custom-title` | Generated and user-set session titles | 🟡 `ai-title` only |
| `system` / `turn_duration` | How long each turn took (`durationMs`) | ❌ |
| `system` / `compact_boundary` | Context was compacted (`compactMetadata`, `trigger`) | ❌ |
| `system` / `stop_hook_summary` | Hooks that ran at the end of a turn (`hookCount`, `hookErrors`, `durationMs`) | ❌ |
| `system` / `away_summary` | Recap written while you were away | ❌ |
| `system` / `scheduled_task_fire` | Scheduled / looped tasks firing | ❌ |
| `system` / `model_refusal_fallback` | A refusal triggered a fallback model | ❌ |
| `permission-mode` | Permission mode changes (default, acceptEdits, plan, auto) | ❌ |
| `pr-link` | Pull requests created or linked in the session | ❌ |
| `worktree-state` | Session ran in a git worktree | ❌ |
| `agent-name`, `agent-setting` | Named agent sessions and their settings | ❌ |
| `queue-operation` | Prompts queued while the agent was busy | ❌ |
| `fork-context-ref`, `continued-in`, `relocated` | Session lineage: forks, continuations, moves | ❌ |
| `file-history-snapshot`, `file-history-delta` | Checkpoints | 🚫 |
| `attachment` | Context Claude Code injected: `plan_mode`, `skill_listing`, `invoked_skills`, `nested_memory`, `edited_text_file`, `total_tokens_reminder`, `ultra_effort_enter`, … | ❌ |
| `mode`, `last-prompt`, `atis-latch`, `cost-state`, `started`/`result`/`launched`/`failed` | UI and lifecycle state | 🚫 |

### Fields worth using

| Field | Where | What it adds | calvin |
|---|---|---|---|
| `origin.kind` | `user` | `human` vs `task-notification` vs `auto-continuation` vs `peer`: the reliable way to tell your prompts from injected ones | ❌ (text prefix used instead) |
| `effort`, `perTurnEffort` | `assistant` | Reasoning effort per request | ✅ |
| `usage.speed`, `service_tier`, `inference_geo` | `assistant` | Fast mode use, tier, region | ❌ |
| `usage.server_tool_use` | `assistant` | Web searches / fetches run by the API | ❌ |
| `attributionMcpServer`, `attributionMcpTool` | `assistant` | Which MCP server a turn used | ❌ |
| `attributionPlugin`, `attributionAgent`, `attributionSkill` | `assistant` | Which plugin, agent or skill drove a turn | 🟡 skill only |
| `advisorModel` | `assistant` | Advisor model in use | ❌ |
| `isApiErrorMessage`, `apiErrorStatus`, `error`, `isAbortedMidStream` | `assistant` | API errors and aborted responses | ❌ |
| `toolDenialKind` | `user` | Why a tool call was stopped: `user-rejected`, `automode-blocked`, `permission-rule`, `automode-unavailable` | ❌ (inferred from text) |
| `userFeedback` | `user` | What you wrote when rejecting | 🟡 parsed from text |
| `isCompactSummary` | `user` | The summary that replaced compacted context | ❌ |
| `toolUseResult.structuredPatch`, `oldString`/`newString` | `user` | Lines added and removed per edit | ❌ |
| `toolUseResult.gitOperation` | `user` | Commits, pushes and other git operations | ❌ |
| `toolUseResult.interrupted`, `backgroundTaskId` | `user` | Interrupted and background commands | ❌ |
| `permissionMode`, `promptSource`, `entrypoint` | `user` | Mode and where the prompt came from (CLI, IDE, SDK) | ❌ |
| `gitBranch`, `cwd`, `version` | most records | Branch, folder, Claude Code version | ✅ |

## Feature adoption calvin could report

Signals already on disk that show whether someone uses a feature, and a nudge when they
don't:

| Feature | How to detect it | Nudge |
|---|---|---|
| Project memory (`CLAUDE.md`) | file in the repo root | Projects with heavy use and no `CLAUDE.md` |
| Shared project settings | `.claude/settings.json` (not only `.local`) | Share permissions with the team |
| Hooks | `hooks` in any settings file; `stop_hook_summary` records | Hooks that error or are slow; no hooks at all |
| Permission allowlist | `permissions.allow` | Commands you approve or reject repeatedly |
| Custom subagents | `.claude/agents/*.md` | Built-in subagents on an expensive model → define agents with a cheaper `model` |
| Subagent model | `CLAUDE_CODE_SUBAGENT_MODEL` in settings `env` | Same |
| MCP servers | configured vs `mcp__<server>__*` tool calls | Servers configured but never used cost context on every turn |
| Plugins | `enabledPlugins` vs `pluginUsage` | Plugins enabled but unused |
| Skills | installed vs used | Skills never used, or run by hand but never chosen by the model |
| Plan mode | `permission-mode: plan`, `ExitPlanMode` calls | Large tasks started without a plan |
| Effort | `effort` per request | Routine work at `xhigh`/`max` |
| Fast mode | `usage.speed` | — |
| Context compaction | `compact_boundary` | Sessions long enough to compact → split or `/clear` |
| Worktrees | `worktree-state` | Parallel work without worktrees |
| Status line, keybindings, output styles | settings / files present | — |
| Resume / continue / fork | `continued-in`, `fork-context-ref` | — |
| Scheduled tasks and loops | `scheduled_task_fire` | — |
| Outcomes | `pr-link`, `gitOperation` | Sessions that ended in a PR or commit vs abandoned |
