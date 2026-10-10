//! The grammar registry: the only tree-sitter user in the crate.
//!
//! Every parse in the tool goes through this module — a rule the
//! conventions test enforces: no other file references the `tree_sitter`
//! crate or any grammar crate. The registry links the nine tier-1 grammars
//! (eight exact-pinned grammar crates; TypeScript and TSX come from
//! `tree-sitter-typescript`), checks each grammar's ABI against the runtime
//! (an incompatible grammar fails at initialization, not at first parse),
//! validates every node kind, closing token and field name the adapters
//! declare against the linked grammar — so adapter/grammar drift fails
//! loudly at startup instead of silently misbehaving at scan time —
//! compiles one comment completeness query per language, and converts
//! source text into [`PlainTree`] plain data.
//!
//! Two tree-sitter 0.27 realities shape the module:
//!
//! - [`Language::node_kind_for_id`] and [`Language::field_name_for_id`]
//!   borrow their names from the language handle, and [`Node::kind`]
//!   borrows from the tree. Plain data carries `&'static str` kind and
//!   field names, so each of the nine [`Language`] handles is `Box::leak`ed
//!   once per process and the registry builds `&'static str` symbol tables
//!   from it; the parser and the compiled [`Query`] also retain pointers
//!   into the language. The leak is deliberate, bounded (nine handles), and
//!   permanent by design.
//! - Nothing tree-sitter-shaped (`Parser`, `Tree`, `Node`, `Query`,
//!   `QueryCursor`, `TreeCursor`) leaves this module. RAII owns every
//!   tree, so parsing is memory-safe by construction.
//!
//! Depth is checked iteratively *before* any plain node is built: a tree
//! nested deeper than [`MAX_TREE_DEPTH`] yields the pinned childless-root
//! [`PlainTree`] (`too_deep == true`), so no recursive plain-data consumer —
//! scanner, verifier, `Drop` — can ever be handed a tree that would overflow
//! its stack.

use std::num::NonZeroU16;
use std::ops::Range;
use std::sync::OnceLock;

use tree_sitter::{
    LANGUAGE_VERSION, Language, LanguageError, MIN_COMPATIBLE_LANGUAGE_VERSION, Parser, Query,
    QueryCursor, StreamingIterator, Tree, TreeCursor,
};

use crate::errors::Error;
use crate::lang::adapter::{AnchorWrapper, LanguageAdapter};
use crate::lang::adapters::adapter_for;
use crate::lang::ids::LanguageId;
use crate::lang::types::{MAX_TREE_DEPTH, PlainNode, PlainTree};

/// The name reported for kind ids outside a grammar's symbol table.
/// Tree-sitter's built-in `ERROR` symbol is `u16::MAX` and its internal
/// `_ERROR` is `u16::MAX - 1`; both sit past every table, as does any
/// hypothetical out-of-range id, so they resolve here instead of indexing
/// out of bounds.
const ERROR_KIND: &str = "ERROR";

/// The process-wide grammar registry, initialized on first use. A stored
/// `Err` (a message) makes every later call fail with the same invariant:
/// grammars and adapters are static data, so an initialization failure is
/// permanent by construction and retrying cannot succeed.
static REGISTRY: OnceLock<Result<Registry, String>> = OnceLock::new();

/// The initialized grammar registry: one entry per supported language, in
/// [`LanguageId::ALL`] order. Its only public capability is
/// [`Registry::parse`]; every grammar detail stays private to this module.
pub struct Registry {
    languages: [LanguageEntry; 9],
}

/// One language's linked grammar plus the data derived from it at
/// initialization.
struct LanguageEntry {
    /// The grammar handle, leaked once per process so its symbol names are
    /// `&'static` and the parser's and query's internal pointers into it
    /// stay valid for the life of the process.
    language: &'static Language,
    /// Node-kind names indexed by kind id.
    kinds: Vec<&'static str>,
    /// Field names indexed by `field_id - 1` (field id 0 means "no field"
    /// and never appears on a cursor).
    fields: Vec<&'static str>,
    /// The compiled comment completeness query `[(kind) …] @c`, whose
    /// captures cross-check the scanner's own comment walk.
    comment_query: Query,
}

