# Cleanup pass: audit

The audit measures the codebase, checks it for the problems listed below, and
gives the owner a ranked list of fixes. The owner picks which ones to act on.
The November 2, 2026 audit (thread "Codebase Size and Composition Analysis")
did this on `main` at `5eb4271b`, and its suggestions became #354, #355, #357,
#358, #359, #361–#365, plus this skill.

## Measure

Run the stats script on a fresh checkout of `origin/main`:

```sh
python3 -I .agents/skills/piqueld-cleanup/scripts/stats.py . --json <scratchpad>/stats.json
```

It needs `cloc` (in the Nix dev shell) and takes a few seconds. It prints
the share of generated files, lines by language, Rust source against test
code, the test share of each area, the hand-written files over 1,500 lines,
test counts, and close-but-not-exact counts of `unsafe`, `#[allow]`,
`#[expect]`, `.unwrap()`, `.expect(` and TODOs outside tests. The JSON has
every file's record and the growth history, one point per first-parent commit.

Show the numbers as charts, following the `dataviz` skill, with `html_preview`
and `html_render`. The first audit used two pages: an overview (lines by
kind, language split, size and test share by area, largest files, the fact
counts) and the growth over time by category.

If a previous audit or pass exists, compare with its numbers. The first one
found: 191K hand-written lines, 20% of the repo generated (23% with
lockfiles), 35% of Rust as test code, 2% in `piqueld-ui` against 34–43% for
the daemon, core and CLI, 16 files over 1,500 lines, 0 `unsafe`, 0 TODOs,
15 `.unwrap()`, 45 `.expect(` and 4 hand-written `#[allow]`. The October pass
was the only visible drop in the growth chart.

## Check

Each check below lists what the first audit found and what came of it. Check
against the current code, not these notes.

1. **Coverage.** Run `just coverage` (added in #359; it takes about
   15 minutes cold) and list the least covered crates and modules. Line counts
   only say how much test code exists, not what it exercises. The first
   measurement was 78.7% of lines overall, with `xtask` at 18%, the client at
   64% and `piquelctl` at 66%. The wasm-only half of `piqueld-ui` can't be
   instrumented, so the dashboard's number covers only its native modules.
2. **Untested areas.** Any area whose test share is far below the others, and
   the largest files in it with no tests. In the first audit it was
   `piqueld-ui`, which then got tests one area per PR (#358, #361–#364):
   logic moved out of components into plain functions and unit-tested there,
   plus Playwright tests for flows the specs didn't cover.
3. **Generated files.** Every file `just generate` writes (each path passed to
   `Artifacts::sync` in `crates/piqueld-generated`) and every lockfile is
   marked `linguist-generated` in `.gitattributes`, so GitHub collapses it in
   diffs (#354). The script reads the list from there, so a generated file
   missing from it shows up as hand-written.
4. **Large files.** Hand-written files over about 1,500 lines that mix
   unrelated jobs. Split them into modules by area, moving code only: no
   behavior change, and the same tests in each test binary (#365, which split
   the 5 largest). Lessons from that split:
   - A test binary split into modules becomes `tests/<name>/main.rs`, which
     keeps its target name, so CI and `--test <name>` work as before.
   - Clippy rejects `use super::*` outside tests. Use explicit imports.
   - Don't name a module after a crate it uses: an `http` module shadowed the
     `http` crate.
   - Items sibling modules now share become `pub(super)`, no wider.
   - Count each test binary's tests before and after.
5. **Module layout.** A module with child modules is `foo/mod.rs`, never
   `foo.rs` next to `foo/`. Clippy's `self_named_module_files` lint enforces
   that half (enabled in #365). A module without children stays a plain
   `foo.rs`. No lint covers that half, so the script lists any `mod.rs` with
   no children. `tests/support/mod.rs` is the exception: a `tests/support.rs`
   would be built as its own test binary. So is `apps/piqueld/build.rs`, which
   is Cargo's build script.
6. **Panics outside tests.** Every `.unwrap()`, `.expect(`, and close relative
   (`unwrap_unchecked`, indexing that untrusted input can push out of bounds)
   in hand-written production code, generated code excluded. For each site:
   can it panic in practice, and should it be kept with a message that says
   why it holds, propagate its error with context, encode the invariant in a
   type, or be restructured so the case can't occur? #355 audited 46 sites
   and 21 related ones: most were kept with clearer messages, two now return
   errors (`Client::unix`, `ApplicationService::new`), two moved into `const`
   blocks, and one timestamp overflow on untrusted input was fixed.
7. **Lint suppressions.** Every hand-written `#[allow]` either becomes an
   `#[expect(..., reason = "...")]` on the smallest item that needs it, or
   goes. An `allow` goes stale silently; an `expect` fails once its lint stops
   firing. Check with each target and cfg it builds for, wasm32 included.
   #357 converted all of them, including the generated client's header, which
   now expects only the lints that fire. Any new `#[allow]` is a finding.
8. **Doc comments.** Plain `//` comments outside tests should be few and
   explain why; the first audit found 84% of comments were doc comments,
   which fits the references being generated from them.
9. **Duplication and growth.** Lines added per day since the last pass. At
   about 3,000 a day, the first audit suggested a cleanup pass about once a
   month. If duplication has built up since the last one, suggest the area
   pass (SKILL.md, from step 1).

`unsafe` blocks and TODOs were at zero. Report any that appear.

## Suggest

Send the owner one message: the charts, a few lines on what the numbers show
that the charts don't, then the suggestions ranked by value. For each: what
you found, with the counts and paths behind it, and the change you propose.
Say which ones you can do on a branch now and which need a closer read first.
List what is already in good shape, so it isn't changed. Don't open a PR yet.

## Act on what the owner picks

The owner picks suggestions and says how to run each, usually as its own
thread. Start each one with `t3_thread_launch`, on Claude Opus 5.5, in a new
worktree from `origin/main` on a branch named for the change. Never start a
thread the owner didn't ask for. Its message has to stand on its own:

- What to do and why, with the numbers and paths from the audit.
- The scope: what is in, what is left out, and what not to change. For an
  audit, the exact sites and the verdicts to choose from.
- What to report: an audit ends with a table of every site, its verdict and a
  one-line reason, then the PR link.
- `AGENTS.md` for commits and PRs, the `file-pr` skill, draft PRs, and
  `piqueld-docs` when docs change.

The first audit's threads, as examples of scope:

| Suggestion | Branch | Result |
| --- | --- | --- |
| A `just coverage` recipe, not a CI gate; headline numbers in the PR | `chore/coverage-recipe` | #359 |
| Test `piqueld-ui`, as many PRs as needed, nothing unnecessary | `test/ui-coverage` | #358, #361–#364 |
| Mark generated files and lockfiles in `.gitattributes` | `chore/gitattributes-generated` | #354 |
| Review every `unwrap`/`expect` outside tests and report a table | `refactor/unwrap-expect-audit` | #355 |
| Re-check every `#[allow]` reason | `chore/lint-allow-review` | #357 |
| Split the largest files | done in the audit thread | #365 |

The file split ran inside the audit thread and a session restart stopped it
partway; the threads finished on their own. Give long work its own thread.

Changes that move or rewrite code in many areas (file splits, panic fixes)
collide with an area pass. Run them before the area pass starts and merge
them first, or give each area agent its own share in its task.
