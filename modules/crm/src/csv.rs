//! The contact and company CSV the screens import and export (docs/requests/REQ-051, slice 2).
//!
//! Import is the half that has to be careful, so it is worth saying what it refuses and why:
//!
//! * **The header decides the mapping.** A caller sends columns in any order, with its own
//!   names; the row answers which of its columns fed which field. A file whose first line names
//!   no known field is refused outright rather than imported as a list of empty rows.
//! * **A dry run writes nothing.** It parses every row, reports what each one would do and which
//!   ones it refuses, and the same file committed afterwards writes exactly the rows the preview
//!   accepted. A preview that is not the commit would make the preview a decoration.
//! * **A row is refused, not the file.** One malformed address does not discard the other 199
//!   rows, and the answer names the line number so a person can fix it. The commit reports how
//!   many rows it wrote *and* how many it skipped, so nothing is silently dropped.
//! * **The address stays unique.** A row whose address another row of the *same* file already
//!   used is refused here rather than turned into a `409` halfway through the import.
//!
//! The parser is deliberately hand-written and small: an RFC 4180 reader is a well-understood
//! hundred lines, and pulling a dependency for it would add a build-time cost to every crate in
//! the workspace for one screen's worth of behaviour. Its tests cover the awkward cases a real
//! export contains — a quoted comma, an embedded newline, a doubled quote, a UTF-8 BOM, CRLF
//! endings and a ragged row.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::contacts::{Company, Contact};
use crate::error::{CrmError, Result};
use crate::model::STATUSES;

/// Longest a CSV the import accepts, in bytes. 10 MB is ~60k rows of contacts, which is more
/// than any hand-maintained file and small enough that a single request cannot exhaust the API.
pub const MAX_IMPORT_BYTES: usize = 10 * 1024 * 1024;

/// Rows one import may write.
pub const MAX_IMPORT_ROWS: usize = 5_000;

/// The contact columns the import reads and the export writes, in file order.
pub const CONTACT_COLUMNS: [&str; 11] = [
    "first_name",
    "last_name",
    "email",
    "phone",
    "job_title",
    "company",
    "status",
    "tags",
    "notes",
    "owner",
    "custom",
];

/// The company columns.
pub const COMPANY_COLUMNS: [&str; 8] = [
    "name",
    "domain",
    "industry",
    "status",
    "tags",
    "notes",
    "owner",
    "custom",
];

/// Header names a file may use for a contact field, beyond the canonical one.
///
/// A person exports from a spreadsheet, not from Omnion: `First Name`, `first-name` and `First
/// name` are the same column. The table is the whole tolerance; a header that is not in it is
/// simply not mapped, and the preview says so rather than guessing.
const CONTACT_ALIASES: [(&str, &str); 16] = [
    ("firstname", "first_name"),
    ("first name", "first_name"),
    ("first-name", "first_name"),
    ("given name", "first_name"),
    ("lastname", "last_name"),
    ("last name", "last_name"),
    ("last-name", "last_name"),
    ("surname", "last_name"),
    ("family name", "last_name"),
    ("e-mail", "email"),
    ("e mail", "email"),
    ("mail", "email"),
    ("phone number", "phone"),
    ("mobile", "phone"),
    ("company name", "company"),
    ("job", "job_title"),
];

/// Header names a file may use for a company field.
const COMPANY_ALIASES: [(&str, &str); 8] = [
    ("company", "name"),
    ("company name", "name"),
    ("website", "domain"),
    ("url", "domain"),
    ("web address", "domain"),
    ("sector", "industry"),
    ("note", "notes"),
    ("owner name", "owner"),
];

/// What the import resolved the header row to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    /// For every canonical field: the zero-based column it came from.
    pub columns: Vec<(String, usize)>,
    /// The headers the file carried that no field claims.
    pub ignored: Vec<String>,
    /// Headers that named the same field more than once.
    pub duplicates: Vec<String>,
}

impl Mapping {
    /// The column a field was mapped to, when the file had one.
    #[must_use]
    pub fn column_of(&self, field: &str) -> Option<usize> {
        self.columns
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, index)| *index)
    }
}

