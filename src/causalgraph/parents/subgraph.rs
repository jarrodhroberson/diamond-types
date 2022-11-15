use std::collections::BinaryHeap;
use smallvec::{SmallVec, smallvec};
use crate::causalgraph::parents::{Parents, ParentsEntryInternal};
use crate::{DTRange, Frontier, LV};
use crate::rle::RleVec;

fn push_light_dedup(f: &mut Frontier, new_item: LV) {
    if f.0.last() != Some(&new_item) {
        f.0.push(new_item);
    }
}

impl Parents {
    pub fn subgraph(&self, filter: &[DTRange], parents: &[LV]) -> Parents {
        let filter_iter = filter.iter().copied().rev();
        self.subgraph_raw(filter_iter, parents)
    }

    // The filter iterator must be reverse-sorted.
    pub(crate) fn subgraph_raw<I: Iterator<Item=DTRange>>(&self, mut rev_filter_iter: I, parents: &[LV]) -> Parents {
        #[derive(PartialOrd, Ord, Eq, PartialEq, Clone, Debug)]
        struct QueueEntry {
            target_parent: LV,
            children: SmallVec<[usize; 2]>,
        }

        let mut queue: BinaryHeap<QueueEntry> = BinaryHeap::new();
        let mut result_rev = Vec::<ParentsEntryInternal>::new();
        for p in parents {
            queue.push(QueueEntry {
                target_parent: *p,
                children: smallvec![]
            });
        }

        if let Some(mut filter) = rev_filter_iter.next() {
            'outer: while let Some(mut entry) = queue.pop() {
                // There's essentially 2 cases here:
                // 1. The entry is either inside a filtered item, or an earlier item in this txn
                //    is allowed by the filter.
                // 2. The filter doesn't allow the txn the entry is inside.

                let txn = self.0.find_packed(entry.target_parent);

                'txn_loop: loop {
                    // Could replace this with a call to filter_iter.find(..). Not sure if its
                    // cleaner - it would let me remove the loop label though.
                    while filter.start > entry.target_parent {
                        if let Some(f) = rev_filter_iter.next() { filter = f; }
                        else { break 'txn_loop; }
                    }

                    if filter.end <= txn.span.start {
                        break;
                    }

                    debug_assert!(txn.span.start < filter.end);
                    debug_assert!(entry.target_parent >= filter.start);
                    debug_assert!(entry.target_parent >= txn.span.start);

                    // Case 1. We'll add a new parents entry this loop iteration.

                    let p = entry.target_parent.min(filter.end - 1);
                    let idx_here = result_rev.len();

                    for idx in entry.children {
                        push_light_dedup(&mut result_rev[idx].parents, p);
                    }

                    let base = filter.start.max(txn.span.start);
                    // For simplicity, pull out anything that is within this txn *and* this filter.
                    while let Some(peeked_entry) = queue.peek() {
                        if peeked_entry.target_parent < base { break; }

                        let peeked_target = peeked_entry.target_parent.min(filter.end - 1);
                        for idx in &peeked_entry.children {
                            push_light_dedup(&mut result_rev[*idx].parents, peeked_target);
                        }

                        queue.pop();
                    }

                    result_rev.push(ParentsEntryInternal {
                        span: (base..p + 1).into(),
                        shadow: txn.shadow, // This is pessimistic.
                        parents: Frontier::default(), // Parents current unknown!
                    });

                    if filter.start > txn.span.start {
                        // The item we've just added has an (implicit) parent of base-1. We'll
                        // update entry and loop - which might either find more filter items
                        // within this txn, or it might bump us to the case below where the txn's
                        // items are added.
                        entry = QueueEntry {
                            target_parent: filter.start - 1,
                            children: smallvec![idx_here],
                        };
                    } else {
                        // filter.start <= txn.span.start. We're done with this txn.
                        if !txn.parents.is_empty() {
                            for p in txn.parents.iter() {
                                queue.push(QueueEntry {
                                    target_parent: *p,
                                    children: smallvec![idx_here],
                                })
                            }
                        }
                        continue 'outer;
                    }
                }

                // Case 2. The remainder of this txn is filtered out.
                //
                // We'll create new queue entries for all of this txn's parents.
                let mut child_idxs = entry.children;

                while let Some(peeked_entry) = queue.peek() {
                    if peeked_entry.target_parent < txn.span.start { break; } // Next item is out of this txn.

                    for i in peeked_entry.children.iter() {
                        if !child_idxs.contains(&i) { child_idxs.push(*i); }
                    }
                    queue.pop();
                }

                if txn.parents.0.len() == 1 {
                    // A silly little optimization to avoid an unnecessary clone() below.
                    queue.push(QueueEntry { target_parent: txn.parents.0[0], children: child_idxs })
                } else {
                    for p in txn.parents.iter() {
                        queue.push(QueueEntry {
                            target_parent: *p,
                            children: child_idxs.clone()
                        })
                    }
                }
            }
        }

