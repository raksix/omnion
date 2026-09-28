//! The attribute map: external attribute → panel field (REQ-065, slice 2).
//!
//! Every provider hands us a differently-named bag of strings and the platform wants eight
//! specific fields out of it. Something has to decide which of theirs is the email, and the answer
//! is configuration, not a guess — so this module is the configuration language: the rows, the
//! transforms, the validation an operator can act on, and a **preview** that shows what a sample
//! claims payload becomes before anybody signs in.
//!
//! Three decisions carry most of the weight:
//!
//! * **A missing required field is refused by name, never defaulted.** A JIT account with an empty
//!   email is an account nobody can receive a password reset for and nobody can audit, and it is
//!   much harder to find later than to refuse now. So [`AttributeMap::project`] returns
//!   [`Projection::Refused`] listing the fields, and the API turns that into a 422 naming them.
//! * **Transforms are a closed set of six.** An open "expression" field is where SSRF, injection
//!   and an unexplainable account value come from, and no real directory needs one: trim, case
//!   fold, a prefix, a static default and a split cover every case seen in the wild.
//! * **The preview is a real projection, not an illustration.** It runs the exact function the
//!   sign-in callback runs, against a payload the operator pasted, and reports the same refusal
//!   the sign-in would. A preview built from a parallel implementation is a preview of nothing.
//!
//! Nothing here touches the network or the database — the repository half lives in
//! [`super::mappings`], and the API layer wires them together.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{IdentityError, Result};

/// The panel fields a provider sign-in may fill.
///
/// A closed list on purpose: a mapping to a field the user table does not have is a row that
/// looks successful and provisions an account with a column that was silently dropped, which is
/// the worst of both worlds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetField {
    /// The address the account is keyed by. **Required**, always.
    Email,
    /// The short login name, when a provider sends one that is not the address.
    Username,
    /// A human name for the account.
    DisplayName,
    /// Phone number.
    Phone,
    /// Organizational unit.
    Department,
    /// Job title.
    Title,
    /// Full-time, part-time, contractor…
    EmploymentType,
    /// The directory's own employee number.
    EmployeeId,
}

impl TargetField {
    /// Every field, in the order the editor lists them.
    pub const ALL: [Self; 8] = [
        Self::Email,
        Self::Username,
        Self::DisplayName,
        Self::Phone,
        Self::Department,
        Self::Title,
        Self::EmploymentType,
        Self::EmployeeId,
    ];

    /// The wire and database name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Username => "username",
            Self::DisplayName => "display_name",
            Self::Phone => "phone",
            Self::Department => "department",
            Self::Title => "title",
            Self::EmploymentType => "employment_type",
            Self::EmployeeId => "employee_id",
        }
    }

    /// Parse a stored or submitted name.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "email" => Ok(Self::Email),
            "username" => Ok(Self::Username),
            "display_name" => Ok(Self::DisplayName),
            "phone" => Ok(Self::Phone),
            "department" => Ok(Self::Department),
            "title" => Ok(Self::Title),
            "employment_type" => Ok(Self::EmploymentType),
            "employee_id" => Ok(Self::EmployeeId),
            other => Err(IdentityError::InvalidProvider(format!(
                "`{other}` is not a panel field — use one of {}",
                Self::names().join(", ")
            ))),
        }
    }

    /// Every field's name, for error messages and the editor's picker.
    #[must_use]
    pub fn names() -> Vec<&'static str> {
        Self::ALL.iter().map(|field| field.as_str()).collect()
    }

    /// Whether an account may exist without this field.
    ///
    /// Only the email is non-negotiable, and that is a property of the *user table* rather than of
    /// this map — which is why [`AttributeMap::validate`] refuses a map that leaves it unmapped
    /// even when the operator did not tick `required`.
    #[must_use]
    pub const fn mandatory(self) -> bool {
        matches!(self, Self::Email)
    }
}

