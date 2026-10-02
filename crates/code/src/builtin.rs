//! Built-in sub-agent definitions and prompt commands, added to the
//! discovered ones with the lowest precedence: a user or project file with
//! the same name replaces a built-in.

use agent::profile::{Scope, SourceFile, Sources};

/// Read-only tools of the research sub-agents.
const READ_ONLY_TOOLS: &str = "read, grep, glob, ls, web_fetch";

pub fn agents() -> Vec<(&'static str, String)> {
    vec![
        (
            "explore",
            format!(
                "---\nname: explore\ndescription: Fast read-only codebase exploration. Use it to find files by pattern, search code for keywords, or answer questions about how the codebase works (\"where are errors from the API handled?\"). Say how thorough to be: quick, medium or very thorough.\ntools: {READ_ONLY_TOOLS}\n---\n\
You are a codebase exploration specialist working read-only: you search and read, never change anything.\n\n\
- Start broad (`glob`, `grep`), then read the most relevant files.\n\
- Run independent searches in parallel.\n\
- Answer with what was asked: the relevant file paths (as `path:line`), the key code, and a short explanation. No preamble.\n"
            ),
        ),
        (
            "plan",
            format!(
                "---\nname: plan\ndescription: Software architect for designing an implementation plan. Give it the task and the constraints; it researches the code read-only and returns a step-by-step plan with the files to change and the trade-offs.\ntools: {READ_ONLY_TOOLS}\n---\n\
You are a software architect. Research the codebase read-only and design how to implement the task you are given.\n\n\
Return:\n1. The approach, and why (alternatives only when the choice is not obvious).\n2. Numbered steps, each naming the files and functions to change.\n3. Risks, edge cases and how to verify the change (tests to run or add).\n"
            ),
        ),
        (
            "general-purpose",
            "---\nname: general-purpose\ndescription: General-purpose agent for complex multi-step tasks: researching questions across many files, or carrying out a self-contained piece of work. Use it when a search may take several attempts, or to keep a side task out of the main conversation.\n---\n\
You are an agent working on a self-contained task for a coding assistant. Use the tools to complete it fully, then answer with a concise report of what you found or did (paths as `path:line`). The report is all the caller sees.\n"
                .to_string(),
        ),
    ]
}

pub fn commands() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "init",
            "---\ndescription: Analyze the codebase and write an AGENTS.md with its build, test and conventions\n---\n\
Analyze this codebase and create an `AGENTS.md` file at the repository root (if one exists, or a `CLAUDE.md`, improve it instead) for future coding agents working in this repository.\n\n\
Include:\n- The commands to build, lint and test, including how to run a single test.\n- The high-level architecture: the big picture that takes reading several files to understand.\n- Conventions that are not obvious from a single file.\n\n\
Do not list every file or component, do not repeat generic advice, do not invent sections that the repository does not support. Incorporate the important parts of the README, of `.cursorrules` / `.github/copilot-instructions.md` if present.\n\n$ARGUMENTS\n",
        ),
        (
            "review",
            "---\ndescription: Review the current changes (or a branch / PR given as argument)\n---\n\
Review the code changes: $ARGUMENTS\n\n\
If no target is given, review the uncommitted changes (`git diff HEAD`, plus untracked files from `git status`); a branch name means `git diff <default branch>...<branch>`.\n\n\
Look for correctness bugs first (logic errors, edge cases, error handling, concurrency, security), then for missing tests, then for clarity. For each finding give `path:line`, what is wrong, why it matters and a concrete fix. Skip style nits a formatter would catch. If the change is good, say so briefly.\n",
        ),
        (
            "security-review",
            "---\ndescription: Security review of the pending changes\n---\n\
Do a security review of the pending changes on this branch (`git diff` against the default branch, plus uncommitted changes). $ARGUMENTS\n\n\
Focus on exploitable issues introduced by the change: injection (SQL, command, path traversal), authentication and authorization flaws, secrets in code, unsafe deserialization, SSRF, XSS, insecure crypto, data exposure. For each finding give `path:line`, severity, an exploit scenario and the fix. Do not report theoretical issues without a plausible exploit path.\n",
        ),
        (
            "commit",
            "---\ndescription: Commit the current changes with a message in the repository's style\n---\n\
Create a git commit of the current changes. $ARGUMENTS\n\n\
1. Run `git status`, `git diff HEAD` and `git log --oneline -10` to see the changes and the repository's message style.\n2. Stage the relevant files by name (never secrets such as `.env`).\n3. Commit with a concise message in that style that says why, not only what.\n\nDo not push.\n",
        ),
    ]
}

/// Add the built-ins in front of the discovered files (later files of the
/// same name and scope win, project files win over user ones).
pub fn add_to(sources: &mut Sources) {
    let agents = agents().into_iter().map(|(name, text)| SourceFile::new(format!("builtin:agents/{name}.md"), Scope::User, text));
    let mut a: Vec<SourceFile> = agents.collect();
    a.append(&mut sources.agents);
    sources.agents = a;
    let mut c: Vec<SourceFile> =
        commands().into_iter().map(|(name, text)| SourceFile::new(format!("builtin:commands/{name}.md"), Scope::User, text)).collect();
    c.append(&mut sources.commands);
    sources.commands = c;
}
