use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use once_cell::sync::Lazy;
use regex::Regex;

static DATE_ISO: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(20\d{2}-\d{2}-\d{2})\b").unwrap());
static DATE_YEAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(20\d{2})\b").unwrap());
static RELATIVE_DATE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(last|this)\s+(quarter|month|year|week)\b").unwrap()
});

// Structural patterns — GTE checked before GT to avoid mis-capturing "at least"
static STRUCT_GTE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:at least|minimum|no less than)\s+(\d+)\s+(pages?|words?)").unwrap()
});
static STRUCT_GT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:more than|over|greater than|longer than)\s+(\d+)\s+(pages?|words?)").unwrap()
});
static STRUCT_LTE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:at most|maximum|no more than)\s+(\d+)\s+(pages?|words?)").unwrap()
});
static STRUCT_LT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:fewer than|less than|under|shorter than)\s+(\d+)\s+(pages?|words?)").unwrap()
});

const MONTH_NAMES: &[(&str, &str)] = &[
    ("january", "01"), ("february", "02"), ("march", "03"), ("april", "04"),
    ("may", "05"), ("june", "06"), ("july", "07"), ("august", "08"),
    ("september", "09"), ("october", "10"), ("november", "11"), ("december", "12"),
    ("jan", "01"), ("feb", "02"), ("mar", "03"), ("apr", "04"),
    ("jun", "06"), ("jul", "07"), ("aug", "08"), ("sep", "09"),
    ("oct", "10"), ("nov", "11"), ("dec", "12"),
];

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "for", "from", "with", "by", "in", "on", "at",
    "to", "and", "or", "is", "are", "was", "has", "it", "its", "be", "all",
    "show", "find", "get", "list", "search", "document", "documents", "file", "files",
    "my", "me", "do", "can", "that", "this", "have",
    // Relative-date tokens — stripped by RELATIVE_DATE_RE first, but listed here as backstop
    "last", "next", "quarter", "month", "year", "week",
];

const MIN_KEYWORD_LEN: usize = 3;

#[derive(Debug, Clone)]
pub enum StructField {
    Pages,
    Words,
}

#[derive(Debug, Clone)]
pub enum StructOp {
    Gt,
    Gte,
    Lt,
    Lte,
}

#[derive(Debug, Clone)]
pub struct StructSignal {
    pub field: StructField,
    pub op: StructOp,
    pub value: u32,
}

#[derive(Debug, Clone, Default)]
pub struct IntentSignals {
    pub persons: Vec<String>,
    pub doc_types: Vec<String>,
    pub dates: Vec<String>,
    pub structural: Option<StructSignal>,
    /// Remaining keyword tokens after stripping signals and stopwords.
    pub keywords: Vec<String>,
}

impl IntentSignals {
    /// The primary keyword (longest token) for grep/FTS5 matching.
    pub fn primary_keyword(&self) -> Option<&str> {
        self.keywords.iter().max_by_key(|k| k.len()).map(|s| s.as_str())
    }
}

pub struct IntentIndex {
    model: TextEmbedding,
    pub doc_type_embeddings: Vec<(String, Vec<f32>)>,
    pub threshold: f32,
}