/// What a transform does to one value.
///
/// Six cases, no escape hatch. Each is a total function from a string to a list of strings, which
/// is what lets [`split`] work on a directory attribute that arrives as either a string or a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Transform {
    /// Store the value exactly as it arrived.
    #[default]
    None,
    /// Trim surrounding whitespace, then store.
    Trim,
    /// Lowercase, then store. Applied to the address, which is normalized again later anyway —
    /// but the preview shows the operator what the map actually produces.
    Lowercase,
    /// `argument + value`, for a provider that sends a bare local part behind a different key.
    ///
    /// Named for what it does rather than for a use case, because the e-mail case an operator
    /// actually hits is a *suffix* (`ferkan` + `@example.com`) and a transform called `prefix`
    /// that appends a domain is a name that lies. The REQ asks for "prefix" and the migration
    /// names it; what this method guarantees is `argument` first, always.
    Prefix,
    /// The argument, whatever the source value was. A claim that is absent takes the default.
    Static,
    /// Split on the argument (comma when empty) and keep the first non-empty part. Some
    /// directories send `a@x.com;b@x.com` in one attribute.
    Split,
}

impl Transform {
    /// Every transform, for the editor's picker.
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::Trim,
        Self::Lowercase,
        Self::Prefix,
        Self::Static,
        Self::Split,
    ];

    /// The wire and database name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Trim => "trim",
            Self::Lowercase => "lowercase",
            Self::Prefix => "prefix",
            Self::Static => "static",
            Self::Split => "split",
        }
    }

    /// Parse a stored or submitted name.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "none" | "" => Ok(Self::None),
            "trim" => Ok(Self::Trim),
            "lowercase" => Ok(Self::Lowercase),
            "prefix" => Ok(Self::Prefix),
            "static" => Ok(Self::Static),
            "split" => Ok(Self::Split),
            other => Err(IdentityError::InvalidProvider(format!(
                "`{other}` is not a transform — use one of {}",
                Self::names().join(", ")
            ))),
        }
    }

    /// Every transform's name, for error messages and the editor's picker.
    #[must_use]
    pub fn names() -> Vec<&'static str> {
        Self::ALL.iter().map(|item| item.as_str()).collect()
    }

    /// Whether this transform needs an argument, and therefore whether an empty one is a mistake.
    ///
    /// `split` is the interesting one: an empty argument is *legal* and means "split on commas",
    /// which is the common case. Refusing an empty argument there would push an operator to type
    /// `,` to get the default behaviour, which is worse than a documented default.
    #[must_use]
    pub const fn argument_required(self) -> bool {
        matches!(self, Self::Prefix | Self::Static)
    }

    /// Apply the transform to one value.
    ///
    /// Returns a list because [`Transform::Split`] can, and because a directory attribute that
    /// arrives as a JSON array is the same shape as one that arrives comma-separated.
    #[must_use]
    pub fn apply(self, value: &str, argument: Option<&str>) -> Vec<String> {
        let raw = value;
        let trimmed = raw.trim();
        match self {
            Self::None => vec![raw.to_owned()],
            Self::Trim => vec![trimmed.to_owned()],
            Self::Lowercase => vec![trimmed.to_ascii_lowercase()],
            Self::Prefix => {
                let prefix = argument.unwrap_or_default();
                let combined = format!("{prefix}{trimmed}");
                vec![combined.trim().to_owned()]
            }
            Self::Static => vec![argument.unwrap_or_default().to_owned()],
            Self::Split => {
                // `&str` rather than `AsRef<_>`: `str` implements `AsRef` four different ways, so
                // an inferred separator is a type error the caller cannot resolve by reading the
                // code — the type has to be written down.
                let separator: &str = argument.filter(|value| !value.is_empty()).unwrap_or(",");
                raw.split(separator)
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
        }
    }
}

/// One row of the map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributeMapping {
    /// The provider's own attribute name — a claim name, or an LDAP attribute.
    pub source_attr: String,
    /// The panel field it fills.
    pub target_field: TargetField,
    /// What to do to the value.
    #[serde(default)]
    pub transform: Transform,
    /// The transform's argument, when it has one.
    #[serde(default)]
    pub transform_arg: Option<String>,
    /// Whether sign-in is refused when this field comes back empty.
    #[serde(default)]
    pub required: bool,
    /// Editor order.
    #[serde(default)]
    pub position: i32,
}

