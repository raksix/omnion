//! The `--json` envelope — the CLI's one machine-readable contract.
//!
//! The request calls this a *public* contract (REQ-131, acceptance 15): "a stable envelope
//! (`ok`, `command`, `data`, `warnings`, `error { code, message, hint }`) with documented error
//! codes; data on stdout, progress on stderr, so `omnion doctor --json | jq` never breaks".
//!
//! Three properties make it usable by a script, and each one is a place a naive implementation
//! quietly breaks it:
//!
//! 1. **stdout carries data only.** Progress, spinners and human tables go to stderr. A command
//!    that prints its table and *then* the JSON document produces a stream `jq` cannot parse,
//!    and the failure surfaces in someone's pipeline rather than here.
//! 2. **The error shape is always present.** A failed run answers `ok: false` with `error.code`
//!    set, and a successful run answers `error: null`. A consumer that reads `.error.code` on
//!    success must get `null`, not a missing key that raises.
//! 3. **`--json` never changes *what* the command does.** It changes how the outcome is written
//!    down. `omnion doctor --json` runs the same checks and exits with the same code as
//!    `omnion doctor`; if JSON mode could exit 0 where the human run exits 1, the flag would be a
//!    way to turn a broken installation into a green pipeline.
//!
//! Codes are additive-only and never repurposed (the request's risk note): a script that matches
//! on `config_unreadable` keeps working across releases.

use std::io::Write;

/// The envelope version, so a script can branch on the contract it is reading.
///
/// This is not the CLI version: the two move independently. A patch release adds no field, so
/// the version stays `1`; a field is added or removed and it becomes `2`.
pub const ENVELOPE_VERSION: u32 = 1;

/// A documented error code. The set is closed on purpose — see [`ErrorCode::as_str`].
///
/// Each code is stable: it is what a script matches on, so it never changes meaning. A new
/// failure mode gets a new code rather than being folded into a near neighbour, because a script
/// that branches on the neighbour then takes a branch its author never wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The command line itself was wrong: unknown command, unknown flag, missing value.
    Usage,
    /// The environment could not be read (a missing or contradictory `OMNION_*` variable).
    ConfigUnreadable,
    /// The database answered no, or the migration ledger disagrees with the files on disk.
    DatabaseUnreachable,
    /// A dependency the command needs is not answering (redis, object storage, container runtime).
    DependencyUnavailable,
    /// The command reached its goal and found nothing to do that is safe to do (nothing pending).
    NothingToDo,
    /// The command would do something destructive and the confirmation did not match.
    ConfirmationRequired,
    /// The linked instance answered, but refused what was asked (auth, permission, state).
    Refused,
    /// Anything that does not fit a code above. Never used to avoid picking one.
    Internal,
}

impl ErrorCode {
    /// The stable wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::ConfigUnreadable => "config_unreadable",
            Self::DatabaseUnreachable => "database_unreachable",
            Self::DependencyUnavailable => "dependency_unavailable",
            Self::NothingToDo => "nothing_to_do",
            Self::ConfirmationRequired => "confirmation_required",
            Self::Refused => "refused",
            Self::Internal => "internal",
        }
    }
}

/// One warning line: something the operator should know that did not stop the command.
#[derive(Debug, Clone)]
pub struct Warning {
    /// Stable warning code, in the same namespace as the error codes.
    pub code: &'static str,
    /// Human sentence for a human; never contains a secret.
    pub message: String,
    /// What to do about it, when there is something to do.
    pub hint: Option<String>,
}

impl Warning {
    /// A warning with no fix to suggest.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    /// A warning that names its own remedy.
    pub fn with_hint(
        code: &'static str,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            hint: Some(hint.into()),
        }
    }
}

/// A failure, in the shape the envelope documents.
#[derive(Debug, Clone)]
pub struct Failure {
    /// The documented code.
    pub code: ErrorCode,
    /// What went wrong, in one sentence, safe to print.
    pub message: String,
    /// What to do about it.
    pub hint: Option<String>,
}

impl Failure {
    /// A failure without a remedy to suggest.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    /// A failure that names its own remedy.
    pub fn with_hint(code: ErrorCode, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: Some(hint.into()),
        }
    }
}

/// Write the envelope for a successful run.
///
/// `data` is whatever the command produced. It is passed as a `serde_json::Value` rather than
/// generated so a command cannot accidentally emit a shape its own tests do not know about.
pub fn success(command: &str, data: serde_json::Value, warnings: &[Warning]) -> String {
    document(command, true, data, warnings, None)
}

/// Write the envelope for a failed run.
pub fn failure(command: &str, failure: &Failure, warnings: &[Warning]) -> String {
    document(
        command,
        false,
        serde_json::Value::Null,
        warnings,
        Some(failure),
    )
}

