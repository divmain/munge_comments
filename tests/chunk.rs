//! Large-file chunking (`llm::chunk`): the partition invariants on the
//! multi-declaration fixtures and the golden Java large class, plus the
//! edge cases — the threshold, trailing comments at chunk ends, oversized
//! passthrough, container descent, packing density, EOF comments,
//! declaration-free files and range re-chunking.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::ops::Range;

use munge_comments::artifact::types::{FileWarning, FileWarningKind};
use munge_comments::comments::redact::{TransformOpts, remove_comments};
use munge_comments::comments::types::Position;
use munge_comments::lang::adapters::adapter_for;
use munge_comments::llm::{Chunk, build_chunk_texts, chunk_file};
use munge_comments::{FileRef, Inventory, LanguageId, Mode, inventory_file};

fn inventory(text: &str, language: LanguageId, file_name: &str) -> Inventory {
    inventory_file(
        text,
        FileRef {
            language,
            file_name: file_name.to_string(),
        },
    )
    .expect("test sources inventory cleanly")
}

fn read_fixture(rel: &str) -> String {
    fs::read_to_string(common::fixture_path(rel)).expect("fixture is readable")
}

/// Chunks `text` and checks every partition invariant over `range` (the
/// whole file without one).
fn chunk_checked(
    text: &str,
    language: LanguageId,
    max_chars: usize,
    range: Option<Range<usize>>,
) -> (Inventory, Vec<Chunk>, Vec<FileWarning>) {
    let file_name = format!("input.{}", extension(language));
    let inventory = inventory(text, language, &file_name);
    let (chunks, warnings) =
        chunk_file(text, language, &inventory, max_chars, range.clone()).expect("chunks");
    let span = range.unwrap_or(0..text.len());
    assert_partition(text, &inventory, &chunks, &span);
    assert_prompt_texts(text, &inventory, language, &chunks, &span);
    // The outline is deterministic.
    let (again, again_warnings) =
        chunk_file(text, language, &inventory, max_chars, Some(span)).expect("chunks");
    assert_eq!(again, chunks);
    assert_eq!(again_warnings, warnings);
    (inventory, chunks, warnings)
}

fn extension(language: LanguageId) -> &'static str {
    match language {
        LanguageId::Typescript => "ts",
        LanguageId::Javascript => "js",
        LanguageId::Python => "py",
        LanguageId::Java => "java",
        LanguageId::Rust => "rs",
        other => panic!("no test extension for {other:?}"),
    }
}

/// Chunks are indexed in order, cover `span` with no gap or overlap, cut
/// only at line starts, share one outline, and every record inside the
/// span is listed by exactly the chunk containing it.
fn assert_partition(text: &str, inventory: &Inventory, chunks: &[Chunk], span: &Range<usize>) {
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0].original_start, span.start);
    assert_eq!(chunks.last().unwrap().original_end, span.end);
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.index, index);
        assert!(chunk.original_start < chunk.original_end || span.is_empty());
        assert_eq!(chunk.outline, chunks[0].outline);
        if index > 0 {
            assert_eq!(chunk.original_start, chunks[index - 1].original_end);
            assert_eq!(
                text.as_bytes()[chunk.original_start - 1],
                b'\n',
                "chunk {index} does not start at a line start"
            );
        }
    }
    for comment in &inventory.comments {
        if comment.end <= span.start || span.end <= comment.start {
            continue;
        }
        let holders: Vec<&Chunk> = chunks
            .iter()
            .filter(|chunk| chunk.comment_ids.contains(&comment.id))
            .collect();
        assert_eq!(
            holders.len(),
            1,
            "{} is not in exactly one chunk",
            comment.id
        );
        let holder = holders[0];
        assert!(holder.original_start <= comment.start && comment.end <= holder.original_end);
    }
    for chunk in chunks {
        let listed: Vec<&String> = inventory
            .comments
            .iter()
            .filter(|c| chunk.original_start <= c.start && c.end <= chunk.original_end)
            .map(|c| &c.id)
            .collect();
        assert_eq!(listed, chunk.comment_ids.iter().collect::<Vec<_>>());
    }
}

