use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::Context;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum FieldType {
    Enum { values: Vec<String>, default: String },
    Date,
    DateRange,
    Person,
    FreeText,
    Currency,
}

#[derive(Debug, Clone)]
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
    pub required: bool,
    pub searchable: bool,
}

#[derive(Debug, Clone)]
pub struct SchemaRegistry {
    pub doc_type_values: Vec<String>,
    pub doc_type_default: String,
    pub global_fields: Vec<FieldDef>,
    /// Per-type fields keyed by doc_type value.
    pub type_fields: HashMap<String, Vec<FieldDef>>,
    /// Non-cryptographic hash of the schema source for change detection.
    pub schema_hash: String,
}

// ---------------------------------------------------------------------------
// TOML deserialization (internal)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawSchema {
    doc_type: RawDocTypeConfig,
    #[serde(default)]
    fields: Vec<RawFieldDef>,
    #[serde(default)]
    types: HashMap<String, HashMap<String, RawInlineField>>,
}

#[derive(Deserialize)]
struct RawDocTypeConfig {
    values: Vec<String>,
    default: String,
}

#[derive(Deserialize)]
struct RawFieldDef {
    name: String,
    #[serde(rename = "type")]
    field_type: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    searchable: bool,
}

#[derive(Deserialize, Default)]
struct RawInlineField {
    #[serde(rename = "type", default)]
    field_type: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    searchable: bool,
}

// ---------------------------------------------------------------------------
// Regex constants for normalisation
// ---------------------------------------------------------------------------

static DATE_ISO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(20\d{2}-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12]\d|3[01]))").unwrap());
static DATE_MDY_SEP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\d{1,2})[/\-](\d{1,2})[/\-](20\d{2})").unwrap());
static DATE_YEAR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(20\d{2})\b").unwrap());
static DATE_DAY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(\d{1,2})\b").unwrap());
static DATE_RANGE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(20\d{2}-\d{2}-\d{2})\s*(?:to|/|-)\s*(20\d{2}-\d{2}-\d{2})").unwrap());

static MONTH_NAMES: &[(&str, &str)] = &[
    ("january", "01"), ("february", "02"), ("march", "03"), ("april", "04"),
    ("may", "05"), ("june", "06"), ("july", "07"), ("august", "08"),
    ("september", "09"), ("october", "10"), ("november", "11"), ("december", "12"),
    ("jan", "01"), ("feb", "02"), ("mar", "03"), ("apr", "04"),
    ("jun", "06"), ("jul", "07"), ("aug", "08"), ("sep", "09"),
    ("oct", "10"), ("nov", "11"), ("dec", "12"),
];

static HONORIFICS: &[&str] = &[
    "mr. ", "mrs. ", "ms. ", "dr. ", "prof. ", "rev. ", "hon. ",
    "mr ", "mrs ", "ms ", "dr ", "prof ",
];

// ---------------------------------------------------------------------------
// SchemaRegistry implementation
// ---------------------------------------------------------------------------

