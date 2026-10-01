# Rules for Agents and Contributors

This document defines the working rules that every agent (human or AI) must follow when contributing to this repository.

## 0. Required reading

Before working on this codebase, read the architecture reference at
[`docs/architecture.md`](docs/architecture.md). It describes the
workspace layout and the three primary subsystems — **API** (HTTP/WS
transport), **agents** (`nagent-agents` crate), and **tools** (LLM
function-calling integration) — so the rest of `AGENTS.md` lands in
context. If a change touches the architectural breakdown of any
subsystem, update the relevant section in `docs/architecture.md` in
the same commit (see §5 — Documentation Sync).

## 1. Language

All content produced in this repository must be written **in English**, including but not limited to:

- Commit messages (title and body).
- Source code, identifiers, strings, and inline comments.
- Documentation files (Markdown, READMEs, specs, etc.).
- Pull request and issue descriptions.
- Test names, assertions, and error messages.

Do not mix languages. When a feature originates from a non-English request or discussion, translate it before committing. Translations of existing documentation may be added later in a dedicated, clearly named file (e.g. `README.fr.md`), but the canonical version is always the English one.

## 2. Commit Convention

Before committing any change:

- Ensure code is properly tested
- run `cargo fmt`
- run `cargo build --all-targets` (and `--all-features` when relevant) with **zero warnings** — warnings are treated as errors; do not introduce code that compiles with `warning:` output under any default feature set the touched code is built with. Fix the warning at its source rather than silencing it (`#[allow(...)]` is only acceptable when explicitly justified in a comment and approved during review).
- run `cargo clippy` (if applicable) and resolve every reported lint
- run `cargo audit`

Commits must follow a strict format so the history stays readable and usable.

- **Title (first line)**:
  - Explicit: it must describe what the commit does, not just "update" or "fix".
  - **Maximum 120 characters**.
  - Single line, no trailing period.
- **Description (body)**:
  - **Maximum 5 sentences**.
  - **Maximum 450 characters** in total.
  - Explain the **why** more than the **what** when relevant.
  - Separate the title from the body by a blank line, per standard Git convention.

Example:

```
feat: add user authentication via OAuth2

Implements OAuth2 login flow to replace the legacy password-based auth.
Tokens are stored in HTTP-only cookies for better security. Adds unit tests
covering the success and failure paths. No breaking change on existing
public API.
```

## 3. Unit Tests

Unit tests are an integral part of the delivered work. They must be **run systematically** once the current task is finished, before considering the work done.

- Run the test suite appropriate for the project (`cargo test`, `pytest`, `npm test`, etc.) at the end of the work.
- Any failing test must be analyzed before moving on.
- **Arbitration between fixing the code or fixing the tests**:
  - If the user has not explicitly stated which solution to favor, **ask them** before changing anything.
  - Never assume that either the production code or the test is "obviously" the source of the bug: both hypotheses must be presented to the user.
- Do not disable, delete, or skip a test to make it pass, unless the user explicitly asks for it.

## 4. Code Quality and Comments

Code must be **self-explanatory** as much as possible: clear variable, function, and structure names, short functions, single responsibility.

A comment is required as soon as one of these cases applies:

- The intent of the code is not immediately obvious when reading it.
- There is a known pitfall, limitation, or counter-intuitive behavior.
- A non-trivial technical decision was made (workaround, dependency bug circumvention, etc.).
- An external reference is required (issue number, link to a spec, etc.).

On the contrary, avoid comments that:

- Repeat what the code already says ("increment i by 1" above `i++`).
- Comment on code that has been removed or disabled for a long time.
- Serve as a personal journal ("TODO: redo later" without context).

## 5. Documentation Sync for Build, Runtime, and Environment Changes

Any change that affects how the project is **built**, **run**, or **configured** at runtime must be reflected in the documentation in the **same commit**. Documentation that drifts from the actual behavior is treated as a bug.

Scope — this rule applies whenever a change touches any of the following:

