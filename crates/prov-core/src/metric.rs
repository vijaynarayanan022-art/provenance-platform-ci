use crate::Category;
use serde::{Deserialize, Serialize};

/// Line counts per category for one PR (or any scope).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineCounts {
    pub direct: u64,
    pub strong: u64,
    pub probable: u64,
    pub mixed: u64,
    pub unknown: u64,
    pub excluded: u64,
}

impl LineCounts {
    pub fn add(&mut self, c: Category, n: u64) {
        match c {
            Category::AiDirect => self.direct += n,
            Category::AiStrong => self.strong += n,
            Category::AiProbable => self.probable += n,
            Category::Mixed => self.mixed += n,
            Category::Unknown => self.unknown += n,
            Category::Excluded => self.excluded += n,
        }
    }
    /// Add another set of counts into this one.
    pub fn merge(&mut self, other: &LineCounts) {
        self.direct += other.direct;
        self.strong += other.strong;
        self.probable += other.probable;
        self.mixed += other.mixed;
        self.unknown += other.unknown;
        self.excluded += other.excluded;
    }
    /// E = D + S + P + M + U. Excluded lines are never part of E.
    pub fn eligible(&self) -> u64 {
        self.direct + self.strong + self.probable + self.mixed + self.unknown
    }
    /// All lines seen, including excluded (for reconciliation displays).
    pub fn total_seen(&self) -> u64 {
        self.eligible() + self.excluded
    }
}

/// PR metric (doc 02/07):
///   AI_deterministic% = 100 * (D + S) / E
///   AI_linked%        = 100 * (D + S + P + M) / E
///
/// Zero denominator: the docs disagree (0 vs `not_applicable`). We return `None`
/// (serialised as null) = not_applicable, because 0% would claim "no AI" without evidence.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PrMetric {
    pub counts: LineCounts,
    pub eligible: u64,
    pub ai_deterministic_pct: Option<f64>,
    pub ai_linked_pct: Option<f64>,
}

impl PrMetric {
    pub fn from_counts(counts: LineCounts) -> Self {
        let e = counts.eligible();
        let (det, linked) = if e == 0 {
            (None, None)
        } else {
            let ef = e as f64;
            (
                Some(100.0 * (counts.direct + counts.strong) as f64 / ef),
                Some(
                    100.0 * (counts.direct + counts.strong + counts.probable + counts.mixed) as f64
                        / ef,
                ),
            )
        };
        PrMetric { counts, eligible: e, ai_deterministic_pct: det, ai_linked_pct: linked }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: Option<f64>, b: f64) {
        let a = a.expect("metric should be defined");
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    /// Golden example from doc 07: 100 eligible added lines.
    #[test]
    fn golden_doc07_example() {
        let c = LineCounts { direct: 30, strong: 20, probable: 10, mixed: 5, unknown: 35, excluded: 0 };
        let m = PrMetric::from_counts(c);
        assert_eq!(m.eligible, 100);
        approx(m.ai_deterministic_pct, 50.0);
        approx(m.ai_linked_pct, 65.0);
    }

    /// Doc 07: generated lockfile adds 500 lines, policy excludes them -> not in denominator.
    #[test]
    fn excluded_lines_do_not_change_percentages() {
        let base = LineCounts { direct: 30, strong: 20, probable: 10, mixed: 5, unknown: 35, excluded: 0 };
        let with_lockfile = LineCounts { excluded: 500, ..base };
        let a = PrMetric::from_counts(base);
        let b = PrMetric::from_counts(with_lockfile);
        assert_eq!(a.eligible, b.eligible);
        assert_eq!(a.ai_deterministic_pct, b.ai_deterministic_pct);
        assert_eq!(a.ai_linked_pct, b.ai_linked_pct);
        assert_eq!(b.counts.total_seen(), 600);
    }

    #[test]
    fn zero_denominator_is_not_applicable_not_zero() {
        let m = PrMetric::from_counts(LineCounts { excluded: 10, ..Default::default() });
        assert_eq!(m.eligible, 0);
        assert_eq!(m.ai_deterministic_pct, None);
        assert_eq!(m.ai_linked_pct, None);
    }

    #[test]
    fn all_unknown_is_zero_ai_but_defined() {
        let m = PrMetric::from_counts(LineCounts { unknown: 40, ..Default::default() });
        approx(m.ai_deterministic_pct, 0.0);
        approx(m.ai_linked_pct, 0.0);
    }

    #[test]
    fn add_by_category_reconciles() {
        let mut c = LineCounts::default();
        c.add(Category::AiDirect, 3);
        c.add(Category::Unknown, 2);
        c.add(Category::Excluded, 7);
        assert_eq!(c.eligible(), 5);
        assert_eq!(c.total_seen(), 12);
    }

    #[test]
    fn merge_sums_each_bucket() {
        let mut a = LineCounts {
            direct: 1,
            strong: 2,
            probable: 3,
            mixed: 4,
            unknown: 5,
            excluded: 6,
        };
        let b = LineCounts {
            direct: 10,
            strong: 20,
            probable: 30,
            mixed: 40,
            unknown: 50,
            excluded: 60,
        };
        a.merge(&b);
        assert_eq!(
            a,
            LineCounts {
                direct: 11,
                strong: 22,
                probable: 33,
                mixed: 44,
                unknown: 55,
                excluded: 66,
            }
        );
    }
}