/// Write a failure envelope that still carries `data`.
///
/// Not every failure is a failure to have produced anything. `omnion doctor` runs every check and
/// then fails on the ones that did not pass, and the per-check detail — which check, what it
/// said, what to do — is the entire value of the run. A failure envelope with `data: null` throws
/// that away, so a machine asked "is this installation healthy" would get `ok: false` and nothing
/// to act on, while a human running the same command without `--json` would get all of it.
///
/// So `data` is permitted alongside an error, and `null` stays the default for the failures that
/// genuinely produced nothing. The envelope's documentation says `data` is the command's output,
/// and a check list the command did produce is output — reading the key as "output on success
/// only" is the mistake this constructor exists to prevent.
pub fn failure_with_data(
    command: &str,
    failure: &Failure,
    data: serde_json::Value,
    warnings: &[Warning],
) -> String {
    document(command, false, data, warnings, Some(failure))
}

/// Assemble the envelope.
fn document(
    command: &str,
    ok: bool,
    data: serde_json::Value,
    warnings: &[Warning],
    error: Option<&Failure>,
) -> String {
    let warning_values: Vec<serde_json::Value> = warnings
        .iter()
        .map(|warning| {
            serde_json::json!({
                "code": warning.code,
                "message": warning.message,
                "hint": warning.hint,
            })
        })
        .collect();

    let error_value = error.map(|failure| {
        serde_json::json!({
            "code": failure.code.as_str(),
            "message": failure.message,
            "hint": failure.hint,
        })
    });

    let document = serde_json::json!({
        "envelope_version": ENVELOPE_VERSION,
        "ok": ok,
        "command": command,
        "data": data,
        "warnings": warning_values,
        "error": error_value,
    });

    serde_json::to_string_pretty(&document).expect("an envelope serializes")
}

/// Print the envelope to **stdout** and flush it.
///
/// The flush matters: a caller piping the CLI into `jq` sees nothing until the process exits, and
/// a pipeline that appears to hang looks like a server that never answered.
pub fn print(envelope: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{envelope}");
    let _ = stdout.flush();
}

/// Print a failure envelope to stdout and the sentence to stderr.
///
/// Both, on purpose. A human running the command sees the sentence; a script reading stdout gets
/// a document it can branch on. Writing the sentence *only* to stderr would leave a script with
/// an empty stdout and no document, which is the failure this envelope exists to remove.
pub fn print_failure(command: &str, failure: &Failure, warnings: &[Warning]) {
    print(&failure_envelope(command, failure, warnings));
    eprintln!(
        "omnion {command}: {} [{}]",
        failure.message,
        failure.code.as_str()
    );
}

