#![allow(dead_code)] // TODO: turn this off and clean up before releasing.

pub use alloc::*;
pub use common::{AgentId};

pub mod list;

mod common;
mod range_tree;
mod splitable_span;
mod alloc;
mod order;
mod rle;
mod unicount;
mod merge_iter;

#[cfg(test)]
mod tests {
    // As per smartstring's documentation.
    #[test]
    fn validate_smartstring() {
        smartstring::validate();
    }

    #[test]
    fn foo() {
        use smartstring::alias::{String as SmartString};

        dbg!(std::mem::size_of::<SmartString>());
        dbg!(std::mem::size_of::<String>());
    }
}

#[cfg(test)]
pub mod fuzz_helpers {
    use rand::prelude::SmallRng;
    use rand::Rng;
    use crate::list::ListCRDT;
    use ropey::Rope;
    use crate::{AgentId};

    pub fn random_str(len: usize, rng: &mut SmallRng) -> String {
        let mut str = String::new();
        let alphabet: Vec<char> = "abcdefghijklmnop_".chars().collect();
        for _ in 0..len {
            str.push(alphabet[rng.gen_range(0..alphabet.len())]);
        }
        str
    }

    pub fn make_random_change(doc: &mut ListCRDT, rope: Option<&mut Rope>, agent: AgentId, rng: &mut SmallRng) -> usize {
        let doc_len = doc.len();
        let insert_weight = if doc_len < 100 { 0.6 } else { 0.4 };
        let op_len = if doc_len == 0 || rng.gen_bool(insert_weight) {
            // Insert something.
            let pos = rng.gen_range(0..=doc_len);
            let len: usize = rng.gen_range(1..2); // Ideally skew toward smaller inserts.
            // let len: usize = rng.gen_range(1..10); // Ideally skew toward smaller inserts.

            let content = random_str(len as usize, rng);
            // println!("Inserting '{}' at position {}", content, pos);
            if let Some(rope) = rope {
                rope.insert(pos, content.as_str());
            }

            doc.local_insert(agent, pos, content.as_str());
            len as usize
        } else {
            // Delete something
            let pos = rng.gen_range(0..doc_len);
            // println!("range {}", u32::min(10, doc_len - pos));
            let span = rng.gen_range(1..=usize::min(10, doc_len - pos));
            // dbg!(&state.marker_tree, pos, len);
            // println!("deleting {} at position {}", span, pos);
            if let Some(rope) = rope {
                rope.remove(pos..pos + span);
            }
            doc.local_delete(agent, pos, span);
            span
        };
        // dbg!(&doc.markers);
        doc.check(false);
        op_len
    }
}