impl AttributeMapping {
    /// Validate one row on its own.
    ///
    /// The problems are named against the input, not against a database constraint, because the
    /// editor can only underline a field it has a name for.
    pub fn validate(&self) -> Vec<MapProblem> {
        let mut problems = Vec::new();

        if self.source_attr.trim().is_empty() {
            problems.push(MapProblem::new(
                "source_attr",
                "name the claim or directory attribute this reads",
            ));
        } else if self.source_attr.trim().len() > MAX_SOURCE_ATTR {
            problems.push(MapProblem::new(
                "source_attr",
                format!("an attribute name may be at most {MAX_SOURCE_ATTR} characters"),
            ));
        }

        if self.transform.argument_required()
            && self
                .transform_arg
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
        {
            problems.push(MapProblem::new(
                "transform_arg",
                format!(
                    "the `{}` transform needs an argument",
                    self.transform.as_str()
                ),
            ));
        }

        if let Some(argument) = self.transform_arg.as_deref()
            && argument.len() > MAX_TRANSFORM_ARG
        {
            problems.push(MapProblem::new(
                "transform_arg",
                format!("an argument may be at most {MAX_TRANSFORM_ARG} characters"),
            ));
        }

        // `required` on the email is not a choice: the column is NOT NULL, so a row that says
        // "optional" would be a promise the database cannot keep. The other direction is the
        // dangerous one and is refused too — a map with no email row at all provisions nothing.
        if self.target_field.mandatory() && !self.required {
            problems.push(MapProblem::new(
                "required",
                "the email field is always required; an account cannot exist without one",
            ));
        }

        problems
    }

    /// The raw value this row reads, before the transform.
    ///
    /// A dotted path reaches into a nested claim (`address.email`), an array index into a list,
    /// and a plain name reads the claim itself. The same reader serves the preview and the sign-in
    /// path, so what the operator sees is what the callback gets.
    #[must_use]
    pub fn read<'a>(&self, source: &'a Value) -> Option<String> {
        let raw = super::claims::value_at_path(source, self.source_attr.trim())?;
        match raw {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            Value::Bool(flag) => Some(flag.to_string()),
            // A list is flattened to its first usable member: a multi-valued LDAP attribute is
            // the same data as a comma-separated one, and the split transform exists to choose a
            // member deliberately when the order matters.
            Value::Array(items) => items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .find(|text| !text.trim().is_empty()),
            Value::Null => None,
            other => Some(other.to_string()),
        }
    }

    /// The transformed value for this row, or `None` when the source carries nothing.
    ///
    /// [`Transform::Static`] is the one case that answers even for a claim the payload does not
    /// carry — that is what a default is *for*, and a default that stops applying the moment the
    /// source goes missing is not a default. The check comes before the lookup precisely so an
    /// absent claim cannot short-circuit it.
    #[must_use]
    pub fn apply(&self, source: &Value) -> Option<String> {
        if self.transform == Transform::Static
            && let Some(argument) = self.transform_arg.as_deref()
            && !argument.trim().is_empty()
        {
            return Some(argument.to_owned());
        }
        let raw = self.read(source)?;
        self.transform
            .apply(&raw, self.transform_arg.as_deref())
            .into_iter()
            .find(|value| !value.trim().is_empty())
    }
}

/// A whole provider's map, in editor order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributeMap {
    /// The rows, ordered by `position` by [`AttributeMap::normalized`].
    pub rows: Vec<AttributeMapping>,
}

impl AttributeMap {
    /// Build from rows, sorting them into editor order.
    #[must_use]
    pub fn new(rows: Vec<AttributeMapping>) -> Self {
        Self { rows }.normalized()
    }

