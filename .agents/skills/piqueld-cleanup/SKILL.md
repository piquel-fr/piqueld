---
name: piqueld-cleanup
description: >
  Run a repository-wide, behavior-preserving cleanup pass on piqueld: split the
  workspace into areas, have one agent per area remove duplication and
  inconsistency and open a draft refactor PR, have every PR reviewed, run a
  docs accuracy pass alongside, and report the bugs and design issues found.
  Use when asked for a "cleanup pass", "refactor pass", "clean up the
  codebase", to "deduplicate" or "simplify" piqueld as a whole, or to repeat
  the October 2026 pass (#241–#249). Not for cleaning up a single PR or file.
---

# Run a cleanup pass

A cleanup pass is a set of pure refactor PRs, one per area of the workspace,
written in parallel and each reviewed before the owner merges it. The pass
does not fix bugs. It writes them down, and they are fixed afterwards in their
own PRs. The first pass (#241–#246, #248 and #249, October 17–18, 2026) removed
about 3,700 lines net, most of them from tests, with no change to any API,
schema, manifest, CLI output or configuration format. Its docs PR (#247) ran
alongside it.

You are the orchestrator. You split the work, brief the agents, route reviews,
and keep the owner informed. You write little code yourself.

## 1. Plan the split

Delegated agents start from your current commit and read their briefs from
this skill's `references/`, so run the pass from a checkout of
`origin/main` (or a branch off it) that contains this skill.

Fetch `origin/main` and measure the areas (count lines with
`git ls-files <dir> | xargs cat | wc -l`, leaving out generated files). Start
from this split, which kept the first pass's PRs almost free of conflicts:

| Area | Paths | Title scope |
| --- | --- | --- |
| Core | `crates/piqueld-core/**` | `core` |
| Store | `apps/piqueld/src/store/**` | `store` |
| Daemon API | `apps/piqueld/src/{api,auth,config,application,bin}/**`, the top-level files in `apps/piqueld/src` except `certificate.rs`, `apps/piqueld/{build.rs,build,examples}` | `server` |
| Daemon runtime | `apps/piqueld/src/{docker,reconcile,ingress,dns,tailnet,git,secrets,telemetry}/**`, `apps/piqueld/src/certificate.rs` | `server` |
| Daemon tests | `apps/piqueld/tests/**` | `server` |
| CLI | `apps/piquelctl/**` | `cli` |
| Dashboard | `apps/piqueld-ui/**`, `tests/playwright/**` | `ui` |
| Tooling | `crates/piqueld-{client,probe,generated}/**` except generated code, `tools/**`, `scripts/**`, `nix/**`, `deploy/**`, `.github/**`, and the root build files (`justfile`, `Cargo.toml`, `flake.nix`, `deny.toml`) | dominant scope |

Nobody edits `migrations/**` (no schema changes) or generated files (they
change only through `just generate`). Markdown belongs to the docs lane.

Name each branch `refactor/cleanup-<area>-<YYYY-MM-DD>`, with the pass's
start date. The first pass's `refactor/cleanup-<area>` branches still exist.

Adjust the split to the code as it is now:

- Put any new top-level module in the area that already owns its neighbours.
- Split an area that has grown past about 30,000 lines along a module
  boundary. Merge areas that are small.
- Give every file exactly one owner. In the first pass, the runtime and tests
  agents both edited `apps/piqueld/tests/fake_docker.rs`, and the second one
  merged needed a manual rebase.

Run all areas at once unless the owner has asked for fewer threads. Cap the
builds of every agent that compiles (implementers, reviewers and the docs
lane) so they fit the machine together. There is no swap, and 8 unbounded
builds run out of memory. In the first pass, `CARGO_BUILD_JOBS=4` for 8
implementers and their reviewers on 16 cores and 58 GB worked, because they
rarely all build at the same moment. Scale it to `nproc` and `free -g`, and
pass the value to every task.

Tell the owner the split in one short message, then start. Don't wait for
approval: the split is cheap to change, and the owner can interrupt.

## 2. Start the agents

Call `orchestrator_capabilities` once to confirm the provider and model IDs.

For each area, start a `delegate_task` with `isolation: "worktree"`,
`role: "implementation"`, Claude Opus 5.5 as the target, and a task that:

1. Tells it to read `.agents/skills/piqueld-cleanup/references/agent-brief.md`
   in its checkout and follow it.
2. Names its area, its paths, its package or packages, its branch and its
   title scope.
3. Lists what the other agents own, so it knows what not to touch.
4. Lists its area's typical targets and its checks, from
   [Area notes](#area-notes) below.
5. Gives its `CARGO_BUILD_JOBS` value.

Then start the docs lane: one more `delegate_task`, isolated and with Claude
Opus 5.5 like the others, on branch `docs/cleanup-pass-<YYYY-MM-DD>`. Its task
tells it to:

- Follow the `piqueld-docs` skill, check every page against the code, and fix
  drift in `docs/`, `README.md`, `AGENTS.md`, `CONTRIBUTING.md`, `SECURITY.md`,
  `tests/README.md` and `examples/`.
- Report stale `///` doc comments and clap help instead of editing them.
  Those feed generated pages, the OpenAPI document and `--help`, which the
  refactor PRs must leave unchanged, and they live in files the area agents
  own. In the first pass the docs agent edited them, merged first, and left
  all eight refactor PRs to resolve doc-comment conflicts.
- Run `just docs`, open a draft PR with the `file-pr` skill, and end with the
  same final report as the area agents: the PR, the checks, and the bigger
  issues it found.

Review its PR like the others, with a focus list of the claims it changed.

Keep the stale doc comments it reports for a follow-up PR after the area PRs
merge (see [Follow up after the merge](#6-follow-up-after-the-merge)).

Keep a ledger file under `/tmp/piqueld-cleanup-<date>/`: each area's agent,
branch, PR, review state, and every finding the agents and reviewers report.
The first pass's thread was compacted four times and its server restarted
once. The ledger is what survives that.

## 3. Review each PR as it opens

When an agent reports its draft PR, link it with `link_pull_request` and
start a review right away. Don't wait for the other areas.

Each review is a new `delegate_task` with `role: "review"`,
`isolation: "worktree"`, and GPT 6.1 Sol as the target with
`target.options: { reasoningEffort: "high" }`. Its task tells it to read
`.agents/skills/piqueld-cleanup/references/review-brief.md`, and gives it the
PR number, the head commit SHA it reviews, its `CARGO_BUILD_JOBS` value, and a
focus list. Write the focus list from the PR description: list each
"unchanged" or "byte-identical" claim the author makes, and each place a
merged helper, macro or test could have changed behavior. Every regression the
first pass's reviewers found came from checking claims like these (see
[Area notes](#area-notes)).

When a review comes back:

- Check its findings against the code before you act on them.
- Send real findings back to the area's implementer, so it fixes them with
  the context it already has. Fix a one-line nit yourself.
- After a fix that rewrites code, start a new, narrower review of what
  changed. Don't reuse the old reviewer.
- A finding that is a judgment call, like the error-precedence change in
  #245, goes to the owner with both sides and your recommendation.

Mark a PR ready for review only once its review is clean and it is rebased on
`origin/main`. That starts the slow CI jobs (Docker, Playwright, macOS, Nix),
which must pass before the PR can merge. Follow them with the `babysit-pr`
skill, and send a real failure back to the implementer. Update the note at the
end of the description to name the reviewers that actually reviewed it, for
example `Written by Claude Opus 5.5 in Claude Code and reviewed by GPT 6.1 Sol
(via T3 Code)`.

If Sol is over its usage limit, tell the owner and run the same review with a
Claude Opus reviewer instead of waiting for the reset. The first pass lost no
time this way, while waiting would have cost hours. Sol stays the default.

## 4. Keep the PRs current

The owner keeps merging features during the pass. When something lands on
`main`:

- Have each open PR's agent rebase, and apply its area's cleanup to the new
  code, so the PR doesn't land with a fresh copy of the pattern it removed.
- If the rebase merged real logic, start a short review of only that part.
- An area PR that updated a doc reference can conflict with the docs PR.
  Resolve it against the code as it is after the rebase, and rerun
  `just docs`.

Suggest merging the core PR first: the daemon and the client depend on it, and
the CLI and dashboard through the client.

## 5. Report

When every PR's latest head is reviewed and its CI is green, send the owner
one report:

1. A table of the PRs: area, link, lines added and removed.
2. Decisions only the owner can make, with your recommendation for each.
3. Every deliberate behavior change, and the PR and commit it is in.
4. Merge notes: suggested order, and the PRs that share a file.
5. Bugs found but not fixed. For each: `file:line`, what goes wrong, impact.
6. Design issues and cross-area duplication, each with a suggested fix.
7. Features missing from a surface: in the manifest but not the CLI or the
   dashboard, or the other way around (see `AGENTS.md`).

Before you send it, have one Sol task check every bug and parity gap against
current `main`. In the first pass it dropped 2 of 38 as not bugs and flagged 2
as product decisions.

Number the items `NEW-1`, `NEW-2`, …, never `#1`: on GitHub `#N` links to
issue or PR N.

## 6. Follow up after the merge

The pass isn't done when the area PRs merge. Offer, in this order:

1. **Cross-area PRs.** Duplication that crossed areas was held back so the
   agents wouldn't collide. Once the area PRs merge, open one small PR per
   theme. The first pass produced #264 (std locks to `parking_lot`), #265
   (client helpers and the wasm32 lint) and #266 (limits and helpers shared
   across crates).
2. **A doc-comment PR** for the stale `///` comments and clap help the docs
   lane reported. It regenerates the generated pages, OpenAPI document and
   CLI reference on purpose, so its review checks the new text against the
   code.
3. **Issues for parity gaps,** once the owner agrees. The first pass filed
   #250–#263.
4. **Fix rounds** for the verified bugs, grouped by area so the PRs in a round
   don't share files. The owner merges a round before the next one starts. Run
   at most 3 at a time, with Opus implementing and Sol reviewing before each
   PR opens. Before each round, check that every item is still open. Features
   keep landing, and some items get fixed or changed along the way.

Start each of these only when the owner asks.

## Area notes

Typical targets, checks, and what the first pass's reviews caught. Give each
agent its row, and use the "caught" column in its reviewer's focus list.

**Core.** Targets: shared validation error constructors, error codes as
constants in `codes.rs`, validators as methods, one `string_enum!` macro for
string-backed enums, shared traits for keyed edits. Check that
`just openapi-check` regenerates nothing. Caught: a blanket text replace
(`resources.` to `self.`) changed three validation error paths and their
order. Reviewers compare error codes, paths and order over many manifests on
`main` and on the branch.

**Store.** Targets: one paging and cursor helper, shared row decoding, one
"single write behind the writer queue" helper, shared test fixtures, and
`foo.rs` with `foo_tests.rs` as `foo/mod.rs` with `foo/tests.rs`. Run the
store tests plus the `api_contract`, `persistence` and `sqlx_stack`
integration tests. Caught: merging near-identical queries behind an optional
filter stopped two of them using their index. Reviewers compare
`EXPLAIN QUERY PLAN` on `main` and on the branch. Also caught: a missing row
that was an error became a quiet `false`, and test helpers that stopped
asserting.

**Daemon API.** Targets: shared request extractors instead of decoding in
each handler, splitting long `mod.rs` files, cookie names and lifetimes
defined once. Run the daemon tests (`api_contract` covers the HTTP layer) and
`just openapi-check`. Caught: decoding everything in an extractor changed
which error a request with several faults gets (#245; the owner kept the new
order).

**Daemon runtime.** Targets: one helper for Docker "not found" responses,
ownership checks and journal entries; shared container spec bases; DNS
provider boilerplate. Run `just docker-test` (real Docker, in
Docker-in-Docker) if the Docker adapter changed; CI runs it too. Caught: renamed
log fields (`network_id` became `id`, which breaks log filters), and a test
that built its expected bytes with the production function. Reviewers
re-serialize the container specs on both sides and compare the bytes and
hashes, since a changed spec replaces running containers.

**Daemon tests.** Targets: setup moved into `tests/support/`, one harness per
binary, table-driven cases. Every behavior asserted before is still asserted,
and the PR accounts for every test it merged or removed: compare test names
and per-file counts with `main`, and explain each difference. Caught: a wait shortened
from 10 to 2 seconds (a new flake), and a helper that became a free function.

**CLI.** Targets: one place that owns confirmation prompts, commands as
methods on their argument structs, shared report and table rendering. Every
command's `--help`, human output, JSON output and exit code stays
byte-identical: compare `--help` for every command against a build of `main`.

**Dashboard.** Targets: one client error path, a shared draft and save type
for forms, shared view helpers, CSS duplicates, Playwright helpers. Run wasm32
Clippy (`just lint` covers it), the unit tests, and
`pnpm --dir tests/playwright check`. Caught: a lost test assertion, a
behavior change hidden in an 8,000-line commit (split it into logical commits,
with the fix in its own `fix(ui):` commit), and a module-wide lint exception
(put `#[expect(..., reason = "...")]` on each item instead).

**Tooling.** Targets: client wrapper boilerplate behind extension traits,
error helpers as constructors, xtask command helpers, NixOS module tables.
Never edit generated code. Check the client natively and for wasm32, and
compare the NixOS module's evaluated output before and after. The probe is
size-sensitive, so don't add dependencies. Caught: a merged test that lost a
check, and changed xtask error messages.

## When things go wrong

- **Flaky tests.** `fake_docker` timing tests can fail under the load of 8
  parallel builds, and one Playwright step
  (`tests/playwright/tests/branches.spec.ts`, the webhook secret) is flaky in
  CI. Rerun once, compare with `main`, and say so in the report. Don't let an
  agent change a test to make it pass.
- **Session or usage limits.** Background work stops when the Claude session
  hits its limit or the T3 server restarts. Afterwards, check each agent's
  state against the ledger and restart only what didn't finish.
- **Waiting on a bot.** CodeRabbit runs out of free reviews and may never
  review a PR. Don't hold a PR for it. Tell the owner it's ready and why.