/// Each chunk's prompt text hides exactly its own records: in redact mode
/// it carries exactly the placeholders of its editable records; in remove
/// mode its clean text is the matching slice of the whole-span clean text
/// and its offset map covers exactly its range.
fn assert_prompt_texts(
    text: &str,
    inventory: &Inventory,
    language: LanguageId,
    chunks: &[Chunk],
    span: &Range<usize>,
) {
    let adapter = adapter_for(language);
    let redacted = build_chunk_texts(text, inventory, adapter, chunks, Mode::Redact, None)
        .expect("chunk texts");
    for chunk_text in &redacted {
        let chunk = &chunk_text.chunk;
        let editable: BTreeSet<&String> = inventory
            .comments
            .iter()
            .filter(|c| c.editable && chunk.comment_ids.contains(&c.id))
            .map(|c| &c.id)
            .collect();
        let id_map = chunk_text
            .id_map
            .as_ref()
            .expect("redact mode has an id map");
        assert_eq!(id_map.keys().collect::<BTreeSet<_>>(), editable);
        assert_eq!(chunk_text.prompt_text.matches('⟦').count(), editable.len());
        for id in &editable {
            assert!(chunk_text.prompt_text.contains(&format!("⟦{id}⟧")));
        }
        assert_eq!(chunk_text.original_base, chunk.original_start);
        assert!(chunk_text.offset_map.is_none());
    }

    let removed = build_chunk_texts(text, inventory, adapter, chunks, Mode::Remove, None)
        .expect("chunk texts");
    let whole = remove_comments(
        text,
        inventory,
        TransformOpts {
            targets: None,
            range: Some(span.clone()),
        },
    );
    let joined: String = removed.iter().map(|c| c.prompt_text.as_str()).collect();
    assert_eq!(joined, whole.clean_text);
    for chunk_text in &removed {
        let chunk = &chunk_text.chunk;
        let map = chunk_text
            .offset_map
            .as_ref()
            .expect("remove mode has a map");
        assert_eq!(chunk_text.original_base, chunk.original_start);
        assert_eq!(map.first().unwrap().original_start, chunk.original_start);
        assert_eq!(map.last().unwrap().original_end, chunk.original_end);
        assert_eq!(map.last().unwrap().clean_end, chunk_text.prompt_text.len());
        assert!(chunk_text.id_map.is_none());
    }
}

fn chunk_slice<'a>(text: &'a str, chunk: &Chunk) -> &'a str {
    &text[chunk.original_start..chunk.original_end]
}

/// Every trailing comment shares a chunk with the code it trails.
fn assert_trailing_with_code(inventory: &Inventory, chunks: &[Chunk]) {
    for comment in &inventory.comments {
        if comment.position != Position::Trailing {
            continue;
        }
        let code_end = comment
            .prev_code_end
            .expect("trailing comments follow code");
        let holder = chunks
            .iter()
            .find(|chunk| chunk.comment_ids.contains(&comment.id))
            .expect("every comment has a chunk");
        assert!(
            holder.original_start < code_end,
            "{} is split from the code it trails",
            comment.id
        );
    }
}

/// Every dangling end-of-file comment lies in the last chunk.
fn assert_eof_comments_last(inventory: &Inventory, chunks: &[Chunk]) {
    let last = chunks.last().unwrap();
    let eof: Vec<&String> = inventory
        .comments
        .iter()
        .filter(|c| c.anchor.node_kind == "end_of_file")
        .map(|c| &c.id)
        .collect();
    assert!(!eof.is_empty(), "the source has EOF comments");
    for id in eof {
        assert!(
            last.comment_ids.contains(id),
            "{id} is not in the last chunk"
        );
    }
}

