# GitHub Copilot CLI local data

calvin reads GitHub Copilot CLI only. Copilot integrations embedded in IDEs are outside
this adapter's scope.

## Sources

The adapter combines two local sources under `~/.copilot`:

- `session-store.db`: authoritative normalized sessions, turns, files, GitHub references
  and usage records.
- `session-state/<session-id>/events.jsonl`: append-only live events, especially tool
  execution and session metadata.

The session store supplies prompts so the same turn is not imported twice from both sources.
Event files supply tool calls and outcomes. Either source may be absent; ingestion continues
with the data that is available.

## Normalization

- Native identifiers are prefixed with `copilot-cli:` before insertion.
- SQLite timestamps are converted to ISO-8601 UTC strings.
- Token counts and AI units are recorded independently. AI units are never presented as USD
  or compared directly with Claude Code's estimated API cost.
- Missing usage remains unavailable rather than becoming a zero-valued measurement.
- Real transcripts are never used as test fixtures. Fixtures are handwritten and anonymous.

The local format is not a public compatibility contract. Parsing is intentionally tolerant,
and unknown event types or fields are ignored.
