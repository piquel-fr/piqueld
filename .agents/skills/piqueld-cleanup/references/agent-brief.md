# Cleanup pass: agent brief

You are one of several agents running a parallel cleanup pass over piqueld.
Each agent owns one area of the workspace, works in its own worktree, and
opens one draft PR. Your task message names your area, paths, packages,
branch, title scope and `CARGO_BUILD_JOBS` value, and lists what the other
agents own.

Read `AGENTS.md` first.

## Goal

The owner asked for this: "a full code cleanup pass. Look for hard to read,
inconsistent, undocumented code. Remove redundancy, duplication. Simply clean
the code. If you find bigger issues, report them. You have permission to
refactor all the code to make it cleaner and easier to read. Be thorough, go
through the whole codebase."

Read every file in your area; don't sample. Make real simplifications, not
cosmetic churn:

- Merge duplicated functions, even small ones.
- Collapse near-identical code with a helper, a trait or a macro.
- Replace stringly-typed code with types.
- Turn free functions into methods on the type they work on.
- Move a check that every caller repeats into one place, such as an
  extractor, a constructor or middleware.
- Delete dead code. Use `rg` across the whole workspace, including the wasm32
  dashboard, to confirm nothing calls it.
- Deduplicate test setup into shared fixtures.
- Split files that mix unrelated jobs. A module with child modules is
  `foo/mod.rs`, never `foo.rs` next to `foo/` (Clippy's
  `self_named_module_files` enforces this), so a module with a sibling
  `foo_tests.rs` becomes `foo/mod.rs` with `foo/tests.rs`. A module without
  children stays a plain `foo.rs`.

## The owner's style

- Simplicity first. Don't add an abstraction that isn't paying for itself.
- Concise doc comments on items that are hard to use without one, not on
  every line. Keep existing comments accurate when the code under them moves.
- Match the surrounding code.
- Use the type system and macros for repetitive boilerplate. Prefer
  compile-time checks to runtime ones.
- No new `#[allow(...)]`. When a lint must be silenced, use
  `#[expect(..., reason = "...")]` on the one item that needs it, never for a
  whole module. Remove existing exceptions that no longer hold.
- Errors keep their context and source (`thiserror`, `anyhow`).
- Tests stay focused. Don't add smoke tests. Remove a test only when another
  test asserts exactly the same thing, and list every test you merge or
  remove in the PR description.

## Rules

1. **Behavior stays the same.** Don't change wire formats, HTTP responses
   (status, error code, message, and which error a request with several
   faults gets), OpenAPI output, the database schema or migrations, the
   manifest format, CLI flags, help text, output or exit codes, the
   configuration format, NixOS module options, Docker names and labels, log
   field names, or query plans. Don't touch generated files: `just
   openapi-check` must pass, and `git diff origin/main` must show no change
   to any file `just generate` writes (`openapi-check` only checks that they
   are fresh, so a regenerated change would also pass it). Each of these
   changed by accident in the first pass, and a reviewer caught it. Avoid blanket search-and-replace: one in the first pass rewrote three
   error paths.
2. **Report bugs, don't fix them.** Write each one down for your final report.
   The exception is an obvious one-line fix: put it in its own commit and call
   it out in the PR description. If a refactor changes behavior on purpose
   (for example, an edge case that used to panic), it gets its own commit and
   a line in the description too.
3. **Stay in your area.** The other agents are editing the rest at the same
   time. Edit a file outside your area only when a change inside it forces
   you to, such as a renamed public item, and keep that edit minimal. Report
   duplication that crosses areas instead of fixing it: it is fixed in a
   follow-up PR after the pass merges.
4. **Docs.** Another agent checks the documentation. Don't rewrite pages, but
   if you rename or move something a page under `docs/` names, update that
   reference in the same commit (`rg` for the old name), following the
   `piqueld-docs` skill, and run `just docs`. Keep `///` doc comments
   accurate when the code under them moves, but don't reword ones that feed
   generated output.
5. **Never use bare `git stash`.** The stash is shared with other worktrees.
   Commit work in progress instead.
6. **When something needs a design decision, stop and report it.** Don't
   guess.

## Checks

Export your `CARGO_BUILD_JOBS` value for every cargo command: several agents
build at once, and the machine has no swap. Don't run `just validate`, or any
`--all-features` build of `piqueld` your area doesn't need (it compiles the
wasm dashboard). Run these for your packages:

- `cargo fmt --all`
- `cargo clippy --locked -p <package> --all-targets -- -D warnings`. The
  workspace lints are `clippy::pedantic`.
- For the dashboard or the client, the wasm32 Clippy step from `just lint`.
- `cargo nextest run --locked -p <package>`, filtered where the suite is huge,
  but running every test binary that covers code you changed.
- `cargo test --locked --doc -p <package>` if you touched doc examples.
- `just openapi-check` if you touched any type that is serialized, has a
  schema, or feeds the CLI reference.

Prove each "unchanged" claim you make in the PR description with a
before-and-after comparison against `origin/main`: `--help` output, serialized
specs, query plans, error codes and paths, NixOS module output. Your reviewer
will check them.

If a test fails, check whether it also fails on `origin/main` from the same
path before calling it unrelated. `fake_docker` timing tests can fail under
load; rerun them once and say so.

## Delivery

- Make logical, reviewable commits, for example one per module or per kind of
  cleanup. Keep any single commit small enough to review; a behavior change
  hidden in an 8,000-line commit had to be split out in the first pass.
- Conventional commit messages, such as
  `refactor(store): share row decoding between deployment queries`, with no
  `Co-Authored-By` line.
- Before you push, `git fetch origin && git rebase origin/main`, and re-run
  your checks if the rebase brought in changes to your area. Apply your
  cleanup to any new code the rebase brought in.
- Push the branch your task names and open a draft PR with the `file-pr`
  skill. Title: `refactor(<scope>): <what got simpler>`, for example
  `refactor(store): share paging, decoding, writes and test fixtures`. Open
  the description with what was duplicated or hard to read, then how you
  cleaned it up, grouped by module. List each deliberate behavior change and
  one-line fix separately, and say what you compared to prove the rest is
  unchanged. End it with:

  ```md
  > [!NOTE]
  > Written by Claude Opus 5.5 in Claude Code (via T3 Code)

  🤖 Generated with [Claude Code](https://claude.com/claude-code)
  ```

- Don't mark the PR ready for review. The orchestrator does that after the
  review.

## Final report

Your last message, concisely:

1. The PR URL and branch.
2. What you cleaned up, in a few bullets.
3. The checks you ran, with the exact commands and results.
4. Bigger issues you found but didn't fix: bugs, design problems, risky code,
   and duplication that crosses areas. For each one: `file:line`, what's
   wrong, why it matters, and a suggested fix. Check each against the source
   before you report it. This list matters as much as the PR.

When the orchestrator sends you review findings, fix them on the same branch,
re-run the checks they affect, push, and report what changed.
