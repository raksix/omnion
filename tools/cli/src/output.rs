//! Small terminal helpers: aligned key/value lines and check markers.

/// Width the check and key columns are padded to.
const COLUMN: usize = 15;

/// Print one `key   value` line, aligned for a terminal.
pub fn pair(key: &str, value: &str) {
    println!("  {key:<COLUMN$}  {value}");
}

/// Print one check line (`ok`/`fail`), aligned like a pair.
pub fn check(marker: &str, name: &str, detail: &str) {
    println!("  {marker:<5} {name:<COLUMN$}  {detail}");
}

/// Print a hint under a check line.
pub fn hint(text: &str) {
    println!("        {:<COLUMN$}  hint: {text}", "");
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
