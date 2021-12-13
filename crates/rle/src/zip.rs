use std::cmp::Ordering;
use std::mem::take;
use crate::{HasLength, SplitableSpan};

#[derive(Clone, Debug)]
enum Remainder<A, B> {
    Nothing,
    SomeA(A),
    SomeB(B),
}

impl<A, B> Default for Remainder<A, B> {
    fn default() -> Self { Remainder::Nothing }
}

/// A RleZip is a zip iterator over 2 SplitableSpan iterators. Each item it yields contains the
/// longest readable span from each of A and B.
///
/// The iterator ends at the min of A and B.
#[derive(Clone, Debug)]
pub struct RleZip<A, B, AIter, BIter>
    where A: SplitableSpan + HasLength, B: SplitableSpan + HasLength,
          AIter: Iterator<Item = A>, BIter: Iterator<Item = B>
{
    rem: Remainder<A, B>,
    a: AIter,
    b: BIter,
}

impl<A, B, AIter, BIter> Iterator for RleZip<A, B, AIter, BIter>
    where A: SplitableSpan + HasLength, B: SplitableSpan + HasLength,
          AIter: Iterator<Item = A>, BIter: Iterator<Item = B>
{
    type Item = (A, B);

    fn next(&mut self) -> Option<Self::Item> {
        let (mut a, mut b) = match take(&mut self.rem) {
            Remainder::Nothing => {
                // Fetch from both.
                let a = self.a.next()?;
                let b = self.b.next()?;
                (a, b)
            }
            Remainder::SomeA(a) => {
                let b = self.b.next()?;
                (a, b)
            }
            Remainder::SomeB(b) => {
                let a = self.a.next()?;
                (a, b)
            }
        };

        let a_len = a.len();
        let b_len = b.len();

        self.rem = match a_len.cmp(&b_len) {
            Ordering::Equal => {
                // Take all of both.
                Remainder::Nothing
            }
            Ordering::Less => {
                // a < b.
                let b_rem = b.truncate(a_len);
                Remainder::SomeB(b_rem)
            }
            Ordering::Greater => {
                // a > b.
                let a_rem = a.truncate(b_len);
                Remainder::SomeA(a_rem)
            }
        };

        Some((a, b))
    }
}

pub fn rle_zip<A, B, AIter, BIter>(a: AIter, b: BIter) -> RleZip<A, B, AIter, BIter>
    where A: SplitableSpan + HasLength, B: SplitableSpan + HasLength,
        AIter: Iterator<Item = A>, BIter: Iterator<Item = B>
{
    RleZip {
        rem: Remainder::Nothing,
        a,
        b
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::RleRun;

    fn check_zip(a: &[RleRun<u32>], b: &[RleRun<u32>], expect: &[(RleRun<u32>, RleRun<u32>)]) {
        assert_eq!(rle_zip(a.iter().copied(), b.iter().copied())
                       .collect::<Vec<_>>(), expect);

        // And check that if we swap the parameter order we get the same thing.
        assert_eq!(rle_zip(b.iter().copied(), a.iter().copied())
                       .map(|(a, b)| (b, a))
                       .collect::<Vec<_>>(), expect);
    }

    #[test]
    fn smoke() {
        let one = vec![
            RleRun { val: 1, len: 1 },
            RleRun { val: 2, len: 4 }
        ];
        let two = vec![
            RleRun { val: 11, len: 4 },
            RleRun { val: 12, len: 1 }
        ];

        let expected = vec![
            (RleRun { val: 1, len: 1 }, RleRun { val: 11, len: 1}),
            (RleRun { val: 2, len: 3 }, RleRun { val: 11, len: 3}),
            (RleRun { val: 2, len: 1 }, RleRun { val: 12, len: 1}),
        ];

        check_zip(&one, &two, &expected);
    }

    #[test]
    fn one_is_longer() {
        let one = vec![
            RleRun { val: 1, len: 100 },
        ];
        let two = vec![
            RleRun { val: 11, len: 10 },
        ];

        let expected = vec![
            (RleRun { val: 1, len: 10 }, RleRun { val: 11, len: 10}),
        ];

        check_zip(&one, &two, &expected);
    }
}