impl SchemaRegistry {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("reading schema from {}", path.display()))?;
        let hash = compute_hash(&data);
        let raw: RawSchema = toml::from_str(&data)
            .with_context(|| format!("parsing schema TOML from {}", path.display()))?;
        Ok(Self::from_raw(raw, hash))
    }

    /// Default filesystem location for schema.toml (config/pdf-lab/schema.toml, relative to CWD).
    pub fn default_config_path() -> PathBuf {
        PathBuf::from("config/pdf-lab/schema.toml")
    }

    /// Load from the default path. Fails if the file does not exist or cannot be parsed.
    pub fn load_default() -> anyhow::Result<Self> {
        Self::load(&Self::default_config_path())
    }

    /// Parse a SchemaRegistry from a TOML string. Intended for unit tests.
    #[cfg(test)]
    pub(crate) fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let raw: RawSchema = toml::from_str(s).context("parsing schema TOML")?;
        Ok(Self::from_raw(raw, "inline".to_string()))
    }

    /// Load from a file path, a directory (first `schema.toml` found, sorted), or the default
    /// config path. Fails with a clear error when no schema can be located or parsed.
    /// Pass `None` to use the default path.
    pub fn load_auto(path: Option<&Path>) -> anyhow::Result<Self> {
        match path {
            None => Self::load_default(),
            Some(p) if p.is_file() => Self::load(p),
            Some(p) if p.is_dir() => {
                let mut found: Vec<std::path::PathBuf> = ignore::WalkBuilder::new(p)
                    .hidden(false)
                    .git_ignore(false)
                    .build()
                    .flatten()
                    .map(|e| e.path().to_path_buf())
                    .filter(|f| f.file_name().and_then(|n| n.to_str()) == Some("schema.toml"))
                    .collect();
                found.sort();
                match found.first() {
                    Some(f) => Self::load(f),
                    None => anyhow::bail!("no schema.toml found under {}", p.display()),
                }
            }
            Some(p) => anyhow::bail!("schema path does not exist: {}", p.display()),
        }
    }

    fn from_raw(raw: RawSchema, schema_hash: String) -> Self {
        let global_fields = raw
            .fields
            .into_iter()
            .map(|f| FieldDef {
                name: f.name,
                field_type: parse_field_type(&f.field_type, &[], ""),
                required: f.required,
                searchable: f.searchable,
            })
            .collect();

        let type_fields: HashMap<String, Vec<FieldDef>> = raw
            .types
            .into_iter()
            .map(|(type_name, fields_map)| {
                let mut fields: Vec<FieldDef> = fields_map
                    .into_iter()
                    .map(|(name, f)| FieldDef {
                        name,
                        field_type: parse_field_type(&f.field_type, &[], ""),
                        required: f.required,
                        searchable: f.searchable,
                    })
                    .collect();
                // Stable ordering: sort by name so effective_fields is deterministic.
                fields.sort_by(|a, b| a.name.cmp(&b.name));
                (type_name, fields)
            })
            .collect();

        // Any [types.X] section implicitly registers X as a known doc type,
        // so [doc_type] values doesn't need to be manually updated when new sections
        // are appended (e.g. by --auto-schema).
        let mut doc_type_values = raw.doc_type.values;
        for key in type_fields.keys() {
            if !doc_type_values.contains(key) {
                doc_type_values.push(key.clone());
            }
        }

        SchemaRegistry {
            doc_type_values,
            doc_type_default: raw.doc_type.default,
            global_fields,
            type_fields,
            schema_hash,
        }
    }

    // -----------------------------------------------------------------------
    // Field resolution
    // -----------------------------------------------------------------------

    /// Global fields + per-type fields for the given doc_type.
    pub fn effective_fields(&self, doc_type: &str) -> Vec<&FieldDef> {
        let mut fields: Vec<&FieldDef> = self.global_fields.iter().collect();
        if let Some(type_fields) = self.type_fields.get(doc_type) {
            fields.extend(type_fields.iter());
        }
        fields
    }

    /// All field names whose type is Person AND searchable = true, across global + all per-type sections.
    pub fn searchable_person_field_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for f in &self.global_fields {
            if matches!(f.field_type, FieldType::Person) && f.searchable {
                names.push(&f.name);
            }
        }
        for fields in self.type_fields.values() {
            for f in fields {
                if matches!(f.field_type, FieldType::Person) && f.searchable {
                    names.push(&f.name);
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }

    /// Field names whose type is `date` (not `date_range`) AND `searchable = true`,
    /// across global + all per-type sections. Used to populate `MetadataIndex` date
    /// filtering; `date_range` fields are intentionally excluded as they are not
    /// used for point-in-time date queries.
    pub fn searchable_date_field_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for f in &self.global_fields {
            if matches!(f.field_type, FieldType::Date) && f.searchable {
                names.push(&f.name);
            }
        }
        for fields in self.type_fields.values() {
            for f in fields {
                if matches!(f.field_type, FieldType::Date) && f.searchable {
                    names.push(&f.name);
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }

    // -----------------------------------------------------------------------
    // Schema mutation (auto-schema)
    // -----------------------------------------------------------------------

    /// Register per-type fields for `doc_type` in memory.
    /// Drops any fields whose names collide with global fields.
    /// Also pushes `doc_type` into `doc_type_values` if absent so coerce_doc_type
    /// can match it in future sessions.
    pub fn add_type_fields(&mut self, doc_type: String, mut fields: Vec<FieldDef>) {
        let global_names: Vec<&str> = self.global_fields.iter().map(|f| f.name.as_str()).collect();
        fields.retain(|f| !global_names.contains(&f.name.as_str()));
        fields.sort_by(|a, b| a.name.cmp(&b.name));
        self.type_fields.insert(doc_type.clone(), fields);
        if !self.doc_type_values.contains(&doc_type) {
            self.doc_type_values.push(doc_type);
        }
    }

    /// Serialize the full registry to a TOML string.
    /// Used when creating a schema file from scratch.
    pub fn to_toml(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();

        // [doc_type]
        let values_str = self.doc_type_values
            .iter()
            .map(|v| format!("\"{v}\""))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(out, "[doc_type]").unwrap();
        writeln!(out, "values  = [{values_str}]").unwrap();
        writeln!(out, "default = \"{}\"", self.doc_type_default).unwrap();

        // [[fields]]
        for f in &self.global_fields {
            writeln!(out).unwrap();
            writeln!(out, "[[fields]]").unwrap();
            writeln!(out, "name       = \"{}\"", f.name).unwrap();
            writeln!(out, "type       = \"{}\"", field_type_str(&f.field_type)).unwrap();
            writeln!(out, "required   = {}", f.required).unwrap();
            writeln!(out, "searchable = {}", f.searchable).unwrap();
        }

        // [types.X]
        let mut type_names: Vec<&str> = self.type_fields.keys().map(|s| s.as_str()).collect();
        type_names.sort();
        for type_name in type_names {
            let fields = &self.type_fields[type_name];
            out.push_str(&render_type_section(type_name, fields));
        }

        out
    }

    /// Append a `[types.{doc_type}]` section to the schema file at `path`.
    /// If the file doesn't exist, writes the full schema instead.
    /// Idempotent: no-ops if the section already exists.
    pub fn append_type_to_file(
        &self,
        path: &std::path::Path,
        doc_type: &str,
        fields: &[FieldDef],
    ) -> anyhow::Result<()> {
        use anyhow::Context as _;

        let section_header = format!("[types.{doc_type}]");
        let tmp_path = path.with_extension("toml.tmp");

        let content = if path.exists() {
            let existing = std::fs::read_to_string(path)
                .with_context(|| format!("reading schema at {}", path.display()))?;
            if existing.contains(&section_header) {
                return Ok(()); // already present
            }
            format!("{}\n{}", existing.trim_end(), render_type_section(doc_type, fields))
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating schema directory {}", parent.display()))?;
            }
            self.to_toml()
        };

        std::fs::write(&tmp_path, &content)
            .with_context(|| format!("writing schema to {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Normalisation
    // -----------------------------------------------------------------------

    pub fn normalise(&self, field: &FieldDef, raw: &str) -> String {
        let trimmed = raw.trim();
        match &field.field_type {
            FieldType::Enum { values, default } => normalise_enum(trimmed, values, default),
            FieldType::Date => normalise_date(trimmed),
            FieldType::DateRange => normalise_date_range(trimmed),
            FieldType::Person => normalise_person(trimmed),
            FieldType::FreeText => trimmed.to_string(),
            FieldType::Currency => normalise_currency(trimmed),
        }
    }

    pub fn normalise_doc_type(&self, raw: &str) -> String {
        normalise_enum(raw.trim(), &self.doc_type_values, &self.doc_type_default)
    }

    // -----------------------------------------------------------------------
    // Filename inference
    // -----------------------------------------------------------------------

    /// Try to infer doc_type from a filename stem using schema values (longest match wins).
    pub fn infer_doc_type_from_stem(&self, stem: &str) -> Option<String> {
        let lower = stem.to_lowercase();
        let mut best: Option<&str> = None;
        for val in &self.doc_type_values {
            if val == "unknown" {
                continue;
            }
            if lower.contains(val.as_str()) {
                if best.map(|b: &str| b.len()).unwrap_or(0) < val.len() {
                    best = Some(val.as_str());
                }
            }
        }
        best.map(|s| s.to_string())
    }

    // -----------------------------------------------------------------------
    // Prompt generation
    // -----------------------------------------------------------------------

    /// One-sentence classification prompt used in Pass 1.
    pub fn build_type_detection_prompt(&self) -> String {
        let values: Vec<&str> = self.doc_type_values
            .iter()
            .filter(|v| v.as_str() != "unknown")
            .map(|s| s.as_str())
            .collect();
        format!(
            "Classify this document. If it matches a known type, reply with exactly one value \
             from this list: {}. If none of those fit, reply with a short snake_case label that \
             best describes the document type (1–3 words joined by underscores, e.g. \
             utility_bill, tax_return, medical_report). Reply with only the type label, nothing else.",
            values.join(" | ")
        )
    }

    /// Match `raw` against known schema doc_type values (fuzzy). If no known value matches,
    /// sanitize the raw string into a snake_case label rather than falling back to "unknown".
    pub fn coerce_doc_type(&self, raw: &str) -> String {
        let matched = normalise_enum(raw.trim(), &self.doc_type_values, "");
        if !matched.is_empty() {
            return matched;
        }
        let s = raw.trim().to_lowercase();
        let s: String = s.chars()
            .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
            .collect();
        let s = s.split('_').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("_");
        if s.is_empty() { "unknown".to_string() } else { s }
    }

    /// Description line for each field used inside the extraction prompt.
    pub fn field_prompt_line(&self, field: &FieldDef) -> String {
        match &field.field_type {
            FieldType::Enum { values, .. } => {
                format!(
                    "- {} (string, {}): one of [{}]",
                    field.name,
                    if field.required { "required" } else { "optional" },
                    values.join(", ")
                )
            }
            FieldType::Date => format!(
                "- {} (string, {}): document date in YYYY-MM-DD, or empty string if absent",
                field.name,
                if field.required { "required" } else { "optional" }
            ),
            FieldType::DateRange => format!(
                "- {} (string, {}): date range as 'YYYY-MM-DD/YYYY-MM-DD', or empty string if absent",
                field.name,
                if field.required { "required" } else { "optional" }
            ),
            FieldType::Person => format!(
                "- {} (string, {}): full name of the person, or empty string if absent",
                field.name,
                if field.required { "required" } else { "optional" }
            ),
            FieldType::Currency => format!(
                "- {} (string, {}): amount as decimal without currency symbol e.g. '1234.56', or empty string if absent",
                field.name,
                if field.required { "required" } else { "optional" }
            ),
            FieldType::FreeText => format!(
                "- {} (string, {}): or empty string if absent",
                field.name,
                if field.required { "required" } else { "optional" }
            ),
        }
    }

    /// JSON Schema property for a field, used to build the submit_extraction tool schema.
    pub fn field_json_schema_property(&self, field: &FieldDef) -> serde_json::Value {
        let description = match &field.field_type {
            FieldType::Enum { values, .. } => {
                format!("One of: {}", values.join(", "))
            }
            FieldType::Date => {
                format!("{} date in YYYY-MM-DD, or empty string if absent", field.name)
            }
            FieldType::DateRange => {
                format!("Date range as YYYY-MM-DD/YYYY-MM-DD, or empty string if absent")
            }
            FieldType::Person => {
                format!("Full name of the person, or empty string if absent")
            }
            FieldType::Currency => {
                format!("Decimal amount without currency symbol e.g. '1234.56', or empty string if absent")
            }
            FieldType::FreeText => {
                format!("{}, or empty string if absent", field.name)
            }
        };
        serde_json::json!({ "type": "string", "description": description })
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn field_type_str(ft: &FieldType) -> &'static str {
    match ft {
        FieldType::Date => "date",
        FieldType::DateRange => "date_range",
        FieldType::Person => "person",
        FieldType::Currency => "currency",
        FieldType::FreeText | FieldType::Enum { .. } => "freetext",
    }
}

fn render_type_section(doc_type: &str, fields: &[FieldDef]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(out, "[types.{doc_type}]").unwrap();
    for f in fields {
        writeln!(
            out,
            "{} = {{ type = \"{}\", required = {}, searchable = {} }}",
            f.name,
            field_type_str(&f.field_type),
            f.required,
            f.searchable,
        ).unwrap();
    }
    out
}

fn parse_field_type(s: &str, _values: &[String], _default: &str) -> FieldType {
    match s {
        "date" => FieldType::Date,
        "date_range" => FieldType::DateRange,
        "person" => FieldType::Person,
        "currency" => FieldType::Currency,
        _ => FieldType::FreeText,
    }
}

fn compute_hash(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:x}", h.finish())
}

// ---------------------------------------------------------------------------
// Normalisation functions
// ---------------------------------------------------------------------------

pub fn normalise_enum(raw: &str, values: &[String], default: &str) -> String {
    if raw.is_empty() || raw == "null" {
        return default.to_string();
    }
    let lower = raw.to_lowercase();

    // Exact match first (handles underscored values like bank_statement)
    if values.iter().any(|v| v == &lower) {
        return lower;
    }

    // Strip non-alphanumeric except space/underscore, then compare
    let cleaned: String = lower
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '_')
        .collect();
    let cleaned = cleaned.trim();
    if values.iter().any(|v| v.as_str() == cleaned) {
        return cleaned.to_string();
    }

    // Longest-match: check if any value is a substring of the input
    let mut best: Option<&str> = None;
    for val in values {
        if val == "unknown" {
            continue;
        }
        let val_norm = val.replace('_', " ");
        if cleaned.contains(val.as_str()) || cleaned.contains(val_norm.as_str()) {
            if best.map(|b: &str| b.len()).unwrap_or(0) < val.len() {
                best = Some(val.as_str());
            }
        }
    }

    best.map(|s| s.to_string()).unwrap_or_else(|| default.to_string())
}

pub fn normalise_date(raw: &str) -> String {
    if raw.is_empty() || raw == "null" {
        return String::new();
    }

    // Already ISO YYYY-MM-DD
    if let Some(m) = DATE_ISO.find(raw) {
        return m.as_str().to_string();
    }

    // MM/DD/YYYY or MM-DD-YYYY (North American format)
    if let Some(caps) = DATE_MDY_SEP.captures(raw) {
        let m: u32 = caps[1].parse().unwrap_or(0);
        let d: u32 = caps[2].parse().unwrap_or(0);
        let y = &caps[3];
        if m >= 1 && m <= 12 && d >= 1 && d <= 31 {
            return format!("{y}-{m:02}-{d:02}");
        }
    }

    let lower = raw.to_lowercase();

    // Natural language: "October 30, 2025" or "30 October 2025"
    for (month_name, month_num) in MONTH_NAMES {
        if lower.contains(month_name) {
            if let Some(year_caps) = DATE_YEAR.captures(raw) {
                let yyyy = &year_caps[1];
                // Try to extract a day number
                let day = DATE_DAY.captures_iter(raw)
                    .filter_map(|c| c[1].parse::<u32>().ok())
                    .find(|&d| d >= 1 && d <= 31 && d.to_string() != yyyy)
                    .map(|d| format!("{d:02}"))
                    .unwrap_or_else(|| "01".to_string());
                return format!("{yyyy}-{month_num}-{day}");
            }
        }
    }

    // Bare year → first day of year
    if let Some(caps) = DATE_YEAR.captures(raw) {
        return format!("{}-01-01", &caps[1]);
    }

    String::new()
}

pub fn normalise_date_range(raw: &str) -> String {
    if raw.is_empty() || raw == "null" {
        return String::new();
    }
    if let Some(caps) = DATE_RANGE.captures(raw) {
        return format!("{}/{}", &caps[1], &caps[2]);
    }
    String::new()
}

pub fn normalise_person(raw: &str) -> String {
    if raw.is_empty() || raw == "null" || raw == "unknown" || raw == "N/A" {
        return String::new();
    }

    let lower = raw.to_lowercase();
    let mut s = raw.to_string();

    // Strip leading honorific (case-insensitive prefix match)
    for honorific in HONORIFICS {
        if lower.starts_with(honorific) {
            s = s[honorific.len()..].trim().to_string();
            break;
        }
    }

    // Title Case
    s.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn normalise_currency(raw: &str) -> String {
    if raw.is_empty() || raw == "null" {
        return String::new();
    }

    // Strip currency symbols and codes
    let cleaned = raw
        .replace('$', "")
        .replace('£', "")
        .replace('€', "")
        .replace("C$", "")
        .replace("USD", "")
        .replace("CAD", "")
        .replace("GBP", "")
        .replace("EUR", "");

    // Keep only digits, dot, dash (for negatives)
    let numeric: String = cleaned
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();

    let trimmed = numeric.trim_matches('-');
    if trimmed.is_empty() || trimmed.parse::<f64>().is_err() {
        return String::new();
    }

    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_schema() -> SchemaRegistry {
        let toml_src = r#"
[doc_type]
values  = ["receipt","invoice","drivers_license","check","w9","unknown"]
default = "unknown"

[[fields]]
name       = "date"
type       = "date"
required   = false
searchable = true

[[fields]]
name       = "person"
type       = "person"
required   = false
searchable = true
"#;
        let raw: RawSchema = toml::from_str(toml_src).expect("test schema should parse");
        SchemaRegistry::from_raw(raw, "test".to_string())
    }

    #[test]
    fn coerce_doc_type_known_value() {
        let s = test_schema();
        // Known values still fuzzy-match to the schema entry
        assert_eq!(s.coerce_doc_type("receipt"), "receipt");
        assert_eq!(s.coerce_doc_type("INVOICE"), "invoice");
        assert_eq!(s.coerce_doc_type("Driver's License"), "drivers_license");
    }

    #[test]
    fn coerce_doc_type_free_text() {
        let s = test_schema();
        // Unknown types come back sanitized, not "unknown"
        assert_eq!(s.coerce_doc_type("utility bill"), "utility_bill");
        assert_eq!(s.coerce_doc_type("Tax Return"), "tax_return");
        assert_eq!(s.coerce_doc_type("medical report"), "medical_report");
    }

    #[test]
    fn coerce_doc_type_empty_falls_back() {
        let s = test_schema();
        assert_eq!(s.coerce_doc_type(""), "unknown");
        assert_eq!(s.coerce_doc_type("!!!"), "unknown");
    }

    #[test]
    fn normalise_date_iso() {
        assert_eq!(normalise_date("2025-10-30"), "2025-10-30");
    }

    #[test]
    fn normalise_date_mdy() {
        assert_eq!(normalise_date("10/30/2025"), "2025-10-30");
    }

    #[test]
    fn normalise_date_natural() {
        let d = normalise_date("October 30, 2025");
        assert_eq!(d, "2025-10-30");
    }

    #[test]
    fn normalise_person_strips_honorific() {
        assert_eq!(normalise_person("Mr. John Smith"), "John Smith");
        assert_eq!(normalise_person("Dr. Sarah Connor"), "Sarah Connor");
        assert_eq!(normalise_person("Hon. Robert Lee"), "Robert Lee");
    }

    #[test]
    fn normalise_currency_strips_symbol() {
        assert_eq!(normalise_currency("$1,234.56"), "1234.56");
        assert_eq!(normalise_currency("USD 500.00"), "500.00");
        assert_eq!(normalise_currency("C$2,000"), "2000");
    }

    #[test]
    fn normalise_enum_soft_match() {
        let values: Vec<String> = vec!["drivers_license", "invoice", "bank_statement", "unknown"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(normalise_enum("Driver's License", &values, "unknown"), "drivers_license");
        assert_eq!(normalise_enum("Bank Statement", &values, "unknown"), "bank_statement");
        assert_eq!(normalise_enum("foobar", &values, "unknown"), "unknown");
    }

    #[test]
    fn schema_has_expected_global_fields_and_doc_types() {
        let s = test_schema();
        assert!(s.global_fields.iter().any(|f| f.name == "person"));
        assert!(s.global_fields.iter().any(|f| f.name == "date"));
        assert!(s.infer_doc_type_from_stem("John_W9_2025.pdf").is_some());
    }

    #[test]
    fn toml_loading_with_per_type_fields() {
        let toml_src = r#"
[doc_type]
values  = ["drivers_license","w9","invoice","bank_statement","unknown"]
default = "unknown"

[[fields]]
name       = "date"
type       = "date"
required   = false
searchable = true

[[fields]]
name       = "person"
type       = "person"
required   = false
searchable = true

[types.invoice]
vendor = { type = "freetext", required = true,  searchable = true }
amount = { type = "currency", required = false, searchable = false }

[types.bank_statement]
account_holder = { type = "person",   required = false, searchable = true }
account_number = { type = "freetext", required = false, searchable = false }
"#;
        let raw: RawSchema = toml::from_str(toml_src).expect("toml should parse");
        let schema = SchemaRegistry::from_raw(raw, "test".to_string());

        assert_eq!(schema.doc_type_values, vec!["drivers_license","w9","invoice","bank_statement","unknown"]);
        assert_eq!(schema.global_fields.len(), 2);

        let invoice_fields = schema.effective_fields("invoice");
        assert_eq!(invoice_fields.len(), 4); // date, person (global) + vendor, amount (per-type)
        assert!(invoice_fields.iter().any(|f| f.name == "vendor" && f.required));
        assert!(invoice_fields.iter().any(|f| f.name == "amount"));

        let bank_fields = schema.effective_fields("bank_statement");
        assert_eq!(bank_fields.len(), 4); // date, person (global) + account_holder, account_number
        assert!(bank_fields.iter().any(|f| f.name == "account_holder"));

        // pan has no per-type fields — only global fields
        let pan_fields = schema.effective_fields("pan");
        assert_eq!(pan_fields.len(), 2);

        // Normalisation
        assert_eq!(schema.normalise_doc_type("Bank Statement"), "bank_statement");
        assert_eq!(schema.normalise_doc_type("INVOICE"), "invoice");
        assert_eq!(schema.normalise_doc_type("garbage"), "unknown");
    }

    #[test]
    fn searchable_person_field_names_excludes_non_searchable() {
        let toml_src = r#"
[doc_type]
values  = ["invoice","unknown"]
default = "unknown"

[[fields]]
name       = "person"
type       = "person"
required   = false
searchable = true

[types.invoice]
vendor         = { type = "freetext", required = false, searchable = true }
contact        = { type = "person",   required = false, searchable = false }
account_holder = { type = "person",   required = false, searchable = true }
"#;
        let raw: RawSchema = toml::from_str(toml_src).expect("toml should parse");
        let schema = SchemaRegistry::from_raw(raw, "test".to_string());

        let names = schema.searchable_person_field_names();
        assert!(names.contains(&"person"), "global searchable person should be included");
        assert!(names.contains(&"account_holder"), "type-level searchable person should be included");
        assert!(!names.contains(&"contact"), "non-searchable person should be excluded");
        assert_eq!(names.len(), 2, "only the 2 searchable person fields should be returned");
    }

    #[test]
    fn searchable_date_field_names_excludes_non_searchable() {
        let toml_src = r#"
[doc_type]
values  = ["invoice","unknown"]
default = "unknown"

[[fields]]
name       = "date"
type       = "date"
required   = false
searchable = true

[types.invoice]
invoice_date = { type = "date", required = false, searchable = false }
due_date     = { type = "date", required = false, searchable = false }
"#;
        let raw: RawSchema = toml::from_str(toml_src).expect("toml should parse");
        let schema = SchemaRegistry::from_raw(raw, "test".to_string());

        let names = schema.searchable_date_field_names();
        assert!(names.contains(&"date"), "global searchable date should be included");
        assert!(!names.contains(&"invoice_date"), "non-searchable invoice_date should be excluded");
        assert!(!names.contains(&"due_date"), "non-searchable due_date should be excluded");
        assert_eq!(names.len(), 1, "only the 1 searchable date field should be returned");
    }

    #[test]
    fn infer_doc_type_from_stem_extended() {
        let schema = test_schema();
        assert_eq!(schema.infer_doc_type_from_stem("Receipt_10_30_2025"), Some("receipt".to_string()));
        assert_eq!(schema.infer_doc_type_from_stem("Alex_Carter_Drivers_License"), Some("drivers_license".to_string()));
        assert_eq!(schema.infer_doc_type_from_stem("Voided_Check_Chase"), Some("check".to_string()));
        assert_eq!(schema.infer_doc_type_from_stem("WhatsApp Image 2026"), None);
    }
}