impl IntentIndex {
    pub fn new(doc_type_values: &[String]) -> anyhow::Result<Self> {
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::BGESmallENV15)
                .with_show_download_progress(true),
        )?;

        let labels: Vec<String> = doc_type_values
            .iter()
            .filter(|s| s.as_str() != "unknown")
            .cloned()
            .collect();

        let raw: Vec<Vec<f32>> = if labels.is_empty() {
            vec![]
        } else {
            model.embed(labels.iter().map(|s| s.as_str()).collect(), None)?
        };

        let doc_type_embeddings = labels.into_iter().zip(raw.into_iter()).collect();

        Ok(Self {
            model,
            doc_type_embeddings,
            threshold: 0.70,
        })
    }

    pub fn parse(&self, query: &str, known_persons: &[String]) -> IntentSignals {
        let q_lower = query.to_lowercase();

        let persons = extract_persons(&q_lower, known_persons);
        let dates = extract_dates(&q_lower);
        let structural = extract_structural(&q_lower);

        let doc_types = self.extract_doc_types_embedding(query, &persons, &dates, &structural);

        IntentSignals { persons, doc_types, dates, structural, keywords: vec![] }
    }

    fn extract_doc_types_embedding(
        &self,
        query: &str,
        persons: &[String],
        dates: &[String],
        structural: &Option<StructSignal>,
    ) -> Vec<String> {
        if self.doc_type_embeddings.is_empty() {
            return vec![];
        }

        let mut q = query.to_lowercase();

        // Strip possessives
        q = q.replace("'s", "").replace('\u{2019}', "");

        // Strip relative date phrases
        q = RELATIVE_DATE_RE.replace_all(&q, " ").to_string();

        // Strip structural phrases
        if structural.is_some() {
            for pat in &[&*STRUCT_GTE, &*STRUCT_GT, &*STRUCT_LTE, &*STRUCT_LT] {
                q = pat.replace_all(&q, " ").to_string();
            }
        }

        // Strip person tokens
        for p in persons {
            for token in p.split_whitespace() {
                let escaped = regex::escape(&token.to_lowercase());
                if let Ok(re) = Regex::new(&format!(r"\b{escaped}\b")) {
                    q = re.replace_all(&q, " ").to_string();
                }
            }
        }

        // Strip date tokens
        for date in dates {
            let year = &date[..4.min(date.len())];
            if let Ok(re) = Regex::new(&format!(r"\b{year}\b")) {
                q = re.replace_all(&q, " ").to_string();
            }
        }
        for (month_name, _) in MONTH_NAMES {
            let escaped = regex::escape(month_name);
            if let Ok(re) = Regex::new(&format!(r"\b{escaped}\b")) {
                q = re.replace_all(&q, " ").to_string();
            }
        }

        let tokens: Vec<&str> = q.split_whitespace()
            .filter(|t| t.len() >= MIN_KEYWORD_LEN && !STOPWORDS.contains(t))
            .collect();
        if tokens.is_empty() {
            return vec![];
        }

        let candidates: Vec<String> = build_ngram_candidates(&tokens);
        if candidates.is_empty() {
            return vec![];
        }

        let candidate_refs: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
        let embeddings = match self.model.embed(candidate_refs, None) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("warning: IntentIndex embed failed: {e}");
                return vec![];
            }
        };

        let mut matched = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for cand_emb in &embeddings {
            for (dt_label, dt_emb) in &self.doc_type_embeddings {
                let score = cosine_similarity(cand_emb, dt_emb);
                if score >= self.threshold && seen.insert(dt_label.clone()) {
                    matched.push(dt_label.clone());
                }
            }
        }

        matched
    }
}

/// Parse a raw query into structured intent signals.
pub fn parse(query: &str, known_persons: &[String], doc_type_values: &[String]) -> IntentSignals {
    let q_lower = query.to_lowercase();

    let persons = extract_persons(&q_lower, known_persons);
    let doc_types = extract_doc_types(&q_lower, doc_type_values);
    let dates = extract_dates(&q_lower);
    let structural = extract_structural(&q_lower);
    let keywords = extract_keywords(query, &persons, &doc_types, &dates, &structural);

    IntentSignals { persons, doc_types, dates, structural, keywords }
}

fn extract_persons(query_lower: &str, known_persons: &[String]) -> Vec<String> {
    let stripped = query_lower.replace("'s", "").replace('\u{2019}', "");
    let mut found = Vec::new();
    for person in known_persons {
        let p_lower = person.to_lowercase();
        if query_lower.contains(&p_lower) || stripped.contains(&p_lower) {
            found.push(person.clone());
        }
    }
    found
}

fn extract_doc_types(query_lower: &str, doc_type_values: &[String]) -> Vec<String> {
    let mut found = Vec::new();
    for dt in doc_type_values {
        if dt == "unknown" {
            continue;
        }
        // Word-boundary match for singular and plural
        let escaped = regex::escape(dt);
        let pattern = format!(r"\b(?:{escaped}s?)\b");
        if let Ok(re) = Regex::new(&pattern) {
            if re.is_match(query_lower) {
                found.push(dt.clone());
            }
        }
    }
    found
}

