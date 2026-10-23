use serde::{Deserialize, Serialize};

/// Canonical result categories (doc 02 section 1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    AiDirect,
    AiStrong,
    AiProbable,
    Mixed,
    Unknown,
    Excluded,
}

impl Category {
    /// Snake_case spelling matching the serde serialized form.
    pub fn as_str(self) -> &'static str {
        match self {
            Category::AiDirect => "ai_direct",
            Category::AiStrong => "ai_strong",
            Category::AiProbable => "ai_probable",
            Category::Mixed => "mixed",
            Category::Unknown => "unknown",
            Category::Excluded => "excluded",
        }
    }
    /// Counts toward the eligible denominator E.
    pub fn is_eligible(self) -> bool {
        !matches!(self, Category::Excluded)
    }
    /// Numerator of `AI_deterministic%` = direct + strong.
    pub fn is_deterministic_ai(self) -> bool {
        matches!(self, Category::AiDirect | Category::AiStrong)
    }
    /// Numerator of `AI_linked%` = direct + strong + probable + mixed.
    pub fn is_ai_linked(self) -> bool {
        matches!(
            self,
            Category::AiDirect | Category::AiStrong | Category::AiProbable | Category::Mixed
        )
    }
    /// Default score band from doc 04 section 5.1. Scores rank evidence strength;
    /// they are NOT authorship probabilities.
    pub fn default_score_band(self) -> Option<(f32, f32)> {
        match self {
            Category::AiDirect => Some((0.98, 1.00)),
            Category::AiStrong => Some((0.80, 0.97)),
            Category::AiProbable => Some((0.50, 0.79)),
            Category::Mixed | Category::Unknown | Category::Excluded => None,
        }
    }
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Basis codes (doc 04 section 5.2). One primary basis per assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Basis {
    DirectAcceptedEdit,
    DirectAgentPatch,
    ExactFragmentMatch,
    ExactKeyedLineHash,
    TokenFingerprintMatch,
    PatchLineage,
    MoveLineage,
    FormatTransformation,
    RebasePatchId,
    SignedCorrection,
    ConflictingEvidence,
    IncompleteSource,
    ContentRestricted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_never_ai_and_there_is_no_human_variant() {
        assert!(!Category::Unknown.is_ai_linked());
        assert!(!Category::Unknown.is_deterministic_ai());
        assert!(Category::Unknown.is_eligible());
    }

    #[test]
    fn excluded_is_not_eligible() {
        assert!(!Category::Excluded.is_eligible());
    }

    #[test]
    fn basis_serializes_as_doc_codes() {
        let s = serde_json::to_string(&Basis::DirectAcceptedEdit).unwrap();
        assert_eq!(s, "\"DIRECT_ACCEPTED_EDIT\"");
        let c = serde_json::to_string(&Category::AiStrong).unwrap();
        assert_eq!(c, "\"ai_strong\"");
    }

    #[test]
    fn as_str_matches_serialized_snake_case() {
        for cat in [
            Category::AiDirect,
            Category::AiStrong,
            Category::AiProbable,
            Category::Mixed,
            Category::Unknown,
            Category::Excluded,
        ] {
            let json = serde_json::to_string(&cat).unwrap();
            assert_eq!(json, format!("\"{}\"", cat.as_str()));
            assert_eq!(cat.to_string(), cat.as_str());
        }
    }
}
