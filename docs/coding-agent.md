# `peras`: the coding agent

`crates/code` (package `agent-code`, binary `peras`) is a coding agent for the terminal built on the framework and
modeled on Claude Code. The framework supplies the loop, the journal, gating, sandboxing, sub-agents, hooks, MCP,
skills, plugins and rewind. The crate adds the product layer: the coding system prompt, the editing and planning
tools, permission modes, built-in sub-agents and commands, workspace trust, a conversation index and two frontends.

## Layout

| Module | Role |
|---|---|
| `prompt` | Coding system prompt (replaces the framework's base prompt) and the static environment block: working directory, platform, whether it is a git repository |
| `context` | State snapshots (permission mode, task list, git state, date) and long-term memory (`user` scope in `~/.agent/memory`, `project` scope in `.agent/memory`) |
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

## Context engineering

The framework does most of it on its own: three layers, append-only requests with write-time rendering, cache
breakpoints, pressure relief (spill, trim, image offload, summaries), subdirectory instructions injected on access,
skills loaded progressively, and the trust framing. `peras` uses that machinery in four places.

- **Stable system prompt.** The system prompt holds only what does not change during a session, so its cache prefix
  survives.
- **State snapshots.** Before each turn, the values that changed are sent as state (`Chat::set_state`): the
  permission mode, the open task list, the git state (at most once a minute, via `[[snapshots]]`) and the date. The
  kernel appends a snapshot only when a value changed. Old snapshots are superseded and dropped first under
  pressure. The model sees a mode change without a change to the tool definitions: the design's "visibility is
  separate from executability".
- **Manual compaction.** `/compact` goes through the kernel's own summary path: a replacement event, the
  `PreCompact` hook, taint carried over to the summary. It does not start a new session.
- **Long-term memory.** Memory is loaded into the Durable layer when a session starts. `remember` and `recall` are
  `mem:` resources. Memory is a persistence target, so a tainted session cannot write it without a human.

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
| Auto-compaction, `/compact` | ✅ | The kernel compacts under context pressure. `/compact [focus]` is `Control::Compact`: a turn of its own that replaces the whole history with a summary, which keeps its taint. |
| Memory (`#`, CLAUDE.md) | ✅ | `#note` → `AGENTS.md`. `remember` / `recall` give durable memory that is loaded into the next session; writes are gated, and a tainted session needs a human to approve them. |
| `/cost`, `/status`, `/model` | ✅ | Cost shows only when the meter has a price for the model. |
| Workspace trust dialog | ✅ | Remembered per directory; `--trust` skips the prompt. |
| Sandboxed bash | ➕ | bubblewrap / landlock / seatbelt / container. An opaque command runs isolated and its file changes are reviewed before they merge. |
| Prompt-injection defenses | ➕ | Taint tracking from trust annotations. Egress and persistence invariants, secret redaction. |
| Steering while the agent works | ✅ | Lines typed during a turn reach the agent at its next step. |
| `@file` mentions with completion, images, vim mode, status line, output styles, IDE integration | ❌ | |
| Other model vendors | ✅ | `--provider anthropic` (default; `[model] id` or `--model`, `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`) or `--provider openai` (`--model` required, `OPENAI_API_KEY`, `--base-url` / `OPENAI_BASE_URL`, `/v1` added when the URL has no version) for any OpenAI-compatible chat-completions endpoint. `--context-window` sets the window (openai default 128000). `PERAS_PROVIDER` selects the provider. |

✅ supported · ⚠️ partial or different · ➕ beyond Claude Code · ❌ missing

## Testing

`crates/code/tests/e2e.rs` drives the agent with a scripted model through:
- the editing tools;
- plan mode: refusal, approval and rejection;
- `--allowed-tools` / `--disallowed-tools`;
- the assembled tool set;
- built-in definitions yielding to project files;
- print mode (`json`, `stream-json`, denied questions);
- the REPL: commands, approval with "don't ask again", denial with an instruction, `!` and `#`;
- the OpenAI-compatible provider against a local endpoint;
- state snapshots reaching the model only when they change;
- `/compact` replacing history in place;
- memory written in one session being loaded in the next.
