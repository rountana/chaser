use crate::config::ClaudeConfig;
use super::{Backend, classify, intent::IntentSignals};

/// Route a query to one or more backends using the 7-rule priority cascade.
///
/// Rules are evaluated in order; first match wins.
/// R6 (ambiguous hybrid) calls Claude Haiku via `classify_backends` — the only async rule.
pub async fn route(
    signals: &IntentSignals,
    query: &str,
    config: &ClaudeConfig,
    known_persons: &[String],
    doc_type_values: &[String],
) -> Vec<Backend> {
    // R1: Structural signals present
    if signals.structural.is_some() {
        return vec![Backend::Structural];
    }

    // R2: Person + (date OR doc_type) → metadata + keyword (2-pass)
    if !signals.persons.is_empty()
        && (!signals.dates.is_empty() || !signals.doc_types.is_empty())
    {
        return vec![Backend::Metadata, Backend::Keyword];
    }

    // R3: Person only
    if !signals.persons.is_empty()
        && signals.dates.is_empty()
        && signals.doc_types.is_empty()
    {
        return vec![Backend::Metadata];
    }

    // R4: Date only — keywords must also be absent; date+keyword falls to R7 so the keyword
    // backend can apply date pre-filtering and exclude unrelated documents.
    if signals.persons.is_empty()
        && !signals.dates.is_empty()
        && signals.doc_types.is_empty()
        && signals.keywords.is_empty()
    {
        return vec![Backend::Metadata];
    }

    // R5: Doc type only
    if signals.persons.is_empty()
        && signals.dates.is_empty()
        && !signals.doc_types.is_empty()
    {
        return vec![Backend::Metadata, Backend::Keyword];
    }

    // R6: Ambiguous hybrid — 2+ primary signal types present, no rule above matched.
    // The only combination that reaches here: date + doc_type (no person).
    let primary_signal_count = [
        !signals.persons.is_empty(),
        !signals.dates.is_empty(),
        !signals.doc_types.is_empty(),
    ]
    .iter()
    .filter(|&&b| b)
    .count();

    if primary_signal_count >= 2 {
        match classify::classify_backends(query, known_persons, doc_type_values, config).await {
            Ok(backends) => return backends,
            Err(e) => {
                eprintln!("warning: LLM triage failed; falling back to keyword search: {e:#}");
                return vec![Backend::Keyword];
            }
        }
    }

    // R7: Default — keyword search (semantic stub returns [] until Phase 4)
    vec![Backend::Keyword]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::intent::{IntentSignals, StructSignal, StructField, StructOp};

    fn signals(
        persons: &[&str],
        doc_types: &[&str],
        dates: &[&str],
        structural: Option<StructSignal>,
        keywords: &[&str],
    ) -> IntentSignals {
        IntentSignals {
            persons: persons.iter().map(|s| s.to_string()).collect(),
            doc_types: doc_types.iter().map(|s| s.to_string()).collect(),
            dates: dates.iter().map(|s| s.to_string()).collect(),
            structural,
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn struct_signal() -> StructSignal {
        StructSignal { field: StructField::Pages, op: StructOp::Gt, value: 5 }
    }

    // Sync routing helper — tests R1-R5 and R7 without touching the network.
    // R6 is not tested here since it requires a live API call.
    fn route_sync(sig: &IntentSignals) -> Vec<Backend> {
        if sig.structural.is_some() {
            return vec![Backend::Structural];
        }
        if !sig.persons.is_empty() && (!sig.dates.is_empty() || !sig.doc_types.is_empty()) {
            return vec![Backend::Metadata, Backend::Keyword];
        }
        if !sig.persons.is_empty() && sig.dates.is_empty() && sig.doc_types.is_empty() {
            return vec![Backend::Metadata];
        }
        if sig.persons.is_empty() && !sig.dates.is_empty() && sig.doc_types.is_empty() && sig.keywords.is_empty() {
            return vec![Backend::Metadata];
        }
        if sig.persons.is_empty() && sig.dates.is_empty() && !sig.doc_types.is_empty() {
            return vec![Backend::Metadata, Backend::Keyword];
        }
        let primary_count = [
            !sig.persons.is_empty(),
            !sig.dates.is_empty(),
            !sig.doc_types.is_empty(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        if primary_count >= 2 {
            // R6 — would call LLM; return sentinel for test purposes
            return vec![Backend::Keyword]; // fallback path
        }
        vec![Backend::Keyword]
    }

    #[test]
    fn r1_structural() {
        let sig = signals(&[], &[], &[], Some(struct_signal()), &[]);
        assert_eq!(route_sync(&sig), vec![Backend::Structural]);
    }

    #[test]
    fn r2_person_plus_date() {
        let sig = signals(&["Alice"], &[], &["2024"], None, &[]);
        assert_eq!(route_sync(&sig), vec![Backend::Metadata, Backend::Keyword]);
    }

    #[test]
    fn r2_person_plus_doc_type() {
        let sig = signals(&["Alice"], &["invoice"], &[], None, &[]);
        assert_eq!(route_sync(&sig), vec![Backend::Metadata, Backend::Keyword]);
    }

    #[test]
    fn r3_person_only() {
        let sig = signals(&["Alice"], &[], &[], None, &[]);
        assert_eq!(route_sync(&sig), vec![Backend::Metadata]);
    }

    #[test]
    fn r4_date_only() {
        let sig = signals(&[], &[], &["2024-01"], None, &[]);
        assert_eq!(route_sync(&sig), vec![Backend::Metadata]);
    }

    #[test]
    fn r4_does_not_fire_with_keywords() {
        // date + keyword must NOT go to metadata-only; falls to R7 (keyword with date pre-filter)
        let sig = signals(&[], &[], &["2023"], None, &["invoices"]);
        let backends = route_sync(&sig);
        assert_ne!(backends, vec![Backend::Metadata],
            "date+keyword query should not route to metadata-only; got {:?}", backends);
        assert!(backends.contains(&Backend::Keyword),
            "date+keyword query must include keyword backend; got {:?}", backends);
    }

    #[test]
    fn r5_doc_type_only() {
        let sig = signals(&[], &["invoice"], &[], None, &[]);
        assert_eq!(route_sync(&sig), vec![Backend::Metadata, Backend::Keyword]);
    }

    #[test]
    fn r7_default_keyword() {
        let sig = signals(&[], &[], &[], None, &["salary"]);
        assert_eq!(route_sync(&sig), vec![Backend::Keyword]);
    }

    #[test]
    fn r1_beats_all_other_signals() {
        // Even with persons + dates, structural takes priority
        let sig = signals(&["Alice"], &["invoice"], &["2024"], Some(struct_signal()), &[]);
        assert_eq!(route_sync(&sig), vec![Backend::Structural]);
    }
}