/// Resolve relative time phrases ("last quarter", "this month", etc.) to concrete YYYY-MM prefixes.
/// Returns (dates, matched) — if matched is false the caller should fall through to other patterns.
fn extract_relative_dates(query_lower: &str) -> (Vec<String>, bool) {
    let caps = match RELATIVE_DATE_RE.captures(query_lower) {
        Some(c) => c,
        None => return (vec![], false),
    };

    let anchor = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let unit   = caps.get(2).map(|m| m.as_str()).unwrap_or("");

    use chrono::{Datelike, Local};
    let now = Local::now();
    let (year, month) = (now.year(), now.month());

    let dates = match (anchor, unit) {
        ("last", "month") => {
            let (y, m) = if month == 1 { (year - 1, 12) } else { (year, month - 1) };
            vec![format!("{y}-{m:02}")]
        }
        ("this", "month") => vec![format!("{year}-{month:02}")],
        ("last", "year")  => vec![format!("{}", year - 1)],
        ("this", "year")  => vec![format!("{year}")],
        ("last", "week") | ("this", "week") => {
            // Approximate: week spans at most 2 calendar months; just return current month
            vec![format!("{year}-{month:02}")]
        }
        ("last", "quarter") => {
            let (start_month, q_year) = match (month - 1) / 3 {
                0 => (10u32, year - 1), // current Q1 → last is Q4 of prev year
                1 => (1,  year),
                2 => (4,  year),
                _ => (7,  year),
            };
            (0..3).map(|i| {
                let m = start_month + i;
                format!("{q_year}-{m:02}")
            }).collect()
        }
        ("this", "quarter") => {
            let start_month = ((month - 1) / 3) * 3 + 1;
            (0..3).map(|i| {
                let m = start_month + i;
                format!("{year}-{m:02}")
            }).collect()
        }
        _ => return (vec![], false),
    };

    (dates, true)
}

fn extract_dates(query_lower: &str) -> Vec<String> {
    // Relative expressions take priority
    let (rel, matched) = extract_relative_dates(query_lower);
    if matched {
        return rel;
    }

    // ISO date takes priority
    if let Some(caps) = DATE_ISO.captures(query_lower) {
        return vec![caps[1].to_string()];
    }

    // Month name + year
    for (month_name, month_num) in MONTH_NAMES {
        if query_lower.contains(month_name) {
            if let Some(caps) = DATE_YEAR.captures(query_lower) {
                return vec![format!("{}-{}", &caps[1], month_num)];
            }
        }
    }

    // Bare year
    if let Some(caps) = DATE_YEAR.captures(query_lower) {
        return vec![caps[1].to_string()];
    }

    vec![]
}

fn extract_structural(query_lower: &str) -> Option<StructSignal> {
    // GTE must be checked before GT (both patterns can overlap)
    if let Some(caps) = STRUCT_GTE.captures(query_lower) {
        let value: u32 = caps[1].parse().ok()?;
        return Some(StructSignal { field: parse_field(&caps[2]), op: StructOp::Gte, value });
    }
    if let Some(caps) = STRUCT_GT.captures(query_lower) {
        let value: u32 = caps[1].parse().ok()?;
        return Some(StructSignal { field: parse_field(&caps[2]), op: StructOp::Gt, value });
    }
    if let Some(caps) = STRUCT_LTE.captures(query_lower) {
        let value: u32 = caps[1].parse().ok()?;
        return Some(StructSignal { field: parse_field(&caps[2]), op: StructOp::Lte, value });
    }
    if let Some(caps) = STRUCT_LT.captures(query_lower) {
        let value: u32 = caps[1].parse().ok()?;
        return Some(StructSignal { field: parse_field(&caps[2]), op: StructOp::Lt, value });
    }
    None
}

fn parse_field(s: &str) -> StructField {
    if s.to_lowercase().starts_with("word") { StructField::Words } else { StructField::Pages }
}

