# Restack commit ledger — duplicate-subject mapping

During the semantic restack of the product stack (`plan/06-docker-reconciliation`
→ `plan/06a-simplification` → `plan/06b-cli`), several fixes were re-landed onto
their corrected base. The result is that some commit subjects appear twice when
diffing across the pre-restack backup refs and the current stack. Nothing is
wrong with the code; this note maps the duplicated subjects so history tracing
across generations is not confusing.

| Subject | Pre-restack copy | Current-stack copy |
| --- | --- | --- |
| `fix(docker): add timeout to resolve image` | `e4827c8` | `49de23b` |
| `fix(reconcile): recover drift and propagate shutdown` | `2d08f48` | `b1b6769` |
| `fix(reconcile): harden operation execution` | `bb547ae` | `f0b32c2` |
| `chore: format rebased sources` | `b755627` | `b37b8bf` |

The authoritative copies are on the current published branches
(`origin/plan/06*`). The pre-restack copies survive only on the local backup
refs created before each branch rewrite; per the stabilization plan, neither
generation of backups may be deleted.

Provisional ledger maintained during review hardening (2026-08); extend it as
further restacks touch these branches.