/// The process-wide grammar registry, initializing it on the first call.
///
/// Initialization handles all nine languages at once: each grammar gets an
/// ABI check, a symbol table, adapter/grammar validation and a compiled
/// completeness query. Grammars are static data, so this is cheap — a few
/// dozen symbol lookups and one small query compile per language — and
/// there is no per-language preload step and no "language not loaded"
/// failure mode. A failure is stored and surfaces as [`Error::Invariant`]
/// on every call.
pub fn registry() -> Result<&'static Registry, Error> {
    lookup(REGISTRY.get_or_init(init))
}

impl Registry {
    /// The initialized entry for `language`. [`init`] fills the table in
    /// [`LanguageId::ALL`] order, so [`LanguageId::index`] is the slot.
    fn entry(&self, language: LanguageId) -> &LanguageEntry {
        &self.languages[language.index()]
    }

    /// Parses `text` under `language`'s grammar into plain data.
    ///
    /// A fresh [`Parser`] per call keeps every parse independent of prior
    /// parser state; construction is negligible next to parsing itself.
    /// The procedure is pinned by the design: parse, refuse any tree nested
    /// deeper than [`MAX_TREE_DEPTH`] *before* building plain data (a
    /// refused tree becomes the childless-root [`PlainTree`] with `too_deep`
    /// set, `has_error` false and no captures — callers skip such a file on
    /// `too_deep` alone), otherwise convert via an iterative [`TreeCursor`]
    /// walk, record `has_error` from the root node, and run the stored
    /// completeness query. Dirty trees convert normally: the verifier
    /// re-parses edited text before checking `has_error`, so a plain tree
    /// must exist for it either way.
    ///
    /// `Parser::parse` returns `None` only when no language is assigned or
    /// a timeout/cancellation flag is in play — neither can occur here —
    /// so a `None` is an invariant, not a parse result.
    pub fn parse(&self, text: &str, language: LanguageId) -> Result<PlainTree, Error> {
        let entry = self.entry(language);
        let mut parser = Parser::new();
        parser
            .set_language(entry.language)
            .map_err(|error| Error::Invariant(abi_failure_message(language, error)))?;
        let tree = parser.parse(text, None).ok_or_else(|| {
            Error::Invariant(format!(
                "the {} parser produced no tree without a timeout or cancellation in play \
                 (this is a bug)",
                language.as_str(),
            ))
        })?;
        if depth_exceeds(&tree, MAX_TREE_DEPTH) {
            // The pinned shape for refused trees: a childless root, no
            // error report, no captures. Every consumer refuses the file on
            // `too_deep` before looking at anything else.
            let cursor = tree.walk();
            let root = plain_node(entry, &cursor);
            return Ok(PlainTree {
                root,
                has_error: false,
                too_deep: true,
                comment_query_ranges: Vec::new(),
            });
        }
        let root = convert_tree(entry, &tree)?;
        let has_error = tree.root_node().has_error();
        let comment_query_ranges = comment_query_ranges(entry, &tree, text);
        Ok(PlainTree {
            root,
            has_error,
            too_deep: false,
            comment_query_ranges,
        })
    }
}

/// Parses `text` under `language`'s grammar — the crate-wide convenience
/// entry point (`registry()?.parse(..)`), re-exported as `lang::parse` for
/// the scanning pipeline.
pub fn parse(text: &str, language: LanguageId) -> Result<PlainTree, Error> {
    registry()?.parse(text, language)
}

/// Maps the stored initialization result to the public result type: a
/// stored message becomes [`Error::Invariant`] on every call.
fn lookup(result: &Result<Registry, String>) -> Result<&Registry, Error> {
    match result {
        Ok(registry) => Ok(registry),
        Err(message) => Err(Error::Invariant(format!(
            "grammar registry initialization failed: {message}"
        ))),
    }
}

/// Initializes the registry: one entry per language, in
/// [`LanguageId::ALL`] order, stopping at the first failure.
fn init() -> Result<Registry, String> {
    let mut languages = Vec::with_capacity(LanguageId::ALL.len());
    for id in LanguageId::ALL {
        languages.push(init_language(id).map_err(invariant_message)?);
    }
    // The loop pushes exactly one entry per `LanguageId::ALL` element
    // (nine), so this conversion always succeeds; `try_into` keeps the
    // function total rather than indexing or unwrapping.
    let languages = languages.try_into().map_err(|_| {
        String::from("the registry built a number of language entries other than nine (a bug)")
    })?;
    Ok(Registry { languages })
}