#[test]
fn multi_declaration_typescript_fixture_chunks_cleanly() {
    let text = read_fixture("chunk/multi-decl.ts");
    let (inventory, chunks, warnings) = chunk_checked(&text, LanguageId::Typescript, 400, None);
    assert!(chunks.len() >= 4, "got {} chunks", chunks.len());
    assert!(warnings.is_empty(), "{warnings:?}");
    for chunk in &chunks[..chunks.len() - 1] {
        assert!(chunk_slice(&text, chunk).chars().count() <= 400);
    }
    assert_trailing_with_code(&inventory, &chunks);
    assert_eof_comments_last(&inventory, &chunks);
    // Leading comments travel with the node they precede.
    let load = chunks
        .iter()
        .find(|chunk| chunk_slice(&text, chunk).contains("export function load"))
        .unwrap();
    assert!(chunk_slice(&text, load).contains("// Loads a store from a JSON file."));

    let outline = &chunks[0].outline;
    assert!(outline.contains("describe: export function describe(item: Item): string {"));
    assert!(outline.contains("Store: export class Store {"));
    assert!(outline.contains("PAGE_SIZE: export const PAGE_SIZE = 50;"));
    assert!(outline.contains("Pair: type Pair = [string, number];"));
    assert!(outline.contains("import { readFile } from \"fs\";"));
    assert_eq!(outline.lines().count(), 9);
    for comment in &inventory.comments {
        assert!(
            !outline.contains(&comment.text),
            "outline leaks {}",
            comment.id
        );
    }
}

#[test]
fn multi_declaration_python_fixture_chunks_cleanly() {
    let text = read_fixture("chunk/multi-decl.py");
    let (inventory, chunks, warnings) = chunk_checked(&text, LanguageId::Python, 250, None);
    assert!(chunks.len() >= 4, "got {} chunks", chunks.len());
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_trailing_with_code(&inventory, &chunks);
    assert_eof_comments_last(&inventory, &chunks);
    // A decorator stays with its function.
    let square = chunks
        .iter()
        .find(|chunk| chunk_slice(&text, chunk).contains("def square"))
        .unwrap();
    assert!(chunk_slice(&text, square).contains("@memoize\n"));

    let outline = &chunks[0].outline;
    assert!(outline.contains("Item: class Item:"));
    assert!(outline.contains("load: def load(path):"));
    assert!(!outline.contains("reads a JSON list"));
    assert!(outline.contains("PAGE_SIZE = 50"));
}

/// `count` uniform top-level functions of 32 chars each, line included.
fn uniform_functions(count: usize) -> String {
    (0..count)
        .map(|n| format!("function f{n:03}() {{ return {n:03}; }}\n"))
        .collect()
}

#[test]
fn a_file_at_the_threshold_is_one_chunk_and_one_over_it_is_split() {
    let text = uniform_functions(10);
    assert_eq!(text.chars().count(), 320);

    let (inventory, chunks, warnings) = chunk_checked(&text, LanguageId::Javascript, 320, None);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        (chunks[0].original_start, chunks[0].original_end),
        (0, text.len())
    );
    assert_eq!(chunks[0].comment_ids.len(), inventory.comments.len());
    assert!(
        chunks[0].outline.is_empty(),
        "a single-chunk file has no outline"
    );
    assert!(warnings.is_empty());

    let (_, chunks, warnings) = chunk_checked(&text, LanguageId::Javascript, 319, None);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[1].original_start, 9 * 32);
    assert_eq!(chunks[0].outline.lines().count(), 10);
    assert!(warnings.is_empty());
}

#[test]
fn a_file_over_the_threshold_only_by_its_eof_comments_stays_one_chunk() {
    let text = read_fixture("chunk/multi-decl.ts");
    let length = text.chars().count();
    let (_, chunks, warnings) = chunk_checked(&text, LanguageId::Typescript, length - 1, None);
    assert_eq!(chunks.len(), 1);
    assert!(!chunks[0].outline.is_empty(), "the file is over the budget");
    assert!(warnings.is_empty());
}

