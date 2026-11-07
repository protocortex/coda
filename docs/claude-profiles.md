# Claude Code profiles

`[providers.claude_code] profile` decides how much of Claude Code's own setup a
session keeps. Coda starts your installed, unmodified `claude` with flags it has
tested against the real binary. It never sees the token `claude auth login`
produces.

A plain `claude -p` sends tens of thousands of prompt tokens before your first
word. The profiles cut that down.

| Profile | Flags it adds | Prompt size | What you lose |
| --- | --- | --- | --- |
| `full` | none | 26.8k tokens | nothing; this is Claude Code as it starts alone |
| `project` (default) | `--setting-sources project --strict-mcp-config --disable-slash-commands` | 22.3k tokens plus the repository's `CLAUDE.md` | user and local settings, MCP servers, skills and slash commands |
| `lean` | the flags of `project`, but with no settings source at all, plus `--system-prompt <Coda's short prompt>` and `--tools Read,Grep,Glob,Edit,Write,Bash` | 5.2k tokens with those six tools | all settings, and every tool outside that list |

The sizes are what `claude` 2.1.296 reported on 2026-11-08 for a one word reply
on the `haiku` model in an empty folder, counting input, cache reads and cache
writes. They change with the model, the version and the user's setup. The opt-in
live probes (`docs/live-recording.md`) check them again after each `claude`
upgrade and fail when `project` is over 25k, `lean` is over 12k, or the order
`lean` < `project` < `full` no longer holds.

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

## Helper calls

`coda_providers::claude_helper::run_structured` makes one cheap call of the
installed `claude` for short jobs such as a session title, a commit message or a
branch name. It sends the prompt on standard input and returns the
`structured_output` of the JSON result, which `claude` checks against the schema
the caller gives. The call is `claude -p --output-format json --json-schema <schema>`
with no tools (`--tools ""`), no settings (`--setting-sources ""`), no MCP servers,
no skills, no saved session and a one line system prompt. It runs in an empty
temporary folder, removed afterwards, so no `CLAUDE.md` joins the prompt, and stops
after 180 seconds. It gets the same environment as a session, including the names
in `env_passthrough`. On claude 2.1.296 with the `haiku` model it sent about 1.1k
prompt tokens; without the system prompt and the settings flag it sent 6.7k.
It keeps at most 1 MiB of output. The call ends with an error when the binary
cannot start, when it runs past the timeout or prints more than that, when it
exits with a failure, when it prints something that is not a JSON result, when the
result says `is_error` (the error then carries the first 200 characters of
Claude Code's own message, such as "Not logged in"), or when the result has no
structured output. When the call ends, however it ends, Coda stops the child and
everything it started. Nothing in Coda calls it yet; storing a title needs a `title` column on sessions,
which is a separate change.
