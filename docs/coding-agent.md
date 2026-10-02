# `peras`: the coding agent

`crates/code` (package `agent-code`, binary `peras`) is a coding agent for the terminal built on the framework and
modeled on Claude Code. The framework supplies the loop, the journal, gating, sandboxing, sub-agents, hooks, MCP,
skills, plugins and rewind. The crate adds the product layer: the coding system prompt, the editing and planning
tools, permission modes, built-in sub-agents and commands, workspace trust, a conversation index and two frontends.

## Layout

| Module | Role |
|---|---|
| `prompt` | Coding system prompt (replaces the framework's base prompt) and the environment block: working directory, git branch / status / recent commits, platform, date |
| `tools` | `edit` with `replace_all` (replaces the built-in `edit`), atomic `multi_edit`, `ls`, `todo_write`, `exit_plan_mode` |
| `mode` | Permission modes and session grants: a ring-4 gate (plan mode) and a ring-5 auto-answer rule (accept edits, bypass, grants) |
| `builtin` | Sub-agents `explore`, `plan`, `general-purpose` and commands `/init`, `/review`, `/security-review`, `/commit`, added to the discovered files with the lowest precedence |
| `trust` | Workspace trust remembered in `~/.agent/code/trusted.json` |
| `history` | Conversation index `.agent/code/sessions.jsonl` (`--continue`, `--resume`, `/resume`) |
| `render` | Tool titles, result previews, diffs, the todo checklist, usage totals |
| `repl` | Interactive REPL |
| `print` | Print mode (`-p`) with `text`, `json` and `stream-json` output |

Everything goes through the framework's gate chain, so the modes can only do two things. They can make calls
stricter: plan mode is a ring-4 gate that refuses anything but reads. They can answer policy-level questions: accept
edits, bypass and grants are ring-5 auto-answer rules. Invariant-level questions (exfiltration, persistence,
self-modification, unknown effects) always reach the user, including in `bypassPermissions`.

Supporting framework changes:
- `Agent::system_prompt`, `settings` (command-line layer as text), `sources` (built-in definitions), `extra_tools`
  (adds tools to the defaults) and `model_port`.
- `Chat::control` and `Ask::subject`.
- Questions an auto-answer rule answers are opened already answered (`GateExecutor::auto_answer`,
  `AskBoard::open_answered`), so clients only ever see questions a human has to answer.

## Compared with Claude Code

| Claude Code | `peras` | Notes |
|---|---|---|
| Interactive session, initial prompt | ✅ | Line-based REPL with streaming output, not a full-screen UI. `agent tui` is the full-screen client of the framework. |
| `-p` print mode, `--output-format text/json/stream-json`, piped stdin | ✅ | `--on-ask deny/defer` decides what happens to questions in print mode. `--input-format stream-json` is not supported. |
| `-c` / `-r` / `/resume` | ✅ | Conversations live in the journal (`.agent/runs.db`). |
| Read, Write, Edit, MultiEdit, Glob, Grep, LS | ✅ | `read`, `write`, `edit` (`replace_all`), `multi_edit`, `glob`, `grep`, `ls`. Stale-write detection comes from the capability handles. |
| Bash, background shells, BashOutput / KillShell | ✅ | `bash` (`background: true`), `task_output`, `task_kill`, `task_list`. |
| WebFetch | ✅ | `web_fetch`. Its output is untrusted and taints the session. |
| WebSearch, NotebookEdit | ❌ | No search provider; no notebook tool. |
| TodoWrite | ✅ | `todo_write`, shown as a checklist, `/todos`. |
| Task tool with sub-agents | ✅ | Built in: `explore`, `plan`, `general-purpose`. You can add more in `.agent/agents/*.md`, and they can run in the background or as a fork. |
| Permission modes, Shift+Tab | ✅ | `--permission-mode`, `/mode`, `/plan`. There is no Shift+Tab: the REPL reads whole lines. |
| Plan mode + ExitPlanMode approval | ✅ | Approval options: auto-accept edits, approve edits manually, or keep planning. |
| `--allowedTools` / `--disallowedTools`, `Bash(cmd:*)` | ✅ | `--allowed-tools` grants the tool or command prefix; `--disallowed-tools` adds a policy deny rule. |
| "Don't ask again" | ✅ | For the session only: grants are not written to settings. Allowing a command prefix never covers a chained command (`&&`, `;`, `\|`). |
| `--dangerously-skip-permissions` | ⚠️ | Same as `bypassPermissions`; invariant checks still ask. |
| CLAUDE.md memory hierarchy, `#` notes | ✅ | `AGENTS.md` / `CLAUDE.md` from the project root to the working directory, plus subdirectories on first access. `#note` appends to `AGENTS.md`. There is no `/memory` editor. |
| Custom slash commands, `/init`, `/review`, `/security-review` | ✅ | `.agent/commands/*.md` and `$ARGUMENTS`, plus `/commit`. |
| `!` bash mode | ✅ | The output goes with the next message. |
| Hooks, MCP servers, plugins, skills | ✅ | From the framework (`[[hooks]]`, `[mcp.*]`, plugins, `skills/*/SKILL.md`). Changes are hot reloaded between turns. |
| Checkpoints, `/rewind` | ✅ | Restores the conversation and the agent's file changes. Conflicting edits and irreversible operations are reported. |
| Auto-compaction, `/compact` | ✅ | The kernel compacts under context pressure. `/compact` starts a new conversation seeded with a summary. |
| `/cost`, `/status`, `/model` | ✅ | Cost shows only when the meter has a price for the model. |
| Workspace trust dialog | ✅ | Remembered per directory; `--trust` skips the prompt. |
| Sandboxed bash | ➕ | bubblewrap / landlock / seatbelt / container. An opaque command runs isolated and its file changes are reviewed before they merge. |
| Prompt-injection defenses | ➕ | Taint tracking from trust annotations. Egress and persistence invariants, secret redaction. |
| Steering while the agent works | ✅ | Lines typed during a turn reach the agent at its next step. |
| `@file` mentions with completion, images, vim mode, status line, output styles, IDE integration | ❌ | |
| Other model vendors | ⚠️ | Anthropic models through `[model] id` or `--model`. The framework has an OpenAI-compatible adapter, but `peras` does not expose it. |

✅ supported · ⚠️ partial or different · ➕ beyond Claude Code · ❌ missing

## Testing

`crates/code/tests/e2e.rs` drives the agent with a scripted model through:
- the editing tools;
- plan mode: refusal, approval and rejection;
- `--allowed-tools` / `--disallowed-tools`;
- the assembled tool set;
- built-in definitions yielding to project files;
- print mode (`json`, `stream-json`, denied questions);
- the REPL: commands, approval with "don't ask again", denial with an instruction, `!` and `#`.