#[test]
fn a_trailing_comment_on_a_chunk_last_line_stays_in_that_chunk() {
    let text: String = (0..12)
        .map(|n| format!("const a{n:02} = {n:02}; // note {n:02}\n"))
        .collect();
    // Two 30-char declarations per chunk.
    let (inventory, chunks, _) = chunk_checked(&text, LanguageId::Typescript, 60, None);
    assert_eq!(chunks.len(), 6);
    for (n, chunk) in chunks.iter().enumerate() {
        let slice = chunk_slice(&text, chunk);
        assert!(
            slice.ends_with(&format!("; // note {:02}\n", 2 * n + 1)),
            "{slice:?}"
        );
        assert_eq!(chunk.comment_ids.len(), 2);
    }
    assert_trailing_with_code(&inventory, &chunks);
}

#[test]
fn an_over_long_declaration_without_a_container_body_is_sent_whole() {
    let elements: String = (0..40).map(|n| format!("  {n},\n")).collect();
    let big = format!("// The big table.\nexport const TABLE = [\n{elements}];\n");
    let text = format!(
        "// Before.\nconst before = 1;\n\n{big}\n// After.\nconst after = 2; // trailing\n"
    );
    let (_, chunks, warnings) = chunk_checked(&text, LanguageId::Typescript, 60, None);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunk_slice(&text, &chunks[1]), format!("\n{big}"));
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].kind, FileWarningKind::OversizedChunk);
    assert!(
        warnings[0].detail.starts_with("chunk 1 (lines 3-"),
        "{}",
        warnings[0].detail
    );
}

#[test]
fn a_java_file_with_one_large_class_splits_at_its_members() {
    let text = read_fixture("golden/java/large-class.java");
    let (inventory, chunks, warnings) = chunk_checked(&text, LanguageId::Java, 600, None);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(chunks.len() >= 4, "got {} chunks", chunks.len());
    let first = chunk_slice(&text, &chunks[0]);
    assert!(first.starts_with("package com.example.big;"));
    assert!(first.contains("public class LargeClass {"));
    for chunk in &chunks[1..] {
        let slice = chunk_slice(&text, chunk);
        assert!(!slice.contains("class LargeClass"));
        // Each later chunk starts at a member (its leading comment).
        let member = slice.trim_start_matches('\n');
        assert!(
            member.starts_with("    /**") || member.starts_with("    //"),
            "{slice:?}"
        );
        assert!(slice.chars().count() <= 600);
    }
    assert!(chunk_slice(&text, chunks.last().unwrap()).ends_with("\n}\n"));
    assert_trailing_with_code(&inventory, &chunks);
    assert_eq!(
        chunks[0].outline,
        "package com.example.big;\nLargeClass: public class LargeClass {"
    );
}

#[test]
fn two_hundred_small_declarations_pack_densely() {
    let text = uniform_functions(200);
    assert_eq!(text.chars().count(), 6400);
    let (_, chunks, _) = chunk_checked(&text, LanguageId::Javascript, 640, None);
    assert_eq!(chunks.len(), 10);
    // 31 declarations fit 1000 chars: ⌈200 / 31⌉ = 7 = ⌈6400 / 1000⌉.
    let (_, chunks, warnings) = chunk_checked(&text, LanguageId::Javascript, 1000, None);
    assert_eq!(chunks.len(), 7);
    assert!(warnings.is_empty());
    assert_eq!(chunks[0].outline.lines().count(), 200);
}

#[test]
fn dangling_eof_comments_join_the_last_chunk_even_past_the_budget() {
    let tail: String = (0..10)
        .map(|n| format!("// closing remark {n}\n"))
        .collect();
    let text = format!("{}\n{tail}", uniform_functions(4));
    let (inventory, chunks, warnings) = chunk_checked(&text, LanguageId::Javascript, 64, None);
    assert_eq!(chunks.len(), 2);
    assert!(
        warnings.is_empty(),
        "the EOF tail never warns: {warnings:?}"
    );
    assert!(chunk_slice(&text, &chunks[1]).ends_with(&tail));
    assert!(chunk_slice(&text, &chunks[1]).chars().count() > 64);
    assert_eof_comments_last(&inventory, &chunks);
}