/// The values of one row, already trimmed — what validation runs on.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ContactChangesView {
    /// Given name.
    pub first_name: String,
    /// Family name.
    pub last_name: String,
    /// Address.
    pub email: Option<String>,
    /// Phone number.
    pub phone: Option<String>,
    /// Job title.
    pub job_title: Option<String>,
    /// Company name, matched or created.
    pub company: Option<String>,
    /// Status.
    pub status: Option<String>,
    /// Tags.
    pub tags: Vec<String>,
    /// Note.
    pub notes: Option<String>,
    /// Owner name.
    pub owner: Option<String>,
    /// Custom values, parsed as JSON when the cell holds an object.
    pub custom: Option<Value>,
}

/// A company row of the import.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompanyChangesView {
    /// Display name.
    pub name: String,
    /// Web domain.
    pub domain: Option<String>,
    /// Industry.
    pub industry: Option<String>,
    /// Status.
    pub status: Option<String>,
    /// Tags.
    pub tags: Vec<String>,
    /// Note.
    pub notes: Option<String>,
    /// Owner name.
    pub owner: Option<String>,
    /// Custom values.
    pub custom: Option<Value>,
}

/// One refused row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RowError {
    /// One-based line in the file.
    pub line: usize,
    /// The field that failed, when the refusal names one.
    pub field: Option<String>,
    /// The sentence a person reads.
    pub message: String,
}

impl RowError {
    /// The JSON the import preview carries.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "line": self.line,
            "field": self.field,
            "message": self.message,
        })
    }
}

/// The whole answer of one import: the mapping, and every row with its verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportPreview {
    /// Which file column fed which field.
    pub mapping: Mapping,
    /// Rows parsed, header included.
    pub total_rows: usize,
    /// Rows the import would write.
    pub valid_rows: usize,
    /// Rows it refuses, with the line number and the field.
    pub errors: Vec<RowError>,
    /// The first rows' parsed values, so the screen can show what it read.
    pub sample: Vec<ContactChangesView>,
}

impl ImportPreview {
    /// A short summary line the screen prints above the table.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.total_rows == 0 {
            return "The file has a header but no rows.".to_owned();
        }
        if self.errors.is_empty() {
            return format!(
                "{} row{} ready to import.",
                self.valid_rows,
                if self.valid_rows == 1 { "" } else { "s" }
            );
        }
        format!(
            "{} of {} row{} ready, {} refused.",
            self.valid_rows,
            self.total_rows,
            if self.total_rows == 1 { "" } else { "s" },
            self.errors.len()
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Reading the file
// ---------------------------------------------------------------------------------------------

/// One parsed CSV row: its cells, and the line it started on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvRow {
    /// One-based line in the file.
    pub line: usize,
    /// The cells, with the quotes removed.
    pub cells: Vec<String>,
}

/// Parse a CSV document into its rows.
///
/// Quoted fields may hold commas, newlines and doubled quotes; a UTF-8 BOM and CRLF endings are
/// normal in a spreadsheet export and are stripped rather than refused.
pub fn parse_csv(input: &str) -> Vec<CsvRow> {
    let text = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut rows: Vec<CsvRow> = Vec::new();
    let mut cells: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let mut line = 1usize;
    let mut row_line = 1usize;
    let mut started = false;

    while let Some(ch) = chars.next() {
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    // A doubled quote inside a quoted field is one quote.
                    chars.next();
                    cell.push('"');
                } else {
                    quoted = false;
                }
            } else {
                if ch == '\n' {
                    line += 1;
                }
                cell.push(ch);
            }
            continue;
        }

        match ch {
            '"' if cell.is_empty() => {
                quoted = true;
                started = true;
            }
            ',' => {
                cells.push(std::mem::take(&mut cell));
                started = true;
            }
            '\r' => {}
            '\n' => {
                cells.push(std::mem::take(&mut cell));
                if started || cells.iter().any(|value| !value.is_empty()) {
                    rows.push(CsvRow {
                        line: row_line,
                        cells: std::mem::take(&mut cells),
                    });
                }
                started = false;
                line += 1;
                row_line = line;
            }
            other => {
                cell.push(other);
                started = true;
            }
        }
    }

    if started || !cell.is_empty() {
        cells.push(cell);
        rows.push(CsvRow {
            line: row_line,
            cells,
        });
    }

    rows
}

