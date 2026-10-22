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
| `description` | Required. Routing reads it, so it should say when to use the agent and when not to. |
| `tier` | Required. `fast`, `standard` or `deep`. |
| `effort` | Required. `low`, `medium` or `high`. |
| `tools` | Required. The only tools the agent can call. Tool names, for example `read` or `grep`. An agent is read-only because the list holds no write tool, not because its prompt says so. |

A `model` key is an error: an agent file never names a model id.

## Skill

Frontmatter: `name`, `description`. The body is the instructions.

## Command

Frontmatter: `name`, `description`, and an optional `argument_hint` such as `<issue>`. The body is the prompt template.

## Hooks

Hooks load only from the user's `config.toml`. A hook key (`hook`, `hooks`, `pre_tool`, `post_tool`, `pre_tool_use`, `post_tool_use`, `session_start`) in any definition file is an error, and that file is not loaded.

## Precedence and trust

User files load first. A project file with the same name replaces the user definition's description and prompt. For an agent, the user's tier and effort stay, and the project's `tools` list can only narrow the user's: a tool the user definition lacks is ignored and reported. A project-only agent keeps its own `tools`.

Project files are untrusted prompt input. Until you trust the repository they are listed as pending and are never used, and nothing from them is used.

```
coda agents list            # usable definitions and pending project files
coda agents trust           # trust the project definitions of the current repository
coda agents trust --revoke  # stop trusting them
```

The decision is stored per repository root in the Coda database, with a digest of the trusted files: the sorted relative paths and content hashes of `.coda/agents`, `.coda/skills` and `.coda/commands` (`*.md`). If any of those files is edited, added or removed afterwards, every project definition goes back to pending and `coda agents list` says they changed since they were trusted. Review the files, then run `coda agents trust` again. The digest is taken over the same bytes that are parsed, so a file edited while Coda loads cannot slip unreviewed text in. Only the store-backed loader can admit project files; nothing else can mark a repository as trusted.
