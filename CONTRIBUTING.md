# Contributing to munge-comments

Thanks for helping. This guide covers running the suite, adding fixtures,
and adding a language. The [README](README.md) describes the tool itself;
its [Development](README.md#development) section lists the gates every
change must pass.

## Running the suite offline

Everything runs offline, with no API key and no network:

```console
$ cargo test
```

- **The default suite is endpoint-free.** LLM behavior is scripted with
  `FakeLlm` (`src/llm/fake.rs`), a deterministic client that never makes
  a request: scenarios map comments (by their whitespace-collapsed body,
  or anchor name) to regenerations and judge verdicts, comments without an
  entry get a conservative default (a `DIFFICULT` echo, then
  `KEEP_ORIGINAL`), and failures can be injected per file. HTTP transport
  tests run against a local `wiremock` server.
- **Real-endpoint tests** live in `tests/real_endpoint.rs`. Every test
  there is `#[ignore]`d *and* returns early unless `MUNGE_COMMENTS_E2E=1`
  is set, so neither `cargo test` nor `cargo test -- --ignored` alone
  touches the network. To run them, configure the endpoint as for the
  binary (`OPENAI_API_KEY`, `OPENAI_BASE_URL`, `MUNGE_COMMENTS_MODEL`) and
  run:

  ```console
  $ MUNGE_COMMENTS_E2E=1 cargo test --test real_endpoint -- --ignored
  ```

- **Library tests without the CLI**:
  `cargo test --no-default-features --lib` and
  `cargo test --no-default-features --lib --test library_api`.
- **Property tests** use pinned seeds, so failures always reproduce. The
  verifier's acceptance run uses more cases:
  `PROPTEST_CASES=1000 cargo test --test verifier_props`.
- **Repository hygiene.** Tests write only into temp directories; a test
  that leaks a default output file (`munge-comments-*.json`/`.md`) into the
  package root fails `tests/conventions.rs`.

Before sending a change, run every gate listed in the README's
[Development](README.md#development) section (fmt, clippy for both feature
sets, the test runs, and the MSRV check `cargo +1.90 check --all-targets`).

## Project conventions

`tests/conventions.rs` enforces the structural rules:

- **Only `src/lang/registry.rs` may use `tree_sitter`** or the grammar
  crates. Everything else works on the plain syntax tree the registry
  returns.
- **Adapters are data only.** The modules in `src/lang/adapters/` hold
  `&'static` slices and enums, with no engine imports.
- **Dependencies stay within a hard-coded allow-list**, and grammar crates
  are exact-pinned (`"=x.y.z"`): node kinds drift silently between grammar
  versions.
- `.rs` file names are snake_case.

Library code never prints, reads the environment or the process cwd, or
exits; ambient inputs are parameters (clippy's `disallowed-methods` and the
crate-level lints in `src/lib.rs` enforce this).

## Adding golden fixtures

Golden fixtures are the scanner's regression backbone: each file under
`tests/fixtures/golden/<adapter-id>/` is one lexical trap (a comment-like
string, a directive, a CRLF file, …). The directory name must be the
adapter id that the file's extension maps to. Golden files must parse
cleanly and must not contain placeholder-shaped text (`⟦C1⟧`); files that
must fail to parse go in `tests/fixtures/syntax-errors/` instead.

To add one:

1. Add the file under `tests/fixtures/golden/<adapter-id>/`.
2. Add its expectation entry (the ordered raw comment texts and their
   classifications) to `EXPECTATIONS` in `tests/golden.rs`. The suite
   cross-checks the inventory against this hand-written table, so a
   snapshot can't simply pin whatever the code emits.
3. Record the inventory snapshot. `cargo-insta` isn't required:

   ```console
   $ INSTA_UPDATE=always cargo test --test golden
   ```

   Then review the new or changed `tests/snapshots/*.snap` files in
   `git diff` before committing.

The golden tree is also planned end to end, offline (`tests/plan_e2e.rs`,
`tests/library_api.rs`, the transform and verifier suites). Expect those
to exercise any fixture you add.

## Adding a language

A language whose rules the existing adapter vocabulary can express needs
one grammar crate, adapter data and fixtures — no engine changes. Work
through these steps in order; the tests named below fail until each step
is done, so `cargo test` is your checklist.

1. **Grammar crate.** Add the tree-sitter grammar crate to `Cargo.toml`
   with an exact pin (`tree-sitter-foo = "=x.y.z"`), and add its name to
   the hard-coded `ALLOWED_DEPENDENCIES` list in `tests/conventions.rs`.
   The grammar must be ABI-compatible with the pinned `tree-sitter`.
2. **Language id.** Add a `LanguageId` variant in `src/lang/ids.rs`, plus
   its entry in `LanguageId::ALL` (variant order and `ALL` order must
   agree), its adapter id in `as_str` (also the serde name stored in
   artifacts) and its human name in `display_name` (used in prompts).
3. **Grammar registration.** Register the grammar's `LanguageFn` in
   `grammar()` in `src/lang/registry.rs`.
4. **Adapter.** Add the adapter data module `src/lang/adapters/<id>.rs`
   exporting `pub const ADAPTER: LanguageAdapter`: comment node kinds,
   delimiters, the directive table (built with the row constructors in
   `src/lang/adapters/rows.rs`), the doc-comment policy, positional rules,
   anchor and container kinds. Register it in `src/lang/adapters/mod.rs`
   (`pub mod`, and its entry in `ALL_ADAPTERS`, in `LanguageId::ALL`
   order). The existing adapters are the best reference; the vocabulary is
   defined and documented in `src/lang/adapter.rs`.
5. **Extensions.** Map the file extensions in `src/lang/extensions.rs`.
   Then update the hand-written default list in the `--include` help
   string in `src/cli.rs` (the guard test
   `include_help_string_matches_the_extension_table` fails until you do)
   and paste the new `munge-comments plan --help` output into the README's
   embedded help block (`tests/docs.rs` fails until you do).
6. **Golden fixtures.** Add `tests/fixtures/golden/<id>/` with one file per
   lexical trap of the language (directives, doc comments, strings that
   look like comments, nested or grouped comments, …), each with an
   `EXPECTATIONS` entry and an accepted snapshot (see
   [Adding golden fixtures](#adding-golden-fixtures)). Include an
   `empty-comments.<ext>` fixture with content-empty prose comments
   (`//`, `/* */`): offline planning removes those, which is how the
   offline end-to-end tests prove that apply edits and verifies every
   language. Per-language test helpers outside the language layer (e.g.
   the sample files in `tests/prompts.rs`) cover only the languages they
   exercise and need no change.
7. **Noisy repo.** Add realistic source files under
   `tests/fixtures/noisy-repo/` and a `ground-truth.json` entry for every
   comment in them: `path`, `key` (the comment's whitespace-collapsed body,
   without delimiters), `anchorName`, and `label` — `superfluous`,
   `valuable`, `rewrite` (with `rewriteTo`, the improved text), `functional`
   or `protected`. Every language needs at least one `superfluous` and one
   `valuable` entry; `tests/noisy_manifest.rs` fails until the manifest
   and the tree agree. The `FakeLlm` scenario for the efficacy e2e
   (`tests/noisy_repo.rs`) is generated from this manifest.
8. **README.** Add the language to the
   [supported-languages table](README.md#supported-languages), with its
   adapter id in the first column (`tests/docs.rs` fails until you do).
9. **Registry validation.** Run `cargo test`. Registry initialization
   validates every node kind, field and closing token the adapter declares
   against the linked grammar, and checks the grammar's ABI; a mismatch is
   an internal error (exit 7) at startup. The registry, adapter and
   classifier unit tests (directive patterns compile, every adapter is
   validated) must pass.

**When the vocabulary isn't enough.** Some languages need a rule the
adapter vocabulary can't express — for example Ruby's `=begin`/`=end`
blocks or HTML conditional comments. Then also add the new variant
(`PositionalRuleId`, `DocDetect` or `ExportedRule`) to
`src/lang/adapter.rs` and implement it in the scanner
(`src/comments/scan.rs`) or classifier (`src/comments/classify.rs`), with
unit tests and golden fixtures that pin the new behavior.

## Changing behavior

- **Artifact format.** The serde structs in `src/artifact/types.rs`,
  `src/comments/types.rs` and `src/config.rs` are the single source of
  truth, and artifacts reject unknown keys. Update the README's artifact
  section with any user-visible change.
- **CLI.** Keep `src/cli.rs`, the README's option documentation and its
  embedded `plan --help` output in sync; `tests/docs.rs` checks the help
  block and that every command in the README parses.
- **Exit codes** are a stable contract (see the README); new failure modes
  map onto the existing `Error` variants in `src/errors.rs`.
- **Safety.** Any change that affects what gets written must keep the
  semantic verifier (`src/apply/verify.rs`) as the final gate, and must
  come with tests. The verifier's mutation suites (`tests/verifier.rs`,
  `tests/verifier_props.rs`) must stay green.