/// Read the header row and resolve it to the fields the import writes.
pub fn map_header(entity: &str, header: &[String]) -> Result<Mapping> {
    let (fields, aliases) = match entity {
        "companies" => (&COMPANY_COLUMNS[..], &COMPANY_ALIASES[..]),
        _ => (&CONTACT_COLUMNS[..], &CONTACT_ALIASES[..]),
    };

    let mut columns: Vec<(String, usize)> = Vec::new();
    let mut ignored: Vec<String> = Vec::new();
    let mut duplicates: Vec<String> = Vec::new();

    for (index, raw) in header.iter().enumerate() {
        match canonical_header(raw, fields, aliases) {
            Some(field) if columns.iter().any(|(existing, _)| existing == &field) => {
                duplicates.push(raw.trim().to_owned());
            }
            Some(field) => columns.push((field, index)),
            None => ignored.push(raw.trim().to_owned()),
        }
    }

    if columns.is_empty() {
        return Err(CrmError::invalid(
            if entity == "companies" { "company" } else { "contact" },
            "file",
            format!(
                "no column of the header names a {entity} field. Expected at least one of: {}",
                fields.join(", ")
            ),
        ));
    }

    Ok(Mapping {
        columns,
        ignored,
        duplicates,
    })
}

/// The field a header cell names, if any: the canonical name, an alias of it, or nothing.
fn canonical_header(raw: &str, fields: &[&str], aliases: &[(&str, &str)]) -> Option<String> {
    let cleaned = raw
        .trim()
        .trim_start_matches('\u{feff}')
        .to_lowercase()
        .replace(['_', '-'], " ");
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");

    if let Some(field) = fields.iter().find(|field| field.replace('_', " ") == cleaned) {
        return Some((*field).to_owned());
    }
    aliases
        .iter()
        .find(|(alias, _)| *alias == cleaned)
        .map(|(_, field)| (*field).to_owned())
}

/// Read every row of a contact file and say what the import would do with it.
///
/// No row is written here — the caller runs the same function on `mode=commit`, so a preview and
/// a commit cannot disagree.
pub fn preview_contacts(input: &str) -> Result<ImportPreview> {
    let rows = parse_csv(input);
    let Some(header) = rows.first() else {
        return Err(CrmError::invalid(
            "contact",
            "file",
            "the file is empty — a CSV needs a header row",
        ));
    };

    if rows.len() > MAX_IMPORT_ROWS {
        return Err(CrmError::invalid(
            "contact",
            "file",
            format!("a file may hold at most {MAX_IMPORT_ROWS} rows"),
        ));
    }

    let mapping = map_header("contacts", &header.cells)?;
    let mut errors: Vec<RowError> = Vec::new();
    let mut sample: Vec<ContactChangesView> = Vec::new();
    let mut valid_rows = 0usize;
    // An address the same file already used is refused here, so the commit cannot fail halfway
    // through on a `409` the preview promised would not happen.
    let mut seen_emails: Vec<String> = Vec::new();

    for row in rows.iter().skip(1) {
        if row.cells.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }

        let view = read_contact_row(&mapping, &row.cells);
        match validate_imported_contact(&view) {
            Ok(()) => {
                if let Some(address) = view.email.as_deref() {
                    let key = email_key(address);
                    if seen_emails.contains(&key) {
                        errors.push(RowError {
                            line: row.line,
                            field: Some("email".to_owned()),
                            message: format!(
                                "line {} repeats {address}, which an earlier row already used",
                                row.line
                            ),
                        });
                        continue;
                    }
                    seen_emails.push(key);
                }
                valid_rows += 1;
                if sample.len() < 5 {
                    sample.push(view);
                }
            }
            Err((field, message)) => errors.push(RowError {
                line: row.line,
                field: Some(field.to_owned()),
                message,
            }),
        }
    }

    Ok(ImportPreview {
        mapping,
        total_rows: rows.len().saturating_sub(1),
        valid_rows,
        errors,
        sample,
    })
}