fn extract_keywords(
    query: &str,
    persons: &[String],
    doc_types: &[String],
    dates: &[String],
    structural: &Option<StructSignal>,
) -> Vec<String> {
    let mut q = query.to_lowercase();

    // Strip possessives
    q = q.replace("'s", "").replace('\u{2019}', "");

    // Strip relative date phrases before structural/person/doc-type passes
    q = RELATIVE_DATE_RE.replace_all(&q, " ").to_string();

    // Strip structural phrases to avoid "pages", "words" etc. appearing as keywords
    if structural.is_some() {
        for pat in &[&*STRUCT_GTE, &*STRUCT_GT, &*STRUCT_LTE, &*STRUCT_LT] {
            q = pat.replace_all(&q, " ").to_string();
        }
    }

    // Strip known person tokens
    for p in persons {
        for token in p.split_whitespace() {
            let escaped = regex::escape(&token.to_lowercase());
            if let Ok(re) = Regex::new(&format!(r"\b{escaped}\b")) {
                q = re.replace_all(&q, " ").to_string();
            }
        }
    }

    // Strip doc_type tokens (singular + plural)
    for dt in doc_types {
        let escaped = regex::escape(dt);
        if let Ok(re) = Regex::new(&format!(r"\b{escaped}s?\b")) {
            q = re.replace_all(&q, " ").to_string();
        }
    }

    // Strip date tokens (years, month names)
    for date in dates {
        // Strip year portion
        let year = &date[..4.min(date.len())];
        if let Ok(re) = Regex::new(&format!(r"\b{year}\b")) {
            q = re.replace_all(&q, " ").to_string();
        }
    }
    for (month_name, _) in MONTH_NAMES {
        let escaped = regex::escape(month_name);
        if let Ok(re) = Regex::new(&format!(r"\b{escaped}\b")) {
            q = re.replace_all(&q, " ").to_string();
        }
    }

    // Tokenize, filter stopwords and short tokens, dedup
    let mut seen = std::collections::HashSet::new();
    q.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .filter(|t| t.len() >= MIN_KEYWORD_LEN)
        .filter(|t| !STOPWORDS.contains(t))
        .filter(|t| seen.insert(t.to_string()))
        .map(|t| t.to_string())
        .collect()
}

