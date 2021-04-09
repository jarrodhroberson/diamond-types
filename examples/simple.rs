use text_crdt_rust::CRDTState;
use rand::{Rng, SeedableRng};
use rand::rngs::SmallRng;


fn random_str(len: usize, rng: &mut SmallRng) -> String {
    let mut str = String::new();
    let alphabet: Vec<char> = "abcdefghijklmnop ".chars().collect();
    for _ in 0..len {
        str.push(alphabet[rng.gen_range(0..alphabet.len())]);
    }
    str
}

fn random_inserts_deletes() {
    let mut doc_len = 0;
    let mut state = CRDTState::new();
    state.get_or_create_client_id("seph"); // Create client id 0.

    // Stable between runs for reproducing bugs.
    let mut rng = SmallRng::seed_from_u64(1234);

    for i in 0..1000000 {
        if i % 10000 == 0 {
            println!("i {} doc len {}", i, doc_len);
        }

        let insert_weight = if doc_len < 1000 { 0.55 } else { 0.45 };
        if doc_len == 0 || rng.gen_bool(insert_weight) {
            // Insert something.
            let pos = rng.gen_range(0..=doc_len);
            let len: usize = rng.gen_range(1..10); // Ideally skew toward smaller inserts.
            state.insert(0, pos, random_str(len as usize, &mut rng).as_str());
            doc_len += len;
        } else {
            // Delete something
            let pos = rng.gen_range(0..doc_len);
            // println!("range {}", u32::min(10, doc_len - pos));
            let len = rng.gen_range(1..=usize::min(10, doc_len - pos));
            state.delete(0, pos, len);
            doc_len -= len;
        }

        // Calling check gets slow as the document grows. There's a tradeoff here between
        // iterations and check() calls.
        // if i % 1000 == 0 { state.check(); }
        assert_eq!(state.len(), doc_len as usize);
    }
}

fn main() {
    random_inserts_deletes();
    return;
    let mut state = CRDTState::new();

    state.insert_name("fred", 0, "a");
    state.insert_name("george", 1, "bC");

    state.insert_name("fred", 3, "D");
    state.insert_name("george", 4, "EFgh");

    // println!("tree {:#?}", state.marker_tree);
    // Delete CDEF
    let _result = state.delete_name("amanda", 2, 4);
    // eprintln!("delete result {:#?}", result);
    assert_eq!(state.len(), 4);
}