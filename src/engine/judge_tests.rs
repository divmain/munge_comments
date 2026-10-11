use super::*;

use crate::engine::heuristics::{FULL_SET, REDUCED_SET, RESTRICTED_SET};

fn packet(id: &str, original: &str, allowed: &[Decision]) -> Packet {
    Packet {
        id: id.to_owned(),
        anchor_snippet: "run();".to_owned(),
        original: original.to_owned(),
        regenerated: None,
        difficulty: None,
        difficulty_rationale: None,
        would_you_comment_here: None,
        allowed: allowed.to_vec(),
    }
}

fn cost(packet: &Packet) -> usize {
    estimate_tokens(&render_packet(packet, LanguageId::Javascript))
}

fn ids(batches: &[Vec<Packet>]) -> Vec<Vec<&str>> {
    batches
        .iter()
        .map(|batch| batch.iter().map(|p| p.id.as_str()).collect())
        .collect()
}

#[test]
fn a_token_is_a_rounded_up_quarter_of_the_chars() {
    assert_eq!(estimate_tokens(""), 0);
    assert_eq!(estimate_tokens("abcd"), 1);
    assert_eq!(estimate_tokens("abcde"), 2);
    // Unicode scalar values, not bytes.
    assert_eq!(estimate_tokens("ééé"), 1);
}

#[test]
fn batches_pack_greedily_and_ignore_allowed_sets() {
    let packets = vec![
        packet("C1", "// one", &FULL_SET),
        packet("C2", "// two", &RESTRICTED_SET),
        packet("C3", "// three", &REDUCED_SET),
        packet("C4", "// four", &FULL_SET),
    ];
    let costs: Vec<usize> = packets.iter().map(cost).collect();
    // Room for everything: one batch, whatever the sets.
    let all = batch_packets(packets.clone(), costs.iter().sum(), LanguageId::Javascript);
    assert_eq!(ids(&all), [["C1", "C2", "C3", "C4"]]);
    // One token short: the last packet spills.
    let spill = batch_packets(
        packets.clone(),
        costs.iter().sum::<usize>() - 1,
        LanguageId::Javascript,
    );
    assert_eq!(ids(&spill), [vec!["C1", "C2", "C3"], vec!["C4"]]);
    // Room for C1 + C2 exactly.
    let pairs = batch_packets(packets.clone(), costs[0] + costs[1], LanguageId::Javascript);
    assert_eq!(ids(&pairs)[0], ["C1", "C2"]);
    // No room at all: every packet alone, none dropped.
    let alone = batch_packets(packets, 0, LanguageId::Javascript);
    assert_eq!(ids(&alone), [["C1"], ["C2"], ["C3"], ["C4"]]);
}

#[test]
fn no_packets_make_no_batches() {
    assert!(batch_packets(Vec::new(), 100, LanguageId::Javascript).is_empty());
}
