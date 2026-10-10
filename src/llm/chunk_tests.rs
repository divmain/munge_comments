use super::*;
use crate::comments::inventory::inventory_file;
use crate::comments::types::FileRef;

fn ctx_for<'a>(text: &'a str, inventory: &'a Inventory) -> Ctx<'a> {
    Ctx {
        text,
        adapter: adapter_for(LanguageId::Javascript),
        comments: &inventory.comments,
        max_chars: 1,
    }
}

fn js_inventory(text: &str) -> Inventory {
    inventory_file(
        text,
        FileRef {
            language: LanguageId::Javascript,
            file_name: "input.js".into(),
        },
    )
    .expect("inventories")
}

#[test]
fn cut_after_follows_trailing_comments_across_the_lines_they_reach() {
    // A trailing block comment ending on the next line, followed there by
    // another trailing comment: the cut is after both.
    let text = "a(); /* one\n two */ // three\nb();\n";
    let inventory = js_inventory(text);
    let ctx = ctx_for(text, &inventory);
    let cut = ctx.cut_after(text.find(';').unwrap() + 1);
    assert_eq!(&text[cut..], "b();\n");
    assert!(ctx.cuttable(cut));
}

#[test]
fn cut_after_stops_at_the_end_of_the_text() {
    let text = "a(); // last";
    let inventory = js_inventory(text);
    let ctx = ctx_for(text, &inventory);
    assert_eq!(ctx.cut_after(4), text.len());
}

#[test]
fn cuttable_refuses_offsets_inside_a_record() {
    let text = "/* a\nb */\nc();\n";
    let inventory = js_inventory(text);
    let ctx = ctx_for(text, &inventory);
    assert!(ctx.cuttable(0));
    assert!(!ctx.cuttable(5));
    assert!(ctx.cuttable(10));
}

fn whole_file_chunk(text: &str, inventory: &Inventory) -> Vec<Chunk> {
    chunk_file(text, LanguageId::Javascript, inventory, 120_000, None)
        .expect("chunks")
        .0
}

#[test]
fn build_chunk_texts_rejects_redact_mode_on_a_placeholder_collision() {
    let text = "// why\nconst s = \"⟦C1⟧\";\n";
    let inventory = js_inventory(text);
    let chunks = whole_file_chunk(text, &inventory);
    let adapter = adapter_for(LanguageId::Javascript);
    let err = build_chunk_texts(text, &inventory, adapter, &chunks, Mode::Redact, None)
        .expect_err("redact mode on a collision is a caller bug");
    assert!(matches!(err, Error::Invariant(_)), "{err:?}");
    // The file's effective mode, remove, works.
    let texts = build_chunk_texts(text, &inventory, adapter, &chunks, Mode::Remove, None)
        .expect("remove mode");
    assert_eq!(texts[0].prompt_text, "const s = \"⟦C1⟧\";\n");
}

#[test]
fn build_chunk_texts_rejects_a_chunk_range_off_char_boundaries() {
    let text = "// é\nf();\n";
    let inventory = js_inventory(text);
    let mut chunks = whole_file_chunk(text, &inventory);
    chunks[0].original_end = text.find('é').unwrap() + 1;
    let adapter = adapter_for(LanguageId::Javascript);
    for mode in [Mode::Redact, Mode::Remove] {
        let err = build_chunk_texts(text, &inventory, adapter, &chunks, mode, None)
            .expect_err("a mid-char range is a caller bug");
        assert!(matches!(err, Error::Invariant(_)), "{err:?}");
    }
    chunks[0].original_end = text.len() + 1;
    let err = build_chunk_texts(text, &inventory, adapter, &chunks, Mode::Remove, None)
        .expect_err("a range past the end is a caller bug");
    assert!(matches!(err, Error::Invariant(_)), "{err:?}");
}

#[test]
fn cap_cuts_long_lines_with_an_ellipsis() {
    let short = "x".repeat(OUTLINE_LINE_MAX_CHARS);
    assert_eq!(cap(&short), short);
    let long = "é".repeat(OUTLINE_LINE_MAX_CHARS + 5);
    let capped = cap(&long);
    assert_eq!(capped.chars().count(), OUTLINE_LINE_MAX_CHARS + 1);
    assert!(capped.ends_with('…'));
}
