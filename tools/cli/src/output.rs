//! Small terminal helpers: aligned key/value lines and check markers.

/// Where a command's human report goes.
///
/// `--json` promises that stdout carries a document and nothing else (REQ-131), so the table a
/// human reads has to move rather than be dropped — an operator who pastes `--json` into a script
/// should still see what happened when they remove the flag, and the evidence should be in their
/// terminal history either way. So the report follows the flag: stdout for a plain run, stderr
/// under `--json`, and nowhere under `--quiet`.
///
/// The alternatives are both worse. Printing the table to stdout as well makes the stream
/// unparseable and the parser's complaint points at the consumer; dropping it makes the JSON-only
/// run unauditable, and an operator debugging with the very flag a CI step uses would see
/// nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// A plain run: the report is the point of the command, so it goes to stdout.
    Stdout,
    /// `--json`: stdout belongs to the envelope, the report moves to stderr.
    Stderr,
    /// `--quiet`: no report. The exit code and the failure sentences still happen.
    Silent,
}

impl Sink {
    /// Choose a sink from the two global flags.
    ///
    /// `json` wins over `quiet`: a caller who asked for both wants the document, and the table is
    /// the thing `quiet` was suppressing.
    pub fn new(json: bool, quiet: bool) -> Self {
        match (json, quiet) {
            (true, _) => Self::Stderr,
            (false, true) => Self::Silent,
            (false, false) => Self::Stdout,
        }
    }

    /// Write one report line to whichever stream this sink owns.
    pub fn print(self, args: std::fmt::Arguments<'_>) {
        match self {
            Self::Stdout => println!("{args}"),
            Self::Stderr => eprintln!("{args}"),
            Self::Silent => {}
        }
    }

    /// Whether a report would reach the terminal at all.
    pub fn is_visible(self) -> bool {
        !matches!(self, Self::Silent)
    }
}

/// Width the check and key columns are padded to.
const COLUMN: usize = 15;

/// Print one `key   value` line, aligned for a terminal.
///
/// Takes a [`Sink`] rather than choosing a stream itself, for the same reason the sink exists at
/// all: a helper that hard-codes stdout is one more place `--json` can be defeated, and these
/// three are the report's only printers.
pub fn pair(sink: Sink, key: &str, value: &str) {
    sink.print(format_args!("  {key:<COLUMN$}  {value}"));
}

/// Print one check line (`ok`/`warn`/`fail`), aligned like a pair.
pub fn check(sink: Sink, marker: &str, name: &str, detail: &str) {
    sink.print(format_args!("  {marker:<5} {name:<COLUMN$}  {detail}"));
}

/// Print a hint under a check line.
pub fn hint(sink: Sink, text: &str) {
    sink.print(format_args!("        {:<COLUMN$}  hint: {text}", ""));
}

/// Render a connection string without its password — terminal output is often pasted around.
pub fn describe_database_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<unparseable database URL>".to_owned();
    };
    let (authority, tail) = match rest.split_once('/') {
        Some((authority, tail)) => (authority, format!("/{tail}")),
        None => (rest, String::new()),
    };
    let authority = match authority.rsplit_once('@') {
        Some((userinfo, host)) => {
            let user = userinfo.split(':').next().unwrap_or(userinfo);
            format!("{user}:***@{host}")
        }
        None => authority.to_owned(),
    };
    format!("{scheme}://{authority}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sink_is_chosen_by_the_flags_and_json_wins_over_quiet() {
        assert_eq!(Sink::new(false, false), Sink::Stdout);
        assert_eq!(Sink::new(true, false), Sink::Stderr);
        assert_eq!(Sink::new(false, true), Sink::Silent);
        // Both flags together is a contradictory request, and the resolution has to be stated
        // rather than left to whichever arm happens to be written first.
        assert_eq!(Sink::new(true, true), Sink::Stderr);
    }

    #[test]
    fn only_silent_swallows_the_report() {
        // The distinction a gate depends on: `--json` moves the table off stdout but still shows
        // it, while `--quiet` is the only flag that discards it.
        assert!(Sink::Stdout.is_visible());
        assert!(Sink::Stderr.is_visible());
        assert!(!Sink::Silent.is_visible());
    }

    #[test]
    fn database_descriptions_never_carry_the_password() {
        assert_eq!(
            describe_database_url("postgres://omnion:sup3r-secret@127.0.0.1:5433/omnion"),
            "postgres://omnion:***@127.0.0.1:5433/omnion"
        );
        assert_eq!(
            describe_database_url("postgres://omnion@db:5432/omnion?sslmode=disable"),
            "postgres://omnion:***@db:5432/omnion?sslmode=disable"
        );
        assert!(!describe_database_url("postgres://u:p@h/db").contains(":p@"));
        assert_eq!(
            describe_database_url("not-a-url"),
            "<unparseable database URL>"
        );
    }
}