/// The stored form of an initialization failure: the message inside the
/// invariant (every init failure is one), kept as an owned `String` because
/// the [`OnceLock`] stores its result by value.
fn invariant_message(error: Error) -> String {
    match error {
        Error::Invariant(message) => message,
        other => other.to_string(),
    }
}

/// The linked grammar for each language — the registry's `LanguageFn`
/// entries. Adding a language means adding its exact-pinned grammar crate
/// and one arm here (plus adapter data and fixtures), per the contributor
/// contract.
fn grammar(id: LanguageId) -> Language {
    match id {
        LanguageId::Typescript => Language::from(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
        LanguageId::Tsx => Language::from(tree_sitter_typescript::LANGUAGE_TSX),
        LanguageId::Javascript => Language::from(tree_sitter_javascript::LANGUAGE),
        LanguageId::Python => Language::from(tree_sitter_python::LANGUAGE),
        LanguageId::Go => Language::from(tree_sitter_go::LANGUAGE),
        LanguageId::Java => Language::from(tree_sitter_java::LANGUAGE),
        LanguageId::C => Language::from(tree_sitter_c::LANGUAGE),
        LanguageId::Cpp => Language::from(tree_sitter_cpp::LANGUAGE),
        LanguageId::Rust => Language::from(tree_sitter_rust::LANGUAGE),
    }
}

/// Initializes one language: leak its grammar handle, ABI-check it against
/// the runtime, build its symbol tables, validate its adapter against the
/// grammar, and compile its completeness query — in that order.
fn init_language(id: LanguageId) -> Result<LanguageEntry, Error> {
    let adapter = adapter_for(id);
    // Leaked once per process: the symbol tables below borrow their names
    // from this handle, and the parser and query keep pointers into it.
    let language: &'static Language = Box::leak(Box::new(grammar(id)));
    // The ABI check. A grammar compiled against an incompatible tree-sitter
    // ABI fails here, at startup, and never reaches a parse.
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .map_err(|error| Error::Invariant(abi_failure_message(id, error)))?;
    let kinds = kind_table(id, language)?;
    let fields = field_table(id, language)?;
    validate_adapter(language, adapter)?;
    let comment_query = compile_comment_query(language, adapter)?;
    Ok(LanguageEntry {
        language,
        kinds,
        fields,
        comment_query,
    })
}

/// The invariant message for a failed ABI check, naming the language and
/// the versions involved.
fn abi_failure_message(id: LanguageId, error: LanguageError) -> String {
    match error {
        LanguageError::Version(version) => format!(
            "the {} grammar's ABI version {version} is outside the linked tree-sitter \
             runtime's supported range {}..={}",
            id.as_str(),
            MIN_COMPATIBLE_LANGUAGE_VERSION,
            LANGUAGE_VERSION,
        ),
        LanguageError::NotParseable => format!(
            "the {} grammar is not parseable by the linked tree-sitter runtime",
            id.as_str()
        ),
    }
}

/// Builds the node-kind symbol table: kind names indexed by kind id. Every
/// id below `node_kind_count` carries a name in a well-formed grammar, so a
/// hole means corrupt grammar data and fails initialization. The names
/// borrow from `language` — the caller passes the leaked `'static` handle.
fn kind_table(id: LanguageId, language: &Language) -> Result<Vec<&str>, Error> {
    (0..language.node_kind_count())
        .map(|kind_id| language.node_kind_for_id(kind_id as u16))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            Error::Invariant(format!(
                "the {} grammar has a hole in its node-kind symbol table (corrupt grammar data)",
                id.as_str()
            ))
        })
}

/// Builds the field-name table: names indexed by `field_id - 1`. Field ids
/// `1..=field_count` are the named ones (id 0 is "no field"); a hole below
/// the count means corrupt grammar data and fails initialization. The names
/// borrow from `language` — the caller passes the leaked `'static` handle.
fn field_table(id: LanguageId, language: &Language) -> Result<Vec<&str>, Error> {
    (1..=language.field_count() as u16)
        .map(|field_id| language.field_name_for_id(field_id))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            Error::Invariant(format!(
                "the {} grammar has a hole in its field-name table (corrupt grammar data)",
                id.as_str()
            ))
        })
}

