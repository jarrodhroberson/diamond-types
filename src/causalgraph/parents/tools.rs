//! This file contains tools to manage the document as a time dag. Specifically, tools to tell us
//! about branches, find diffs and move between branches.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use smallvec::{smallvec, SmallVec};
use rle::{AppendRle, SplitableSpan};

use crate::frontier::{advance_frontier_by, debug_assert_frontier_sorted, frontier_is_sorted};
use crate::causalgraph::parents::Parents;
use crate::causalgraph::parents::tools::DiffFlag::*;
use crate::dtrange::DTRange;
use crate::{LocalVersion, ROOT_TIME, Time};
use crate::causalgraph::parents::scope::ScopedParents;

#[cfg(feature = "serde")]
use serde_crate::Serialize;

// Diff function needs to tag each entry in the queue based on whether its part of a's history or
// b's history or both, and do so without changing the sort order for the heap.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize), serde(crate = "serde_crate"))]
pub(crate) enum DiffFlag { OnlyA, OnlyB, Shared }

impl Parents {
    fn shadow_of(&self, time: Time) -> Time {
        if time == ROOT_TIME {
            ROOT_TIME
        } else {
            self.entries.find(time).unwrap().shadow
        }
    }

    /// Does the frontier `[a]` contain `[b]` as a direct ancestor according to its shadow?
    fn txn_shadow_contains(&self, a: Time, b: Time) -> bool {
        // wrapping_add(1) so we compute ROOT correctly.
        let a_1 = a.wrapping_add(1);
        let b_1 = b.wrapping_add(1);
        a_1 == b_1 || (a_1 > b_1 && self.shadow_of(a).wrapping_add(1) <= b_1)
    }

    /// This is similar to txn_shadow_contains, but it also checks that a doesn't have any other
    /// ancestors which aren't included in b's history. Eg:
    ///
    /// ```text
    /// 1
    /// | 2
    /// \ /
    ///  3
    /// ```
    ///
    /// `txn_shadow_contains(3, 2)` is true, but `is_direct_descendant(3, 2)` is false.
    ///
    /// See `diff_shadow_bubble` test below for an example.
    pub(crate) fn is_direct_descendant_coarse(&self, a: Time, b: Time) -> bool {
        // This is a bit more strict than we technically need, but its fast for short circuit
        // evaluation.
        a == b
            || (b == ROOT_TIME && self.txn_shadow_contains(a, ROOT_TIME))
            || (a != ROOT_TIME && a > b && self.entries.find(a).unwrap().contains(b))
    }

    pub(crate) fn version_cmp(&self, v1: Time, v2: Time) -> Option<Ordering> {
        match v1.cmp(&v2) {
            Ordering::Equal => Some(Ordering::Equal),
            Ordering::Less => {
                if self.version_contains_time(&[v2], v1) {
                    Some(Ordering::Less)
                } else {
                    None
                }
            },
            Ordering::Greater => {
                if self.version_contains_time(&[v1], v2) {
                    Some(Ordering::Greater)
                } else {
                    None
                }
            },
        }
    }

    /// Calculates whether the specified version contains (dominates) the specified time.
    pub(crate) fn version_contains_time(&self, frontier: &[Time], target: Time) -> bool {
        if target == ROOT_TIME || frontier.contains(&target) { return true; }
        if frontier.is_empty() { return false; }

        // Fast path. This causes extra calls to find_packed(), but you usually have a branch with
        // a shadow less than target. Usually the root document. And in that case this codepath
        // avoids the allocation from BinaryHeap.
        for &o in frontier {
            if o > target {
                let txn = self.entries.find(o).unwrap();
                if txn.shadow_contains(target) { return true; }
            }
        }

        // So I don't *need* to use a priority queue here. The options are:
        // 1. Use a priority queue, scanning from the highest to lowest orders
        // 2. Use a simple list and do DFS, potentially scanning some items twice
        // 3. Use a simple list and do DFS, with another structure to mark which items we've
        //    visited.
        //
        // Honestly any approach should be obnoxiously fast in any real editing session anyway.

        // TODO: Consider moving queue into a threadlocal variable so we don't need to reallocate it
        // with each call to branch_contains_order.
        let mut queue = BinaryHeap::new();

        // This code could be written to use parent_indexes but its a bit tricky, as an index isn't
        // enough specificity. We'd need the parent and the parent_index. Eh...
        for &o in frontier {
            debug_assert_ne!(o, target);
            if o > target { queue.push(o); }
        }

        while let Some(order) = queue.pop() {
            debug_assert!(order > target);
            // dbg!((order, &queue));

            // TODO: Skip these calls to find() using parent_index.
            let entry = self.entries.find(order).unwrap();
            if entry.shadow_contains(target) { return true; }

            while let Some(&next_time) = queue.peek() {
                if next_time >= entry.span.start {
                    // dbg!(next_order);
                    queue.pop();
                } else { break; }
            }

            // dbg!(order);
            for &p in &entry.parents {
                #[allow(clippy::comparison_chain)]
                if p == target { return true; }
                else if p > target { queue.push(p); }
                // If p < target, it can't be a child of target. So we can discard it.
            }
        }

        false
    }
}

pub(crate) type DiffResult = (SmallVec<[DTRange; 4]>, SmallVec<[DTRange; 4]>);

