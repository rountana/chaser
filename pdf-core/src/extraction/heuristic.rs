use once_cell::sync::Lazy;
use regex::Regex;

static DOC_TYPE_PATTERNS: &[(&str, &str)] = &[
    ("aadhaar", "aadhaar"),
    ("passport", "passport"),
    ("bank_statement", "bank statement"),
    ("bank_statement", "statement of account"),
    ("invoice", "invoice"),
    ("w9", "w-9"),
    ("w2", "w-2"),
    ("receipt", "receipt"),
    ("agreement", "agreement"),
    ("agreement", "this agreement"),
    ("deed", "deed of"),
    ("cheque", "cheque"),
    ("cheque", "check no"),
    ("pan", "permanent account number"),
    ("pan", "pan card"),
    ("id_document", "driver's license"),
    ("id_document", "driving licence"),
    ("id_document", "identity card"),
    ("layout", "layout plan"),
    ("oc", "occupancy certificate"),
    ("ecc", "encumbrance certificate"),
    ("khata", "khata"),
];

static MONTH_MAP: &[(&str, &str)] = &[
    ("january","01"),("february","02"),("march","03"),("april","04"),
    ("may","05"),("june","06"),("july","07"),("august","08"),
    ("september","09"),("october","10"),("november","11"),("december","12"),
];

static DATE_ISO: Lazy<Regex> = Lazy::new(||
    Regex::new(r"\b(20\d{2})-(0[1-9]|1[0-2])-(0[1-9]|[12]\d|3[01])\b").unwrap()
);
static DATE_DMY: Lazy<Regex> = Lazy::new(||
    Regex::new(r"\b(0?[1-9]|[12]\d|3[01])/(0?[1-9]|1[0-2])/(20\d{2})\b").unwrap()
);
static DATE_WRITTEN: Lazy<Regex> = Lazy::new(||
    Regex::new(r"(?i)\b(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{1,2}),?\s+(20\d{2})\b").unwrap()
);
static PERSON_NEAR_LABEL: Lazy<Regex> = Lazy::new(||
    Regex::new(r"(?i)(?:Name|To|Issued to|Recipient|Customer|Client|Payee|Employee)\s*[:\.]?\s*([A-Z][a-z]+(?:[ \t]+[A-Z][a-z]+){1,3})").unwrap()
);
static ORG_SUFFIX: Lazy<Regex> = Lazy::new(||
    Regex::new(r"\b([A-Z][A-Za-z\s&,\.]{2,40}(?:Ltd|LLC|Inc|Corp|Co\b|Bank|Trust|Association|Services|Solutions|Group|International|Limited)\.?)").unwrap()
);

pub fn infer_doc_type_from_text(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    for (doc_type, keyword) in DOC_TYPE_PATTERNS {
        if lower.contains(keyword) {
            return Some(doc_type.to_string());
        }
    }
    None
}

pub fn infer_date_from_text(text: &str) -> Option<String> {
    let scan: String = text.chars().take(3000).collect();
    if let Some(c) = DATE_ISO.captures(&scan) {
        return Some(c[0].to_string());
    }
    if let Some(c) = DATE_DMY.captures(&scan) {
        let d = format!("{:02}", c[1].parse::<u32>().unwrap_or(1));
        let m = format!("{:02}", c[2].parse::<u32>().unwrap_or(1));
        return Some(format!("{}-{}-{}", &c[3], m, d));
    }
    if let Some(c) = DATE_WRITTEN.captures(&scan) {
        let month = MONTH_MAP.iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&c[1]))?
            .1;
        let day = format!("{:02}", c[2].parse::<u32>().ok()?);
        return Some(format!("{}-{}-{}", &c[3], month, day));
    }
    None
}

pub fn infer_person_from_text(text: &str) -> Option<String> {
    let scan: String = text.chars().take(3000).collect();
    PERSON_NEAR_LABEL.captures(&scan)
        .and_then(|c| {
            let name = c[1].trim().to_string();
            if name.is_empty() { None } else { Some(name) }
        })
}

pub fn infer_institution_from_text(text: &str) -> Option<String> {
    let scan: String = text.chars().take(3000).collect();
    ORG_SUFFIX.captures(&scan)
        .map(|c| c[1].trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_doc_type_invoice() {
        assert_eq!(infer_doc_type_from_text("INVOICE\nBill To: Acme Corp"), Some("invoice".to_string()));
    }

    #[test]
    fn infer_doc_type_agreement() {
        assert_eq!(infer_doc_type_from_text("THIS AGREEMENT entered into between the parties"), Some("agreement".to_string()));
    }

    #[test]
    fn infer_doc_type_none() {
        assert_eq!(infer_doc_type_from_text("random text with no doc type"), None);
    }

    #[test]
    fn infer_date_iso() {
        assert_eq!(infer_date_from_text("Date: 2025-10-30 details follow"), Some("2025-10-30".to_string()));
    }

    #[test]
    fn infer_date_dmy() {
        assert_eq!(infer_date_from_text("Dated 30/10/2025"), Some("2025-10-30".to_string()));
    }

    #[test]
    fn infer_date_written() {
        assert_eq!(infer_date_from_text("October 30, 2025"), Some("2025-10-30".to_string()));
    }

    #[test]
    fn infer_date_none() {
        assert_eq!(infer_date_from_text("no date here"), None);
    }

    #[test]
    fn infer_person_near_label() {
        let text = "Name: John Smith\nAddress: 123 Main St";
        assert_eq!(infer_person_from_text(text), Some("John Smith".to_string()));
    }

    #[test]
    fn infer_person_none() {
        assert_eq!(infer_person_from_text("no person here"), None);
    }

    #[test]
    fn infer_institution_ltd() {
        let text = "Issued by Acme Solutions Ltd. for the period ending";
        let result = infer_institution_from_text(text);
        assert!(result.is_some(), "expected institution match, got None");
        assert!(result.unwrap().contains("Acme"));
    }
}