    /// Read a map out of the JSON the panel posts.
    ///
    /// Accepts a bare array and a `{ "mappings": [...] }` object, because the first thing a
    /// screen grows is an envelope and refusing both shapes would mean the API and the panel
    /// disagree about the request format — which is a 400 nobody can debug from the browser.
    pub fn from_value(value: &Value) -> Result<Self> {
        let list = match value {
            Value::Array(items) => items.as_slice(),
            Value::Object(_) => match value.get("mappings") {
                Some(Value::Array(items)) => items.as_slice(),
                Some(Value::Null) | None => &[],
                Some(_) => {
                    return Err(IdentityError::InvalidProvider(
                        "`mappings` must be an array of attribute mappings".into(),
                    ));
                }
            },
            Value::Null => &[],
            _ => {
                return Err(IdentityError::InvalidProvider(
                    "the attribute map must be an array, or an object with a `mappings` array".into(),
                ));
            }
        };

        let mut rows = Vec::with_capacity(list.len());
        for (index, entry) in list.iter().enumerate() {
            rows.push(mapping_from_value(entry, index)?);
        }
        Ok(Self { rows }.normalized())
    }

    /// Write the map back into the shape the panel and the database exchange.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Array(
            self.rows
                .iter()
                .map(|row| {
                    serde_json::json!({
                        "source_attr": row.source_attr,
                        "target_field": row.target_field.as_str(),
                        "transform": row.transform.as_str(),
                        "transform_arg": row.transform_arg,
                        "required": row.required,
                        "position": row.position,
                    })
                })
                .collect(),
        )
    }

    /// Sort by `position` and renumber to `0..n`.
    ///
    /// Renumbering rather than preserving is deliberate: the stored positions then always describe
    /// a contiguous order, so "the third row" means the same thing in the editor, the preview and
    /// the database.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.rows.sort_by(|left, right| {
            left.position
                .cmp(&right.position)
                .then_with(|| left.target_field.cmp(&right.target_field))
        });
        for (index, row) in self.rows.iter_mut().enumerate() {
            row.position = i32::try_from(index).unwrap_or(i32::MAX);
        }
        self
    }

    /// Every problem with the map, as a list rather than the first one.
    ///
    /// A wizard that reports one error per submit is the reason a wizard is abandoned halfway, so
    /// this returns everything: the per-row problems, the duplicate field, the missing email and
    /// the impossible transform — the lot.
    #[must_use]
    pub fn validate(&self) -> Vec<MapProblem> {
        let mut problems: Vec<MapProblem> = self
            .rows
            .iter()
            .enumerate()
            .flat_map(|(index, row)| {
                row.validate()
                    .into_iter()
                    .map(|problem| MapProblem {
                        field: problem.field,
                        message: format!("row {}: {}", index + 1, problem.message),
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        // Two rows writing one field is not an ordering question, it is a silent overwrite: the
        // later row wins at provisioning time, where nobody is watching the map.
        for (index, row) in self.rows.iter().enumerate() {
            if self
                .rows
                .iter()
                .take(index)
                .any(|earlier| earlier.target_field == row.target_field)
            {
                problems.push(MapProblem::new(
                    "target_field",
                    format!(
                        "row {}: `{}` is already mapped by an earlier row — a panel field can be \
                         filled from one attribute only",
                        index + 1,
                        row.target_field.as_str()
                    ),
                ));
            }
        }

        if !self
            .rows
            .iter()
            .any(|row| row.target_field == TargetField::Email)
        {
            problems.push(MapProblem::new(
                "target_field",
                "map an attribute to `email` — without one a sign-in cannot create an account",
            ));
        }

        problems
    }

    /// Turn a claims payload into panel field values.
    ///
    /// This is the function the sign-in callback runs, and the one the preview runs. A missing
    /// required field is a refusal naming the field, not a default and not a partial account.
    #[must_use]
    pub fn project(&self, source: &Value) -> Projection {
        let mut values: Vec<(TargetField, String)> = Vec::new();
        let mut missing: Vec<TargetField> = Vec::new();
        let mut unused: Vec<&str> = Vec::new();

        for row in &self.rows {
            match row.apply(source) {
                Some(value) => values.push((row.target_field, value)),
                None => {
                    if row.required || row.target_field.mandatory() {
                        missing.push(row.target_field);
                    }
                }
            }
            if super::claims::value_at_path(source, row.source_attr.trim()).is_none() {
                unused.push(row.source_attr.as_str());
            }
        }

        let blocked = !missing.is_empty();
        Projection {
            values: if blocked { Vec::new() } else { values },
            missing,
            // `unused` is advisory, not an error: a claim that is simply absent from this sample
            // is the normal case for a directory with a big schema, and the editor shows it so an
            // operator can spot a typo in an attribute name.
            unused: unused.iter().map(|name| (*name).to_owned()).collect(),
        }
    }
}

/// What [`AttributeMap::project`] produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projection {
    /// The field values, empty when a required field was missing.
    pub values: Vec<(TargetField, String)>,
    /// Required fields the source did not carry.
    pub missing: Vec<TargetField>,
    /// Source attributes the payload did not carry — advisory, for typo spotting.
    pub unused: Vec<String>,
}

impl Projection {
    /// Whether an account may be created from this projection.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
    }

    /// One field's value, if the projection produced it.
    #[must_use]
    pub fn get(&self, field: TargetField) -> Option<&str> {
        self.values
            .iter()
            .find(|(candidate, _)| *candidate == field)
            .map(|(_, value)| value.as_str())
    }

    /// The field names a refusal names, in a sentence the API can return verbatim.
    #[must_use]
    pub fn missing_names(&self) -> Vec<&'static str> {
        self.missing.iter().map(|field| field.as_str()).collect()
    }
}