        result_rev.reverse();

        for e in result_rev.iter_mut() {
            if e.parents.len() >= 2 {
                e.parents.0.reverse(); // Parents will always end up in reverse order.
                // I wish I didn't need to do this. At least I don't think it'll show up on the
                // performance profile.
                e.parents = self.find_dominators(&e.parents.0);
            }
        }

        Parents(RleVec(result_rev))
    }
}

#[cfg(test)]
mod test {
    use std::ops::Range;
    use smallvec::smallvec;
    use rle::intersect::{rle_intersect, rle_intersect_first};
    use rle::MergeableIterator;
    use crate::causalgraph::parents::{Parents, ParentsEntryInternal};
    use crate::{DTRange, Frontier, LV};
    use crate::rle::RleVec;

    fn fancy_parents() -> Parents {
        let p = Parents(RleVec(vec![
            ParentsEntryInternal { // 0-2
                span: (0..3).into(), shadow: 0,
                parents: Frontier::from_sorted(&[]),
            },
            ParentsEntryInternal { // 3-5
                span: (3..6).into(), shadow: 3,
                parents: Frontier::from_sorted(&[]),
            },
            ParentsEntryInternal { // 6-8
                span: (6..9).into(), shadow: 6,
                parents: Frontier::from_sorted(&[1, 4]),
            },
            ParentsEntryInternal { // 9-10
                span: (9..11).into(), shadow: 6,
                parents: Frontier::from_sorted(&[2, 8]),
            },
        ]));

        p.dbg_check(true);
        p
    }

    fn check_subgraph(p: &Parents, filter_r: &[Range<usize>], frontier: &[LV], expect_parents: &[&[LV]]) {
        let filter: Vec<DTRange> = filter_r.iter().map(|r| r.clone().into()).collect();
        let subgraph = p.subgraph(&filter, frontier);
        // dbg!(&subgraph);

        // The entries in the subgraph should be the same as the diff, passed through the filter.
        let mut diff = p.diff(&[], frontier).1;
        diff.reverse();

        // dbg!(&diff, &filter);
        let expected_items = rle_intersect_first(diff.iter().copied(), filter.iter().copied())
            .collect::<Vec<_>>();

        let actual_items = subgraph.0.iter()
            .map(|e| e.span)
            .merge_spans()
            .collect::<Vec<_>>();

        // dbg!(&expected_items, &actual_items);
        assert_eq!(expected_items, actual_items);

        for (entry, expect_parents) in subgraph.0.iter().zip(expect_parents.iter()) {
            assert_eq!(entry.parents.as_ref(), *expect_parents);
        }

        subgraph.dbg_check_subgraph(true);
    }

    #[test]
    fn test_subgraph() {
        let parents = fancy_parents();

        check_subgraph(&parents, &[0..11], &[5, 10], &[
            &[], &[], &[1, 4], &[2, 8],
        ]);
        check_subgraph(&parents, &[1..11], &[5, 10], &[
            &[], &[], &[1, 4], &[2, 8],
        ]);
        check_subgraph(&parents, &[5..6], &[5, 10], &[&[]]);
        check_subgraph(&parents, &[0..1, 10..11], &[5, 10], &[
            &[], &[0]
        ]);
        check_subgraph(&parents, &[0..11], &[10], &[
            &[], &[], &[1, 4], &[2, 8],
        ]);
        check_subgraph(&parents, &[0..11], &[5], &[
            &[]
        ]);
        check_subgraph(&parents, &[0..3, 9..11], &[10], &[
            &[], &[2]
        ]);
        check_subgraph(&parents, &[9..11], &[3], &[]);
        check_subgraph(&parents, &[5..6], &[9], &[]);
        check_subgraph(&parents, &[0..1, 2..3], &[2], &[&[], &[0]]);
        check_subgraph(&parents, &[0..1, 2..3], &[9], &[&[], &[0]]);

    }
}