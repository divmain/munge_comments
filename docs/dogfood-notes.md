# Dogfood notes: munge-comments on its own source

Ticket `task-1a91` (plan `plan-2a36`, Phase 8). The dogfood runs
`munge-comments` over its own `src/` and `tests/*.rs`, with a live
endpoint and a human reviewer.

## Status: live dogfood deferred

**The live dogfood has not been run.** It needs two things that this
environment did not have when the ticket was worked (2026-10-10):

| Prerequisite | State |
|---|---|
| Live OpenAI-compatible endpoint (`OPENAI_API_KEY` and/or `OPENAI_BASE_URL`) | **Unavailable.** Both variables were unset and no `--api-key`/`--base-url` was supplied. A live `plan` invocation exits 1 with `usage error: no API key …`. |
| Human reviewer for the pass-1 artifact | **Unavailable.** |

As the ticket requires, the live steps were skipped without blocking the
phase. No implementer agent reviewed or approved any decision in place of
a human. **None of the live acceptance criteria below has been checked**:
zero verifier failures on self-apply, a green suite afterwards, and pass-2
convergence ≥ 95%. Nothing in this file is a live result. The section
[Offline rehearsal](#offline-rehearsal-not-a-substitute-for-the-live-run)
records what *could* be checked without an endpoint, and it is labeled as
such.

`docs/dogfood-plan.json`, `docs/dogfood-plan-report.md` and
`docs/dogfood-plan-pass2.json` therefore do not exist yet. A human
creates them with the procedure below.

## Procedure for the live run (human-in-the-loop)

Run everything from the repository root on a clean working tree (commit
or stash first, so `git diff` shows exactly what the tool changed). Build
the binary once:

```console
$ cargo install --locked --path .        # or: cargo build --release && alias munge-comments=target/release/munge-comments
$ export OPENAI_API_KEY=sk-...            # and/or OPENAI_BASE_URL=... for a local server
$ export MUNGE_COMMENTS_MODEL=gpt-4.1     # optional; this is the default
```

Optional smoke test of the endpoint first. These are ignored by default
and gated on the variable:

```console
$ MUNGE_COMMENTS_E2E=1 cargo test --test real_endpoint -- --ignored
```

### 1. Pass-1 plan

Quote `tests/*.rs` so the tool expands the glob, not the shell, and
**always** exclude the fixtures:

```console
$ munge-comments plan src/ 'tests/*.rs' --exclude 'tests/fixtures/**' \
    --out docs/dogfood-plan.json --report docs/dogfood-plan-report.md
```

- Expect 105 files (78 under `src/`, 27 `tests/*.rs`), all Rust. Any
  nonzero exit is a finding: 5 means an endpoint/LLM failure (retry with
  `--resume`, same flags), 130 means interrupted (continue with `--resume`).
- `tests/*.rs` does not reach `tests/common/` (the shared test helpers).
  Add `tests/common/` as a path if those should be dogfooded too.
- Keep the stderr summary line (`plan: … convergence N%`); it is the pass-1
  baseline.

### 2. Human review: every decision

Read `docs/dogfood-plan-report.md` end to end, especially the closing
sections: *needs human review*, *flagged*, *mode fallbacks*, *warnings*.
Override decisions by editing `docs/dogfood-plan.json` as the README's
editing guide describes: flip `decision`, edit `finalComment`, and set
`"decidedBy": "human"`. Things to check:

- No `REMOVE`/`KEEP_GENERATED`/`HYBRID` loses a *why* that the code does
  not show: invariants, ordering constraints, spec references, bug links.
- Every `NEEDS_REVIEW` is resolved deliberately (left as is it stays
  untouched, which is safe).
- The 8 files expected to fall back to remove mode (listed below) carry a
  `placeholder-collision` warning and show `effectiveMode: "remove"`.
- No `NEW` comment is applied (`allowAdd` is off by default).

### 3. Preview and apply

```console
$ munge-comments apply docs/dogfood-plan.json --dry-run > /tmp/dogfood.diff; echo "exit=$?"
$ git apply --check /tmp/dogfood.diff
$ munge-comments apply docs/dogfood-plan.json --report docs/dogfood-apply-report.md; echo "exit=$?"
```

**Acceptance: exit 0 and zero verifier failures.** Exit 4 means some file
failed verification and was skipped. List those files from the apply
report; each is a finding. Then:

```console
$ cargo fmt                       # see "rustfmt after removal" below; review its diff too
$ cargo build --all-targets
$ cargo clippy --all-targets -- -D warnings
$ cargo test                      # acceptance: suite green
$ git diff --stat
```

Commit the applied tree, including `docs/dogfood-plan*.{json,md}`.

### 4. Pass-2 plan (convergence)

```console
$ munge-comments plan src/ 'tests/*.rs' --exclude 'tests/fixtures/**' \
    --out docs/dogfood-plan-pass2.json --report docs/dogfood-plan-pass2-report.md
```

**Acceptance: the summary line's `convergence` ≥ 95%.** This is
`convergence_pct`: `(KEEP_ORIGINAL + KEEP_PROTECTED + NEEDS_REVIEW) /
scanned records`. Record the figure here. Also record the **prose-only**
convergence (see the caveat below), for example:

```console
$ python3 -c "import json;a=json.load(open('docs/dogfood-plan-pass2.json'));p=[c for f in a['files'] for c in f['comments'] if c['classification']=='prose' and c['original'] is not None];k=sum(c['decision'] in('KEEP_ORIGINAL','NEEDS_REVIEW') for c in p);print(f'prose convergence {k}/{len(p)} = {100*k/max(len(p),1):.1f}%')"
```

### 5. Write up

Replace the "Status" section above with the live results: pass-1 summary
line, override count, verifier/apply exit code, suite result, pass-2
`convergence` and prose-only convergence. Add the insights. Every prompt or
heuristic change that the run suggests becomes a follow-up ticket, not an
edit made during the dogfood.

## Offline rehearsal (not a substitute for the live run)

To check the mechanics that need no endpoint, an offline dry rehearsal ran
on a **temporary copy** of the working tree (HEAD `cc42ba4` plus the
uncommitted Phase-8 changes present at the time; rustc 1.98.0, release
binary). The real working tree was never modified, and every artifact,
diff and report went to a temp directory. `--offline` makes no LLM calls:
heuristic rows 1–3 decide what they can, and every other prose comment is
kept as `KEEP_ORIGINAL` (tag `offline`). **It says nothing about the
quality of the model's decisions, about the redact transform, or about
live convergence.**

### Pass 1, offline

```console
$ munge-comments plan --offline src/ 'tests/*.rs' --exclude 'tests/fixtures/**' --out $TMP/pass1.json --report $TMP/pass1.md
plan: 105 file(s): 105 complete, 0 failed, 0 skipped
plan: 6209 comment(s) (866 prose): 0 removed, 0 rewritten, 866 kept, 0 needs review, 5343 protected, 0 new, 0 new held, 0 warning(s); convergence 100.0%
```

Exit 0. Observations:

- **Discovery.** 105 Rust files: 78 in `src/`, 27 in `tests/*.rs`. Nothing
  from `tests/fixtures/**`, `tests/snapshots/` or `tests/common/` was
  picked up.
- **Doc comments are protected. Most are `doc-protected`, but not all.**
  5,253 doc comments (`///`, `//!`) are `doc-protected`. Another 88 `//!`
  lines are classified `functional` with tag `license` instead. They are the
  leading module-doc runs of `src/comments/classify.rs`,
  `src/engine/verify_rail.rs`, `src/lang/adapters/global.rs` and
  `tests/verifier_props.rs`, and each mentions "license" or `SPDX` in its
  first comment run. This is the documented license-header-run rule
  (`verify_rail.rs`'s own module doc describes it). Both classes decide
  `KEEP_PROTECTED`, so the dogfood behaves the same either way. The
  ticket's "all `doc-protected`" expectation holds for *protection*, not
  for the literal class. No follow-up is needed unless a reviewer wants
  `//!` module docs exempt from license detection.
- **The dogfood exercises `//` prose only.** All 866 prose records are
  `line` comments: 535 in `src/`, 331 in `tests/`. The other 2 functional
  records are a `// SAFETY:` directive and its adjacent comment in
  `src/lang/adapters/rust.rs`.
- **Flagged.** 9 prose comments are tagged `flagged-for-review`, all `todo`
  and one also `fixme` (in `src/engine/heuristics.rs`,
  `src/comments/inventory_tests.rs`, `src/engine/align_tests.rs`,
  `tests/regenerate.rs`, `tests/report.rs`). They are worth a look in the
  live review, since they probably quote the TODO patterns under test.
- **`placeholder-collision` cannot be observed offline.** The offline branch
  runs no transform, so the collision probe never fires: all 105 files
  report `effectiveMode: "redact"` with no warnings. The same probe
  (`has_placeholder_collision`, matching `⟦C<digits>⟧`) applied to the
  file texts predicts that **8 files fall back to remove mode** in the live
  run, recorded as `placeholder-collision`:
  `src/comments/redact.rs`, `src/comments/redact_tests.rs`,
  `src/comments/sanitize.rs`, `src/llm/chunk_tests.rs`,
  `src/llm/prompts.rs`, `tests/judge.rs`, `tests/noisy_repo.rs`,
  `tests/regenerate.rs`. Together they hold 124 of the 866 prose comments.
  `src/engine/regenerate.rs` and `tests/golden.rs` mention `⟦C` only in the
  generic form `⟦C<digits>⟧`, so they stay in redact mode. Confirm this
  list in the live report's *mode fallbacks* section.
- `apply --dry-run` on the offline artifact: exit 0, `apply: 105
  unchanged`, an empty diff. Every decision is a keep, so this exercises
  only the drift and hash checks, not the verifier.
- Re-running `plan` with an existing `--out` and no `--resume` exits 1, as
  documented, so no paid artifact is overwritten.

### Synthetic verifier stress test, offline

To exercise the verifier and the apply path on this codebase without a
model, a **synthetic** copy of the offline artifact had *every* prose
record flipped to `REMOVE` (866 records), entirely in the temp directory.
This is a mechanical worst case for prose removal, not a review and not
a recommendation. A human would never accept these decisions wholesale.

| Step (temp copy) | Result |
|---|---|
| `apply --dry-run` | exit 0; `apply: 81 dry-run, 24 unchanged`; 6,852-line diff |
| `git apply --check` of that diff | passes |
| `apply` | exit 0; `apply: 81 applied, 24 unchanged`; 81 files, +25 / −1,349 lines (the `+` lines are code lines whose trailing comment was removed) |
| `cargo build --all-targets` | succeeds, no warnings |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo test` | **900 passed, 0 failed, 2 ignored** (the env-gated `real_endpoint` tests) |
| `cargo fmt --check` | **1 diff**, see below |
| offline pass 2 on the applied tree | exit 0; 5,343 comments (0 prose), all protected; convergence 100.0% |

- **The verifier passed on every file.** No file was rejected, and all 81
  changed files compiled and the full suite stayed green.
- **Protected comments survived byte for byte.** The multiset of
  `(path, kind, classification, original)` over the 5,343 `KEEP_PROTECTED`
  records is identical before and after the apply.
- **rustfmt after removal.** In `src/comments/sanitize.rs` (~line 160), a
  match arm `Shape::Block(_) if first => { // comment… segment… }` was a
  block only because it held the comment. Once the comment is gone,
  rustfmt wants to collapse it to an expression arm. Semantics are
  unchanged and the verifier is right to accept it. The point is that **the
  live procedure should run `cargo fmt` after `apply`**, as above. This
  may deserve a line in the README's known limitations ("removing comments
  can leave code that your formatter reflows").

### Caveat for the live convergence criterion

`convergence_pct` counts `KEEP_PROTECTED` records in both numerator and
denominator. On this codebase protected records dominate: 5,343 of 6,209
scanned records (86%) in pass 1. Pass 2 can therefore reach ≥ 95% even if
a sizable share of the remaining prose comments churn again. For example,
with all 866 prose comments still present in pass 2, up to about 310 of
them could change and the metric would still read ≥ 95%. The ≥ 95%
criterion stays the acceptance gate. The live write-up should also report
prose-only convergence (command in step 4), which is the meaningful
idempotency signal for the model.

## Insights and follow-ups

From the offline rehearsal only. Revisit after the live run.

1. No tool defect was found. Discovery, protection, apply, the verifier
   and drift checks all behaved as documented on this repository.
2. Candidate doc tweak (not filed as a ticket; needs a maintainer's call):
   mention in the README that comment removal can leave code that rustfmt
   or another formatter reflows, so run the formatter after `apply`.
3. Candidate heuristic question (not filed; behavior is documented and
   harmless here): a leading `//!` module doc that merely *mentions*
   "license" or SPDX is classified as a license header run (`functional`)
   rather than `doc-protected`.
4. Prompt or heuristic changes suggested by the live run become follow-up
   tickets.
