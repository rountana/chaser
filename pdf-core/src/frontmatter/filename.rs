use once_cell::sync::Lazy;
use regex::Regex;

// Date patterns evaluated in order
static DATE_DD_MM_YYYY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\d{2})_(\d{2})_(20\d{2})").unwrap());
static DATE_DDMMYYYY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\d{2})(\d{2})(20\d{2})").unwrap());
static DATE_ISO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(20\d{2}-\d{2}-\d{2})").unwrap());


pub fn extract_date(stem: &str) -> Option<String> {
    let s = stem.replace('-', "_");

    if let Some(caps) = DATE_DD_MM_YYYY.captures(&s) {
        let dd = &caps[1];
        let mm = &caps[2];
        let yyyy = &caps[3];
        return Some(format!("{yyyy}-{mm}-{dd}"));
    }

    if let Some(caps) = DATE_DDMMYYYY.captures(&s) {
        let dd = &caps[1];
        let mm = &caps[2];
        let yyyy = &caps[3];
        // Validate ranges loosely
        let mm_n: u32 = mm.parse().unwrap_or(0);
        let dd_n: u32 = dd.parse().unwrap_or(0);
        if mm_n >= 1 && mm_n <= 12 && dd_n >= 1 && dd_n <= 31 {
            return Some(format!("{yyyy}-{mm}-{dd}"));
        }
    }

    if let Some(caps) = DATE_ISO.captures(stem) {
        return Some(caps[1].to_string());
    }

    None
}

pub fn extract_person(
    stem: &str,
    known_persons: &[String],
    doc_type_tokens: &[String],
) -> Option<String> {
    let lower = stem.to_lowercase();

    // Strip doc_type tokens so they don't pollute person matching
    let mut cleaned = lower.clone();
    for token in doc_type_tokens {
        cleaned = cleaned.replace(token.as_str(), " ");
    }

    // Strip front/back tokens
    cleaned = cleaned.replace("front", " ").replace("back", " ");

    // Split on separators and check against known persons
    let parts: Vec<&str> = cleaned
        .split(|c: char| c == '_' || c == '-' || c == ' ')
        .filter(|s| !s.is_empty())
        .collect();

    for person in known_persons {
        let person_lower = person.to_lowercase();
        for part in &parts {
            if *part == person_lower.as_str() {
                return Some(person.clone());
            }
        }
        // Also check if person name is a substring of the stem
        if cleaned.contains(&person_lower) {
            return Some(person.clone());
        }
    }

    // If no known person matches, return the first capitalized word from stem as candidate
    None
}
