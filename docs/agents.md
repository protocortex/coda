# Agents, skills and commands

Definitions are markdown files. The frontmatter is TOML (not YAML), between a `+++` line at the very start and a closing `+++` line. Everything after the closing line is the body, which is the prompt. Coda reads the frontmatter with the TOML parser it already uses, so there is no YAML dependency. Unknown frontmatter keys are errors.

## Where files live

| Kind | User | Project |
| --- | --- | --- |
| Agents | `<CODA_HOME>/agents/<name>.md` | `<repo>/.coda/agents/<name>.md` |
| Skills | `<CODA_HOME>/skills/<name>.md` | `<repo>/.coda/skills/<name>.md` |
| Commands | `<CODA_HOME>/commands/<name>.md` | `<repo>/.coda/commands/<name>.md` |

A file is at most 64 KiB, must be a regular file (a symlink is refused, and so is a symlinked `.coda` folder or `agents`, `skills` or `commands` subfolder), and its `name` must equal its file name without `.md`. Names use lowercase letters, digits and hyphens. A leading byte order mark is ignored, and CRLF line endings are accepted.

## Agent

```
+++
name = "reviewer"
description = "Reviews a diff for bugs. Use when a change needs review. Do not use when code must be written."
tier = "standard"
effort = "high"
tools = ["read", "grep", "code_search"]
+++
You review diffs. Report findings only.
```

| Field | Notes |
| --- | --- |
| `name` | Required. |
| `description` | Required. Routing reads it. It must say when to use the agent ("Use when ...") and when not to ("Do not use ..."). |
| `tier` | Required. `fast`, `standard` or `deep`. A `[routing]` table in `config.toml` maps it to a model (see [config.md](config.md)). |
| `effort` | Required. `low`, `medium` or `high`. |
| `tools` | Required. The only tools the agent can call. Each must be a tool Coda ships: `bash`, `code_outline`, `code_read_range`, `code_search`, `code_symbol`, `edit`, `grep`, `read`, `retrieve`. An agent without `edit` and `bash` cannot change files, because the list holds no such tool, not because its prompt says so. `bash` can still change files, so an agent that has it is not read-only, and each command it runs goes through the permission broker. |

A `model` key is an error: an agent file never names a model id.

## Skill

Frontmatter: `name`, `description`. The body is the instructions.

A model-API session (Anthropic, OpenAI-compatible) that has any skill gets one extra tool, `skill`. The tool's description is an index with one line per skill, `- name: description`, and its only argument is the name of a skill from that index. The model sees the names and descriptions in every request and pays for nothing else. A body enters the conversation only when the model calls `skill` for it, as that call's result. An unused skill's body is never sent to the provider.

A tool call, not an index in the system prompt, carries the load for three reasons. The model needs a way to ask for a body, and a file read cannot be it, because the file tools are confined to the workspace and a user skill lives outside it. The one place a body is returned is also the one place the size cap and the trust check apply. And a session with no skills gets no extra tool and no extra prompt text.

The caps are fixed, in bytes or characters: the index lists at most 4 KiB of entries, in alphabetical order, with each description cut to 200 characters on one line, and says how many skills it left out (those cannot be loaded). A body longer than 32 KiB is cut and says so. A session with no skills has no `skill` tool.

The skills are read again at every send. When the files or the trust decision changed since the session's last send, for example you ran `coda agents trust --revoke`, edited a project skill, or added a skill, the session's conversation is rebuilt from its stored history with the new index before the model is called. A skill that is gone is no longer in the index and a call for it is refused, and the same holds for the first send after a daemon restart. A skill body the session already loaded stays in its history after a revoke, and only new loads are refused. The rebuild keeps the tokens the session already spent, so it does not reset the session's token budget. When nothing changed, the conversation is kept as it is. The tool needs no permission prompt: it reads only definitions already admitted for the session.