impl Parents {
    /// Returns (spans only in a, spans only in b). Spans are in reverse (descending) order.
    ///
    /// Also find which operation is the greatest common ancestor.
    pub(crate) fn diff(&self, a: &[Time], b: &[Time]) -> DiffResult {
        // First some simple short circuit checks to avoid needless work in common cases.
        // Note most of the time this method is called, one of these early short circuit cases will
        // fire.
        if a == b { return (smallvec![], smallvec![]); }

        if a.len() == 1 && b.len() == 1 {
            // Check if either operation naively dominates the other. We could do this for more
            // cases, but we may as well use the code below instead.
            let a = a[0];
            let b = b[0];
            if a == b { return (smallvec![], smallvec![]); }

            if self.is_direct_descendant_coarse(a, b) {
                // a >= b.
                return (smallvec![(b.wrapping_add(1)..a.wrapping_add(1)).into()], smallvec![]);
                // return (smallvec![(b.wrapping_add(1)..a.wrapping_add(1)).into()], smallvec![], b);
            }
            if self.is_direct_descendant_coarse(b, a) {
                // b >= a.
                return (smallvec![], smallvec![(a.wrapping_add(1)..b.wrapping_add(1)).into()]);
                // return (smallvec![], smallvec![(a.wrapping_add(1)..b.wrapping_add(1)).into()], a);
            }
        }

        // Otherwise fall through to the slow version.
        self.diff_slow(a, b)
    }

    fn diff_slow(&self, a: &[Time], b: &[Time]) -> DiffResult {
        let mut only_a = smallvec![];
        let mut only_b = smallvec![];

        // marks range [ord_start..ord_end] *inclusive* with flag in our output.
        let mark_run = |ord_start, ord_end, flag: DiffFlag| {
            let target = match flag {
                OnlyA => { &mut only_a }
                OnlyB => { &mut only_b }
                Shared => { return; }
            };
            // dbg!((ord_start, ord_end));

            target.push_reversed_rle(DTRange::new(ord_start, ord_end + 1));
        };

        self.diff_slow_internal(a, b, mark_run);
        (only_a, only_b)
    }

    fn diff_slow_internal<F>(&self, a: &[Time], b: &[Time], mut mark_run: F)
        where F: FnMut(Time, Time, DiffFlag) {
        // Sorted highest to lowest.
        let mut queue: BinaryHeap<(Time, DiffFlag)> = BinaryHeap::new();
        for a_ord in a {
            queue.push((*a_ord, OnlyA));
        }
        for b_ord in b {
            queue.push((*b_ord, OnlyB));
        }

        let mut num_shared_entries = 0;

        while let Some((mut ord, mut flag)) = queue.pop() {
            if flag == Shared { num_shared_entries -= 1; }

            // dbg!((ord, flag));
            while let Some((peek_ord, peek_flag)) = queue.peek() {
                if *peek_ord != ord { break; } // Normal case.
                else {
                    // 3 cases if peek_flag != flag. We set flag = Shared in all cases.
                    if *peek_flag != flag { flag = Shared; }
                    if *peek_flag == Shared { num_shared_entries -= 1; }
                    queue.pop();
                }
            }

            // Grab the txn containing ord. This will usually be at prev_txn_idx - 1.
            // TODO: Remove usually redundant binary search

            let containing_txn = self.entries.find_packed(ord);

            // There's essentially 2 cases here:
            // 1. This item and the first item in the queue are part of the same txn. Mark down to
            //    the queue head and continue.
            // 2. Its not. Mark the whole txn and queue parents.

            // 1:
            while let Some((peek_ord, peek_flag)) = queue.peek() {
                // dbg!((peek_ord, peek_flag));
                if *peek_ord < containing_txn.span.start { break; } else {
                    if *peek_flag != flag {
                        // Mark from peek_ord..=ord and continue.
                        // Note we'll mark this whole txn from ord, but we might do so with
                        // different flags.
                        mark_run(*peek_ord + 1, ord, flag);
                        ord = *peek_ord;
                        // offset -= ord - peek_ord;
                        flag = Shared;
                    }
                    if *peek_flag == Shared { num_shared_entries -= 1; }
                    queue.pop();
                }
            }

            // 2: Mark the rest of the txn in our current color and repeat. Note we still need to
            // mark the run even if ord == containing_txn.order because the spans are inclusive.
            mark_run(containing_txn.span.start, ord, flag);

            for p in containing_txn.parents.iter() {
                queue.push((*p, flag));
                if flag == Shared { num_shared_entries += 1; }
            }

            // If there's only shared entries left, abort.
            if queue.len() == num_shared_entries { break; }
        }
    }

    /// Given 2 versions, return a version which contains all the operations in both.
    ///
    /// TODO: This needs unit tests.
    pub fn version_union(&self, a: &[Time], b: &[Time]) -> LocalVersion {
        // This method could be written to use diff_internal's closure. That would be faster, but it
        // would probably add a fair bit of code size from monomorphizing for something thats just a
        // utility method. So eh.
        let (only_a, only_b) = self.diff(a, b);
        if only_a.is_empty() {
            b.into()
        } else if only_b.is_empty() {
            a.into()
        } else {
            let mut result = a.into();
            for span in only_b {
                advance_frontier_by(&mut result, self, span);
            }
            result
        }
    }

    // *** Conflicts! ***