#[test]
fn script_style_javascript_without_declarations_chunks_at_statements() {
    let text: String = (0..20)
        .map(|n| format!("// Step {n:02}.\nconsole.log({n:02}); // logged\n"))
        .collect();
    // Two 39-char statements per chunk.
    let (inventory, chunks, warnings) = chunk_checked(&text, LanguageId::Javascript, 100, None);
    assert_eq!(chunks.len(), 10);
    assert!(warnings.is_empty());
    assert_trailing_with_code(&inventory, &chunks);
    assert!(
        chunks[0]
            .outline
            .starts_with("console.log(00);\nconsole.log(01);")
    );
}

#[test]
fn top_level_python_statements_chunk_at_statements() {
    let text: String = (0..20)
        .map(|n| format!("# Step {n:02}.\nprint({n:02})  # printed\n"))
        .collect();
    // Three 32-char statements per chunk.
    let (inventory, chunks, warnings) = chunk_checked(&text, LanguageId::Python, 100, None);
    assert_eq!(chunks.len(), 7);
    assert!(warnings.is_empty());
    assert_trailing_with_code(&inventory, &chunks);
}

#[test]
fn a_comment_only_file_over_the_budget_is_one_chunk() {
    let text: String = (0..10).map(|n| format!("# remark {n}\n")).collect();
    let (inventory, chunks, warnings) = chunk_checked(&text, LanguageId::Python, 20, None);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].comment_ids.len(), inventory.comments.len());
    assert!(chunks[0].outline.is_empty());
    assert!(warnings.is_empty());
}

#[test]
fn rust_attributes_travel_with_the_item_they_precede() {
    let text = "\
fn one() {}

#[cfg(test)]
mod tests {
    #[test]
    fn works() {}
}
";
    let (_, chunks, _) = chunk_checked(text, LanguageId::Rust, 20, None);
    let module = chunks
        .iter()
        .find(|chunk| chunk_slice(text, chunk).contains("mod tests {"))
        .unwrap();
    assert!(chunk_slice(text, module).contains("#[cfg(test)]\nmod tests {"));
    let test_fn = chunks
        .iter()
        .find(|chunk| chunk_slice(text, chunk).contains("fn works"))
        .unwrap();
    assert!(chunk_slice(text, test_fn).contains("#[test]\n    fn works"));
}

#[test]
fn range_rechunking_yields_absolute_offsets_inside_the_range() {
    let text = read_fixture("golden/java/large-class.java");
    let (_, chunks, _) = chunk_checked(&text, LanguageId::Java, 1200, None);
    assert!(chunks.len() >= 2);
    for chunk in &chunks {
        let range = chunk.original_start..chunk.original_end;
        let (_, sub_chunks, _) = chunk_checked(&text, LanguageId::Java, 600, Some(range.clone()));
        if chunk_slice(&text, chunk).chars().count() > 600 {
            assert!(sub_chunks.len() >= 2, "{range:?} was not re-split");
        } else {
            assert_eq!(sub_chunks.len(), 1);
        }
        for sub_chunk in &sub_chunks {
            assert!(range.start <= sub_chunk.original_start);
            assert!(sub_chunk.original_end <= range.end);
            assert!(
                sub_chunk
                    .comment_ids
                    .iter()
                    .all(|id| chunk.comment_ids.contains(id))
            );
            assert_eq!(sub_chunk.outline, chunk.outline);
        }
        let ids: Vec<&String> = sub_chunks.iter().flat_map(|c| &c.comment_ids).collect();
        assert_eq!(ids, chunk.comment_ids.iter().collect::<Vec<_>>());
    }
}

#[test]
fn invalid_requests_are_refused() {
    let text = uniform_functions(2);
    let inventory = inventory(&text, LanguageId::Javascript, "input.js");
    assert!(chunk_file(&text, LanguageId::Javascript, &inventory, 0, None).is_err());
    let past_end = Some(0..text.len() + 1);
    assert!(chunk_file(&text, LanguageId::Javascript, &inventory, 10, past_end).is_err());
}
