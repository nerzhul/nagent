# Rules for Agents and Contributors

This document defines the working rules that every agent (human or AI) must follow when contributing to this repository.

## 1. Language

All content produced in this repository must be written **in English**, including but not limited to:

- Commit messages (title and body).
- Source code, identifiers, strings, and inline comments.
- Documentation files (Markdown, READMEs, specs, etc.).
- Pull request and issue descriptions.
- Test names, assertions, and error messages.

Do not mix languages. When a feature originates from a non-English request or discussion, translate it before committing. Translations of existing documentation may be added later in a dedicated, clearly named file (e.g. `README.fr.md`), but the canonical version is always the English one.

## 2. Commit Convention

Commits must follow a strict format so the history stays readable and usable.

- **Title (first line)**:
  - Explicit: it must describe what the commit does, not just "update" or "fix".
  - **Maximum 120 characters**.
  - Single line, no trailing period.
- **Description (body)**:
  - **Maximum 5 sentences**.
  - **Maximum 500 characters** in total.
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
- **Configuration file**: any variable read by the application or its tooling in examples/*.toml

Required actions when such a parameter changes:

1. **Locate every documentation surface** that mentions the parameter: `README.md`, `AGENTS.md`, files under `docs/`, kustomize/k8s manifests comments, `--help` output, inline help text, and any spec or design doc.
2. **Update each surface** to match the new name, default, type, range, or behavior. If a parameter is **removed**, also document the removal and, when reasonable, the migration path.
3. **Mention the change in the commit body** when relevant so reviewers can spot the doc updates tied to the parameter change.
4. **Add or update tests** that guard the documented contract (e.g. snapshot tests for `--help`, env-var parsing tests, kustomize-render checks) so a future regression is caught automatically.

If a parameter change genuinely has **no documentation surface** to update (unusual but possible — e.g. an internal helper flag), state that explicitly in the commit body so reviewers know the rule was considered.