    fn find_conflicting_slow<V>(&self, a: &[Time], b: &[Time], mut visit: V) -> LocalVersion
    where V: FnMut(DTRange, DiffFlag) {
        // dbg!(a, b);

        // Sorted highest to lowest (so we get the highest item first).
        #[derive(Debug, PartialEq, Eq, Clone)]
        struct TimePoint {
            last: Time,
            // For merges this is the highest time.
            // TODO: Compare performance here with actually using a vec.
            merged_with: SmallVec<[Time; 1]>, // Always sorted. Usually empty.
        }

        impl Ord for TimePoint {
            #[inline(always)]
            fn cmp(&self, other: &Self) -> Ordering {
                // wrapping_add(1) converts ROOT into 0 for proper comparisons.
                // TODO: Consider pulling this out
                self.last.wrapping_add(1).cmp(&other.last.wrapping_add(1))
                    .then_with(|| other.merged_with.is_empty().cmp(&self.merged_with.is_empty()))
            }
        }

        impl PartialOrd for TimePoint {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }

        impl From<Time> for TimePoint {
            fn from(time: Time) -> Self {
                Self { last: time, merged_with: Default::default() }
            }
        }

        impl From<&[Time]> for TimePoint {
            fn from(version: &[Time]) -> Self {
                debug_assert!(frontier_is_sorted(version));

                let mut result = Self {
                    // Bleh.
                    last: *version.last().unwrap_or(&ROOT_TIME),
                    merged_with: smallvec![]
                };

                if version.len() > 1 {
                    // TODO: Clean this up. I'm sure there's nicer constructions
                    for t in &version[..version.len() - 1] {
                        result.merged_with.push(*t);
                    }
                }

                result
            }
        }

        // The heap is sorted such that we pull the highest items first.
        let mut queue: BinaryHeap<(TimePoint, DiffFlag)> = BinaryHeap::new();
        queue.push((a.into(), OnlyA));
        queue.push((b.into(), OnlyB));

        // Loop until we've collapsed the graph down to a single element.
        let frontier: LocalVersion = 'outer: loop {
            let (time, mut flag) = queue.pop().unwrap();
            let t = time.last;
            // dbg!((&time, flag));

            if t == ROOT_TIME { break smallvec![]; }

            // Discard duplicate entries.

            // I could write this with an inner loop and a match statement, but this is shorter and
            // more readable. The optimizer has to earn its keep somehow.
            // while queue.peek() == Some(&time) { queue.pop(); }
            while let Some((peek_time, peek_flag)) = queue.peek() {
                if *peek_time == time {
                    // Logic adapted from diff().
                    if *peek_flag != flag { flag = Shared; }
                    queue.pop();
                } else { break; }
            }

            if queue.is_empty() {
                // In this order because time.last > time.merged_with.
                let mut frontier: LocalVersion = time.merged_with.as_slice().into();
                // branch.extend(time.merged_with.into_iter());
                frontier.push(t);
                break frontier;
            }

            // If this node is a merger, shatter it.
            if !time.merged_with.is_empty() {
                // We'll deal with time.last directly this loop iteration.
                for t in time.merged_with {
                    queue.push((t.into(), flag));
                }
            }

            let containing_txn = self.entries.find(t).unwrap();

            // I want an inclusive iterator :p
            let mut range = DTRange { start: containing_txn.span.start, end: t + 1 };

            // Consume all other changes within this txn.
            loop {
                if let Some((peek_time, _peek_flag)) = queue.peek() {
                    // println!("peek {:?}", &peek_time);
                    // Might be simpler to use containing_txn.contains(peek_time.last).
                    if peek_time.last != ROOT_TIME && peek_time.last >= containing_txn.span.start {
                        // The next item is within this txn. Consume it.
                        // dbg!((&peek_time, peek_flag));
                        let (time, next_flag) = queue.pop().unwrap();

                        // Only emit inner items when they aren't duplicates.
                        if time.last + 1 < range.end {
                            // +1 because we don't want to include the actual merge point in the returned set.
                            let offset = time.last + 1 - containing_txn.span.start;
                            debug_assert!(offset > 0);
                            let rem = range.truncate(offset);

                            visit(rem, flag);
                        }
                        // result.push_reversed_rle(rem);

                        if next_flag != flag { flag = Shared; }

                        if !time.merged_with.is_empty() {
                            // We've run into a merged item which uses part of this entry.
                            // We've already pushed the necessary span to the result. Do the
                            // normal merge & shatter logic with this item next.
                            // let time = queue.pop().unwrap();
                            for t in time.merged_with {
                                queue.push((t.into(), next_flag));
                            }
                        }
                    } else {
                        // Emit the remainder of this txn.
                        visit(range, flag);
                        // result.push_reversed_rle(range);

                        // If this entry has multiple parents, we'll push a merge here then
                        // immediately pop it. This is so we stop at the merge point.
                        queue.push((containing_txn.parents.as_slice().into(), flag));
                        break;
                    }
                } else {
                    // println!("XXXX {:?}", &range.last());
                    break 'outer smallvec![range.last()];
                }
            }
        };