Trust decides what is offered. A user skill and a skill of a trusted project are instructions, like any text you write for the model. A skill of a project that is not trusted, or that changed since it was trusted, is not in the catalog, so it is not in the index and cannot be loaded. Skills apply to model-API sessions only. Claude Code and Codex sessions run their own tools and have their own skill systems. A sub-agent started by `task` has no `skill` tool: it gets only the tools in its agent file, and `skill` is not a tool an agent file can name.

## Command

Frontmatter: `name`, `description`, and an optional `argument_hint` such as `<issue>`. The body is the prompt template.

```
+++
name = "review"
description = "Reviews a change."
argument_hint = "<change>"
+++
Review $ARGUMENTS and report problems only.
```

A command file is a slash command in the TUI and a flag of `coda run`.

- **TUI.** Type `/review the parser change`. Everything after the command word is the arguments. The expanded prompt is sent as your message. `/help` lists the built-in commands, then every command file with its hint, description and source (`user` or `project`). The command files are read when the TUI starts, so restart it after you add or edit one.
- **`coda run`.** `coda run --command review --args "the parser change"` runs the same prompt headless. `--command` cannot be combined with `--prompt`, and `--args` needs `--command`. A prompt that merely starts with `/` is never read as a command, so a pasted path cannot run one by mistake.

Expansion is plain text. Every `$ARGUMENTS` in the body is replaced by the arguments, trimmed, in one pass: text in the arguments that looks like `$ARGUMENTS`, `${VAR}` or `$(cmd)` is sent as typed and is never expanded again. No shell runs and no file is included. The blank space around the finished prompt is dropped. If the body has no `$ARGUMENTS` and arguments are given, nothing is sent and the command says it takes no arguments.

Which commands run:

- Only your own command files and the command files of a project you trusted (`coda agents trust`) run. A project command that waits for trust is listed in `/help` as pending, with the reason, and `/name` or `--command name` refuses to run it.
- A built-in TUI command (`/sessions`, `/new`, `/resume`, `/model`, `/backend`, `/help`, `/quit`) always wins a name clash. The command file is ignored, and the TUI says so in the transcript and in `/help`. A project command file that waits for trust is reported the same way when its name is a built-in, because trusting it would not make it run.
- `coda run --command` has no TUI built-ins, so a file named like one (`new.md`) runs there and not in the TUI.
- A name must match exactly: `/Deploy`, `/deploy/` and `--command ../deploy` are not `deploy`, and none reaches the model. A first word that is a command name followed only by slashes (`/deploy/`, `/deploy//`) is refused as a mistyped command, the same as `/quit/`. A first word with more after the slash, such as `/deploy/staging` or `/build/out.log`, is a path and the line is sent as a message.
- If the command files cannot be read when the TUI starts (for example the store is unreadable), the TUI still opens with its built-ins and shows a warning.

## Hooks

Hooks load only from the user's `config.toml`, as `[[hooks]]` entries. See [docs/config.md](config.md#hooks) for the events, the payload and what a failure does. A sub-agent's tool calls run the same `pre_tool` and `post_tool` hooks as the session's. A hook key (`hook`, `hooks`, `pre_tool`, `post_tool`, `pre_tool_use`, `post_tool_use`, `session_start`) in any definition file is an error, and that file is not loaded.

## Built-in agents

Four agents ship in the binary. They are ordinary agent files that are checked by the same lint as yours, and `coda agents list` shows them as `built-in`.

