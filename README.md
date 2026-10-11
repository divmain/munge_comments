# munge-comments

`munge-comments` prunes superfluous code comments and regenerates the
valuable ones with an OpenAI-compatible LLM, across many programming
languages. It is a single self-contained command-line tool, and also a Rust
library (`munge_comments`).

- [Why blind regeneration](#why-blind-regeneration)
- [Install](#install)
- [Walkthrough: install → plan → review → apply](#walkthrough-install--plan--review--apply)
- [Supported languages](#supported-languages)
- [Commands and options](#commands-and-options)
- [Configuration and environment](#configuration-and-environment)
- [Hiding modes](#hiding-modes)
- [How decisions are made](#how-decisions-are-made)
- [The plan artifact and how to edit it](#the-plan-artifact-and-how-to-edit-it)
- [Interruption, resume and signals](#interruption-resume-and-signals)
- [Safety model](#safety-model)
- [Exit codes](#exit-codes)
- [File discovery](#file-discovery)
- [Library usage](#library-usage)
- [Known limitations and gotchas](#known-limitations-and-gotchas)
- [Development](#development)
- [Out of scope](#out-of-scope)

## Why blind regeneration

Most comment noise restates what the code already says. The question "is
this comment worth keeping?" is hard to answer by reading the comment next
to its code, because the comment primes the reader. `munge-comments`
answers it with an experiment instead:

1. **Hide** every prose comment from the model (replace it with a
   placeholder, or delete it outright).
2. Have the model **blindly regenerate** comments from the code alone.
3. Record how hard that was: an **EASY / MEDIUM / DIFFICULT** rating with a
   rationale, plus any context the model says it was missing.
4. **Judge** each original against its regeneration and decide: remove it,
   keep the original, keep the regeneration, or write a hybrid.
5. Before anything is written, a **semantic verifier** machine-checks that
   the edit changed only comment text.

If the model can reconstruct a comment's content from the code alone, the
comment carried no information the code didn't already carry: it is a
candidate for removal. If it can't (history, external contracts, subtle
invariants), the original encodes knowledge that would otherwise be lost,
and the tool keeps it. The goals are to cut ~90% of superfluous comments on
the first pass, keep the remaining comments high-value, and converge: a
second pass over already-cleaned code should change almost nothing.

Functional comments (compiler, linter and formatter directives, encoding
cookies, license headers, …) are classified before any LLM call and are
never touched.

The work is split into a durable two-phase workflow, so expensive LLM
determinations are never lost:

- **`plan`** discovers files, scans and classifies comments, runs the LLM
  regeneration and judging, and writes a complete JSON artifact (all
  before/after comments, byte ranges, anchors, decisions and content
  hashes).
- **`apply`** consumes the artifact: it checks that the codebase has not
  drifted, performs offset-based text surgery, re-verifies semantics, and
  writes the files.
- **`report`** renders any artifact as a markdown report, without the
  sources.

The artifact is plain, human-editable JSON, so you can review it and
override decisions between the two phases.

Parsing is 100% [tree-sitter](https://tree-sitter.github.io/): the grammars
are compiled and linked into the binary. There is no runtime grammar
loading and nothing to install besides the executable.

## Install

```console
$ cargo install --locked --path .              # from a checkout
$ cargo install --locked --git <repo-url>      # straight from the repository
```

Requirements:

- **Rust ≥ 1.90** (the MSRV; `rust-version` in `Cargo.toml`).
- **A C compiler.** The tree-sitter grammar crates and the TLS crypto
  backend compile C sources at build time.

Use `--locked`. Without it, `cargo install` ignores the committed
`Cargo.lock` and re-resolves every dependency that isn't exact-pinned
(tokio, reqwest, the transitive `tree-sitter-language`, …) to whatever is
newest, i.e. past what was tested and possibly past the MSRV. `--locked`
builds exactly the tested lockfile.

The result is one `munge-comments` executable with all grammars linked in.
No configuration files are read; see
[Configuration and environment](#configuration-and-environment).

## Walkthrough: install → plan → review → apply

Install the tool, then point it at an endpoint (or use `--offline` to try
it without one: offline runs make no LLM calls and decide by heuristics
only):

```console
$ cargo install --locked --path .
$ export OPENAI_API_KEY=sk-...
```

**1. Plan.** Run `plan` over the paths to clean up. It does all of the LLM
work exactly once, writes the artifact (`munge-comments-plan.json` by
default) and a markdown report (`munge-comments-plan-report.md`), and
prints progress and a summary to stderr:

```console
$ munge-comments plan src/ tests/
$ munge-comments plan --offline src/                       # no endpoint: heuristics only
$ munge-comments plan --mode remove --allow-add src/       # hide by deletion; also apply NEW comments
$ munge-comments plan --exclude 'tests/fixtures/**' --out review/plan.json --report review/plan.md .
```

The summary line looks like this (from an offline run over a small tree):

```text
plan: 41 file(s): 41 complete, 0 failed, 0 skipped
plan: 176 comment(s) (102 prose): 16 removed, 0 rewritten, 86 kept, 0 needs review, 74 protected, 0 new, 0 new held, 1 warning(s); convergence 90.9%
```

`plan` refuses to overwrite an existing `--out` file (exit 1), so paid LLM
work is never destroyed by accident: pass `--resume` to continue it, or
choose another `--out`.

**2. Review.** Read the report. It opens with a summary (counts per
decision, reduction, the difficulty histogram), then lists every comment
per file under a heading with its id, line, difficulty, decision, who
decided it (`heuristic`, `judge`, `protected`, `engine` or `human`) and
the confidence. Each entry shows the comment's tags, its before → after
text, and, when present, the regeneration, a proposed rewrite, the
decision and difficulty rationales and the model's missing context.
Closing sections collect what needs attention: needs human review,
flagged (`TODO`/`FIXME`/…), protected policy items (overrides), new comment
proposals, files not processed, mode fallbacks and warnings. Comments
marked `NEEDS_REVIEW` are the ones the tool would not decide on its own;
they stay untouched unless you flip them.

To override a decision, edit the artifact (see
[the editing guide](#the-plan-artifact-and-how-to-edit-it)). For example,
to accept a rewrite the engine held back, change that record's
`"decision": "NEEDS_REVIEW"` to `"decision": "KEEP_GENERATED"` (its
`finalComment` already holds the proposed text) and, optionally, set
`"decidedBy": "human"`.

**3. Preview.** `apply --dry-run` runs every check a real apply runs,
including verification, and prints unified diffs to stdout. It writes
nothing, not even a report. The diff is valid input for `git apply --check`.

```console
$ munge-comments apply munge-comments-plan.json --dry-run
$ munge-comments apply munge-comments-plan.json --dry-run --only src/lib.rs
```

```diff
--- a/python/empty-comments.py
+++ b/python/empty-comments.py
@@ -1,11 +1,8 @@
 import os
 
-#
 ROOT = os.getcwd()
-# ------
-NAME = "demo"  #
+NAME = "demo"
 
 
 def join(part):
-    #
     return os.path.join(ROOT, part)
```

**4. Apply.** Write the files. Each one is verified and written atomically;
a post-apply report goes to `munge-comments-apply-report.md`.

```console
$ munge-comments apply munge-comments-plan.json
$ munge-comments apply review/plan.json --report review/apply.md
```

**5. Report.** Render any artifact as markdown at any time; the sources
need not exist:

```console
$ munge-comments report munge-comments-plan.json
$ munge-comments report review/plan.json --out review/report.md
```

Then check the result with your usual tooling (`git diff`, the test suite,
the compiler) and commit. To measure convergence, run `plan` again over the
applied tree with a different `--out`: almost every record should come back
as a no-change decision.

## Supported languages

Files are mapped to languages **by extension**. Each language is a
declarative *adapter* (grammar, comment node kinds, directive table,
doc-comment policy, anchor configuration) behind one uniform
scan → classify → edit → verify engine. The **Id** column is the adapter id
recorded per file in the artifact (`"language"`).

| Id | Language | Extensions | Doc comments |
|---|---|---|---|
| `typescript` | TypeScript | `.ts`, `.mts`, `.cts` | All JSDoc is prose, except load-bearing `@internal`, `@deprecated` and release tags (functional) |
| `tsx` | TSX | `.tsx` | As TypeScript |
| `javascript` | JavaScript (incl. JSX) | `.js`, `.jsx`, `.mjs`, `.cjs` | JSDoc on exported symbols is `doc-protected`; load-bearing JSDoc (type tags, access modifiers, `@deprecated`, …) is functional; other JSDoc is prose |
| `python` | Python | `.py` | Docstrings are code (strings), never comments, and are never touched |
| `go` | Go | `.go` | Ordinary prose (`Deprecated:` paragraphs are functional) |
| `java` | Java | `.java` | All Javadoc (`/** */` and Java 23+ `///`) is `doc-protected`; Javadoc carrying `@deprecated` is functional |
| `c` | C | `.c` | Ordinary prose |
| `cpp` | C++ | `.cc`, `.cpp`, `.cxx`, `.hpp`, `.hh`, `.h` | Ordinary prose |
| `rust` | Rust | `.rs` | All doc comments (`///`, `//!`, `/** */`, `/*! */`) are `doc-protected` |

All of these are **tier 1**: fully fixture-covered, with directive tables
and end-to-end coverage, and every acceptance criterion is measured over
them. Notes:

- **`.h` is parsed as C++.** Under the C grammar, a C++ raw string such as
  `R"(a" /* x */ ")"` in a header parses *cleanly* with a phantom comment
  inside the string literal, so removing it would silently change the
  string. Under C++ it is a raw string. A C header that uses C++ keywords as
  identifiers (`new`, `class`, `this`) fails to parse as C++ and is skipped
  as `syntax-errors` (fail-safe).
- `.d.ts`/`.d.mts`/`.d.cts`, `.pyi` and `.min.js` files are excluded by
  default (ambient or generated code).
- A file whose extension maps to no adapter is never discovered; naming one
  explicitly is a usage error (exit 1).

**Tier 2 and beyond.** Ruby, Bash, CSS and HTML are the planned next
tranche. They are not supported yet; each has known quirks (Ruby
`=begin`/`=end` blocks and magic comments, Bash heredocs, CSS block-only
comments, HTML conditional comments). A language is added by adding one
exact-pinned grammar crate, its adapter data and fixtures; the engine does
not change for any language the adapter vocabulary can express. See
[CONTRIBUTING.md](CONTRIBUTING.md#adding-a-language) for the checklist.

## Commands and options

```console
$ munge-comments --help
$ munge-comments plan --help
$ munge-comments --version
```

`--help`/`--version` print to stdout and exit 0. Help output is
deterministic (fixed program name, 100-column wrapping, never colored).
Progress and status lines go to stderr; stdout carries only `--help` and
`--version` text and the diffs of `apply --dry-run`, so a dry run is
pipeable. Reports always go to files.

### `plan`

The output of `munge-comments plan --help` (kept in sync with the binary by
`tests/docs.rs`):

<!-- plan-help:start -->
```text
Plan comment changes: discover files, hide prose comments, have the model regenerate them, and judge each one. Writes a JSON artifact

Usage: munge-comments plan [OPTIONS] [PATHS]...

Arguments:
  [PATHS]...
          Files or directories to process (default: the current directory)

Options:
      --mode <MODE>
          Hiding strategy for prose comments (default: redact)

          Possible values:
          - redact: Replace each prose comment with a stable-id placeholder
          - remove: Delete target prose comments outright

          [default: redact]

      --model <NAME>
          LLM model (env: MUNGE_COMMENTS_MODEL; default: gpt-4.1)

      --base-url <URL>
          OpenAI-compatible endpoint (env: OPENAI_BASE_URL; default: https://api.openai.com/v1)

      --api-key <KEY>
          API key for the endpoint (env: OPENAI_API_KEY)

      --include <GLOB>
          Include glob, repeatable (default: **/*.{ts,tsx,mts,cts,js,jsx,mjs,cjs,py,go,java,c,h,cc,cpp,cxx,hpp,hh,rs})

      --exclude <GLOB>
          Extra exclude glob, repeatable (defaults always applied: **/*.{d.ts,d.mts,d.cts}, **/*.pyi, **/*.min.js, **/node_modules/**, **/vendor/**, **/{dist,build,out}/**)

      --concurrency <N>
          Parallel LLM files (default: 4)

          [default: 4]

      --max-file-chars <N>
          Chunking threshold in characters (default: 120000)

          [default: 120000]

      --similarity-threshold <0..1>
          Heuristic redundancy cutoff (default: 0.6)

          [default: 0.6]

      --allow-add
          Also apply NEW comments (remove-mode files only, including per-file remove-mode fallbacks)

      --offline
          No LLM calls; heuristic-only decisions (demo/no-endpoint path)

      --resume
          Skip files already complete or skipped (unchanged hash) in the --out artifact; retry failed ones

      --out <FILE>
          Artifact path (default: munge-comments-plan.json)

          [default: munge-comments-plan.json]

      --report <PATH>
          Post-plan markdown report (default: munge-comments-plan-report.md)

          [default: munge-comments-plan-report.md]

  -h, --help
          Print help (see a summary with '-h')
```
<!-- plan-help:end -->

Details:

- **Paths.** With no `[PATHS]`, `plan` processes the current directory.
  Each path may be a file, a directory, or a quoted glob pattern (see
  [File discovery](#file-discovery)).
- **Repeatable flags** (`--include`, `--exclude`, and `apply --only`) take
  exactly one value per occurrence, so a following path is never swallowed:
  `plan --exclude x src/` keeps `src/` as a path. Repeat the flag for more
  values.
- **`--include`** replaces the built-in extension set; **`--exclude`** adds
  to the built-in excludes, which always apply. Globs are matched against
  paths relative to the directory `plan` was run from.
- **`--concurrency`** and **`--max-file-chars`** must be ≥ 1;
  **`--similarity-threshold`** must be within `0..=1`. Out-of-range values
  are usage errors (exit 1). `--max-file-chars` counts Unicode scalar
  values; larger files are split into chunks along top-level declaration
  boundaries, and judge batches are sized from the same budget.
- **`--allow-add`** is recorded in the artifact (`config.allowAdd`) and
  honored by `apply`; see [Hiding modes](#hiding-modes).
- **`--out`/`--report`** parent directories are created if missing. The
  artifact may live anywhere, including outside the source tree.

### `apply`

```text
munge-comments apply [OPTIONS] [ARTIFACT]
```

| Option | Meaning |
|---|---|
| `[ARTIFACT]` | The plan artifact. Missing, or naming no file: exit 2. |
| `--dry-run` | Run every check (including verification) and print unified diffs to stdout; write nothing, not even a report. Returns the exit code a real run would. |
| `--only <PATH>` | Apply only this file from the artifact; repeatable. A plain file path (no globs, no directories), resolved against the current directory. A path not in the artifact is a usage error (exit 1). |
| `--report <PATH>` | Post-apply markdown report (default: `munge-comments-apply-report.md`). |

### `report`

```text
munge-comments report [OPTIONS] [ARTIFACT]
```

| Option | Meaning |
|---|---|
| `[ARTIFACT]` | The plan artifact. Missing, or naming no file: exit 2. |
| `--out <FILE>` | Output markdown file (default: `munge-comments-report.md`). |

`report` renders from the artifact alone; the source tree need not exist.

## Configuration and environment

Configuration is **command-line flags plus the process environment only**.
There is no configuration file, and **no `.env` file is ever loaded**.
Precedence is flag > environment variable > default.

| Variable | Flag | Default | Meaning |
|---|---|---|---|
| `OPENAI_API_KEY` | `--api-key` | none | Bearer key for the endpoint |
| `OPENAI_BASE_URL` | `--base-url` | `https://api.openai.com/v1` | OpenAI-compatible endpoint; requests go to `{base}/chat/completions` |
| `MUNGE_COMMENTS_MODEL` | `--model` | `gpt-4.1` | Model name sent with every request |

- **Keyless local endpoints.** A missing API key is a usage error (exit 1)
  only when no base URL was given either. With `--base-url` or
  `OPENAI_BASE_URL` set and no key (a local OpenAI-compatible server such
  as llama.cpp, vLLM or Ollama), requests are sent with `Bearer unused`. An
  empty key counts as missing.
- **`--offline`** needs no endpoint and no key.
- **Proxies.** The CLI honors the usual proxy settings (`HTTP(S)_PROXY`,
  `NO_PROXY`, and the OS proxy configuration), like other network tools in
  your shell.
- **TLS trust** uses the operating system's certificate store (rustls with
  the platform verifier), so corporate CAs installed in the OS just work.
  On Linux that store honors `SSL_CERT_FILE`/`SSL_CERT_DIR`.
- Requests time out after 120 s and are retried with exponential backoff
  (4 retries, 0.5 s base delay, 8 s cap). An authentication failure stops
  the run from scheduling further files; the files it left unattempted are
  recorded as failed, so `--resume` picks them up.
- Structured output is requested with a strict JSON schema; servers that
  don't support schemas fall back to JSON mode, and every prompt also
  embeds the schema. Malformed output is repaired locally first (markdown
  fences are stripped), then by one paid repair retry; truncated output is
  split and retried. A file whose model output stays unusable is recorded
  as `failed` (`failureKind: "llm"`, exit 5) and retried by `--resume`; the
  run itself never crashes on bad model output.

## Hiding modes

**`--mode redact` (default).** Each undecided prose comment is replaced by a
placeholder in the language's own delimiter style, carrying a stable id
unique within its file:

| Original | Placeholder |
|---|---|
| `// some note` (Python: `# some note`) | `//⟦C12⟧` (Python: `#⟦C12⟧`) |
| `/* some note */` | `/*⟦C12⟧*/` |
| `/** doc */` | `/*⟦C13⟧*/` |
| a run of consecutive line comments | one placeholder in the run's line style |

The model regenerates a comment for each placeholder and also answers
`wouldYouCommentHere` — whether the spot needs a comment at all. If a file
already contains text shaped like a placeholder (`⟦C<n>⟧`), that file falls
back to remove mode; the artifact records `effectiveMode: "remove"` and a
`placeholder-collision` warning for it.

**`--mode remove`.** Target prose comments are deleted outright, using
exactly the removal code `apply` uses, and the model freely decides where
comments belong. This is the stronger honesty test (the model isn't told
where comments were), but alignment is fuzzier: regenerations are matched
back to originals by their cited anchor (declaration name, node kind,
snippet overlap), with position as a tiebreaker.

- An original with no matching regeneration means "the model didn't think a
  comment was needed here"; it becomes a `REMOVE` candidate for the judge.
- A regeneration with no original is a **`NEW`** comment. `NEW` comments
  exist only in remove-mode files (including redact-mode collision
  fallbacks), are only ever placed at validated positions between tokens,
  and are **report-only unless `--allow-add`** was passed. A DIFFICULT
  `NEW` is never applied automatically: it is held as `NEEDS_REVIEW`.

In both modes only *undecided* comments are hidden. Functional comments,
TODO-flagged comments and content-empty comments stay verbatim in the text
the model sees, and edits are always computed against the original text.

## How decisions are made

### Classification

Every comment is scanned from the tree-sitter syntax tree and classified
before any LLM call:

- **functional**: load-bearing for some tool. Examples: TypeScript and
  triple-slash directives; linter, formatter and coverage suppressions
  (ESLint, Flow, staticcheck, pyre/pytype, LCOV/GCOVR, NOLINT, NOPMD, …);
  bundler magic comments and pure annotations; JSX pragmas and
  test-environment docblocks; Python `# type:`/`# noqa` and PEP 263
  encoding cookies; Go `//go:*`, the cgo preamble, `Deprecated:`
  paragraphs and `Example…` output blocks; C/C++ `FALLTHROUGH` and IWYU
  pragmas; Rust `SAFETY:` and compiletest comments; scanner suppressions
  (`NOSONAR`, `nosemgrep`, `lgtm`/`codeql`, `nosec`, `gitleaks:allow`,
  detect-secrets pragmas); byte-level hazards (a lone CR or a line splice
  that hides code from the parser, Java `\uXXXX` escapes); modelines,
  `#region`, shebangs, license headers and `sourceMappingURL`. Prose
  sitting between a next-line directive and its target
  (`directive-adjacent`) is protected the same way.
- **doc-protected**: doc comments protected by policy (see
  [Supported languages](#supported-languages)). Never changed
  automatically, but a human may override them in the artifact.
- **prose**: everything else; the candidates.

Functional and doc-protected comments get `KEEP_PROTECTED` and are never
sent to the model.

### Difficulty rubric

The model rates each regeneration. The rubric is given to the model
verbatim, stored per comment and shown in the report:

| Rating | Meaning |
|---|---|
| EASY | Fully reconstructable from immediately visible code; names/types are self-describing |
| MEDIUM | Required reading surrounding code in the file or inferring non-obvious intent |
| DIFFICULT | Requires knowledge absent from the codebase (history, external contracts, subtle invariants); the regenerated comment is guesswork |

`missingContext` records, verbatim, what the model said it was missing:
the strongest signal that the original encodes irreplaceable knowledge.
Comments that never received a rating (decided before the LLM, unmatched,
unreturned, or offline) are counted as `unrated`.

### Decisions

| Decision | Effect |
|---|---|
| `REMOVE` | Delete the comment (with line hygiene: no stray whitespace or emptied lines) |
| `KEEP_ORIGINAL` | Leave the original untouched |
| `KEEP_GENERATED` | Replace it with the regeneration (`finalComment`) |
| `HYBRID` | Replace it with a merge that keeps every fact from the original not visible in the code (`finalComment`) |
| `NEEDS_REVIEW` | The tool won't decide; the original stays. Listed in the report for a human |
| `KEEP_PROTECTED` | Functional or doc-protected; untouched |
| `NEW` | Insert a new comment (remove mode, only with `--allow-add`) |

### Decision matrix

Rows are evaluated in order, except that **row 9 is evaluated right after
row 5**: a comment with no regeneration always goes to the judge with the
reduced decision set, whatever its difficulty, so the judge can never pick
`KEEP_GENERATED` for a comment that has no regeneration.

| # | Situation | Decision path |
|---|---|---|
| 1 | Functional / doc-protected (incl. `directive-adjacent`) | `KEEP_PROTECTED`, never sent to the model |
| 1b | Sole content of a JSX `{/* … */}` container | `KEEP_ORIGINAL` (heuristic), never sent to the model |
| 2 | Tag flag: `TODO`, `FIXME`, `HACK`, `WARNING`, `XXX`, `NOTE` | `KEEP_ORIGINAL` + `flagged-for-review` (heuristic) |
| 3 | Content-empty (no words: `//`, `/* */`, `# -----`) | `REMOVE` (heuristic) |
| 4 | EASY + `wouldYouCommentHere = false` (redact mode) | `REMOVE` (heuristic) |
| 5 | EASY + similarity ≥ `--similarity-threshold` | `REMOVE`: a redundant restatement (heuristic) |
| 6 | EASY + similarity < threshold | Judge, restricted to `REMOVE`/`KEEP_ORIGINAL`/`NEEDS_REVIEW`: an EASY comment is reconstructable from code, so it may be kept or dropped but never rewritten |
| 7 | MEDIUM | Judge, full decision set: pick the version that carries the *why* |
| 8 | DIFFICULT | Judge, biased to `KEEP_ORIGINAL`; anything else is downgraded to `NEEDS_REVIEW` |
| 9 | No regeneration (remove-mode unmatched original; redact-mode id the model didn't answer) | Judge, reduced set `REMOVE`/`KEEP_ORIGINAL`/`NEEDS_REVIEW` |
| 10 | Judge confidence < 0.6 | `NEEDS_REVIEW` |
| 11 | Tie: the judge picked `KEEP_GENERATED`/`HYBRID`, but the result is ≥ 0.8 similar to (or textually the same as) the original | `KEEP_ORIGINAL` (engine) |

Similarity is the Jaccard index over the sets of lowercased words (Unicode
word boundaries) of the two comment bodies, delimiters stripped.

**Post-judge rails**, applied to the judge's output in this fixed order,
first match wins:

1. A decision outside the comment's allowed set, a missing answer, or a
   rewrite without text → `NEEDS_REVIEW`.
2. A rewrite whose text fails sanitization (see
   [Safety model](#safety-model)) → `KEEP_ORIGINAL` + tag `sanitize-failed`.
3. Tie (row 11) → `KEEP_ORIGINAL`.
4. DIFFICULT downgrade (row 8): anything but `KEEP_ORIGINAL` →
   `NEEDS_REVIEW`.
5. Confidence floor (row 10) → `NEEDS_REVIEW`.

Rails set `decidedBy: "engine"` and `confidence: 1`, record the judge's
own decision and confidence in `decisionRationale`, and keep the judge's
proposed `finalComment`. So a reviewer can approve a held rewrite by
changing only `decision`. Rails 4–5 apply only to judge decisions that
aren't already `NEEDS_REVIEW`, and rail 2 only to writing decisions: a
judge `NEEDS_REVIEW` keeps `decidedBy: "judge"`, its own confidence, and
its proposed text, even if that text would fail sanitization (apply
re-validates it if you flip the decision).

**Plan-time verification rail.** As the last step for each file, `plan`
runs the apply pipeline in memory, including the semantic verifier. Some
individually legal edits fail only because of where they leave other
comments (removing line 1 can move a dormant PEP 263 cookie onto line 2).
If the file fails, its edits are re-admitted one at a time in source order,
and any edit that would fail is demoted to `KEEP_ORIGINAL` (a `NEW` to
`NEEDS_REVIEW`) with tag `verify-rail` and a `verify-demoted` warning. So
every `complete` file in a fresh artifact applies cleanly.

**Offline mode** (`--offline`) makes no LLM calls: rows 1–3 still apply,
and every other prose comment is kept (`KEEP_ORIGINAL`,
`decidedBy: "heuristic"`, tag `offline`, unrated).

**Convergence.** A second `plan` over already-cleaned code should be
dominated by no-change decisions. The metric, shown in the summary and the
report, is `(KEEP_ORIGINAL + KEEP_PROTECTED + NEEDS_REVIEW) / scanned
comments` (`NEW` records excluded; zero comments counts as converged); the
target on a second pass is ≥ 95%.

## The plan artifact and how to edit it

The artifact is pretty-printed JSON (2-space indent, files sorted by path).
Abridged:

```json
{
  "version": 1,
  "createdAt": "2026-10-11T04:07:55Z",
  "root": ".",
  "globBase": ".",
  "config": { "mode": "redact", "model": "gpt-4.1", "baseUrl": null, "allowAdd": false, "...": "..." },
  "files": [
    {
      "path": "python/empty-comments.py",
      "contentHash": "sha256:c5b539b2…",
      "byteLength": 118,
      "language": "python",
      "effectiveMode": "redact",
      "status": "complete",
      "failureKind": null,
      "skipReason": null,
      "error": null,
      "warnings": [],
      "comments": [
        {
          "id": "C1",
          "kind": "line",
          "start": 11,
          "end": 12,
          "line": 3,
          "column": 1,
          "position": "leading",
          "classification": "prose",
          "tags": [],
          "original": "#",
          "anchor": { "nodeKind": "assignment", "name": null, "snippet": "ROOT = os.getcwd()\nNAME = \"demo\"" },
          "regenerated": null,
          "wouldYouCommentHere": null,
          "difficulty": null,
          "difficultyRationale": null,
          "missingContext": null,
          "decision": "REMOVE",
          "finalComment": null,
          "confidence": 1.0,
          "decisionRationale": null,
          "decidedBy": "heuristic"
        }
      ]
    }
  ],
  "summary": { "filesConsidered": 41, "removed": 16, "...": "..." }
}
```

Every object rejects unknown keys, so a typo in a hand-edited key fails
loudly (exit 2) instead of being ignored. `apply` re-validates the whole
artifact before doing anything.

**Human-in-the-loop edits.** `apply` honors whatever `decision` and
`finalComment` it finds:

- **Flip a decision.** Change `decision`, e.g. `NEEDS_REVIEW` →
  `KEEP_GENERATED` to accept a held rewrite (rails keep the judge's
  proposal in `finalComment`), `REMOVE` → `KEEP_ORIGINAL` to keep a comment,
  or `KEEP_ORIGINAL` → `REMOVE` to drop one.
- **Edit the text.** `finalComment` is the exact text to write, without
  delimiters; it is required for `KEEP_GENERATED`, `HYBRID` and `NEW`, and
  ignored for every other decision. Apply renders it in the original
  comment's style (same opener, indent and line endings), and re-validates
  it first: text that is empty, contains forbidden characters, contains a
  delimiter that would close the comment, or would itself read as a
  directive is skipped (the original stays) and listed in the apply report.
- **`decidedBy: "human"`.** Optional: mark your edits so the report shows
  who decided. Nothing is inferred from it.
- **Hold or release `NEW` comments.** `NEW` records (ids `N1`, `N2`, …) can
  only be `NEW` or `NEEDS_REVIEW`; scanned records (ids `C…`) can never be
  `NEW`. `config.allowAdd` decides whether `NEW` records are applied at all;
  you may flip it by hand, but then the inserted comments were never
  verified at plan time (apply still verifies them).
- **Don't touch `classification`.** It is informational. `apply` re-scans
  and re-classifies every file it edits with the full classifier, so
  relabeling a functional comment as prose cannot route an edit into it
  (the file is skipped, exit 4).

**Byte offsets.** `start`/`end` are UTF-8 **byte** offsets into the
original file (`line`/`column` are for humans; columns count characters).
For scanned records, `apply` requires `[start, end)` to match a comment it
finds in the current file exactly, with `original` as its text; any
mismatch aborts the run before writing (exit 2). For a `NEW` record,
`start == end` is the insertion point. If you move one, it must lie on a
character boundary, outside every scanned comment, after any shebang line
and BOM, and outside the whitespace a neighboring `REMOVE` deletes;
otherwise the run aborts (exit 2).

**Paths and relocation.** File paths are POSIX and relative to `root`, and
`root` is stored relative to the artifact file's own directory (it may
contain `..`). So `apply` and `report` work from any directory, and the
artifact can live outside the source tree (`--out /tmp/plan.json`). But if
you **move the artifact without its sources**, the relative `root` points
somewhere else, exactly like any relative path. Keep the two in the same
relative position (or move them together).

**Partial applies.** `apply` writes each file atomically, after all checks
have passed for every selected file. If some writes then fail with an I/O
error while others succeed, a plain re-run would abort on drift (the
written files no longer match their hashes). In that case `apply` prints,
and the report repeats, a ready-to-run command that retries only the failed
files:

```console
$ munge-comments apply munge-comments-plan.json --only src/a.rs --only 'src/odd name.rs'
```

Likewise, when unrelated changes drift some files, `--only` can select the
undrifted subset.

**Overriding a doc-protected comment.** Doc-protected is a policy, not a
safety class: you may flip one to `REMOVE`, `KEEP_GENERATED` or `HYBRID`,
and `apply` honors it (counted as an override in the summary and listed in
the report). For JavaScript and Java this only affects tooling (generated
docs, IDE hovers). **For Rust it is a semantic change the verifier cannot
see:** doc comments desugar to `#[doc]` attributes, contain doctests, and
satisfy `missing_docs` lints. The report marks every Rust doc override
"semantic: re-run `cargo test --doc` and `cargo build`"; do so.

## Interruption, resume and signals

**`plan`** is cooperatively interruptible. On the first Ctrl-C (SIGINT, or
SIGTERM on Unix) it stops scheduling new files, lets the files in flight
finish and persist, writes the artifact and the report, prints
`interrupted — continue with --resume`, and exits **130**. A second Ctrl-C
exits immediately (also 130). The artifact is valid but partial; exit 130
overrides every other code for that run, so `plan && apply` never proceeds
on a partial artifact. The artifact is rewritten atomically after every
completed file, so even a hard kill leaves the last consistent state.

Continue with the same command plus `--resume`:

```console
$ munge-comments plan src/ tests/ --resume
```

`--resume` skips files already `complete` or `skipped` whose content hash
is unchanged (so paid LLM work is never repeated), retries `failed` files,
processes newly discovered files and drops entries that are no longer
discovered. It requires the same settings: `mode`, `model`, base URL,
`allowAdd`, similarity threshold, `maxFileChars`, `offline`, paths,
includes and excludes are compared, and any difference is a usage error
(exit 1) listing the differing fields (`--concurrency` may change). Globs
are matched against the directory recorded at the original run, so resume
selects the same files from any directory. `--resume` with no existing
artifact simply starts a fresh run.

**`apply` and `report`** are not interruptible: Ctrl-C exits immediately
with 130, like the OS default. Each file write is individually atomic, but
an interrupted `apply` can leave some files written; re-running reports
them as drift. Use `--only` (or a fresh `plan`) to finish.

## Safety model

The core property: **the tool can only change comment text.** Four
mechanisms enforce it, in order:

1. **Classification firewall.** Functional and protected comments are
   removed from the editable set before any LLM call. `apply` re-runs the
   full scan and classification on every file it edits, ignoring the
   artifact's labels, and skips the whole file (exit 4) if any edit touches
   a comment that is functional *now*. A hand-edited artifact cannot route
   around it.
2. **Offset surgery, never re-printing.** Edits are byte-range
   replacements against the original text. No AST printing, no formatter,
   no line-ending normalization: CRLF files stay CRLF, a BOM stays, the
   shebang stays byte-for-byte. Line hygiene removes the whitespace a
   removal leaves behind (trailing spaces, emptied lines, blank-line runs
   the removal created) and keeps a line break wherever removing a
   multi-line comment would otherwise join two lines (JS ASI, Go
   semicolon insertion).
3. **Sanitizer.** Text bound for a file (a rewrite or a `NEW` comment) is
   rejected if it is empty, contains control or bidi characters or a
   placeholder, contains the block-close delimiter in a block or doc slot,
   would itself classify as functional (no rewrite can introduce a
   `// @ts-ignore` or `# type: ignore`), or matches a language-specific
   hazard (Java `\u` escapes, a C/C++ line ending in `\`, `/*` inside
   C/C++/Rust block comments).
4. **Semantic verifier**, a hard gate before every write. The original and
   edited texts are parsed with the file's grammar, and all of the
   following must hold:
   - the edited text parses cleanly (no error or missing nodes);
   - the token streams are equal (kind and text; comments excluded);
   - the tree shapes are equal (pre-order node kinds with nesting depth,
     which catches a Python dedent);
   - between every pair of tokens, a line break is present after the edit
     exactly when it was present before (the ASI/semicolon-insertion
     guard), and the non-whitespace, non-comment text there is unchanged;
   - the ordered list of functional comments (text and roles) is equal in
     both directions: nothing lost, altered, introduced, or activated by
     moving to a new line.

   A failure skips that file (exit 4); other files proceed. `plan` runs the
   same verifier in memory (the verification rail above), so a fresh
   artifact applies cleanly.

Before verification, `apply` also checks the codebase: every file it would
modify is hashed (sha256 over the raw bytes), and **any** drift aborts the
run before a single byte is written (exit 3). Files with no actionable edit
are never read, so unrelated changes to them can't block an apply. A
symlink found where a planned file was counts as drift. Immediately before
each atomic rename, the file is re-read; if it changed during the apply (an
editor save, a formatter), that write is skipped as an I/O failure.

**What the verifier proves, and what it doesn't.** The verifier compares
tree-sitter parses, not your compiler's. It proves that, *as tree-sitter
sees the file*, only comment text and comment-adjacent whitespace changed.
It cannot see constructs where tree-sitter and the reference compiler
disagree about what a comment is. Those cases are handled by
classification instead (comments that hide code from tree-sitter, such as
a lone CR, a `\` line splice or a Java `\u000a`, are functional and never
edited; files the grammar error-recovers on are skipped), and the
remaining known gaps are listed under
[Known limitations](#known-limitations-and-gotchas). It also cannot see
meaning carried *in* comment text that a tool reads, beyond the directive
tables: overriding a doc-protected Rust comment is the documented example.
Run your build and tests after applying.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success (skipped files do not change the exit code) |
| 1 | Usage error: bad flags or values, unknown `--only` path, unsupported explicit file, missing API key, `--resume` settings mismatch, existing `--out` without `--resume` |
| 2 | Artifact missing, invalid, or disagreeing with the codebase (a path escaping `root`, record ranges that don't match the current file, a misplaced `NEW` offset) |
| 3 | Codebase drift: a file's current content hash ≠ the artifact's (nothing was written) |
| 4 | Semantic verification failure or firewall rejection (that file skipped; others proceed) |
| 5 | LLM/endpoint error (exhausted retries, authentication failure, unusable model output) |
| 6 | Filesystem I/O error |
| 7 | Internal invariant violation or panic: a bug, please report it |
| 130 | `plan` was interrupted; continue with `--resume` (overrides every other code) |

When several failures occur in one run, the code with the highest
precedence wins: 2 > 3 > 7 > 4 > 5 > 6. In `plan`, per-file failures are
recorded in the artifact and the run continues; the exit code reflects the
worst failure kind (`invariant` → 7, `llm` → 5, `io` → 6).

## File discovery

`plan` decides which files to consider as follows:

- **Positional paths.** A directory is walked recursively. A file is taken
  as named: it bypasses `.gitignore`, the include set and the default
  excludes (your `--exclude` globs still apply), and it must have a
  supported extension (otherwise exit 1). A path that doesn't exist but
  contains glob characters (`*`, `?`, `[`, `{`) is a glob pattern; quote it
  so your shell doesn't expand it. Any other nonexistent path is exit 1.
- **`.gitignore` is honored** during walks — in the tree, in parent
  directories, and `.git/info/exclude` — even outside a git repository.
  The global git excludes file and `.ignore` files are not consulted.
- **Dotfiles and dot-directories are skipped** during walks unless you
  name them explicitly as a positional path.
- **Symlinks are never followed.** Symlinked directories are not entered
  and symlinked files found during a walk are not recorded. An explicitly
  named symlinked file is resolved to its real path.
- **Default excludes always apply:** `**/*.{d.ts,d.mts,d.cts}`,
  `**/*.pyi`, `**/*.min.js`, `**/node_modules/**`, `**/vendor/**` and
  `**/{dist,build,out}/**`.
- **Unreadable directories** (and other walk errors) are reported as
  warnings and skipped; the run continues. Paths that aren't valid UTF-8
  are skipped with a warning.
- **Hard links.** On Unix, `apply` warns when a file it writes has other
  hard links: the atomic rename gives the file a new inode, splitting the
  link.

**Skipped files.** Some discovered files are refused at plan time. They are
recorded with `status: "skipped"` and a `skipReason`, listed in the stderr
summary and in the report, and do **not** affect the exit code. `--resume`
retries them only if their content changed.

| `skipReason` | Meaning |
|---|---|
| `non-utf8` | The file isn't valid UTF-8 (files are never decoded lossily) |
| `declared-encoding` | A Python file whose PEP 263 coding cookie names an encoding other than UTF-8; text written back as UTF-8 would be mis-decoded |
| `syntax-errors` | The file doesn't parse cleanly under its grammar. The gate is deliberately strict: grammars occasionally error-recover on *valid* code (e.g. Go's `var i = 0 /*⏎*/ var j = 1`, `⏎` being a line break), and such files are refused rather than risked |
| `too-deep` | The syntax tree is deeper than 1024 levels |

## Library usage

All logic lives in the `munge_comments` library; the binary is a thin
shim behind the default `cli` feature. Depend on the library without the
CLI dependencies (clap, Tokio's signal handling):

```toml
[dependencies]
munge-comments = { git = "<repo-url>", default-features = false }   # or { path = "…" }
```

The crate root re-exports the supported entry points (the crate is 0.x
semver; every module is also `pub`):

- **Pipeline:** `run_plan` (async; needs a Tokio runtime), `PlanRequest`,
  `PlanDeps`, `PlanHooks`, `PlanOutcome`, `PlanConfig`, `Mode`;
  `run_apply` (synchronous), `ApplyRequest`, `ApplyDeps`, `ApplyHooks`,
  `ApplyOutcome`, `ApplyResult`, `ApplyFileResult`, `ApplyStatus`;
  `CancellationToken`.
- **Artifact and reporting:** `read_artifact`, `ValidatedArtifact`,
  `PlanArtifact`, `FilePlan`, `CommentRecord`, `Decision`, `Difficulty`,
  `Summary`, `render_markdown`, `compute_summary`, `convergence_pct`.
- **Comments and verification:** `inventory_file`, `Inventory`,
  `InventoryComment`, `FileRef`, `LanguageId`, `language_for`,
  `verify_semantic_equality`, `functional_sequence`, `FunctionalComment`.
- **LLM:** the `LlmClient` trait, `OpenAiCompatClient`,
  `OpenAiCompatConfig`, `ProxyMode`, `RetryPolicy`, and `FakeLlm` /
  `Scenario` for offline tests.
- **Logging and errors:** the `Log` trait, `CapturingLog`, `NullLog`,
  `WriterLog`; `Error` (with `exit_code()`, the same classification the
  CLI uses) and `LlmErrorReason`.
- Lower-level decision-engine helpers (`DraftRecord`, `Route`,
  `build_draft_records`, `finalize_record`, `route`, `similarity`).

A plan → apply round trip:

```rust
use std::path::Path;
use std::sync::Arc;

use munge_comments::{
    ApplyDeps, ApplyHooks, ApplyRequest, CancellationToken, Error, LlmClient, Mode, NullLog,
    OpenAiCompatClient, OpenAiCompatConfig, PlanConfig, PlanDeps, PlanHooks, PlanRequest,
    run_apply, run_plan,
};

/// Plans `src/` under `project` against an endpoint, then applies the
/// artifact. Returns the CLI-equivalent exit code.
async fn tidy(project: &Path, api_key: String) -> Result<u8, Error> {
    let llm: Arc<dyn LlmClient> = Arc::new(OpenAiCompatClient::new(OpenAiCompatConfig {
        api_key: Some(api_key),
        ..OpenAiCompatConfig::default()
    })?);
    let config = PlanConfig {
        mode: Mode::Redact,
        model: "gpt-4.1".to_owned(),
        base_url: None,
        allow_add: false,
        similarity_threshold: 0.6,
        max_file_chars: 120_000,
        concurrency: 4,
        offline: false, // `true` needs no client: pass `llm: None`
        paths: vec!["src".to_owned()],
        include: None,
        exclude: Vec::new(),
    };
    let log = NullLog;
    let planned = run_plan(
        PlanRequest {
            config,
            out: "munge-comments-plan.json".into(), // relative to `cwd`
            report_path: Some("munge-comments-plan-report.md".into()),
            resume: false,
        },
        PlanDeps {
            log: &log,
            cwd: project,
            llm: Some(llm),
            cancel: CancellationToken::new(),
            hooks: PlanHooks::default(),
        },
    )
    .await?;
    if planned.exit_code != 0 {
        return Ok(planned.exit_code);
    }

    let mut diffs = Vec::new(); // receives unified diffs when `dry_run` is set
    let applied = run_apply(
        ApplyRequest {
            artifact: "munge-comments-plan.json".into(),
            only: Vec::new(),
            dry_run: false,
            report_path: Some("munge-comments-apply-report.md".into()),
        },
        ApplyDeps {
            log: &log,
            stdout: &mut diffs,
            cwd: project,
            hooks: ApplyHooks::default(),
        },
    )?;
    Ok(applied.exit_code)
}
```

The library contract:

- **No ambient I/O.** The library never prints, never reads environment
  variables or the process working directory, never installs signal
  handlers and never exits the process. Every ambient input is a
  parameter: `cwd`, the resolved `PlanConfig`, the `CancellationToken`, the
  `Log` sink, the `LlmClient`. Relative request paths are resolved against
  `cwd`; `read_artifact` requires an absolute path. The one exception is
  TLS trust: the HTTP client consults the OS certificate store (which on
  Linux honors `SSL_CERT_FILE`/`SSL_CERT_DIR`).
- **No implicit client.** When `config.offline` is false, `PlanDeps.llm`
  must be `Some` (otherwise `Error::Usage`). `config.model` is the model
  sent with every request; `config.base_url` is only recorded in the
  artifact — the endpoint is whatever client you pass. `OpenAiCompatConfig`
  defaults to no proxy (`ProxyMode::None`); the CLI uses
  `ProxyMode::System`.
- **Cancellation.** Cancelling `PlanDeps.cancel` behaves like the CLI's
  first Ctrl-C: `run_plan` returns `Ok` with `aborted: true` and
  `exit_code: 130`.
- **Errors.** Fallible functions return `Result<_, Error>`, and
  `Error::exit_code()` gives the CLI's classification. Panics indicate
  bugs; the library does not catch them.
- `tests/library_api.rs` exercises an offline plan → apply → report round
  trip using only the root re-exports, built without default features.

## Known limitations and gotchas

Grammar and verifier edge cases (all fail safe unless noted; `⏎` marks a
line break):

- **TypeScript ASI misparse.** tree-sitter-typescript parses
  `return /*⏎ */ x;` as `return x;`, so the shape check rejects even a
  correct removal of that comment. The plan-time verification rail demotes
  the edit to `KEEP_ORIGINAL` (tag `verify-rail`, a `verify-demoted`
  warning); the comment simply stays.
- **Multi-line comments that end a statement.** JavaScript `a /*⏎*/ b` and
  Go `var i = 0 /*⏎*/ var j = 1` make the grammars error-recover, so such
  files are skipped as `syntax-errors`.
- **C/C++ macros.** A `//` comment on a `#define` line is part of the
  macro's text to tree-sitter-c and is never seen or touched. Rewrites and
  removals of a multi-line block comment inside a `#define` are rejected by
  the verifier (and demoted at plan time). Verifier blind spot:
  tree-sitter-c reads `#define A /* c */(x) x` as a function-like macro (a
  real preprocessor sees an object-like one). A hand-made edit to
  `#define A(x) x` would pass the verifier, but no decision can produce it:
  removal keeps the separating space (`#define A (x) x`).
- **GCC fall-through comments.** `/* FALLTHROUGH */`-style comments are
  functional, recognized by GCC's documented `-Wimplicit-fallthrough=3`
  regular expressions (copied verbatim from the GCC manual). Spellings that
  only the laxer level 1/2 forms accept (e.g. `/* we fall through */`) are
  treated as prose.
- **Rust compiletest annotations.** `//~^` annotations in rustc UI tests are
  functional, but they count lines *upward*: removing an own-line prose
  comment between the annotated line and the annotation shifts the count,
  and nothing models this. Exclude such test directories
  (`--exclude 'tests/ui/**'`).

Classification policy:

- **JavaScript `export { x }`.** JSDoc on a declaration exported through a
  separate `export { x }` or `export default x` statement is classified as
  prose, not `doc-protected`: the export rule recognizes `export`
  declarations and CommonJS exports only. Review those comments in the
  report.
- **Doc-protected overrides** are honored by `apply`; for Rust they are
  semantic changes the verifier cannot see (see the
  [editing guide](#the-plan-artifact-and-how-to-edit-it)).
- **Legacy `<!-- … -->` comments** in JavaScript/TypeScript are never
  enumerated or touched; Python docstrings are code and are never edited.

Operational:

- **Placement is fixed.** A comment's slot is its original position; only
  `NEW` comments (remove mode) go somewhere else. Files with no prose
  comments never receive new ones.
- **Moving an artifact** without its sources breaks its relative `root`.
- **Windows** is expected to build and run, but acceptance is verified on
  macOS (and Linux when available).

## Development

Everything runs offline. The default test suite never touches a network
endpoint: LLM behavior is scripted with `FakeLlm`, and the real-endpoint
tests are `#[ignore]`d and env-gated. See [CONTRIBUTING.md](CONTRIBUTING.md)
for fixtures and for adding a language.

The gates (all must be green):

```console
$ cargo fmt --check
$ cargo clippy --all-targets --all-features -- -D warnings
$ cargo clippy --lib --no-default-features -- -D warnings
$ cargo test
$ cargo test --no-default-features --lib
$ cargo test --no-default-features --lib --test library_api
$ cargo +1.90 check --all-targets                       # MSRV
$ PROPTEST_CASES=1000 cargo test --test verifier_props  # verifier acceptance run
```

The `--no-default-features` runs prove the library builds and works without
the CLI dependencies; the MSRV check needs `rustup toolchain install 1.90`.

**Snapshots.** Golden inventories are [insta](https://insta.rs) snapshots
under `tests/snapshots/`. `cargo-insta` isn't required: to accept changed
snapshots, re-run with `INSTA_UPDATE=always` and review the diff in git:

```console
$ INSTA_UPDATE=always cargo test --test golden
```

**Real-endpoint tests.** Configure the endpoint exactly as for the binary
(`OPENAI_API_KEY`, `OPENAI_BASE_URL`, `MUNGE_COMMENTS_MODEL`), then:

```console
$ MUNGE_COMMENTS_E2E=1 cargo test --test real_endpoint -- --ignored
```

**Docs.** `tests/docs.rs` checks this README: the supported-languages table
lists every adapter, the embedded `plan --help` output matches the binary,
and every `munge-comments` command in it parses. The library example above
is compiled as a doctest by `cargo test`.

## Out of scope

Not implemented in v1 (by design):

- **Languages and content:** tier-2 languages (Ruby, Bash, CSS, HTML —
  the adapter interface is ready for them); legacy `<!-- … -->` comments in
  JS/TS; Vue/Svelte embedded scripts; Python docstring editing.
- **Editing behavior:** relocating comments; applying `NEW` comments
  without `--allow-add`; adding comments to files with no prose comments;
  multi-model judge ensembles.
- **Configuration and tooling:** configuration files (flags and
  environment only); CI workflow provisioning (all gates run locally);
  prebuilt binaries and release automation (`cargo install` is the
  distribution mechanism); publishing to crates.io.

## License

MIT; see [LICENSE](LICENSE).