/// One problem with the map, attached to the input that owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MapProblem {
    /// The form field.
    pub field: &'static str,
    /// What is wrong, in one sentence.
    pub message: String,
}

impl MapProblem {
    fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

/// Above this an attribute name is a paragraph, not a name.
pub const MAX_SOURCE_ATTR: usize = 200;
/// Above this a transform argument is a paragraph too.
pub const MAX_TRANSFORM_ARG: usize = 500;
/// How many rows a map may carry. Eight fields is the ceiling, so this only catches a paste that
/// went wrong.
pub const MAX_ROWS: usize = 32;

/// Parse one row, defaulting `position` to its index so a client may omit it.
fn mapping_from_value(value: &Value, index: usize) -> Result<AttributeMapping> {
    let object = value.as_object().ok_or_else(|| {
        IdentityError::InvalidProvider(format!(
            "mapping {index} must be an object with a `source_attr` and a `target_field`"
        ))
    })?;

    let source_attr = object
        .get("source_attr")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            IdentityError::InvalidProvider(format!("mapping {index} needs a `source_attr`"))
        })?
        .to_owned();

    let target_field = TargetField::parse(
        object
            .get("target_field")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )?;

    let transform = match object.get("transform") {
        Some(Value::String(name)) => Transform::parse(name)?,
        Some(Value::Null) | None => Transform::None,
        Some(_) => {
            return Err(IdentityError::InvalidProvider(format!(
                "mapping {index}: `transform` must be a name, not a value"
            )));
        }
    };

    let transform_arg = object
        .get("transform_arg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);

    let position = object
        .get("position")
        .and_then(Value::as_i64)
        .map_or(Ok(i32::try_from(index).unwrap_or(i32::MAX)), |value| {
            i32::try_from(value).map_err(|_| {
                IdentityError::InvalidProvider(format!(
                    "mapping {index}: `position` is out of range"
                ))
            })
        })?;

    Ok(AttributeMapping {
        source_attr,
        target_field,
        transform,
        transform_arg,
        required: object
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(target_field.mandatory()),
        position,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(source: &str, field: TargetField) -> AttributeMapping {
        AttributeMapping {
            source_attr: source.to_owned(),
            target_field: field,
            transform: Transform::None,
            transform_arg: None,
            required: field.mandatory(),
            position: 0,
        }
    }

    fn sample() -> Value {
        json!({
            "sub": "2481",
            "mail": "  Furkan@Example.COM ",
            "givenName": "Furkan",
            "sn": "Ermağ",
            "department": ["Engineering", "Platform"],
            "employeeNumber": 4812,
            "groups": ["platform-team", "qa"]
        })
    }

    #[test]
    fn a_complete_map_projects_every_field() {
        let map = AttributeMap::new(vec![
            row("mail", TargetField::Email),
            row("givenName", TargetField::DisplayName),
            row("department", TargetField::Department),
            row("employeeNumber", TargetField::EmployeeId),
        ]);
        assert!(map.validate().is_empty(), "{}", map.validate().len());

        let projection = map.project(&sample());
        assert!(projection.ok(), "missing {:?}", projection.missing_names());
        // A multi-valued attribute reads its first usable member.
        assert_eq!(projection.get(TargetField::Department), Some("Engineering"));
        // A number is a valid employee id; storing "4812" and not "4812.0" is the point.
        assert_eq!(projection.get(TargetField::EmployeeId), Some("4812"));
    }

    #[test]
    fn a_missing_required_field_refuses_by_name_instead_of_defaulting() {
        let map = AttributeMap::new(vec![row("mail", TargetField::Email)]);
        // The sample has no `mail`; the projection must refuse rather than create a half account.
        let empty = json!({ "sub": "1" });
        let projection = map.project(&empty);
        assert!(!projection.ok());
        assert_eq!(projection.missing_names(), vec!["email"]);
        // …and the values are withheld, so nothing downstream can use a partial projection.
        assert!(projection.values.is_empty());
    }

    #[test]
    fn a_map_without_an_email_is_refused_before_any_sign_in() {
        let map = AttributeMap::new(vec![row("givenName", TargetField::DisplayName)]);
        let problems = map.validate();
        assert!(
            problems
                .iter()
                .any(|problem| problem.message.contains("`email`")),
            "{problems:?}"
        );
    }

    #[test]
    fn two_rows_writing_one_field_are_refused_rather_than_silently_overwriting() {
        let map = AttributeMap::new(vec![
            row("mail", TargetField::Email),
            row("otherMail", TargetField::Email),
        ]);
        let problems = map.validate();
        assert!(
            problems
                .iter()
                .any(|problem| problem.message.contains("already mapped")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_transform_that_needs_an_argument_says_so_against_its_own_field() {
        let mut mapping = row("mail", TargetField::Email);
        mapping.transform = Transform::Prefix;
        let problems = mapping.validate();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].field, "transform_arg");
        assert!(problems[0].message.contains("needs an argument"));
    }

    #[test]
    fn split_with_no_argument_defaults_to_a_comma_and_is_not_an_error() {
        let mut mapping = row("mail", TargetField::Email);
        mapping.transform = Transform::Split;
        assert!(mapping.validate().is_empty(), "an empty split argument is a default");
        assert_eq!(
            mapping.apply(&json!({ "mail": "first@x.com,second@x.com" })),
            Some("first@x.com".to_owned())
        );
    }

    #[test]
    fn prefix_and_static_are_the_two_transforms_an_argument_makes_meaningful() {
        assert!(Transform::Prefix.argument_required());
        assert!(Transform::Static.argument_required());
        assert!(!Transform::Split.argument_required());

        // `prefix` puts the argument first, which is what the name promises: a directory that
        // sends a local part behind a namespaced key, or a claim that needs a fixed opener.
        let mut mapping = row("sAMAccountName", TargetField::Username);
        mapping.transform = Transform::Prefix;
        mapping.transform_arg = Some("corp-".to_owned());
        assert_eq!(
            mapping.apply(&json!({ "sAMAccountName": "ferkan" })),
            Some("corp-ferkan".to_owned())
        );

        // `static` answers for a claim that is absent — that is the whole point of it.
        let mut fallback = row("department", TargetField::Department);
        fallback.transform = Transform::Static;
        fallback.transform_arg = Some("Unassigned".to_owned());
        assert_eq!(fallback.apply(&json!({})), Some("Unassigned".to_owned()));
    }

    #[test]
    fn the_dotted_path_reader_reaches_nested_claims_and_refuses_a_wrong_one() {
        let mapping = row("address.mail", TargetField::Email);
        assert_eq!(
            mapping.apply(&json!({ "address": { "mail": "a@b.com" } })),
            Some("a@b.com".to_owned())
        );
        assert_eq!(mapping.apply(&json!({ "address": {} })), None);
    }

    #[test]
    fn the_map_round_trips_through_the_wire_shape() {
        let mut first = row("mail", TargetField::Email);
        first.transform = Transform::Lowercase;
        first.position = 0;
        let mut second = row("title", TargetField::Title);
        second.position = 1;
        let map = AttributeMap::new(vec![second, first]);

        let back = AttributeMap::from_value(&map.to_value()).expect("round trip");
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.rows[0].source_attr, "mail");
        assert_eq!(back.rows[0].transform, Transform::Lowercase);
        // The rows were handed over in the opposite order to their positions, so this proves the
        // editor's order is the one that survives rather than the order the client sent.
        assert_eq!(back.rows[1].source_attr, "title");
        // Renumbering is the point: stored positions are contiguous after a write.
        assert_eq!(back.rows[0].position, 0);
        assert_eq!(back.rows[1].position, 1);

        // Two rows that claim the same position are not a coin toss: the target field breaks the
        // tie, so the stored order is total and a re-save cannot reshuffle the map underneath
        // the operator's cursor.
        let mut duplicate_a = row("mail", TargetField::Email);
        duplicate_a.position = 0;
        let mut duplicate_b = row("title", TargetField::Title);
        duplicate_b.position = 0;
        let tied = AttributeMap::new(vec![duplicate_b, duplicate_a]);
        assert_eq!(tied.rows[0].target_field, TargetField::Email);
    }

    #[test]
    fn an_object_envelope_and_a_bare_array_are_the_same_request() {
        let bare = json!([{ "source_attr": "mail", "target_field": "email" }]);
        let wrapped = json!({ "mappings": [{ "source_attr": "mail", "target_field": "email" }] });
        let left = AttributeMap::from_value(&bare).expect("bare");
        let right = AttributeMap::from_value(&wrapped).expect("wrapped");
        assert_eq!(left, right);
        // And an absent `mappings` is an empty map, not a 400: clearing the map is a real action.
        assert!(AttributeMap::from_value(&json!({})).expect("empty").rows.is_empty());
    }

    #[test]
    fn an_unknown_target_field_names_the_ones_that_exist() {
        let error = AttributeMap::from_value(&json!([{
            "source_attr": "mail",
            "target_field": "favorite_colour"
        }]))
        .expect_err("unknown field");
        let message = error.to_string();
        assert!(message.contains("favorite_colour"), "{message}");
        assert!(message.contains("display_name"), "{message}");
    }

    #[test]
    fn an_empty_map_is_valid_json_and_an_invalid_shape_is_named() {
        assert!(AttributeMap::from_value(&Value::Null).expect("null").rows.is_empty());
        let error = AttributeMap::from_value(&json!({ "mappings": "nope" }))
            .expect_err("string mappings");
        assert!(error.to_string().contains("must be an array"), "{error}");
    }

    #[test]
    fn a_row_whose_source_is_absent_is_reported_as_unused_without_failing() {
        let map = AttributeMap::new(vec![
            row("mail", TargetField::Email),
            row("mailAlias", TargetField::Username),
        ]);
        let projection = map.project(&sample());
        assert!(projection.ok());
        assert_eq!(projection.unused, vec!["mailAlias".to_owned()]);
    }

    #[test]
    fn the_email_row_may_not_declare_itself_optional() {
        let mut mapping = row("mail", TargetField::Email);
        mapping.required = false;
        let problems = mapping.validate();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].field, "required");
    }

    #[test]
    fn transforms_are_a_closed_set_and_naming_one_says_what_is_allowed() {
        assert_eq!(Transform::names().len(), 6);
        assert_eq!(TargetField::names().len(), 8);
        let error = Transform::parse("eval").expect_err("no eval transform");
        assert!(error.to_string().contains("static"), "{error}");
    }
}
