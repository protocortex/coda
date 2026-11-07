# Claude Code profiles

`[providers.claude_code] profile` decides how much of Claude Code's own setup a
session keeps. Coda starts your installed, unmodified `claude` with documented
flags only. It never reads or forwards your login.

A plain `claude -p` sends about 35k prompt tokens before your first word. The
profiles cut that down.

| Profile | Flags it adds | Prompt size | What you lose |
| --- | --- | --- | --- |
| `full` | none | about 35k tokens | nothing; this is Claude Code as it starts alone |
| `project` (default) | `--setting-sources project --strict-mcp-config --disable-slash-commands` | about 19.5k tokens plus the repository's `CLAUDE.md` | user and local settings, MCP servers, skills and slash commands |
| `lean` | the flags of `project`, but with no settings source at all, plus `--system-prompt <Coda's short prompt>` and `--tools Read,Grep,Glob,Edit,Write,Bash` | about 0.6k tokens plus about 1.5k per tool | all settings, and every tool outside that list |

The sizes are earlier live measurements. The opt-in live probes (#139) check them
again after each `claude` upgrade.

`project` and `lean` read no user settings, so hooks and allow rules in your user
settings do not apply to Coda sessions. Choose `full` to keep them.

An empty tool list is never passed without `--strict-mcp-config`: with MCP servers
still on, it adds about 22k tokens.

If the installed `claude` rejects a flag of the profile, the session ends with a
`backend_crashed` event. Its `detail` names the flag and says to set
`profile = "full"`. Coda never falls back on its own. Only the flag name is taken
from the error text of `claude`.

## Init check

When a session starts, Claude Code lists the tools, MCP servers and skills it
loaded. For `project` and `lean`, Coda logs a warning for any MCP server or skill
that is there anyway, and for `lean` also for any tool outside its list. The
warning names what was found and goes to the daemon's log (a `warn` line, message
`init check`). It does not stop the session. `full` is never
checked.