fn build_ngram_candidates(tokens: &[&str]) -> Vec<String> {
    let mut candidates = Vec::new();
    for n in 1..=3usize {
        for window in tokens.windows(n) {
            candidates.push(window.join(" "));
        }
    }
    candidates
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    // fastembed BGE embeddings are unit-normalized, so dot product == cosine similarity
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn person_signal_possessive() {
        let persons = vec!["Hema".to_string()];
        let signals = parse("Hema's PAN", &persons, &["pan".to_string()]);
        assert_eq!(signals.persons, vec!["Hema"]);
        assert_eq!(signals.doc_types, vec!["pan"]);
    }

    #[test]
    fn date_month_year() {
        let signals = parse("receipts from October 2025", &[], &["receipt".to_string()]);
        assert_eq!(signals.dates, vec!["2025-10"]);
    }

    #[test]
    fn structural_pages() {
        let signals = parse("documents with more than 5 pages", &[], &[]);
        assert!(signals.structural.is_some());
        let s = signals.structural.unwrap();
        assert!(matches!(s.op, StructOp::Gt));
        assert_eq!(s.value, 5);
    }

    #[test]
    fn keyword_extraction() {
        let signals = parse("stamp duty", &[], &[]);
        assert!(signals.keywords.contains(&"stamp".to_string()));
        assert!(signals.keywords.contains(&"duty".to_string()));
    }

    #[test]
    fn relative_date_last_quarter() {
        let signals = parse("invoices from last quarter", &[], &["invoice".to_string()]);
        assert_eq!(signals.dates.len(), 3, "last quarter should produce 3 month prefixes");
        for d in &signals.dates {
            assert!(d.len() == 7 && d.contains('-'), "each date should be YYYY-MM, got {d}");
        }
        assert!(!signals.keywords.iter().any(|k| k == "last" || k == "quarter"),
            "relative date tokens must not leak into keywords: {:?}", signals.keywords);
    }

    #[test]
    fn relative_date_last_month() {
        let signals = parse("show me documents from last month", &[], &[]);
        assert_eq!(signals.dates.len(), 1);
        assert_eq!(signals.dates[0].len(), 7, "should be YYYY-MM, got {}", signals.dates[0]);
        assert!(!signals.keywords.iter().any(|k| k == "last" || k == "month"));
    }

    #[test]
    fn relative_date_this_year() {
        let signals = parse("tax returns this year", &[], &[]);
        assert_eq!(signals.dates.len(), 1);
        assert_eq!(signals.dates[0].len(), 4, "this year should be YYYY, got {}", signals.dates[0]);
    }

    #[test]
    #[ignore] // downloads ~133 MB model on first run
    fn intent_index_new_succeeds() {
        let types = vec!["receipt".to_string(), "agreement".to_string(), "pan".to_string()];
        let result = IntentIndex::new(&types);
        assert!(result.is_ok(), "IntentIndex::new failed: {:?}", result.err());
        let idx = result.unwrap();
        assert_eq!(idx.doc_type_embeddings.len(), 3);
        assert_eq!(idx.threshold, 0.70);
    }

    #[test]
    #[ignore]
    fn invoice_matches_receipt() {
        let types = vec!["receipt".to_string(), "agreement".to_string(), "pan".to_string()];
        let idx = IntentIndex::new(&types).unwrap();
        let sig = idx.parse("show me invoices", &[]);
        assert!(sig.doc_types.contains(&"receipt".to_string()),
            "expected receipt in doc_types, got {:?}", sig.doc_types);
    }

    #[test]
    #[ignore]
    fn contract_matches_agreement() {
        let types = vec!["receipt".to_string(), "agreement".to_string(), "pan".to_string()];
        let idx = IntentIndex::new(&types).unwrap();
        let sig = idx.parse("find old contracts", &[]);
        assert!(sig.doc_types.contains(&"agreement".to_string()),
            "expected agreement in doc_types, got {:?}", sig.doc_types);
    }

    #[test]
    #[ignore]
    fn aadhaar_matches_aadhaar() {
        let types = vec!["receipt".to_string(), "pan".to_string(), "aadhaar".to_string()];
        let idx = IntentIndex::new(&types).unwrap();
        let sig = idx.parse("aadhaar card", &[]);
        assert!(sig.doc_types.contains(&"aadhaar".to_string()),
            "expected aadhaar in doc_types, got {:?}", sig.doc_types);
    }

    #[test]
    #[ignore]
    fn person_date_receipt_full_parse() {
        let types = vec!["receipt".to_string()];
        let persons = vec!["Hema".to_string()];
        let idx = IntentIndex::new(&types).unwrap();
        let sig = idx.parse("last year Hema's receipts", &persons);
        assert!(sig.persons.contains(&"Hema".to_string()));
        assert!(sig.doc_types.contains(&"receipt".to_string()));
        assert_eq!(sig.dates.len(), 1);
        assert_eq!(sig.dates[0].len(), 4); // YYYY
    }

    #[test]
    #[ignore]
    fn unrecognized_query_produces_no_doc_types() {
        let types = vec!["receipt".to_string(), "agreement".to_string()];
        let idx = IntentIndex::new(&types).unwrap();
        let sig = idx.parse("xkcd foobar baz", &[]);
        assert!(sig.doc_types.is_empty(),
            "expected no doc_types for nonsense query, got {:?}", sig.doc_types);
    }

    #[test]
    #[ignore]
    fn id_query_known_gap_below_threshold() {
        // BGESmallENV15: "id" vs aadhaar = 0.6818, vs pan = 0.6576 — both below 0.70.
        // Generic identity-document queries don't bridge to specific doc types at this threshold.
        // Kept as documentation; do not lower the threshold without re-evaluating false-positive risk.
        let types = vec!["pan".to_string(), "aadhaar".to_string()];
        let idx = IntentIndex::new(&types).unwrap();
        let sig = idx.parse("ID document", &[]);
        assert!(sig.doc_types.is_empty(),
            "expected no doc_types for 'ID document' at threshold 0.70, got {:?}", sig.doc_types);
    }

    #[test]
    #[ignore]
    fn stopwords_do_not_produce_false_positives() {
        // "document" scores 0.7063 against "receipt" without filtering — a false positive.
        // Stopword pre-filtering prevents it.
        let types = vec!["receipt".to_string(), "agreement".to_string()];
        let idx = IntentIndex::new(&types).unwrap();
        let sig = idx.parse("show me documents", &[]);
        assert!(sig.doc_types.is_empty(),
            "stopwords like 'document' must not produce false-positive doc_type matches, got {:?}", sig.doc_types);
    }

    #[test]
    fn ngram_candidates_1_to_3() {
        let tokens = vec!["show", "me", "old", "contracts"];
        let candidates = build_ngram_candidates(&tokens);
        // unigrams
        assert!(candidates.contains(&"show".to_string()));
        assert!(candidates.contains(&"contracts".to_string()));
        // bigrams
        assert!(candidates.contains(&"show me".to_string()));
        assert!(candidates.contains(&"old contracts".to_string()));
        // trigrams
        assert!(candidates.contains(&"show me old".to_string()));
        assert!(candidates.contains(&"me old contracts".to_string()));
        // 4-gram excluded
        assert!(!candidates.contains(&"show me old contracts".to_string()));
    }
}
