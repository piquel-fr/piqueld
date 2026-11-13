# Cleanup pass: review brief

You are reviewing one pull request from a cleanup pass over piqueld. It is
meant to be a pure refactor: less duplication, clearer code, free functions
turned into methods, better use of types, dead code removed. It must not
change behavior, HTTP responses, OpenAPI output, the database schema, the
manifest format, CLI flags or output, the configuration format, or test
coverage. Every behavior asserted before must still be asserted. A one-line
bug fix or a deliberate behavior change is allowed only in its own commit,
named in the PR description.

The owner wants simplicity, no duplication, methods over loose functions,
concise doc comments, no unjustified lint exceptions, errors that keep their
context, and focused tests.

## Your job

Do a deep, read-only review. Don't push, comment on GitHub, or edit the PR.
Read it with `gh pr view <N>` and `gh pr diff <N>`. To build or test, run
`git fetch origin main pull/<N>/head` and check out the head SHA your task
names (if the PR has moved past it, say so and review the SHA you were given).
Set `CARGO_BUILD_JOBS` to the value in your task.

Your task message lists claims to check. Check each one yourself by comparing
`origin/main` with the branch. Don't trust the description. Then look, in this
order of priority, for:

1. **Behavior changes hidden in the refactor.** Changed error codes, messages,
   paths or variants. Changed order, including which error a request with
   several faults gets. Changed defaults, serialization, Docker names and
   labels, or log field names. Lost error context. Dropped edge cases.
   Changed locking, transaction or concurrency behavior. Changed timeouts and
   waits. Queries that stopped using their index (compare
   `EXPLAIN QUERY PLAN`).
2. **Lost test coverage.** An assertion dropped when tests merged, a table
   that lost a case, a check that now passes "eventually" instead of
   immediately, or expected values now built by the production code they
   test. Compare test names and counts per file with `main`.
3. **Refactors that made code harder to read,** or added abstraction that
   doesn't pay for itself.
4. **Obvious cleanup missed** in the files the PR touches. Keep this short.
5. **Stale doc comments.**

Check every finding against the source. Report each one with its severity
(blocker, should-fix or nit), `file:line`, what's wrong, and a concrete fix.
Say which claims you verified and how. If the PR is clean, say so. No
preamble.