/// Checks adapter/grammar agreement: every node kind, closing token and
/// field name the adapter declares must exist in the linked grammar, so
/// grammar drift (node kinds shift silently between grammar versions, hence
/// the exact pins) fails loudly at registry initialization.
///
/// Pure over `(&Language, &LanguageAdapter)`: tests feed it a fake adapter
/// naming nonexistent kinds or fields without corrupting the process-wide
/// registry.
fn validate_adapter(language: &Language, adapter: &LanguageAdapter) -> Result<(), Error> {
    let grammar_version = language.abi_version();
    let drift = |what: &str, declared: &str| {
        Error::Invariant(format!(
            "adapter/grammar drift: the {} adapter declares the {what} `{declared}`, which the \
             linked grammar (ABI version {grammar_version}) does not define",
            adapter.id.as_str(),
        ))
    };
    // Named kinds: comments, anchorable declarations, containers, every
    // kind rule code names, and anchor-wrapper kinds.
    for kind in adapter
        .comment_node_kinds
        .iter()
        .chain(adapter.declaration_kinds)
        .chain(adapter.container_kinds)
        .chain(adapter.referenced_kinds)
        .chain(adapter.anchor_wrappers.iter().map(|wrapper| match wrapper {
            AnchorWrapper::Field { kind, .. } | AnchorWrapper::PrefixSibling { kind } => kind,
        }))
    {
        if language.id_for_node_kind(kind, true) == 0 {
            return Err(drift("node kind", kind));
        }
    }
    // Closing tokens are anonymous tokens.
    for token in adapter.closing_tokens {
        if language.id_for_node_kind(token, false) == 0 {
            return Err(drift("anonymous closing token", token));
        }
    }
    // Field names tried for anchor naming, plus anchor-wrapper fields.
    for field in adapter
        .name_fields
        .iter()
        .chain(
            adapter
                .anchor_wrappers
                .iter()
                .filter_map(|wrapper| match wrapper {
                    AnchorWrapper::Field { field, .. } => Some(field),
                    AnchorWrapper::PrefixSibling { .. } => None,
                }),
        )
    {
        if language.field_id_for_name(field).is_none() {
            return Err(drift("field name", field));
        }
    }
    Ok(())
}

/// Compiles the completeness query `[(kind) …] @c` over the adapter's
/// comment node kinds, so every comment the scanner's own walk finds can be
/// cross-checked against an independent grammar query.
fn compile_comment_query(language: &Language, adapter: &LanguageAdapter) -> Result<Query, Error> {
    let mut source = String::from("[");
    for (position, kind) in adapter.comment_node_kinds.iter().enumerate() {
        if position > 0 {
            source.push(' ');
        }
        source.push('(');
        source.push_str(kind);
        source.push(')');
    }
    source.push_str("] @c");
    Query::new(language, &source).map_err(|error| {
        Error::Invariant(format!(
            "the {} adapter's comment completeness query {source:?} failed to compile: {error}",
            adapter.id.as_str(),
        ))
    })
}

impl LanguageEntry {
    /// The name for a kind id: the table's entry, or [`ERROR_KIND`] for
    /// ids at or past the table's end (the built-in `ERROR` and `_ERROR`
    /// symbols among them, since they sit at the top of the `u16` range).
    fn kind_name(&self, kind_id: u16) -> &'static str {
        self.kinds
            .get(kind_id as usize)
            .copied()
            .unwrap_or(ERROR_KIND)
    }

    /// The name for a field id. Ids never occur outside the table for trees
    /// parsed under this entry's own grammar; a hypothetical out-of-table
    /// id yields `None`, matching [`PlainNode::field_name`]'s optionality
    /// (unlike kinds, a missing field name has a natural absent value).
    fn field_name(&self, field_id: NonZeroU16) -> Option<&'static str> {
        self.fields.get(field_id.get() as usize - 1).copied()
    }
}

/// Whether the tree's nesting exceeds `limit`, measured with an iterative
/// [`TreeCursor`] pass (the root sits at depth 0, so `limit` is the maximum
/// recursion depth any recursive plain-data consumer would hit). Exits on
/// the first breach; the walk itself never recurses, so a pathological
/// tree cannot overflow the stack here.
fn depth_exceeds(tree: &Tree, limit: usize) -> bool {
    let mut cursor = tree.walk();
    let mut depth: usize = 0;
    loop {
        if depth > limit {
            return true;
        }
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            // `goto_parent` only fails on the node the cursor started from
            // (the root): the whole tree has been measured.
            if !cursor.goto_parent() {
                return false;
            }
            depth -= 1;
        }
    }
}

