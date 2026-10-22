//! Vendor-neutral domain types for the Code Provenance Platform.
//!
//! Rules taken from the design docs (02 FR section 1.2, 04 section 5, 07 metric rules):
//! * Categories describe *evidence strength*, never a human author.
//! * There is deliberately NO `Human` category: "unknown is never human".
//! * Excluded lines never enter the eligible denominator.

pub mod category;
pub mod metric;

pub use category::{Basis, Category};
pub use metric::{LineCounts, PrMetric};