- **Build configuration**: `Cargo.toml` features, `Makefile` / `*.mk` targets, build scripts, feature flags, toolchain pins, Dockerfiles, CI workflow files (`.github/workflows/*.yml`), kustomize or Helm manifests, generated lockfiles.
- **Runtime configuration**: command-line arguments, configuration files (e.g. `config.toml`, YAML/JSON defaults), default ports, paths, file locations, log levels, feature toggles read at runtime.
- **Environment variables**: any variable read by the application or its tooling (names, semantics, accepted values, defaults, required vs optional, deprecation status).
- **Configuration file**: any variable read by the application or its tooling in docs/examples/*.toml

Required actions when such a parameter changes:

1. **Locate every documentation surface** that mentions the parameter: `README.md`, `AGENTS.md`, files under `docs/`, kustomize/k8s manifests comments, `--help` output, inline help text, and any spec or design doc.
2. **Update each surface** to match the new name, default, type, range, or behavior. If a parameter is **removed**, also document the removal and, when reasonable, the migration path.
3. **Mention the change in the commit body** when relevant so reviewers can spot the doc updates tied to the parameter change.
4. **Add or update tests** that guard the documented contract (e.g. snapshot tests for `--help`, env-var parsing tests, kustomize-render checks) so a future regression is caught automatically.

If a parameter change genuinely has **no documentation surface** to update (unusual but possible — e.g. an internal helper flag), state that explicitly in the commit body so reviewers know the rule was considered.

## 6. Commit Authorization and Authorship

**The single most violated rule in this codebase. Read this section twice before running `git commit`.**

- **The agent never pushes. Pushing is exclusively the user's manual action.** Even when the user authorises a commit, the agent must stop after `git commit` and wait for the user to run `git push` themselves. This is non-negotiable. Reasons:
  - Pushing publishes history; the agent cannot un-publish. A bad push forces the user to rewrite published commits, which is exactly what the "no amend / no force-push" rule below forbids the agent from doing.
  - The user owns the review-and-publish step. The agent does not get to decide when remote state changes.
  - The agent has no way to recover from a misconfigured remote (wrong upstream, force-push to a shared branch, accidental push of a WIP commit).
  
  If the user says "commit and push", the correct response is to commit, **stop**, and remind the user that they need to push manually. If the user says "ship it" or "deploy", that still does not authorise a push — commit, summarise the diff, and tell the user the local branch is ready for them to push.
- **Never commit, push, tag, or run any other state-mutating Git command without an explicit user instruction.** This holds *for every commit*, including follow-up fixes, refactors, and "obvious" next steps after a feature lands. A feature request is **not** a commit authorisation. The two are separate acts:
  1. The user describes what they want changed.
  2. The agent makes the change in the working tree, runs the tests, and **stops**.
  3. The user reviews and gives a separate, explicit "commit", "commit it", "git commit", or equivalent.
  4. Only then does the agent run `git add` / `git commit`.

  When in doubt, **ask** before committing. Asking costs seconds; an unwanted commit costs the user a force-push or a manual revert.

- **Default to "no commit" at the end of a task.** Even if the user said "fix X" three turns ago, and you've now finished fixing X, do not commit. Show the diff, summarise the change, and wait for the user to type the commit instruction.
- **Preserve the existing Git identity.** Do not change `user.name` or `user.email` (locally, globally, or per-repo) to commit under an agent name. The author of the commit is the user; impersonating a different author is forbidden.
  - This rule covers **all** ways of changing the identity of a commit, including the per-command `git -c user.name=... -c user.email=...` override flag. If a commit would be authored by anyone other than the existing `git config user.name` / `user.email`, do not run `git commit`. Use the user's configured identity, full stop.
  - When adding a `Co-authored-by:` trailer (see below), the trailer email goes in the **trailer**, not in `user.email`. The primary `Author` line is always the user's identity; the model only appears as a co-author.
- **Add the model as a co-author.** When the user asks an AI agent to commit, append a `Co-authored-by:` trailer identifying the model that produced the change. Use the format below; replace `<Model Name>` with the actual model identifier (e.g. `Claude Opus 4.1`, `GPT-5`, `MiniMax-M3`):

  ```
  Co-authored-by: <Model Name> <noreply@anthropic.com>
  ```

  Use a `<Model Name>@<vendor>` address that identifies the provider (e.g. `Claude Opus 4.1 <noreply@anthropic.com>`, `GPT-5 <noreply@openai.com>`). If the exact vendor address is unknown, prefer `<Model Name> <noreply@local>` rather than fabricating a real-looking address.
- **Do not amend, force-push, rebase, skip hooks, or rewrite published history** without an explicit user instruction. If a commit fails or hooks reject it, fix the cause and create a new commit instead.

### Worked examples

| Scenario | Correct response |
|---|---|
| User: "add feature X" | Implement X. Run tests. Show the diff. **Do not commit.** Wait. |
| User: "add feature X and commit it" | Implement X. Run tests. **Commit** with the user-prescribed format and a `Co-authored-by:` trailer. |
| User: "fix the bug" | Diagnose, fix, run tests, show the diff. **Do not commit.** Wait. |
| User: "fix the bug and ship it" | Fix, run tests, **commit** with the user's `Co-authored-by:` trailer. |
| User: "what does `commit -m` do?" | Explain the command. **Do not run it.** |
| Agent finished a task five minutes ago, user has been silent | **Do not commit.** Resume work or wait. |
| Agent wants to add `Co-authored-by: Kilo <...>` | Add the **trailer only**. Never pass `-c user.name=Kilo -c user.email=...` to override the primary `Author` line; that impersonates the user and produces a commit whose `Author` field is the agent's name, not the user's. |
| Agent wonders whether to use `--amend` or `--force-push` | **Don't.** Both rewrite published history. If a commit fails or hooks reject it, fix the cause and create a new commit instead. |
| User: "commit and push" | **Commit** and stop. Remind the user that they need to push manually — pushing is exclusively the user's action. Do not run `git push` under any circumstance. |
| User: "ship it" or "deploy" | **Commit** (with the user's explicit instruction), summarise the diff, tell the user the local branch is ready. Do not push, do not deploy, do not run any remote-mutating command. |
