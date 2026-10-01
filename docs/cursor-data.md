# Cursor local data

Calvin reads Cursor's local agent conversation state. It does not inspect editor keystrokes,
completion telemetry, cloud-agent data that has not been synchronized locally, or Cursor's
account and subscription APIs.

## Sources

The authoritative session source is Cursor's global `state.vscdb`:

- Windows: `%APPDATA%\Cursor\User\globalStorage\state.vscdb`
- macOS: `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
- Linux: `~/.config/Cursor/User/globalStorage/state.vscdb`

Composer records supply session titles, modes, timestamps, model configuration and ordered
conversation bubble identifiers. Bubble records supply prompts, assistant text, token counts,
tool calls, outcomes and tool arguments. Calvin also reads
`User/workspaceStorage/*/workspace.json` to resolve Cursor workspace identifiers to working
directories when that mapping is still available.

Cursor's database is opened read-only with a busy timeout. On the first import Calvin reads all
usable composer records. Later imports check the database modification time and only process
composers whose `lastUpdatedAt` advanced beyond Calvin's stored checkpoint.

## Normalization

- Native identifiers are prefixed with `cursor:` before insertion.
- User bubbles become prompts and model requests. Assistant bubbles retain response text for
  the session timeline.
- Cursor-recorded input and output tokens are imported without inference. Missing token counts
  remain unavailable.
- Tool statuses become successful, failed or cancelled outcomes. Tool arguments are retained
  for the session timeline, and editing-tool paths populate the provider-neutral files-touched
  metric.
- Agent, Plan and Ask modes are stored as session mode events.
- Real transcripts are never used as test fixtures. Fixtures are handwritten and anonymous.

The local SQLite format is not a public compatibility contract. Parsing accepts both text and
blob JSON values and ignores unknown records and fields.

## Cost estimates

Cursor subscription charges and plan usage are not available from the local session database,
so Calvin does not invent them. Where Cursor recorded both a recognized model and token counts,
Calvin shows an **API list-price estimate**. This is useful for workload comparison but is not a
claim about the amount Cursor billed. Requests using unrecognized models or missing token data
remain unpriced.

## Skills

Calvin discovers global Cursor skills from `~/.cursor/skills-cursor/*/SKILL.md` and project
skills from `.cursor/skills/*/SKILL.md`. Skill files are indexed read-only.

## Tailored recommendations

The Opportunities page uses Cursor-only evidence for Cursor recommendations. It checks for:

- substantial Agent sessions that did not use Plan mode;
- high-context sessions that did not use `/summarize` or `/compress`;
- broad edits that did not run `/agent-review`;
- projects without `AGENTS.md` or `.cursor/rules`;
- adoption of rules, hooks, subagents, worktrees, skills, MCP/plugins, Ask and Debug modes,
  session controls, status indicators, sandboxing, headless automation and Cloud Agents.

These deterministic checks are shown without running an AI advisor. The optional advisor
inventory includes Cursor's tools, models, modes, slash commands, setting keys, MCP server
names and customization counts, then requires the advisor to consult
`https://cursor.com/docs/llms.txt` before recommending additional current features.