/// Validate one imported contact with the module's own rules.
fn validate_imported_contact(
    view: &ContactChangesView,
) -> std::result::Result<(), (&'static str, String)> {
    let first_name = view.first_name.trim();
    if first_name.is_empty() {
        return Err(("first_name", "a contact needs a first name".to_owned()));
    }
    if first_name.chars().count() > 80 {
        return Err((
            "first_name",
            "a first name is at most 80 characters".to_owned(),
        ));
    }
    if view.last_name.chars().count() > 80 {
        return Err((
            "last_name",
            "a last name is at most 80 characters".to_owned(),
        ));
    }
    if let Some(address) = view.email.as_deref()
        && !crate::model::is_email(address)
    {
        return Err(("email", "that is not an e-mail address".to_owned()));
    }
    if let Some(number) = view.phone.as_deref()
        && !crate::model::is_phone(number)
    {
        return Err((
            "phone",
            "a phone number is 7–20 digits, spaces, brackets and dashes".to_owned(),
        ));
    }
    if let Some(status) = view.status.as_deref()
        && !STATUSES.contains(&status.to_lowercase().as_str())
    {
        return Err((
            "status",
            format!("unknown status — use one of: {}", STATUSES.join(", ")),
        ));
    }
    if view.tags.len() > crate::model::MAX_TAGS {
        return Err((
            "tags",
            format!("at most {} tags per contact", crate::model::MAX_TAGS),
        ));
    }
    if view
        .notes
        .as_deref()
        .is_some_and(|notes| notes.chars().count() > crate::model::MAX_NOTES_LENGTH)
    {
        return Err((
            "notes",
            format!(
                "a note is at most {} characters",
                crate::model::MAX_NOTES_LENGTH
            ),
        ));
    }
    Ok(())
}

/// Read one contact row through the mapping.
fn read_contact_row(mapping: &Mapping, cells: &[String]) -> ContactChangesView {
    let get = |field: &str| -> Option<String> {
        mapping
            .column_of(field)
            .and_then(|index| cells.get(index))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };

    ContactChangesView {
        first_name: get("first_name").unwrap_or_default(),
        last_name: get("last_name").unwrap_or_default(),
        email: get("email"),
        phone: get("phone"),
        job_title: get("job_title"),
        company: get("company"),
        status: get("status").map(|value| value.to_lowercase()),
        tags: split_tags(get("tags").as_deref()),
        notes: get("notes"),
        owner: get("owner"),
        custom: get("custom").and_then(|raw| serde_json::from_str::<Value>(&raw).ok()),
    }
}

/// Read one company row through the mapping.
pub fn read_company_row(mapping: &Mapping, cells: &[String]) -> CompanyChangesView {
    let get = |field: &str| -> Option<String> {
        mapping
            .column_of(field)
            .and_then(|index| cells.get(index))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };

    CompanyChangesView {
        name: get("name").unwrap_or_default(),
        domain: get("domain").map(|value| value.to_lowercase()),
        industry: get("industry"),
        status: get("status").map(|value| value.to_lowercase()),
        tags: split_tags(get("tags").as_deref()),
        notes: get("notes"),
        owner: get("owner"),
        custom: get("custom").and_then(|raw| serde_json::from_str::<Value>(&raw).ok()),
    }
}

/// A tag cell.
///
/// Two shapes are read, because two kinds of file exist:
///
/// * **A JSON array** — what Omnion's own export writes. A tag may itself contain a space, so a
///   separator-joined list cannot round-trip: `"vip emea"` written as `vip emea` reads back as
///   one long tag, and a two-tag list read as one is data loss the person would only notice
///   months later. The array is the lossless shape, and it is the one the export uses.
/// * **A separated list** — what a spreadsheet produces, where a person typed `a, b` or `a;b`.
///   Splitting on the separators is the only way to read that, and a tag that contains a comma is
///   a spreadsheet problem, not an Omnion one.
fn split_tags(raw: Option<&str>) -> Vec<String> {
    let Some(value) = raw else {
        return Vec::new();
    };
    let trimmed = value.trim();

    if let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(trimmed) {
        let list: Vec<String> = entries
            .into_iter()
            .filter_map(|entry| entry.as_str().map(str::to_owned))
            .collect();
        return crate::model::normalise_tags(&list);
    }

    crate::model::normalise_tags(
        &trimmed
            .split([',', ';', '|'])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>(),
    )
}

/// The tag list as one cell: a JSON array, because it is the only shape that survives the trip.
fn tags_to_cell(tags: &[String]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    Value::Array(tags.iter().map(|tag| Value::String(tag.clone())).collect()).to_string()
}

// ---------------------------------------------------------------------------------------------
// Writing the file
// ---------------------------------------------------------------------------------------------