        frontier
    }

    /// This method is used to find the operation ranges we need to look at that might be concurrent
    /// with incoming edits.
    ///
    /// We need to track all spans back to a *single point in time*. This point in time is usually
    /// a single localtime, but it might be the result of a merge of multiple edits.
    ///
    /// I'm assuming b is a parent of a, but it should all work if thats not the case.
    pub(crate) fn find_conflicting<V>(&self, a: &[Time], b: &[Time], mut visit: V) -> LocalVersion
        where V: FnMut(DTRange, DiffFlag) {

        // First some simple short circuit checks to avoid needless work in common cases.
        // Note most of the time this method is called, one of these early short circuit cases will
        // fire.
        if a == b {
            return a.into();
        }

        if a.len() <= 1 && b.len() <= 1 {
            // Check if either operation naively dominates the other. We could do this for more
            // cases, but we may as well use the code below instead.
            let a = *a.get(0).unwrap_or(&ROOT_TIME); // This is a bit gross.
            let b = *b.get(0).unwrap_or(&ROOT_TIME);

            if self.is_direct_descendant_coarse(a, b) {
                // a >= b.
                visit((b.wrapping_add(1)..a.wrapping_add(1)).into(), OnlyA);
                return smallvec![b];
            }
            if self.is_direct_descendant_coarse(b, a) {
                // b >= a.
                visit((a.wrapping_add(1)..b.wrapping_add(1)).into(), OnlyB);
                return smallvec![a];
            }
        }

        // Otherwise fall through to the slow version.
        self.find_conflicting_slow(a, b, visit)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ConflictZone {
    pub(crate) common_ancestor: LocalVersion,
    pub(crate) spans: SmallVec<[DTRange; 4]>,
}

impl Parents {
    // Turns out I'm not finding this variant useful. Might be worth discarding it?
    #[allow(unused)]
    pub(crate) fn find_conflicting_simple(&self, a: &[Time], b: &[Time]) -> ConflictZone {
        let mut spans = smallvec![];
        let common_ancestor = self.find_conflicting(a, b, |span, _flag| {
            spans.push_reversed_rle(span);
        });

        ConflictZone { common_ancestor, spans }
    }

    pub(crate) fn version_in_scope(&self, version: &[Time], info: &ScopedParents) -> Option<LocalVersion> {
        // If v == creation time, its a bit hacky but I still consider that a valid version, because
        // the CRDT has a value then (the default value for the CRDT).
        debug_assert_frontier_sorted(version);

        let highest_time = if let Some(&t) = version.last() {
            t
        } else {
            // The root item has a creation time at the root time. But nothing else exists then.
            return if info.created_at == ROOT_TIME {
                Some(smallvec![])
            } else {
                None
            }
        };

        // let info = &oplog.items[item];
        if info.created_at != ROOT_TIME && highest_time < info.created_at {
            // If the version exists entirely before this root was created, there is no common
            // ancestor.
            return None;
        }

        if version.len() == 1 {
            if let Some(last) = info.owned_times.last() {
                let last_time = last.last();

                // Fast path. If the last operation in the root is a parent of v, we're done.
                if self.is_direct_descendant_coarse(highest_time, last_time) {
                    return Some(smallvec![last_time]);
                }
            }

            if info.owned_times.find_index(highest_time).is_ok() {
                // Another fast path. The requested version is already in the operation.
                return Some(smallvec![highest_time]);
            }

            // TODO: Should we have more fast paths here?
        }

        // Slow path. We'll trace back through time until we land entirely in the root.
        let mut result = smallvec![];

        // I'm using DiffFlag here, but only the OnlyA and Shared values out of it.
        let mut queue: BinaryHeap<(Time, DiffFlag)> = BinaryHeap::new();

        for &t in version {
            // Append children so long as they aren't earlier than the item's ctime.
            if info.created_at == ROOT_TIME || t >= info.created_at {
                queue.push((t, OnlyA));
            }
        }

        let mut num_shared_entries = 0;

        while let Some((time, mut flag)) = queue.pop() {
            if flag == Shared { num_shared_entries -= 1; }
            debug_assert_ne!(flag, OnlyB);

            // dbg!((ord, flag));
            while let Some((peek_time, peek_flag)) = queue.peek() {
                debug_assert_ne!(*peek_flag, OnlyB);

                if *peek_time != time { break; } // Normal case.
                else {
                    // 3 cases if peek_flag != flag. We set flag = Shared in all cases.
                    // if *peek_flag != flag { flag = Shared; }
                    if flag == OnlyA && *peek_flag == Shared { flag = Shared; }
                    if *peek_flag == Shared { num_shared_entries -= 1; }
                    queue.pop();
                }
            }

            if flag == OnlyA && info.owned_times.find_index(time).is_ok() {
                // The time we've picked is in the CRDT we're looking for. Woohoo!
                result.push(time);
                flag = Shared;
            }

            if flag == Shared && queue.len() == num_shared_entries { break; } // No expand necessary.

            // Ok, we need to expand the item based on its parents. The tricky thing here is what
            // we can skip safely.
            let containing_txn = self.entries.find_packed(time);

            let min_safe_base = if flag == Shared {
                0
            } else {
                // TODO: Reuse binary search from above.
                let r = info.owned_times.find_sparse(time).0;
                r.unwrap_err().start
            };
            let base = min_safe_base.max(containing_txn.span.start);

            // Eat everything >= base in queue.
            while let Some((peek_time, peek_flag)) = queue.peek() {
                // dbg!((peek_ord, peek_flag));
                if *peek_time < base { break; } else {
                    // if *peek_flag != flag {
                    if flag == OnlyA && *peek_flag == Shared {
                        flag = Shared;
                    }
                    if *peek_flag == Shared { num_shared_entries -= 1; }
                    queue.pop();
                }
            }

            containing_txn.with_parents(base, |parents| {
                for &p in parents {
                    queue.push((p, flag));
                    if flag == Shared { num_shared_entries += 1; }
                }
            });

            // If there's only shared entries left, stop.
            if queue.len() == num_shared_entries { break; }
        }

        result.reverse();
        debug_assert_frontier_sorted(&result);
        Some(result)
    }
}

#[cfg(test)]
pub mod test {
    use std::ops::Range;
    use smallvec::smallvec;
    use rle::{AppendRle, MergableSpan};

    use crate::causalgraph::parents::*;
    use crate::causalgraph::parents::tools::DiffFlag::*;
    use crate::dtrange::DTRange;
    use crate::rle::RleVec;
    use crate::{LocalVersion, ROOT_TIME, Time};
    use crate::causalgraph::parents::tools::{DiffFlag, DiffResult};

    // The conflict finder can also be used as an overly complicated diff function. Check this works
    // (This is mostly so I can reuse a bunch of tests).
    fn diff_via_conflicting(history: &Parents, a: &[Time], b: &[Time]) -> DiffResult {
        let mut only_a = smallvec![];
        let mut only_b = smallvec![];

        history.find_conflicting(a, b, |span, flag| {
            // dbg!((span, flag));
            let target = match flag {
                OnlyA => { &mut only_a }
                OnlyB => { &mut only_b }
                Shared => { return; }
            };
            // dbg!((ord_start, ord_end));

            target.push_reversed_rle(span);
        });

        (only_a, only_b)
    }

    #[derive(Debug, Eq, PartialEq)]
    pub struct ConflictFull {
        pub(crate) common_branch: LocalVersion,
        pub(crate) spans: Vec<(DTRange, DiffFlag)>,
    }

    fn push_rev_rle(list: &mut Vec<(DTRange, DiffFlag)>, span: DTRange, flag: DiffFlag) {
        if let Some((last_span, last_flag)) = list.last_mut() {
            if span.can_append(last_span) && flag == *last_flag {
                last_span.prepend(span);
                return;
            }
        }
        list.push((span, flag));
    }
    fn find_conflicting(history: &Parents, a: &[Time], b: &[Time]) -> ConflictFull {
        let mut spans_fast = Vec::new();
        let mut spans_slow = Vec::new();

        let common_branch_fast = history.find_conflicting(a, b, |span, flag| {
            debug_assert!(!span.is_empty());
            // spans_fast.push((span, flag));
            push_rev_rle(&mut spans_fast, span, flag);
        });
        let common_branch_slow = history.find_conflicting_slow(a, b, |span, flag| {
            debug_assert!(!span.is_empty());
            // spans_slow.push((span, flag));
            push_rev_rle(&mut spans_slow, span, flag);
        });
        assert_eq!(spans_fast, spans_slow);
        assert_eq!(common_branch_fast, common_branch_slow);

        ConflictFull {
            common_branch: common_branch_slow,
            spans: spans_slow,
        }
    }

    fn assert_conflicting(history: &Parents, a: &[Time], b: &[Time], expect_spans: &[(Range<usize>, DiffFlag)], expect_common: &[Time]) {
        let expect: Vec<(DTRange, DiffFlag)> = expect_spans
            .iter()
            .rev()
            .map(|(r, flag)| (r.clone().into(), *flag))
            .collect();
        let actual = find_conflicting(history, a, b);
        assert_eq!(actual.common_branch.as_slice(), expect_common);
        assert_eq!(actual.spans, expect);

        #[cfg(feature="gen_test_data")] {
            #[cfg_attr(feature = "serde", derive(Serialize), serde(crate = "serde_crate"))]
            #[derive(Clone)]
            struct Test<'a> {
                hist: Vec<ParentsEntrySimple>,
                a: &'a [Time],
                b: &'a [Time],
                expect_spans: &'a [(Range<usize>, DiffFlag)],
                expect_common: &'a [Time],
            }

            let t = Test {
                hist: history.iter().collect(), a, b, expect_spans, expect_common
            };

            let p: Vec<_> = history.iter().collect();
            use std::io::Write;
            let mut f = std::fs::File::options()
                .write(true)
                .append(true)
                .create(true)
                .open("test_data/causal_graph/conflicting.json").unwrap();
            writeln!(f, "{}", serde_json::to_string(&t).unwrap());
        }
    }

    fn assert_version_contains_time(history: &Parents, frontier: &[Time], target: Time, expected: bool) {
        #[cfg(feature="gen_test_data")] {
            #[cfg_attr(feature = "serde", derive(Serialize), serde(crate = "serde_crate"))]
            #[derive(Clone, Debug)]
            struct Test<'a> {
                hist: Vec<ParentsEntrySimple>,
                frontier: &'a [Time],
                target: isize,
                expected: bool,
            }

            let t = Test {
                hist: history.iter().collect(), frontier, target: target as _, expected
            };

            let p: Vec<_> = history.iter().collect();
            use std::io::Write;
            let mut f = std::fs::File::options()
                .write(true)
                .append(true)
                .create(true)
                .open("test_data/causal_graph/version_contains.json").unwrap();
            writeln!(f, "{}", serde_json::to_string(&t).unwrap());
        }

        assert_eq!(history.version_contains_time(frontier, target), expected);
    }

    fn assert_diff_eq(history: &Parents, a: &[Time], b: &[Time], expect_a: &[DTRange], expect_b: &[DTRange]) {
        #[cfg(feature="gen_test_data")] {
            #[cfg_attr(feature = "serde", derive(Serialize), serde(crate = "serde_crate"))]
            #[derive(Clone)]
            struct Test<'a> {
                hist: Vec<ParentsEntrySimple>,
                a: &'a [Time],
                b: &'a [Time],
                expect_a: &'a [DTRange],
                expect_b: &'a [DTRange],
            }

            let t = Test {
                hist: history.iter().collect(),
                a,
                b,
                expect_a,
                expect_b
            };

            let p: Vec<_> = history.iter().collect();
            use std::io::Write;
            let mut f = std::fs::File::options()
                .write(true)
                .append(true)
                .create(true)
                .open("test_data/causal_graph/diff.json").unwrap();
            writeln!(f, "{}", serde_json::to_string(&t).unwrap());
        }

        let slow_result = history.diff_slow(a, b);
        let fast_result = history.diff(a, b);
        let c_result = diff_via_conflicting(history, a, b);

        assert_eq!(slow_result.0.as_slice(), expect_a);
        assert_eq!(slow_result.1.as_slice(), expect_b);

        // dbg!(&slow_result, &fast_result);
        assert_eq!(slow_result, fast_result);
        // dbg!(&slow_result, &c_result);
        assert_eq!(slow_result, c_result);

        for &(branch, spans, other) in &[(a, expect_a, b), (b, expect_b, a)] {
            for o in spans {
                assert_version_contains_time(history, branch, o.start, true);
                if o.len() > 1 {
                    assert_version_contains_time(history, branch, o.last(), true);
                }
            }

            if branch.len() == 1 {
                // dbg!(&other, branch[0], &spans);
                let expect = spans.is_empty();
                assert_version_contains_time(history, other, branch[0], expect);
            }
        }

        // TODO: Could add extra checks for each specific version in here too. Eh!
    }

    fn fancy_parents() -> Parents {
        Parents {
            entries: RleVec(vec![
                ParentsEntryInternal { // 0-2
                    span: (0..3).into(), shadow: 0,
                    parents: smallvec![],
                    child_indexes: smallvec![2, 3],
                },
                ParentsEntryInternal { // 3-5
                    span: (3..6).into(), shadow: 3,
                    parents: smallvec![],
                    child_indexes: smallvec![2],
                },
                ParentsEntryInternal { // 6-8
                    span: (6..9).into(), shadow: 6,
                    parents: smallvec![1, 4],
                    child_indexes: smallvec![3],
                },
                ParentsEntryInternal { // 9
                    span: (9..11).into(), shadow: usize::MAX,
                    parents: smallvec![2, 8],
                    child_indexes: smallvec![],
                },
            ]),
            root_child_indexes: smallvec![0, 1],
        }
    }

    #[test]
    fn common_item_smoke_test() {
        let parents = fancy_parents();

        for t in 0..=9 {
            // dbg!(t);
            // The same item should never conflict with itself.
            assert_conflicting(&parents, &[t], &[t], &[], &[t]);
        }
        assert_conflicting(&parents, &[5, 6], &[5, 6], &[], &[5, 6]);

        assert_conflicting(&parents, &[1], &[2], &[(2..3, OnlyB)], &[1]);
        assert_conflicting(&parents, &[0], &[2], &[(1..3, OnlyB)], &[0]);
        assert_conflicting(&parents, &[], &[], &[], &[]);
        assert_conflicting(&parents, &[], &[2], &[(0..3, OnlyB)], &[]);

        assert_conflicting(&parents, &[2], &[3], &[(0..3, OnlyA), (3..4, OnlyB)], &[]); // 0,1,2 and 3.
        assert_conflicting(&parents, &[1, 4], &[4], &[(0..2, OnlyA), (3..5, Shared)], &[]); // 0,1,2 and 3.
        assert_conflicting(&parents, &[6], &[2], &[(0..2, Shared), (2..3, OnlyB), (3..5, OnlyA), (6..7, OnlyA)], &[]);
        assert_conflicting(&parents, &[6], &[5], &[(0..2, OnlyA), (3..5, Shared), (5..6, OnlyB), (6..7, OnlyA)], &[]); // 6 includes 1, 0.
        assert_conflicting(&parents, &[5, 6], &[5], &[(0..2, OnlyA), (3..6, Shared), (6..7, OnlyA)], &[]);
        assert_conflicting(&parents, &[5, 6], &[2], &[(0..2, Shared), (2..3, OnlyB), (3..7, OnlyA)], &[]);
        assert_conflicting(&parents, &[2, 6], &[5], &[(0..3, OnlyA), (3..5, Shared), (5..6, OnlyB), (6..7, OnlyA)], &[]);
        assert_conflicting(&parents, &[9], &[10], &[(10..11, OnlyB)], &[9]);
        assert_conflicting(&parents, &[6], &[7], &[(7..8, OnlyB)], &[6]);

        // This looks weird, but its right because 9 shares the same parents.
        assert_conflicting(&parents, &[9], &[2, 8], &[(9..10, OnlyA)], &[2, 8]);

        // Everything! Just because we need to rebase operation 8 on top of 7, and can't produce
        // that without basically all of time. Hopefully this doesn't come up a lot in practice.
        assert_conflicting(&parents, &[9], &[2, 7], &[(0..5, Shared), (6..8, Shared), (8..10, OnlyA)], &[]);
    }

    #[test]
    fn branch_contains_smoke_test() {
        // let mut doc = ListCRDT::new();
        // assert!(doc.txns.branch_contains_order(&doc.frontier, ROOT_TIME_X));
        //
        // doc.get_or_create_agent_id("a");
        // doc.local_insert(0, 0, "S".into()); // Shared history.
        // assert!(doc.txns.branch_contains_order(&doc.frontier, ROOT_TIME_X));
        // assert!(doc.txns.branch_contains_order(&doc.frontier, 0));
        // assert!(!doc.txns.branch_contains_order(&[ROOT_TIME_X], 0));

        let history = fancy_parents();

        assert_version_contains_time(&history, &[], ROOT_TIME, true);
        assert_version_contains_time(&history, &[0], 0, true);
        assert_version_contains_time(&history, &[0], ROOT_TIME, true);

        assert_version_contains_time(&history, &[2], 0, true);
        assert_version_contains_time(&history, &[2], 1, true);
        assert_version_contains_time(&history, &[2], 2, true);

        assert_version_contains_time(&history, &[0], 1, false);
        assert_version_contains_time(&history, &[1], 2, false);

        assert_version_contains_time(&history, &[8], 0, true);
        assert_version_contains_time(&history, &[8], 1, true);
        assert_version_contains_time(&history, &[8], 2, false);
        assert_version_contains_time(&history, &[8], 5, false);

        assert_version_contains_time(&history, &[1,4], 0, true);
        assert_version_contains_time(&history, &[1,4], 1, true);
        assert_version_contains_time(&history, &[1,4], 2, false);
        assert_version_contains_time(&history, &[1,4], 5, false);

        assert_version_contains_time(&history, &[9], 2, true);
        assert_version_contains_time(&history, &[9], 1, true);
        assert_version_contains_time(&history, &[9], 0, true);
    }

    #[test]
    fn diff_for_flat_txns() {
        // Regression.

        // 0 |
        // | 1
        // 2
        let history = Parents {
            entries: RleVec(vec![
                ParentsEntryInternal {
                    span: (0..1).into(), shadow: usize::MAX,
                    parents: smallvec![],
                    child_indexes: smallvec![2]
                },
                ParentsEntryInternal {
                    span: (1..2).into(), shadow: usize::MAX,
                    parents: smallvec![],
                    child_indexes: smallvec![3]
                },
                ParentsEntryInternal {
                    span: (2..3).into(), shadow: 2,
                    parents: smallvec![0],
                    child_indexes: smallvec![4]
                },
            ]),
            root_child_indexes: smallvec![0, 1],
        };

        assert_diff_eq(&history, &[2], &[], &[(2..3).into(), (0..1).into()], &[]);
        assert_diff_eq(&history, &[2], &[1], &[(2..3).into(), (0..1).into()], &[(1..2).into()]);
    }

    #[test]
    fn diff_three_root_txns() {
        // Regression.

        // 0 | |
        //   1 |
        //     2
        let history = Parents {
            entries: RleVec(vec![
                ParentsEntryInternal {
                    span: (0..1).into(),
                    shadow: usize::MAX,
                    parents: smallvec![],
                    child_indexes: smallvec![],
                },
                ParentsEntryInternal {
                    span: (1..2).into(),
                    shadow: 1,
                    parents: smallvec![],
                    child_indexes: smallvec![],
                },
                ParentsEntryInternal {
                    span: (2..3).into(),
                    shadow: 2,
                    parents: smallvec![],
                    child_indexes: smallvec![],
                },
            ]),
            root_child_indexes: smallvec![0, 1, 2],
        };

        assert_diff_eq(&history, &[0], &[0, 1], &[], &[(1..2).into()]);

        for time in [0, 1, 2] {
            assert_diff_eq(&history, &[time], &[], &[(time..time+1).into()], &[]);
            assert_diff_eq(&history, &[], &[time], &[], &[(time..time+1).into()]);
        }

        assert_diff_eq(&history, &[], &[0, 1], &[], &[(0..2).into()]);
        assert_diff_eq(&history, &[0], &[1], &[(0..1).into()], &[(1..2).into()]);
    }

    #[test]
    fn diff_shadow_bubble() {
        // regression

        // 0,1,2   |
        //      \ 3,4
        //       \ /
        //        5,6
        let history = Parents {
            entries: RleVec(vec![
                ParentsEntryInternal {
                    span: (0..3).into(),
                    shadow: usize::MAX,
                    parents: smallvec![],
                    child_indexes: smallvec![2],
                },
                ParentsEntryInternal {
                    span: (3..5).into(),
                    shadow: 3,
                    parents: smallvec![],
                    child_indexes: smallvec![2],
                },
                ParentsEntryInternal {
                    span: (5..6).into(),
                    shadow: usize::MAX,
                    parents: smallvec![2,4],
                    child_indexes: smallvec![],
                },
            ]),
            root_child_indexes: smallvec![0, 1],
        };

        assert_diff_eq(&history, &[4], &[5], &[], &[(5..6).into(), (0..3).into()]);
        assert_diff_eq(&history, &[4], &[], &[(3..5).into()], &[]);
    }

    #[test]
    fn diff_common_branch_is_ordered() {
        // Regression
        // 0 1
        // |x|
        // 2 3
        let history = Parents::from_entries(&[
            ParentsEntryInternal {
                span: (0..1).into(),
                shadow: usize::MAX,
                parents: smallvec![],
                child_indexes: smallvec![1, 2],
            },
            ParentsEntryInternal {
                span: (1..2).into(),
                shadow: 1,
                parents: smallvec![],
                child_indexes: smallvec![1, 2],
            },
            ParentsEntryInternal {
                span: (2..3).into(),
                shadow: usize::MAX,
                parents: smallvec![0, 1],
                child_indexes: smallvec![],
            },
            ParentsEntryInternal {
                span: (3..4).into(),
                shadow: 3,
                parents: smallvec![0, 1],
                child_indexes: smallvec![],
            },
        ]);

        assert_version_contains_time(&history, &[2], 3, false);
        assert_version_contains_time(&history, &[3], 2, false);
        assert_diff_eq(&history, &[2], &[3], &[(2..3).into()], &[(3..4).into()]);
    }


    // #[test]
    // fn diff_smoke_test() {
    //     let mut doc1 = ListCRDT::new();
    //     assert_diff_eq(&doc1.history, &doc1.frontier, &doc1.frontier, &[], &[]);
    //
    //     doc1.get_or_create_agent_id("a");
    //     doc1.local_insert(0, 0, "S".into()); // Shared history.
    //
    //     let mut doc2 = ListCRDT::new();
    //     doc2.get_or_create_agent_id("b");
    //     doc1.replicate_into(&mut doc2); // "S".
    //
    //     // Ok now make some concurrent history.
    //     doc1.local_insert(0, 1, "aaa".into());
    //     let b1 = doc1.frontier.clone();
    //
    //     assert_diff_eq(&doc1.txns, &b1, &b1, &[], &[]);
    //     assert_diff_eq(&doc1.txns, &[ROOT_TIME_X], &[ROOT_TIME_X], &[], &[]);
    //     // dbg!(&doc1.frontier);
    //
    //     // There are 4 items in doc1 - "Saaa".
    //     // dbg!(&doc1.frontier); // [3]
    //     assert_diff_eq(&doc1.txns, &[1], &[3], &[], &[2..4]);
    //
    //     doc2.local_insert(0, 1, "bbb".into());
    //
    //     doc2.replicate_into(&mut doc1);
    //
    //     // doc1 has "Saaabbb".
    //
    //     // dbg!(doc1.diff(&b1, &doc1.frontier));
    //
    //     assert_diff_eq(&doc1.txns, &b1, &doc1.frontier, &[], &[4..7]);
    //     assert_diff_eq(&doc1.txns, &[3], &[6], &[1..4], &[4..7]);
    //     assert_diff_eq(&doc1.txns, &[2], &[5], &[1..3], &[4..6]);
    //
    //     // doc1.replicate_into(&mut doc2); // Also "Saaabbb" but different txns.
    //     // dbg!(&doc1.txns, &doc2.txns);
    // }

    // fn root_id() -> RemoteId {
    //     RemoteId {
    //         agent: "ROOT".into(),
    //         seq: u32::MAX
    //     }
    // }
    //
    // pub fn complex_multientry_doc() -> ListCRDT {
    //     let mut doc = ListCRDT::new();
    //     doc.get_or_create_agent_id("a");
    //     doc.get_or_create_agent_id("b");
    //
    //     assert_eq!(doc.frontier.as_slice(), &[ROOT_TIME_X]);
    //
    //     doc.local_insert(0, 0, "aaa".into());
    //
    //     assert_eq!(doc.frontier.as_slice(), &[2]);
    //
    //     // Need to do this manually to make the change concurrent.
    //     doc.apply_remote_txn(&RemoteTxn {
    //         id: RemoteId { agent: "b".into(), seq: 0 },
    //         parents: smallvec![root_id()],
    //         ops: smallvec![RemoteCRDTOp::Ins {
    //             origin_left: root_id(),
    //             origin_right: root_id(),
    //             len: 2,
    //             content_known: true,
    //         }],
    //         ins_content: "bb".into(),
    //     });
    //
    //     assert_eq!(doc.frontier.as_slice(), &[2, 4]);
    //
    //     // And need to do this manually to make the change not merge time.
    //     doc.apply_remote_txn(&RemoteTxn {
    //         id: RemoteId { agent: "a".into(), seq: 3 },
    //         parents: smallvec![RemoteId { agent: "a".into(), seq: 2 }],
    //         ops: smallvec![RemoteCRDTOp::Ins {
    //             origin_left: RemoteId { agent: "a".into(), seq: 2 },
    //             origin_right: root_id(),
    //             len: 2,
    //             content_known: true,
    //         }],
    //         ins_content: "AA".into(),
    //     });
    //
    //     assert_eq!(doc.frontier.as_slice(), &[4, 6]);
    //
    //     if let Some(ref text) = doc.text_content {
    //         assert_eq!(text, "aaaAAbb");
    //     }
    //
    //     doc
    // }

    // #[test]
    // fn diff_with_multiple_entries() {
    //     let doc = complex_multientry_doc();
    //
    //     // dbg!(&doc.txns);
    //     // dbg!(doc.diff(&smallvec![6], &smallvec![]));
    //     // dbg!(&doc);
    //
    //     assert_diff_eq(&doc.txns, &[6], &[ROOT_TIME_X], &[5..7, 0..3], &[]);
    //     assert_diff_eq(&doc.txns, &[6], &[4], &[5..7, 0..3], &[3..5]);
    //     assert_diff_eq(&doc.txns, &[4, 6], &[ROOT_TIME_X], &[0..7], &[]);
    // }

}