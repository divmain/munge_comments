//! Remove-mode alignment (`engine::align`) on fixtures: a Java class split
//! into sub-chunks — a header chunk, member-only chunks and a closing
//! chunk — whose citations resolve against the whole-file clean tree.

mod common;

use std::collections::BTreeSet;
use std::fs;

use munge_comments::artifact::types::Difficulty;
use munge_comments::comments::types::Anchor;
use munge_comments::engine::align::{Alignment, align_remove_mode};
use munge_comments::lang::adapters::adapter_for;
use munge_comments::llm::{
    ChunkText, CitedPosition, RemoveModeComment, build_chunk_texts, chunk_file,
};
use munge_comments::{FileRef, Inventory, LanguageId, Mode, inventory_file};

fn cite(node_kind: &str, name: Option<&str>, snippet: &str, comment: &str) -> RemoveModeComment {
    RemoveModeComment {
        anchor: Anchor {
            node_kind: node_kind.to_string(),
            name: name.map(str::to_string),
            snippet: snippet.to_string(),
        },
        position: CitedPosition::Leading,
        comment: comment.to_string(),
        difficulty: Difficulty::Medium,
        rationale: "test".to_string(),
        missing_context: None,
    }
}

struct Chunked {
    text: String,
    inventory: Inventory,
    targets: BTreeSet<String>,
    chunk_texts: Vec<ChunkText>,
}

fn large_class(max_chars: usize) -> Chunked {
    let text = fs::read_to_string(common::fixture_path("golden/java/large-class.java"))
        .expect("fixture is readable");
    let inventory = inventory_file(
        &text,
        FileRef {
            language: LanguageId::Java,
            file_name: "LargeClass.java".to_string(),
        },
    )
    .expect("fixture inventories");
    let targets: BTreeSet<String> = inventory
        .comments
        .iter()
        .filter(|comment| comment.editable)
        .map(|comment| comment.id.clone())
        .collect();
    let (chunks, _) =
        chunk_file(&text, LanguageId::Java, &inventory, max_chars, None).expect("chunks");
    let chunk_texts = build_chunk_texts(
        &text,
        &inventory,
        adapter_for(LanguageId::Java),
        &chunks,
        Mode::Remove,
        Some(&targets),
    )
    .expect("chunk texts");
    Chunked {
        text,
        inventory,
        targets,
        chunk_texts,
    }
}

impl Chunked {
    fn align(&self, citations: Vec<(usize, RemoveModeComment)>) -> Alignment {
        align_remove_mode(
            &self.text,
            adapter_for(LanguageId::Java),
            &self.inventory,
            &self.targets,
            &self.chunk_texts,
            citations,
        )
        .expect("alignment succeeds")
    }

    fn id(&self, needle: &str) -> String {
        self.inventory
            .comments
            .iter()
            .find(|comment| comment.text.contains(needle))
            .unwrap_or_else(|| panic!("no comment contains {needle:?}"))
            .id
            .clone()
    }

    /// The chunk whose clean text holds `needle`.
    fn chunk_with(&self, needle: &str) -> usize {
        self.chunk_texts
            .iter()
            .position(|chunk_text| chunk_text.prompt_text.contains(needle))
            .unwrap_or_else(|| panic!("no chunk holds {needle:?}"))
    }
}

#[test]
fn member_only_sub_chunks_resolve_against_the_whole_file_clean_tree() {
    let file = large_class(600);
    assert!(file.chunk_texts.len() >= 3, "{}", file.chunk_texts.len());
    let header = file.chunk_with("public class LargeClass {");
    let member = file.chunk_with("public int multiply(");
    let closing = file.chunk_with("public void reset()");
    assert_eq!(header, 0);
    assert!(
        header < member && member < closing,
        "{header} {member} {closing}"
    );
    // The member chunk is a bare member list: no class header, no closing
    // brace of the class.
    let member_text = &file.chunk_texts[member].prompt_text;
    assert!(!member_text.contains("class LargeClass"));
    assert!(!member_text.trim_end().ends_with("}\n}"));

    let alignment = file.align(vec![
        (
            header,
            cite(
                "field_declaration",
                Some("total"),
                "private int total;",
                "Running total.",
            ),
        ),
        (
            member,
            cite(
                "expression_statement",
                None,
                "total = a * b;",
                "Remember the product.",
            ),
        ),
        (
            closing,
            cite(
                "method_declaration",
                Some("reset"),
                "public void reset() {",
                "Test reset.",
            ),
        ),
    ]);
    assert!(alignment.warnings.is_empty(), "{:#?}", alignment.warnings);
    let pairs: Vec<(String, &str)> = alignment
        .pairs
        .iter()
        .map(|(id, comment)| (id.clone(), comment.comment.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            (file.id("The running total"), "Running total."),
            (file.id("result of multiply"), "Remember the product."),
            (file.id("Reset support"), "Test reset."),
        ]
    );
    assert!(alignment.new_candidates.is_empty());
}

#[test]
fn a_member_cited_from_another_chunk_is_unresolvable() {
    let file = large_class(600);
    let member = file.chunk_with("public int multiply(");
    let closing = file.chunk_with("public void reset()");
    assert_ne!(member, closing);
    let alignment = file.align(vec![(
        closing,
        cite(
            "method_declaration",
            Some("multiply"),
            "public int multiply(int a, int b) {",
            "Product.",
        ),
    )]);
    assert!(alignment.pairs.is_empty());
    assert_eq!(alignment.warnings.len(), 1, "{:#?}", alignment.warnings);
    assert_eq!(
        alignment.warnings[0].kind,
        munge_comments::artifact::types::FileWarningKind::UnresolvableAnchor
    );
}

#[test]
fn a_new_in_a_member_only_chunk_lands_at_its_member() {
    let file = large_class(600);
    let member = file.chunk_with("public int multiply(");
    // `multiply` carries a protected doc comment, so a NEW there is
    // dropped; its body statement's slot already holds a target, too. The
    // `return` statement's slot is free.
    let alignment = file.align(vec![
        (
            member,
            cite(
                "method_declaration",
                Some("multiply"),
                "public int multiply(int a, int b) {",
                "New.",
            ),
        ),
        (
            member,
            cite("return_statement", None, "return total;", "Hand it back."),
        ),
    ]);
    assert_eq!(alignment.new_candidates.len(), 1, "{alignment:#?}");
    let candidate = &alignment.new_candidates[0];
    assert_eq!(candidate.comment.comment, "Hand it back.");
    // Candidate offsets are whole-file offsets, not chunk-relative: the expected position is the start of the line of the chunk's first `return total;`.
    let chunk_start = file.chunk_texts[member].chunk.original_start;
    let expected = chunk_start
        + file.text[chunk_start..]
            .find("        return total;")
            .expect("return");
    assert_eq!(candidate.offset, expected);
    assert_eq!(alignment.warnings.len(), 1, "{:#?}", alignment.warnings);
}