| Agent | Tier | Tools | Use |
| --- | --- | --- | --- |
| `explore` | `fast` | `code_outline`, `code_read_range`, `code_search`, `code_symbol`, `grep`, `read`, `retrieve` | Finds where something is and how it works. Reports `path:line` places. |
| `reviewer` | `standard` | the same read-only tools | Reviews code and replies with a JSON report of classified findings (see [The result](#the-result)). |
| `implementer` | `standard` | the read-only tools, `edit` and `bash` | Makes one clear change, runs the build or tests that cover it, and reports what it changed. |
| `tester` | `fast` | `bash`, `code_read_range`, `grep`, `read`, `retrieve` | Runs the tests or checks it is given and reports passes and failures. It has no `edit` tool, but `bash` can still change files; its prompt tells it to run and report, not to change files. |

`explore` and `reviewer` have no `bash` and no `edit`, so they cannot change anything. `tester` has no `edit` tool, but its `bash` can change files, so it is not read-only: every command it runs goes through the permission broker (see [Dispatch](#dispatch-the-task-tool)), and the user can refuse it.

To change one, write a file with the same name. A user file in `<CODA_HOME>/agents/` replaces the built-in one entirely and may give it other tools. A project file in `.coda/agents/` replaces its description and prompt once the repository is trusted, but can only narrow its tools and keeps the tier and effort of the definition below it. Asking for other tools, another tier or another effort is not an error, but the request is ignored and `coda agents lint` reports it, naming the file and the field. An untrusted project file changes nothing: the built-in or user agent runs as it is.

## Precedence and trust

Built-in agents load first, then user files, then project files. A user file with the same name replaces the built-in definition. A project file with the same name replaces the description and prompt of the definition below it, built-in or user. For an agent, the tier and effort below it stay, and the project's `tools` list can only narrow the one below it: a tool the earlier definition lacks is ignored and reported, and so is a tier or effort that differs from the one below. A project-only agent keeps its own tier, effort and `tools`.

Project files are untrusted prompt input. Until you trust the repository they are listed as pending and are never used, and nothing from them is used.

```
coda agents list            # usable definitions and pending project files
coda agents trust           # trust the project definitions of the current repository
coda agents trust --revoke  # stop trusting them
```

The decision is stored per repository root in the Coda database, with a digest of the trusted files: the sorted relative paths and content hashes of `.coda/agents`, `.coda/skills` and `.coda/commands` (`*.md`). If any of those files is edited, added or removed afterwards, every project definition goes back to pending and `coda agents list` and `coda agents lint` say they changed since they were trusted. Review the files, then run `coda agents trust` again. The digest is taken over the same bytes that are parsed, so a file edited while Coda loads cannot slip unreviewed text in. Only the store-backed loader can admit project files; nothing else can mark a repository as trusted.

## Lint

`coda agents lint` checks the user files and the project files of the current repository, trusted or not. It reports one line per problem as `<file>: <field>: <message>` on standard error and exits 22 when there is any. It rejects an unknown tier, effort or tool, a description without when-to-use and when-not-to-use, a model id, a hook key, an unknown or missing key, and a name that does not match the file.

## Dispatch: the `task` tool

A model-API session (the Anthropic and OpenAI-compatible providers) offers the model a `task` tool with two arguments, `agent` (a name) and `prompt`. The call runs that agent as a child conversation and gives the model the child's result as the tool result (see [The result](#the-result)). The tool lists the agents it can run in its description, which is built when the conversation starts. Claude Code and Codex sessions do not get `task`: those backends run their own agent loops, and Coda does not start sub-agents for them.

| Rule | Detail |
| --- | --- |
| Who can run | The built-in agents, the user's agents, and a project's agents once the repository is trusted. A name that only an untrusted project file defines is refused, and the refusal names `coda agents trust`. The files are read again on every call, so an edit after trusting is caught. |
| Fresh context | The child sees only the prompt, and an empty or blank prompt is refused before a child starts. Its system prompt is the agent's body. |
| Tools | The agent's `tools` list, kept to the tools the parent has. A child cannot start agents: every conversation loop is built in one place, which gives a loop the ability to run `task` only when a child of it would be within the depth cap (`MAX_DEPTH`, 1). A top-level session is at depth 0 and its children at depth 1, so they get none. A `task` entry in an agent file also fails lint. A child's `retrieve` reads the originals stored under the parent session's id, because the child's events are stored there. |
| Model | The agent's tier, mapped through `[routing]`. A tier with no entry uses the session's model. The routed alias must belong to the session's provider. |
| Permissions | Every call the child makes goes through the parent session's broker, so the parent's policy, mode and stored grants apply and the child never has a grant the parent lacks. A prompt reaches the parent's client with the agent's name in its own `agent` field, apart from the command text, which reads as it does for the session's own prompts: the command line, or the tool and path for a tool with no command, for example `cargo test` or `edit src/a.rs`. The terminal UI draws the agent on its own line, so a command of the session's own that starts with `[reviewer]` is not mistaken for the agent's, and the decision log keeps the agent beside the command. An "always allow" answered for a child covers that run only, and the prompt says so. It is kept with the run and checked where the broker checks stored grants, under the same rules: an edit to a path that changes what runs (for example `scripts/`, `.github/workflows/`, `.git/hooks/`, `Cargo.toml` or `build.rs`) still prompts in accept-edits mode every time, and every answered call is recorded. It is never stored, so the parent's grants do not grow. Where nobody can answer, as in `coda run`, the prompt is denied for the child as it is for the parent. Starting a child asks no permission. |
| Cancel and budget | Cancelling the parent cancels the child. The child may spend only the tokens the parent has left, and what it spends counts against the parent's budget and shows in the usage events. |
| Storage | The child's events are stored in the parent session, each as a `child_event` that names the agent. They are sent up the parent's own event channel, so a client sees them after the parent's reply that holds the `task` call and before that call's result. Replay skips them, so a rebuilt parent conversation holds the `task` call and its result and nothing the child did. |

### The result

The tool result begins with `status:`, which is `completed`, `failed` (the run hit a limit, was refused or errored) or `cancelled`, as Coda saw the run end. The agent's own report follows between `<<<DATA #n>>>` and `<<<END DATA #n>>>` markers, with a line before it telling the parent model to treat it as data. Coda picks `n` so that neither marker occurs in the report, so words in the report that imitate a marker can neither end the data early nor start a second block. The status line comes from Coda, so a report that says `status: completed` cannot change it. The report is kept to 16 KiB (`MAX_REPORT_BYTES`). Findings are listed first in the budget, blocking first, then `non_blocking`, then `suggestion`. Each message is cut to 2 KiB (`MAX_FINDING_MESSAGE_BYTES`), and a finding that still does not fit is skipped, so a long one cannot hide the ones after it. The summary gets the rest of the 16 KiB, cut on a character boundary, but a long summary always leaves the findings at least half of it. Lines after the closing marker say what was left out: `[truncated: the summary was N bytes and the first M are shown]`, `[truncated: N finding messages were cut to the first 2048 bytes]` and `[N findings not shown, M of them blocking]`. A malformed-report reason is cut and noted the same way.

An agent's last message is its report. Plain text is the summary. So is JSON of any other shape, such as `{"name": "x"}`, and text that merely starts with `{`. A reply with no text at all has the summary `the agent ended without a reply`. A message is a report only when it is a JSON object, bare or as the whole body of one code fence, with a `summary` or a `findings` key. Text after a bare report is ignored, and text before it makes the whole reply the summary. A fence is the first and last fence lines of the reply, and the report between them may itself hold code fences. A review agent uses this to classify what it found:

```json
{"summary": "One problem.",
 "findings": [{"severity": "blocking", "message": "lock held across await", "location": "src/a.rs:10"}]}
```

`summary` is required. `findings` is optional (`null` means none), and each finding needs a `severity` (`blocking`, `non_blocking` or `suggestion`; `non-blocking` is read as `non_blocking`) and a `message`. A report with a missing field, a wrong type or an unknown severity is rejected, and so is text that starts with `{` and names `"summary"` or `"findings"` but is not valid JSON: the parent gets an error that names the reason, also between markers because it can quote the agent, and none of the report.