/// The failure document, exposed for tests that assert the shape without spawning a process.
///
/// Named differently from [`failure`] because that function takes the very same parameter name,
/// and inside `failure_envelope` the parameter shadows the function — the call below then reads
/// as "call this `&Failure`", which is the error the compiler points at. Renaming the parameter
/// fixes the call site rather than working around it with `Self::`, because the shadowing would
/// still be there for the next reader to trip over.
pub fn failure_envelope(command: &str, reported: &Failure, warnings: &[Warning]) -> String {
    failure(command, reported, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(envelope: &str) -> serde_json::Value {
        serde_json::from_str(envelope).expect("the envelope is valid JSON")
    }

    #[test]
    fn a_successful_envelope_carries_every_documented_key() {
        let envelope = success("doctor", serde_json::json!({ "failures": 0 }), &[]);
        let document = parse(&envelope);

        for key in [
            "envelope_version",
            "ok",
            "command",
            "data",
            "warnings",
            "error",
        ] {
            assert!(
                document.get(key).is_some(),
                "the envelope documents {key:?}; it is missing from {envelope}"
            );
        }

        assert_eq!(document["envelope_version"], ENVELOPE_VERSION);
        assert_eq!(document["ok"], true);
        assert_eq!(document["command"], "doctor");
        assert_eq!(document["data"]["failures"], 0);
        assert_eq!(document["warnings"].as_array().expect("an array").len(), 0);
    }

    #[test]
    fn the_error_key_is_null_on_success_so_a_consumer_never_sees_a_missing_key() {
        // The distinction that matters: a consumer that writes `.error.code` must get `null` on a
        // success, not a key that raises. `get` returns None for a missing key and Some(Null) for
        // an explicit null, and only the second one keeps the consumer's code branch-free.
        let document = parse(&success("version", serde_json::json!({}), &[]));

        assert!(
            document
                .get("error")
                .is_some_and(serde_json::Value::is_null),
            "error is present and null: {document}"
        );
    }

    #[test]
    fn a_failed_envelope_names_the_code_and_the_hint() {
        let failure = Failure::with_hint(
            ErrorCode::DatabaseUnreachable,
            "connection refused",
            "start the stack with `omnion dev`",
        );
        let document = parse(&failure_envelope("migrate", &failure, &[]));

        assert_eq!(document["ok"], false);
        assert_eq!(document["command"], "migrate");
        assert_eq!(document["error"]["code"], "database_unreachable");
        assert_eq!(document["error"]["message"], "connection refused");
        assert_eq!(
            document["error"]["hint"],
            "start the stack with `omnion dev`"
        );
        assert!(document["data"].is_null(), "a failure carries no data");
    }

    #[test]
    fn every_error_code_has_a_distinct_stable_wire_form() {
        let codes = [
            ErrorCode::Usage,
            ErrorCode::ConfigUnreadable,
            ErrorCode::DatabaseUnreachable,
            ErrorCode::DependencyUnavailable,
            ErrorCode::NothingToDo,
            ErrorCode::ConfirmationRequired,
            ErrorCode::Refused,
            ErrorCode::Internal,
        ];

        let mut seen = std::collections::BTreeSet::new();
        for code in codes {
            assert!(
                seen.insert(code.as_str()),
                "{} is used twice; a code is a contract and must be unique",
                code.as_str()
            );
            assert!(
                code.as_str()
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '_'),
                "{} is not snake_case",
                code.as_str()
            );
        }
        assert_eq!(seen.len(), 8, "the code set is closed at eight");
    }

    #[test]
    fn warnings_survive_a_failure_alongside_the_error() {
        // A failing command can still have things worth saying. Collapsing them into the error
        // would lose the distinction between "this stopped you" and "this did not".
        let warning = Warning::with_hint("keychain", "no OS keychain", "use --store=file");
        let failure = Failure::new(ErrorCode::Refused, "nope");
        let document = parse(&failure_envelope("login", &failure, &[warning]));

        assert_eq!(document["ok"], false);
        assert_eq!(document["error"]["code"], "refused");
        assert_eq!(document["warnings"].as_array().expect("an array").len(), 1);
        assert_eq!(document["warnings"][0]["code"], "keychain");
        assert_eq!(document["warnings"][0]["hint"], "use --store=file");
    }

    #[test]
    fn the_envelope_is_one_document_so_a_pipeline_can_pipe_it_straight_into_a_parser() {
        // A second document on stdout — a stray human table, a progress line — makes the stream
        // unparseable, and the parser's complaint points at the consumer rather than at us.
        let envelope = success("doctor", serde_json::json!({ "ok": true }), &[]);
        assert!(
            envelope.lines().count() > 1,
            "pretty-printed, so it is multi-line"
        );
        assert!(serde_json::from_str::<serde_json::Value>(&envelope).is_ok());
    }

    #[test]
    fn a_failure_can_still_carry_the_output_it_produced() {
        // `doctor` runs every check and then fails on the ones that did not pass. If `data` were
        // nulled on failure, the document that explains the failure is the one document the
        // consumer cannot read — `ok: false` and nothing to act on.
        let document = parse(&failure_with_data(
            "doctor",
            &Failure::new(
                ErrorCode::DependencyUnavailable,
                "1 of 6 check(s) failed: database",
            ),
            serde_json::json!({ "failures": 1, "checks": [{ "check": "database" }] }),
            &[],
        ));

        assert_eq!(document["ok"], false);
        assert_eq!(document["error"]["code"], "dependency_unavailable");
        assert_eq!(document["data"]["failures"], 1);
        assert_eq!(document["data"]["checks"][0]["check"], "database");
    }

    #[test]
    fn ok_always_agrees_with_the_presence_of_an_error() {
        // The invariant every consumer relies on and the one a naive implementation breaks: a
        // document must not be able to say `ok: true` and carry an error, or `false` with none.
        // `doctor` shipped exactly that — `ok: true` beside a failing check, exit code 1 — and the
        // gate it would have fed read `.ok`.
        let with_error = parse(&failure_envelope(
            "doctor",
            &Failure::new(ErrorCode::DependencyUnavailable, "a check failed"),
            &[],
        ));
        assert_eq!(with_error["ok"], false);
        assert!(!with_error["error"].is_null());

        let without_error = parse(&success("doctor", serde_json::json!({}), &[]));
        assert_eq!(without_error["ok"], true);
        assert!(without_error["error"].is_null());

        // And the shape that would lie: `ok: true` next to a populated error is unreachable
        // through the constructors, which is the point — there is no path that produces it.
        assert_ne!(
            parse(&success("doctor", serde_json::json!({}), &[]))["ok"],
            json!(false)
        );
    }
}