/// Converts a tree to a [`PlainNode`] with an iterative [`TreeCursor`]
/// walk — no recursion over tree-sitter nodes. The root's frame lives in a
/// local; the stack holds exactly the frames of the root's descendants,
/// with the cursor's current node always on top. When the stack empties,
/// the cursor is back at the root and every child has been attached.
fn convert_tree(entry: &LanguageEntry, tree: &Tree) -> Result<PlainNode, Error> {
    let mut cursor = tree.walk();
    let mut root = plain_node(entry, &cursor);
    let mut stack: Vec<PlainNode> = Vec::new();
    loop {
        if cursor.goto_first_child() {
            stack.push(plain_node(entry, &cursor));
            continue;
        }
        loop {
            // The cursor's node is complete: attach it to its parent.
            let Some(node) = stack.pop() else {
                // No frames remain, so the cursor is at (or never left) the
                // root and every child has been attached.
                return Ok(root);
            };
            match stack.last_mut() {
                Some(parent) => parent.children.push(node),
                None => root.children.push(node),
            }
            if cursor.goto_next_sibling() {
                stack.push(plain_node(entry, &cursor));
                break; // back to the descend phase
            }
            // The popped node always has a parent (the root itself never
            // gets a frame), so this cannot fail; the walk stays total
            // rather than discarding the result.
            if !cursor.goto_parent() {
                return Err(Error::Invariant(String::from(
                    "the plain-tree walk escaped the root node (this is a bug)",
                )));
            }
        }
    }
}

/// Builds one plain node from the cursor's current position. Kind and field
/// names come from ids plus the symbol tables: tree-sitter 0.27's
/// `kind()`/`field_name()` borrow from the tree, which plain data cannot.
fn plain_node(entry: &LanguageEntry, cursor: &TreeCursor) -> PlainNode {
    let node = cursor.node();
    PlainNode {
        kind: entry.kind_name(node.kind_id()),
        kind_id: node.kind_id(),
        named: node.is_named(),
        start: node.start_byte(),
        end: node.end_byte(),
        start_row: node.start_position().row,
        start_col: node.start_position().column,
        end_row: node.end_position().row,
        field_name: cursor
            .field_id()
            .and_then(|field_id| entry.field_name(field_id)),
        is_error: node.is_error(),
        is_missing: node.is_missing(),
        children: Vec::new(),
    }
}

