use jumprope::JumpRope;
use rand::prelude::*;
use crate::AgentId;
use crate::list::{ListCRDT, OpSet};

pub fn random_str(len: usize, rng: &mut SmallRng) -> String {
    let mut str = String::new();
    let alphabet: Vec<char> = "abcdefghijklmnop_".chars().collect();
    for _ in 0..len {
        str.push(alphabet[rng.gen_range(0..alphabet.len())]);
    }
    str
}

fn make_random_change(doc: &mut ListCRDT, rope: Option<&mut JumpRope>, agent: AgentId, rng: &mut SmallRng) {
    let doc_len = doc.len();
    let insert_weight = if doc_len < 100 { 0.55 } else { 0.45 };
    if doc_len == 0 || rng.gen_bool(insert_weight) {
        // Insert something.
        let pos = rng.gen_range(0..=doc_len);
        let len: usize = rng.gen_range(1..2); // Ideally skew toward smaller inserts.
        // let len: usize = rng.gen_range(1..10); // Ideally skew toward smaller inserts.

        let content = random_str(len as usize, rng);
        // eprintln!("Inserting '{}' at position {}", content, pos);
        if let Some(rope) = rope {
            rope.insert(pos, content.as_str());
        }
        doc.local_insert(agent, pos, &content)
    } else {
        // Delete something
        let pos = rng.gen_range(0..doc_len);
        // println!("range {}", u32::min(10, doc_len - pos));
        let span = rng.gen_range(1..=usize::min(10, doc_len - pos));
        // dbg!(&state.marker_tree, pos, len);
        // eprintln!("deleting {} at position {}", span, pos);
        if let Some(rope) = rope {
            rope.remove(pos..pos + span);
        }
        doc.local_delete(agent, pos, span)
    }
    // dbg!(&doc.markers);
    // doc.check(true);
    doc.check(false);
}

#[test]
fn random_single_document() {
    let mut rng = SmallRng::seed_from_u64(3);
    let mut doc = ListCRDT::new();

    let agent = doc.get_or_create_agent_id("seph");
    let mut expected_content = JumpRope::new();

    for _i in 0..1000 {
        // eprintln!("i {}", _i);
        // doc.debug_print_stuff();
        make_random_change(&mut doc, Some(&mut expected_content), agent, &mut rng);
        assert_eq!(doc.branch.content, expected_content);
    }

    doc.check(true);
}

// fn merge_fuzz(seed: u64) {
//     let mut rng = SmallRng::seed_from_u64(seed);
//     let opset = OpSet::new();
//
//     let mut docs = [ListCRDT::new(), ListCRDT::new(), ListCRDT::new()];
//
//     // Each document will have a different local agent ID. I'm cheating here - just making agent
//     // 0 for all of them.
//     for (i, doc) in docs.iter_mut().enumerate() {
//         doc.get_or_create_agent_id(format!("agent {}", i).as_str());
//     }
//
//     for _i in 0..300 {
//         // Generate some operations
//         for _j in 0..5 {
//             let doc_idx = rng.gen_range(0..docs.len());
//             let doc = &mut docs[doc_idx];
//
//             make_random_change(doc, None, 0, &mut rng);
//         }
//
//         // Then merge 2 documents at random
//         let a_idx = rng.gen_range(0..docs.len());
//         let b_idx = rng.gen_range(0..docs.len());
//
//         if a_idx != b_idx {
//             // println!("Merging {} and {}", a_idx, b_idx);
//             // Oh god this is awful. I can't take mutable references to two array items.
//             let (a_idx, b_idx) = if a_idx < b_idx { (a_idx, b_idx) } else { (b_idx, a_idx) };
//             // a<b.
//             let (start, end) = docs[..].split_at_mut(b_idx);
//             let a = &mut start[a_idx];
//             let b = &mut end[0];
//
//             // dbg!(&a.text_content, &b.text_content);
//             // dbg!(&a.content_tree, &b.content_tree);
//
//             // Our frontier should contain everything in the document.
//             let frontier = a.get_frontier_as_localtime().to_vec();
//             let mid_order = a.get_next_time();
//             if mid_order > 0 {
//                 for _k in 0..10 {
//                     let order = rng.gen_range(0..mid_order);
//                     assert!(a.branch_contains_order(&frontier, order));
//                 }
//             }
//
//             // println!("{} -> {}", a_idx, b_idx);
//             a.replicate_into(b);
//             // println!("{} -> {}", b_idx, a_idx);
//             b.replicate_into(a);
//
//             a.check(false);
//             b.check(false);
//
//             // But our old frontier doesn't contain any of the new items.
//             if a.get_next_time() > mid_order {
//                 for _k in 0..10 {
//                     let order = rng.gen_range(mid_order..a.get_next_time());
//                     assert!(!a.branch_contains_order(&frontier, order));
//                 }
//             }
//
//             if a != b {
//                 println!("Docs {} and {} after {} iterations:", a_idx, b_idx, _i);
//                 // dbg!(&a);
//                 // dbg!(&b);
//                 panic!("Documents do not match");
//             }
//         }
//
//         for doc in &docs {
//             doc.check(false);
//         }
//     }
//
//     for doc in &docs {
//         doc.check(true);
//     }
// }