/// One CSV field: quoted when it has to be, with inner quotes doubled (RFC 4180).
#[must_use]
pub fn csv_field(value: &str) -> String {
    let needs_quotes = value
        .chars()
        .any(|ch| matches!(ch, ',' | '"' | '\n' | '\r'))
        || value.starts_with(' ')
        || value.ends_with(' ');
    if !needs_quotes {
        return value.to_owned();
    }
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// The contact export: the header, then one line per row in the order the list showed them.
///
/// The columns are the canonical names, so a file this writes imports without a mapping
/// step — a round trip has to be lossless or the export is a dead end.
#[must_use]
pub fn contacts_to_csv(rows: &[Contact]) -> String {
    let mut body = CONTACT_COLUMNS.join(",");
    body.push('\n');
    for contact in rows {
        body.push_str(
            &[
                csv_field(&contact.first_name),
                csv_field(&contact.last_name),
                csv_field(contact.email.as_deref().unwrap_or_default()),
                csv_field(contact.phone.as_deref().unwrap_or_default()),
                csv_field(contact.job_title.as_deref().unwrap_or_default()),
                csv_field(contact.company_name.as_deref().unwrap_or_default()),
                csv_field(&contact.status),
                csv_field(&tags_to_cell(&contact.tags)),
                csv_field(&contact.notes),
                csv_field(contact.owner_name.as_deref().unwrap_or_default()),
                csv_field(&custom_to_cell(&contact.custom)),
            ]
            .join(","),
        );
        body.push('\n');
    }
    body
}

/// The company export.
#[must_use]
pub fn companies_to_csv(rows: &[Company]) -> String {
    let mut body = COMPANY_COLUMNS.join(",");
    body.push('\n');
    for company in rows {
        body.push_str(
            &[
                csv_field(&company.name),
                csv_field(company.domain.as_deref().unwrap_or_default()),
                csv_field(company.industry.as_deref().unwrap_or_default()),
                csv_field(&company.status),
                csv_field(&tags_to_cell(&company.tags)),
                csv_field(&company.notes),
                csv_field(company.owner_name.as_deref().unwrap_or_default()),
                csv_field(&custom_to_cell(&company.custom)),
            ]
            .join(","),
        );
        body.push('\n');
    }
    body
}

/// A custom object as one cell: JSON when it holds something, empty when it does not.
fn custom_to_cell(custom: &Value) -> String {
    match custom {
        Value::Object(fields) if fields.is_empty() => String::new(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The address identity a duplicate check uses.
#[must_use]
pub fn email_key(address: &str) -> String {
    address.trim().to_lowercase()
}

/// The company identity a name lookup uses.
#[must_use]
pub fn company_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// `true` when a mapped company name is one the import may create on the fly.
#[must_use]
pub fn is_importable_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= 200
}

/// The rows an import may still write, with the ones the preview refused removed.
///
/// The **same file text** the preview ran on: the commit re-reads it and drops exactly the lines
/// the preview refused, so what gets written is what the person agreed to.
#[must_use]
pub fn committable_rows(input: &str, preview: &ImportPreview) -> Vec<(usize, ContactChangesView)> {
    let refused: Vec<usize> = preview.errors.iter().map(|error| error.line).collect();
    parse_csv(input)
        .iter()
        .skip(1)
        .filter(|row| {
            !refused.contains(&row.line) && row.cells.iter().any(|cell| !cell.trim().is_empty())
        })
        .map(|row| (row.line, read_contact_row(&preview.mapping, &row.cells)))
        .collect()
}

/// The rows of a company file, with the refused ones removed.
#[must_use]
pub fn committable_company_rows(
    input: &str,
    preview: &ImportPreview,
) -> Vec<(usize, CompanyChangesView)> {
    let refused: Vec<usize> = preview.errors.iter().map(|error| error.line).collect();
    parse_csv(input)
        .iter()
        .skip(1)
        .filter(|row| {
            !refused.contains(&row.line) && row.cells.iter().any(|cell| !cell.trim().is_empty())
        })
        .map(|row| (row.line, read_company_row(&preview.mapping, &row.cells)))
        .collect()
}

/// The identifier of a record an imported row points at, when it has one.
#[must_use]
pub fn owner_id_of(known: &[(String, Uuid)], name: Option<&str>) -> Option<Uuid> {
    let name = name?.trim();
    known
        .iter()
        .find(|(label, _)| label.as_str().eq_ignore_ascii_case(name))
        .map(|(_, id)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A contact to write into an export.
    fn contact_fixture() -> Contact {
        Contact {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
            display_name: "Ada Lovelace".to_owned(),
            initials: "AL".to_owned(),
            email: Some("ada@example.com".to_owned()),
            phone: Some("+44 20 7946 0000".to_owned()),
            job_title: Some("Analyst".to_owned()),
            company_id: None,
            company_name: Some("Analytical Engines".to_owned()),
            owner_user_id: None,
            owner_name: Some("Furkan".to_owned()),
            status: "lead".to_owned(),
            tags: vec!["vip".to_owned(), "emea".to_owned()],
            custom: json!({}),
            notes: String::new(),
            last_activity_at: None,
            archived_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_plain_file_parses_into_its_cells() {
        let rows = parse_csv("first_name,last_name\nAda,Lovelace\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].line, 2);
        assert_eq!(rows[1].cells, vec!["Ada".to_owned(), "Lovelace".to_owned()]);
    }

    #[test]
    fn a_quoted_field_may_hold_a_comma_a_newline_and_a_doubled_quote() {
        let rows = parse_csv("first_name,notes\nAda,\"called, then\nwrote \"\"yes\"\"\"\n");
        assert_eq!(
            rows.len(),
            2,
            "the newline inside the quotes is not a row break"
        );
        assert_eq!(rows[1].cells[0], "Ada");
        assert_eq!(rows[1].cells[1], "called, then\nwrote \"yes\"");
    }

    #[test]
    fn a_bom_and_crlf_endings_are_stripped_not_refused() {
        let rows = parse_csv("\u{feff}first_name,last_name\r\nAda,Lovelace\r\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].cells[0], "first_name");
        assert_eq!(rows[1].cells[1], "Lovelace");
    }

    #[test]
    fn a_ragged_row_is_short_not_a_crash() {
        let rows = parse_csv("first_name,last_name,email\nAda\n");
        assert_eq!(rows[1].cells.len(), 1);
    }

    #[test]
    fn a_trailing_newline_does_not_add_a_row() {
        let rows = parse_csv("first_name\nAda\n");
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn the_header_maps_through_its_aliases_and_its_canonical_names() {
        let mapping = map_header(
            "contacts",
            &[
                "First Name".to_owned(),
                "Surname".to_owned(),
                "E-Mail".to_owned(),
            ],
        )
        .expect("the header maps");
        assert_eq!(mapping.column_of("first_name"), Some(0));
        assert_eq!(mapping.column_of("last_name"), Some(1));
        assert_eq!(mapping.column_of("email"), Some(2));
    }

    #[test]
    fn a_snake_case_header_is_the_canonical_name() {
        let mapping = map_header(
            "contacts",
            &["first_name".to_owned(), "last_name".to_owned()],
        )
        .unwrap();
        assert_eq!(mapping.column_of("first_name"), Some(0));
        assert_eq!(mapping.column_of("last_name"), Some(1));
    }

    #[test]
    fn a_header_that_names_no_field_is_refused_by_name() {
        let error = map_header("contacts", &["colour".to_owned(), "size".to_owned()])
            .expect_err("a header with no known field is refused");
        assert!(error.to_string().contains("first_name"), "{error}");
    }

    #[test]
    fn a_duplicated_column_is_reported_and_the_first_one_wins() {
        let mapping = map_header(
            "contacts",
            &["first_name".to_owned(), "first name".to_owned()],
        )
        .unwrap();
        assert_eq!(mapping.column_of("first_name"), Some(0));
        assert_eq!(mapping.duplicates, vec!["first name".to_owned()]);
    }

    #[test]
    fn an_unknown_column_is_reported_as_ignored() {
        let mapping = map_header(
            "contacts",
            &["first_name".to_owned(), "internal score".to_owned()],
        )
        .unwrap();
        assert_eq!(mapping.ignored, vec!["internal score".to_owned()]);
    }

    #[test]
    fn a_dry_run_reports_the_good_rows_and_the_refused_one_with_its_line() {
        let file = "first_name,last_name,email\n\
                    Ada,Lovelace,ada@example.com\n\
                    Grace,Hopper,not-an-address\n\
                    Alan,Turing,alan@example.com\n";
        let preview = preview_contacts(file).expect("the file is read");
        assert_eq!(preview.total_rows, 3);
        assert_eq!(preview.valid_rows, 2);
        assert_eq!(preview.errors.len(), 1);
        assert_eq!(preview.errors[0].line, 3, "the header is line 1");
        assert_eq!(preview.errors[0].field.as_deref(), Some("email"));
        assert!(
            preview.summary().contains("2 of 3"),
            "{}",
            preview.summary()
        );
    }

    #[test]
    fn a_nameless_row_is_refused_on_its_first_name() {
        let file = "first_name,email\n,ada@example.com\n";
        let preview = preview_contacts(file).unwrap();
        assert_eq!(preview.valid_rows, 0);
        assert_eq!(preview.errors[0].field.as_deref(), Some("first_name"));
    }

    #[test]
    fn an_address_the_same_file_uses_twice_is_refused_before_the_commit() {
        let file = "first_name,email\nAda,ada@example.com\nAda Again,ADA@example.com\n";
        let preview = preview_contacts(file).unwrap();
        assert_eq!(preview.valid_rows, 1);
        assert_eq!(preview.errors.len(), 1);
        assert_eq!(preview.errors[0].line, 3);
    }

    #[test]
    fn the_commit_writes_exactly_the_rows_the_preview_accepted() {
        let file = "first_name,last_name,email\n\
                    Ada,Lovelace,ada@example.com\n\
                    Grace,Hopper,nope\n\
                    Alan,Turing,alan@example.com\n";
        let preview = preview_contacts(file).unwrap();
        let committed = committable_rows(file, &preview);
        // Line 3 is the one the preview refused; the other two are written.
        assert!(committed.iter().all(|(line, _)| *line != 3));
        assert_eq!(committed.len(), 2);
        assert_eq!(committed[0].0, 2);
        assert_eq!(committed[1].0, 4);
    }

    #[test]
    fn a_committed_contact_carries_the_values_the_preview_parsed() {
        let file = "First Name,Surname,E-Mail,Company Name\nAda,Lovelace,ada@example.com,Engines\n";
        let preview = preview_contacts(file).unwrap();
        let committed = committable_rows(file, &preview);
        assert_eq!(committed.len(), 1);
        let (line, view) = &committed[0];
        assert_eq!(*line, 2);
        assert_eq!(view.first_name, "Ada");
        assert_eq!(view.last_name, "Lovelace");
        assert_eq!(view.email.as_deref(), Some("ada@example.com"));
        assert_eq!(view.company.as_deref(), Some("Engines"));
    }

    #[test]
    fn a_tag_cell_splits_on_every_separator_a_spreadsheet_uses() {
        let mapping =
            map_header("contacts", &["first_name".to_owned(), "tags".to_owned()]).unwrap();
        let view = read_contact_row(&mapping, &["Ada".to_owned(), "vip; emea, west".to_owned()]);
        assert_eq!(
            view.tags,
            vec!["vip".to_owned(), "emea".to_owned(), "west".to_owned()]
        );
    }

    #[test]
    fn a_custom_cell_that_is_not_json_is_dropped_rather_than_failing_the_row() {
        let mapping =
            map_header("contacts", &["first_name".to_owned(), "custom".to_owned()]).unwrap();
        let view = read_contact_row(&mapping, &["Ada".to_owned(), "{oops".to_owned()]);
        assert!(view.custom.is_none());
    }

    #[test]
    fn a_custom_cell_of_json_is_kept_as_an_object() {
        let mapping =
            map_header("contacts", &["first_name".to_owned(), "custom".to_owned()]).unwrap();
        let view = read_contact_row(
            &mapping,
            &["Ada".to_owned(), r#"{"segment":"smb"}"#.to_owned()],
        );
        assert_eq!(view.custom, Some(json!({ "segment": "smb" })));
    }

    #[test]
    fn an_unknown_status_is_refused_with_the_list_of_the_ones_that_exist() {
        let file = "first_name,status\nAda,prospect\n";
        let preview = preview_contacts(file).unwrap();
        assert_eq!(preview.errors[0].field.as_deref(), Some("status"));
        assert!(
            preview.errors[0].message.contains("lead"),
            "{}",
            preview.errors[0].message
        );
    }

    #[test]
    fn a_row_with_too_many_tags_is_refused_with_the_limit() {
        let file = "first_name,tags\nAda,\"a;b;c;d;e;f;g;h;i;j;k\"\n";
        let preview = preview_contacts(file).unwrap();
        assert_eq!(preview.errors[0].field.as_deref(), Some("tags"));
        assert!(
            preview.errors[0].message.contains('1'),
            "{}",
            preview.errors[0].message
        );
    }

    #[test]
    fn a_tag_that_contains_a_space_survives_the_round_trip() {
        let contact = contact_fixture();
        let cell = tags_to_cell(&contact.tags);
        let mapping = map_header(
            "contacts",
            &["first_name".to_owned(), "tags".to_owned()],
        )
        .unwrap();
        let view = read_contact_row(&mapping, &["Ada".to_owned(), cell]);
        assert_eq!(
            view.tags, contact.tags,
            "a joined list would read one two-word tag as one long tag"
        );
    }

    #[test]
    fn an_empty_file_is_refused_and_says_it_needs_a_header() {
        let error = preview_contacts("").expect_err("an empty file is refused");
        assert!(error.to_string().contains("header"), "{error}");
    }

    #[test]
    fn a_header_only_file_reports_no_rows() {
        let preview = preview_contacts("first_name,last_name\n").unwrap();
        assert_eq!(preview.total_rows, 0);
        assert!(
            preview.summary().contains("no rows"),
            "{}",
            preview.summary()
        );
    }

    #[test]
    fn a_file_with_more_rows_than_the_cap_is_refused_before_any_row_is_read() {
        let mut file = String::from("first_name\n");
        for index in 0..=MAX_IMPORT_ROWS {
            file.push_str(&format!("contact{index}\n"));
        }
        let error = preview_contacts(&file).expect_err("the cap is a refusal, not a truncation");
        assert!(error.to_string().contains("at most"), "{error}");
    }

    #[test]
    fn the_export_quotes_only_what_it_must() {
        assert_eq!(csv_field("Ada"), "Ada");
        assert_eq!(csv_field("Lovelace, Ada"), "\"Lovelace, Ada\"");
        assert_eq!(csv_field("He said \"hi\""), "\"He said \"\"hi\"\"\"");
        assert_eq!(csv_field(" padded "), "\" padded \"");
    }

    #[test]
    fn the_export_round_trips_through_the_import() {
        let mut contact = contact_fixture();
        contact.notes = "Called on Tuesday, agreed a \"pilot\".".to_owned();
        let file = contacts_to_csv(std::slice::from_ref(&contact));

        let preview = preview_contacts(&file).expect("an export must import cleanly");
        assert_eq!(preview.errors.len(), 0, "{:?}", preview.errors);
        assert_eq!(preview.valid_rows, 1);
        assert_eq!(
            preview.sample[0].notes.as_deref(),
            Some(contact.notes.as_str())
        );
        assert_eq!(preview.sample[0].tags, contact.tags);
        assert_eq!(
            preview.sample[0].company.as_deref(),
            contact.company_name.as_deref()
        );
    }

    #[test]
    fn the_company_export_round_trips_too() {
        let company = Company {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            name: "Analytical Engines".to_owned(),
            initials: "AE".to_owned(),
            domain: Some("analytical.example".to_owned()),
            industry: Some("Research".to_owned()),
            owner_user_id: None,
            owner_name: Some("Furkan".to_owned()),
            status: "customer".to_owned(),
            tags: vec!["enterprise".to_owned()],
            custom: json!({ "region": "emea" }),
            notes: String::new(),
            archived_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let file = companies_to_csv(std::slice::from_ref(&company));
        assert!(file.starts_with("name,domain,industry,status,tags,notes,owner,custom\n"));
        assert!(
            file.contains("\"emea\""),
            "the custom object travels as JSON"
        );
    }

    #[test]
    fn a_contact_without_a_company_exports_an_empty_cell_rather_than_the_word_none() {
        let mut contact = contact_fixture();
        contact.company_name = None;
        contact.email = None;
        contact.tags.clear();
        let file = contacts_to_csv(std::slice::from_ref(&contact));
        let line = file.lines().nth(1).expect("one row");
        assert!(!line.contains("none"), "{line}");
        assert_eq!(line.matches(',').count(), CONTACT_COLUMNS.len() - 1);
    }

    #[test]
    fn a_row_error_renders_the_json_the_screen_reads() {
        let error = RowError {
            line: 7,
            field: Some("email".to_owned()),
            message: "that is not an e-mail address".to_owned(),
        };
        assert_eq!(
            error.to_json(),
            json!({
                "line": 7,
                "field": "email",
                "message": "that is not an e-mail address",
            })
        );
    }

    #[test]
    fn an_owner_name_is_matched_case_insensitively_against_the_accounts_a_person_may_choose() {
        let known = vec![("Furkan Ermag".to_owned(), Uuid::nil())];
        assert_eq!(owner_id_of(&known, Some("furkan ermag")), Some(Uuid::nil()));
        assert_eq!(owner_id_of(&known, Some("Furkan")), None);
        assert_eq!(owner_id_of(&known, None), None);
    }
}