/// Runs the stored completeness query and collects the captured comment
/// ranges, in capture (document) order. The query carries no text
/// predicates, so the text provider the capture API requires is never
/// actually consulted.
fn comment_query_ranges(entry: &LanguageEntry, tree: &Tree, text: &str) -> Vec<Range<usize>> {
    let mut cursor = QueryCursor::new();
    let mut captures = cursor.captures(&entry.comment_query, tree.root_node(), text.as_bytes());
    let mut ranges = Vec::new();
    while let Some((query_match, capture_index)) = captures.next() {
        let node = query_match.captures()[*capture_index].node;
        ranges.push(node.start_byte()..node.end_byte());
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::adapter::BlockDelimiters;
    use crate::lang::types::{first_child_with_field, leaves};

    /// A hello-world-ish snippet per language; all nine must parse cleanly
    /// through the registry.
    const HELLO: &[(LanguageId, &str)] = &[
        (
            LanguageId::Typescript,
            r#"const x: number = 1;
export function greet(name: string): string {
    return "hello " + name;
}
"#,
        ),
        (
            LanguageId::Tsx,
            r#"const App = () => <div className="greet">hello world</div>;
"#,
        ),
        (
            LanguageId::Javascript,
            r#"function greet(name) {
    return "hello " + name;
}
console.log(greet("world"));
"#,
        ),
        (
            LanguageId::Python,
            r#"def greet(name):
    return f"hello {name}"

print(greet("world"))
"#,
        ),
        (
            LanguageId::Go,
            r#"package main

import "fmt"

func main() {
	fmt.Println("hello world")
}
"#,
        ),
        (
            LanguageId::Java,
            r#"public class Main {
    public static void main(String[] args) {
        System.out.println("hello world");
    }
}
"#,
        ),
        (
            LanguageId::C,
            r#"#include <stdio.h>

int main(void) {
    printf("hello world\n");
    return 0;
}
"#,
        ),
        (
            LanguageId::Cpp,
            r#"#include <iostream>

int main() {
    std::cout << "hello world" << std::endl;
    return 0;
}
"#,
        ),
        (
            LanguageId::Rust,
            r#"fn main() {
    println!("hello world");
}
"#,
        ),
    ];

    /// `x = ((((…1…))));` — a valid assignment nested `depth` parens deep.
    fn nested_parens(depth: usize) -> String {
        let mut source = String::with_capacity(depth * 2 + 10);
        source.push_str("x = ");
        for _ in 0..depth {
            source.push('(');
        }
        source.push('1');
        for _ in 0..depth {
            source.push(')');
        }
        source.push(';');
        source
    }

    /// An all-empty JavaScript-shaped adapter, mutated per validation case.
    fn base_fake_adapter() -> LanguageAdapter {
        LanguageAdapter {
            id: LanguageId::Javascript,
            comment_node_kinds: &[],
            line_prefixes: &["//"],
            line_opener: r"^//[/!]*",
            block_delimiters: Some(BlockDelimiters {
                open: "/*",
                close: "*/",
            }),
            block_comments_nest: false,
            doc: None,
            directives: &[],
            positional_rules: &[],
            declaration_kinds: &[],
            container_kinds: &[],
            closing_tokens: &[],
            name_fields: &[],
            exported_rule: None,
            anchor_wrappers: &[],
            slot_rejects: &[],
            referenced_kinds: &[],
        }
    }

    /// Recursively collects the ranges of `comment_kinds` nodes, in document
    /// order (test inputs are shallow, so recursion is fine here).
    fn collect_comment_ranges(node: &PlainNode, comment_kinds: &[&str]) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        if comment_kinds.contains(&node.kind) {
            ranges.push(node.start..node.end);
        }
        for child in &node.children {
            ranges.extend(collect_comment_ranges(child, comment_kinds));
        }
        ranges
    }

    /// Recursively finds the first `ERROR` node (test inputs are shallow).
    fn first_error(node: &PlainNode) -> Option<&PlainNode> {
        if node.is_error {
            return Some(node);
        }
        node.children.iter().find_map(first_error)
    }

    #[test]
    fn every_grammar_parses_a_hello_world_snippet_cleanly() {
        for &(language, source) in HELLO {
            let tree = parse(source, language)
                .unwrap_or_else(|error| panic!("{} hello-world parse: {error}", language.as_str()));
            assert!(
                !tree.has_error,
                "{}: must parse without errors",
                language.as_str()
            );
            assert!(
                !tree.too_deep,
                "{}: hello-world is not too deep",
                language.as_str()
            );
            assert!(
                !tree.root.children.is_empty(),
                "{}: hello-world must produce a populated tree",
                language.as_str(),
            );
        }
    }

    #[test]
    fn registry_initializes_every_language_with_its_own_grammar() {
        let registry = registry().expect("all nine adapters agree with their grammars");
        assert_eq!(registry.languages.len(), LanguageId::ALL.len());
        for id in LanguageId::ALL {
            let entry = &registry.languages[id.index()];
            for kind in adapter_for(id).comment_node_kinds {
                // Pins slot alignment: each entry's symbol table must know
                // its own language's comment kinds by name.
                assert!(
                    entry.kinds.contains(kind),
                    "the {} entry must know its own comment kind {kind}",
                    id.as_str(),
                );
            }
        }
    }

    #[test]
    fn validation_flags_nonexistent_declarations() {
        let javascript = grammar(LanguageId::Javascript);
        // Control case: a kind the grammar really defines validates.
        let mut adapter = base_fake_adapter();
        adapter.comment_node_kinds = &["comment"];
        validate_adapter(&javascript, &adapter)
            .unwrap_or_else(|error| panic!("a real kind must validate: {error}"));
        // A named kind no JS grammar node carries.
        adapter.comment_node_kinds = &["definitely_not_a_node_kind"];
        let error = validate_adapter(&javascript, &adapter).expect_err("a phantom kind is drift");
        assert!(matches!(error, Error::Invariant(_)));
        let message = error.to_string();
        assert!(
            message.contains("javascript"),
            "names the adapter: {message}"
        );
        assert!(
            message.contains("definitely_not_a_node_kind"),
            "names the kind: {message}"
        );
        assert!(
            message.contains("ABI version"),
            "names the grammar version: {message}"
        );
        // An anonymous closing token the grammar does not define.
        adapter.comment_node_kinds = &[];
        adapter.closing_tokens = &["<->"];
        let message = validate_adapter(&javascript, &adapter)
            .expect_err("a phantom token is drift")
            .to_string();
        assert!(message.contains("closing token"), "{message}");
        assert!(message.contains("<->"), "{message}");
        // A field the grammar never declares.
        adapter.closing_tokens = &[];
        adapter.name_fields = &["not_a_field"];
        let message = validate_adapter(&javascript, &adapter)
            .expect_err("a phantom field is drift")
            .to_string();
        assert!(message.contains("field name"), "{message}");
        assert!(message.contains("not_a_field"), "{message}");
        // Anchor-wrapper fields are validated like name fields…
        adapter.name_fields = &[];
        adapter.anchor_wrappers = &[AnchorWrapper::Field {
            kind: "export_statement",
            field: "also_not_a_field",
        }];
        let message = validate_adapter(&javascript, &adapter)
            .expect_err("a phantom wrapper field is drift")
            .to_string();
        assert!(message.contains("also_not_a_field"), "{message}");
        // …and wrapper kinds like named kinds.
        adapter.anchor_wrappers = &[AnchorWrapper::Field {
            kind: "no_such_wrapper_kind",
            field: "declaration",
        }];
        let message = validate_adapter(&javascript, &adapter)
            .expect_err("a phantom wrapper kind is drift")
            .to_string();
        assert!(message.contains("no_such_wrapper_kind"), "{message}");
    }

    #[test]
    fn plain_data_recovers_fields_and_leaves_skip_subtrees() {
        let source = "function greet(name: string): string {\n    return name;\n}\n";
        let tree = parse(source, LanguageId::Typescript).expect("clean TypeScript parse");
        let function = tree
            .root
            .children
            .iter()
            .find(|node| node.kind == "function_declaration")
            .expect("the snippet declares a function");
        let name = first_child_with_field(function, "name")
            .expect("function_declaration holds its identifier under `name`");
        assert_eq!(name.kind, "identifier");
        // Byte offsets: the field's range spells the declared name.
        assert_eq!(&source[name.start..name.end], "greet");
        let body = first_child_with_field(function, "body")
            .expect("function_declaration holds its block under `body`");
        assert_eq!(body.kind, "statement_block");
        // Skipping the body drops exactly the leaves inside it.
        let all = leaves(&tree.root, |_| false).count();
        let outside = leaves(&tree.root, |node| node.kind == "statement_block").count();
        assert!(outside < all, "skipping the body must skip some leaves");
        assert!(
            leaves(&tree.root, |node| node.kind == "statement_block")
                .all(|leaf| { leaf.end <= body.start || leaf.start >= body.end }),
            "no yielded leaf may come from inside the skipped body"
        );
    }

    #[test]
    fn a_dirty_parse_converts_with_an_error_node() {
        let tree = parse("let x = ;\n", LanguageId::Javascript)
            .expect("dirty parses still convert without panicking");
        assert!(tree.has_error, "the dirty parse must be flagged");
        assert!(!tree.too_deep);
        let error = first_error(&tree.root).expect("the converted tree contains an ERROR node");
        assert_eq!(error.kind, "ERROR");
    }

    #[test]
    fn ten_thousand_parse_drop_cycles_complete() {
        // Sanity only: RAII owns every tree and plain tree, so nothing can
        // accumulate across cycles.
        for _ in 0..10_000 {
            let tree = parse("const answer = 6 * 7; // hide me\n", LanguageId::Javascript)
                .expect("parse succeeds");
            assert!(!tree.has_error);
        }
    }

    #[test]
    fn a_pathologically_deep_expression_is_refused_not_overflowed() {
        let source = nested_parens(5_000);
        let tree = parse(&source, LanguageId::Javascript)
            .expect("the deep parse completes (all walks are iterative)");
        assert!(tree.too_deep, "5,000 levels exceeds MAX_TREE_DEPTH");
        assert!(
            !tree.has_error,
            "the pinned too-deep shape reports no error"
        );
        assert!(
            tree.root.children.is_empty(),
            "the pinned too-deep root is childless"
        );
        assert!(tree.comment_query_ranges.is_empty());

        // The same parse on a thread with a 2 MiB stack (a Tokio worker's
        // budget) must complete without overflow: the depth pass and the
        // (skipped) conversion never recurse, and no PlainNode exists for a
        // refused tree.
        let handler = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || parse(&source, LanguageId::Javascript))
            .expect("tests may spawn threads");
        let tree = handler
            .join()
            .expect("the 2 MiB stack must not overflow")
            .expect("the parse succeeds");
        assert!(tree.too_deep);
    }

    #[test]
    fn a_tree_just_under_the_depth_limit_converts_normally() {
        // 1,000 parens nest the deepest token ~1,003 levels deep: under
        // MAX_TREE_DEPTH, so the tree converts fully (and its recursive
        // Drop is exactly the shape the depth limit exists to keep safe).
        let source = nested_parens(1_000);
        let tree = parse(&source, LanguageId::Javascript).expect("parse succeeds");
        assert!(!tree.too_deep);
        assert!(!tree.has_error);
        assert!(!tree.root.children.is_empty());
    }

    #[test]
    fn out_of_table_kind_ids_map_to_error() {
        let registry = registry().expect("initialization succeeds");
        let entry = &registry.languages[LanguageId::Javascript.index()];
        assert_eq!(entry.kind_name(u16::MAX), "ERROR");
        assert_eq!(entry.kind_name(u16::MAX - 1), "ERROR");
        // An in-table id resolves to its real name.
        let comment_id = entry.language.id_for_node_kind("comment", true);
        assert_eq!(entry.kind_name(comment_id), "comment");
    }

    #[test]
    fn comment_captures_match_the_walked_comment_nodes() {
        // JavaScript: two comments, one line and one block.
        let source = "// leading\nconst x = 1; /* trailing */\n";
        let tree = parse(source, LanguageId::Javascript).expect("clean parse");
        assert_eq!(
            tree.comment_query_ranges,
            collect_comment_ranges(
                &tree.root,
                adapter_for(LanguageId::Javascript).comment_node_kinds
            ),
            "the completeness query must agree with the plain walk"
        );
        assert_eq!(tree.comment_query_ranges.len(), 2);
        assert_eq!(&source[tree.comment_query_ranges[0].clone()], "// leading");
        assert_eq!(
            &source[tree.comment_query_ranges[1].clone()],
            "/* trailing */"
        );
        // Rust: the alternation must catch both comment node kinds.
        let source = "// line\n/* block */\nfn main() {}\n";
        let tree = parse(source, LanguageId::Rust).expect("clean parse");
        assert_eq!(tree.comment_query_ranges.len(), 2);
        assert_eq!(
            tree.comment_query_ranges,
            collect_comment_ranges(&tree.root, adapter_for(LanguageId::Rust).comment_node_kinds),
            "the completeness query must agree with the plain walk"
        );
    }

    #[test]
    fn a_stored_initialization_failure_maps_to_an_invariant() {
        let stored: Result<Registry, String> =
            Err(String::from("the kotlin adapter drifted from its grammar"));
        let error = match lookup(&stored) {
            Ok(_) => panic!("a stored initialization failure must map to an invariant"),
            Err(error) => error,
        };
        assert!(matches!(error, Error::Invariant(_)));
        let message = error.to_string();
        assert!(
            message.contains("grammar registry initialization failed"),
            "{message}"
        );
        assert!(
            message.contains("the kotlin adapter drifted from its grammar"),
            "{message}"
        );
    }

    #[test]
    fn abi_failure_messages_name_the_language_and_versions() {
        let message = abi_failure_message(LanguageId::Go, LanguageError::Version(12));
        assert!(message.contains("go grammar"), "{message}");
        assert!(message.contains("12"), "{message}");
        // The linked runtime's supported range, pinned by the plan (13..=15
        // for tree-sitter 0.27): a runtime upgrade that moves it must be a
        // conscious change.
        assert!(message.contains("13..=15"), "{message}");
        let message = abi_failure_message(LanguageId::Rust, LanguageError::NotParseable);
        assert!(message.contains("rust grammar"), "{message}");
        assert!(message.contains("not parseable"), "{message}");
    }

    #[test]
    fn empty_input_yields_a_childless_clean_tree() {
        for &(language, _) in HELLO {
            let tree = parse("", language).expect("empty parses succeed");
            assert!(
                !tree.has_error,
                "{}: empty is not an error",
                language.as_str()
            );
            assert!(!tree.too_deep);
            assert!(tree.root.children.is_empty());
            assert!(tree.comment_query_ranges.is_empty());
        }
    }
}
