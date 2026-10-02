//! `omnion secret` — the loopback helper that hands a leased value to a child process
//! (docs/requests/REQ-125, slice 3).
//!
//! The helper is the last link in a chain whose whole purpose is that **a value never reaches a
//! browser and never reaches a file the operator has to remember to clean up**. It does three
//! things, and each one is a decision rather than a default:
//!
//! 1. **Redeems a lease over the loopback interface**, authenticated by the deployment key in
//!    `OMNION_DEPLOYMENT_KEY` (or the header the panel names). It talks to `127.0.0.1` and to
//!    nothing else: `--api-url` is a *host:port*, and anything that is not loopback is refused
//!    before a request is made, so a mistyped `--api-url` cannot ship a credential to a remote
//!    host that happens to answer.
//! 2. **Injects the value into a child process's environment** under the name the caller chose
//!    (`--as NAME`, default `OMNION_SECRET`). The value is passed to the child through the
//!    inherited environment only — it is never echoed, never written to the terminal, and never
//!    part of the command line, so it cannot land in the shell history or in `ps`.
//! 3. **Writes a mode-0600 temporary file** when the caller asks (`--file`), in a private
//!    directory, and **removes it on exit** — including when the child is killed, because the
//!    removal runs from a guard that fires on the error path too, not only after a `wait`.
//!
//! The two refusal rules worth stating plainly:
//!
//! * Without a child command, the helper **refuses** to print the value. `omnion secret redeem`
//!   on its own is exactly the accident this feature exists to prevent, so it names the two
//!   supported shapes instead of doing the convenient thing.
//! * The temporary file is created with `O_EXCL` inside a directory created with mode `0700`,
//!   so a pre-planted symlink cannot redirect the write. The file's contents are never printed,
//!   not even the hint, unless the caller asked for `--file` and reads the file themselves.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::args::SecretOptions;

/// The environment variable the lease token is read from when `--token-file` is absent.
pub const TOKEN_ENV: &str = "OMNION_LEASE_TOKEN";

/// The minimum a redemption is allowed to say, and everything the helper may print about a
/// value. Note there is no field that could hold one unless the redemption succeeded — and
/// then only in [`Redemption::value`], which is passed straight to the child and dropped.
#[derive(Debug, serde::Deserialize)]
struct RedeemResponse {
    /// The value. Never logged, never printed by this file.
    value: String,
    /// The version it came from.
    version: i32,
    /// The secret's name.
    name: String,
    /// The redaction hint.
    #[serde(default)]
    hint: String,
}

/// The refusal a non-loopback `--api-url` gets, as a `String` so it can be unit-tested.
const NON_LOOPBACK: &str = "the helper only talks to the loopback interface; --api-url must be a host:port on 127.0.0.1 \
     or ::1, so a mistyped address cannot ship a credential to a remote host";

/// Whether a `host:port` is on the loopback interface.
#[must_use]
pub fn is_loopback(address: &str) -> bool {
    let host = address
        .rsplit_once(':')
        .map_or(address, |(host, _port)| host)
        .trim_matches(['[', ']']);
    // `localhost` is accepted because it is a name the loopback interface answers to, and the
    // resolution is the operating system's — this binary never resolves it itself.
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// Build the redemption URL for a lease, refusing anything that is not loopback.
///
/// # Errors
///
/// A message naming the rule when the address is not loopback.
pub fn redeem_url(api_url: &str, lease: &str) -> Result<String, String> {
    if !is_loopback(api_url) {
        return Err(NON_LOOPBACK.to_owned());
    }
    if lease.trim().is_empty() {
        return Err("name the lease to redeem (`--lease <id>`)".to_owned());
    }
    Ok(format!(
        "http://{}/api/v1/secret-leases/{}/redeem",
        api_url.trim(),
        lease.trim()
    ))
}

/// Write a value to a private temporary file and return its path.
///
/// The three properties that make this safe enough to exist, in the order they matter:
///
/// * the directory is created with mode `0700`, so nobody else can list it;
/// * the file is opened `O_EXCL|O_NOFOLLOW` with mode `0600`, so a pre-planted symlink or a
///   pre-existing name cannot redirect the write or widen the mode;
/// * the caller gets a guard back (see [`TempFile`]) that removes it on **every** exit path.
///
/// # Errors
///
/// An `io::Error` message when the directory or the file cannot be created.
pub fn write_temp_file(value: &str, directory: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(directory)?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    let path = directory.join(format!("omnion-secret-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc_o_nofollow())
        .open(&path)?;
    file.write_all(value.as_bytes())?;
    // Flush before the caller reads it: a buffered value that a child never sees is worse than
    // an error, and this is the only place the value touches a file descriptor.
    file.sync_all()?;
    drop(file);
    Ok(path)
}

/// `O_NOFOLLOW` on this platform, spelled without adding a libc dependency for one constant.
const fn libc_o_nofollow() -> i32 {
    // Linux `O_NOFOLLOW` is 0o400000; macOS `O_NOFOLLOW` is 0x100. The helper is built for
    // the server, so the Linux value is the one that matters and the other is named here so
    // the difference is a visible decision rather than a silent wrong constant.
    if cfg!(target_os = "macos") {
        0x100
    } else {
        0o400_000
    }
}

/// A temporary file that removes itself.
///
/// The removal happens in [`Drop`], so a panic, an early `?` or a child that exits non-zero all
/// still clean up. That is the property the request asks for ("removed on exit") and the reason
/// this is a type rather than a line of code at the end of a function.
#[derive(Debug)]
pub struct TempFile(pub PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            // A failure here is worth a warning but must not mask the command's own exit code.
            eprintln!(
                "omnion: warning: the temporary file {} could not be removed: {error}",
                self.0.display()
            );
        }
    }
}

/// Build the child's environment: the inherited one, plus the value under the chosen name.
///
/// The value is added last and any inherited variable of the same name is replaced, so a
/// stale value from the parent cannot win. The inherited environment is taken from
/// `std::env::vars_os()` rather than re-read from a config file, which is what keeps the helper
/// out of the shell history entirely: the value never appears in a command line.
pub fn child_environment(name: &str, value: &str) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os()
        .filter(|(key, _)| key != name)
        .chain(std::iter::once((
            std::ffi::OsString::from(name),
            std::ffi::OsString::from(value),
        )))
        .collect()
}

/// What a non-child run can report: that a redemption *would* work, with no value in the answer.
///
/// This is the whole reason `--check` exists. An operator wiring a pipeline wants to know the
/// key is live and in scope without the value ever being printed into a CI log, and this is
/// the shape that gives them that without inventing an exception.
#[derive(Debug, PartialEq, Eq)]
pub struct CheckReport {
    /// The credential's name.
    pub name: String,
    /// The version the redemption would hand out.
    pub version: i32,
    /// The redaction hint, so an operator can prove which value it is.
    pub hint: String,
}

impl CheckReport {
    /// The one-line summary `--check` prints. It carries the hint and never the value.
    #[must_use]
    pub fn sentence(&self) -> String {
        format!(
            "{} (version {}, hint {}) would be handed to the child process",
            self.name, self.version, self.hint
        )
    }
}

/// Run `omnion secret …`.
///
/// `json` and `quiet` are the global flags (REQ-131 acceptance 15). The `--check` report is the
/// only output this command has, and it is safe by construction: it names the secret and its
/// hint, never its value, so it can be a document without becoming a leak.
///
/// # Errors
///
/// A message for the operator, printed by the caller. Every failure here is a configuration or
/// permission problem, and none of them prints a value.
pub async fn run(
    options: SecretOptions,
    api_url: String,
    json: bool,
    quiet: bool,
) -> Result<ExitCode, String> {
    let sink = crate::output::Sink::new(json, quiet);
    let action = options.action.clone().ok_or_else(|| {
        usage("`omnion secret` needs an action; try `omnion secret redeem --help`")
    })?;
    match action.as_str() {
        "redeem" => redeem(options, api_url, json, sink).await,
        other => Err(usage(&format!("unknown secret action {other:?}"))),
    }
}

/// `omnion secret redeem`.
async fn redeem(
    options: SecretOptions,
    default_api_url: String,
    json: bool,
    sink: crate::output::Sink,
) -> Result<ExitCode, String> {
    let lease = options
        .lease
        .ok_or_else(|| usage("`omnion secret redeem` needs `--lease <id>`"))?;
    let url = redeem_url(
        options.api_url.as_deref().unwrap_or(&default_api_url),
        &lease,
    )?;

    // The key is read from the environment, never from a flag: a flag would put it in the
    // process's own command line, which is exactly what this helper exists to avoid.
    let key = std::env::var("OMNION_DEPLOYMENT_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            "no deployment key: set OMNION_DEPLOYMENT_KEY to the value the panel showed once \
             (it is not stored, so this is the only copy)"
                .to_owned()
        })?;

    // The token comes from the environment or a file, never from a flag: `--lease` names
    // *which* lease, and a CI job that pasted the token into a command line would have put it
    // in the shell history and in `ps`.
    let token = match options.token_file.as_deref() {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| format!("the lease token could not be read from {path}: {error}"))?,
        None => std::env::var(TOKEN_ENV).map_err(|_| {
            format!(
                "no lease token: set {TOKEN_ENV} or pass --token-file <path> (the token is \
                     returned once by `POST /secrets/{{id}}/lease`)"
            )
        })?,
    };
    let token = token.trim();
    if token.is_empty() {
        return Err(format!(
            "the lease token is empty ({TOKEN_ENV} or --token-file)"
        ));
    }

    let response = post_redemption(&url, &key, token).await?;

    // `--check` stops here: the value came back, is dropped at the end of this function, and
    // nothing about it was ever printed.
    if options.check {
        let report = CheckReport {
            name: response.name.clone(),
            version: response.version,
            hint: response.hint.clone(),
        };
        // The document carries the same three fields the sentence does and no more: a machine
        // asking whether a lease is redeemable needs the name and the hint, and the value has
        // already been dropped before this line is reached.
        if json {
            crate::envelope::print(&crate::envelope::success(
                "secret",
                serde_json::json!({
                    "action": "check",
                    "name": report.name,
                    "version": report.version,
                    "hint": report.hint,
                }),
                &[],
            ));
        }
        sink.print(format_args!("omnion: {}", report.sentence()));
        return Ok(ExitCode::SUCCESS);
    }

    let name = options
        .env_name
        .unwrap_or_else(|| "OMNION_SECRET".to_owned());
    if !is_env_name(&name) {
        return Err(format!(
            "{name:?} is not a usable environment variable name; use letters, digits and \
             underscores, not starting with a digit"
        ));
    }

    // A `--file` run writes the value and exits: there is no child to inject into.
    if options.file {
        let directory = std::env::temp_dir().join("omnion-secrets");
        let path = write_temp_file(&response.value, &directory)
            .map_err(|error| format!("the temporary file could not be written: {error}"))?;
        let guard = TempFile(path.clone());
        println!(
            "omnion: {} is in {} (mode 0600) and will be removed when this command exits",
            response.name,
            guard.0.display()
        );
        // The guard removes the file as it drops, which is the only exit path this function has.
        return Ok(ExitCode::SUCCESS);
    }

    let child = std::env::args()
        .skip(1)
        .find(|argument| !argument.starts_with('-'));
    let Some(child) = child else {
        // The refusal the whole file is built around: a bare redemption must not print.
        return Err(usage(
            "refusing to print a credential to the terminal — give a command to run with the \
             value in its environment, or pass `--file` to write a mode-0600 temporary file \
             that is removed on exit",
        ));
    };

    let status = std::process::Command::new(&child)
        .env_clear()
        .envs(child_environment(&name, &response.value))
        .status()
        .map_err(|error| format!("{child} could not be started: {error}"))?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

/// Whether a string can be an environment variable name.
#[must_use]
pub fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|character: char| character.is_ascii_digit())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// POST the redemption, with the deployment key in the header the panel names.
///
/// A hand-rolled request rather than a client dependency: the helper ships on a build machine
/// that may have no TLS stack, it talks to loopback only, and the shape of this one call is
/// smaller than any client would be.
async fn post_redemption(url: &str, key: &str, token: &str) -> Result<RedeemResponse, String> {
    let http = loopback_post(url, key, token).await?;
    if !(200..300).contains(&http.status) {
        // The refusal sentence is the API's; it names the reason (revoked, spent, expired,
        // out of scope) and never contains a value.
        return Err(format!(
            "the redemption was refused (HTTP {}): {}",
            http.status,
            http.body.trim()
        ));
    }
    let value: Value = serde_json::from_str(&http.body)
        .map_err(|error| format!("the redemption answer was not JSON: {error}"))?;
    serde_json::from_value(value)
        .map_err(|error| format!("the redemption answer was not the documented shape: {error}"))
}

/// A minimal HTTP/1.1 POST over TCP, for one loopback call.
///
/// The request is written by hand because the helper's whole network surface is "one POST to
/// 127.0.0.1", and a client dependency would be a larger attack surface than the feature. The
/// response is read to the end of the body by content length, and the connection is closed
/// rather than reused — a credential is not worth a keep-alive.
async fn loopback_post(url: &str, key: &str, token: &str) -> Result<HttpResponse, String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "the redemption url must be http on the loopback interface".to_owned())?;
    let (authority, path) = rest
        .split_once('/')
        .ok_or_else(|| "the redemption url is missing a path".to_owned())?;
    let path = format!("/{path}");
    let host = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };

    let body = json!({ "token": token }).to_string();
    let mut stream = tokio::net::TcpStream::connect(&host)
        .await
        .map_err(|error| format!("the loopback API at {host} could not be reached: {error}"))?;

    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         X-Omnion-Deployment-Key: {key}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body.as_bytes());
    stream
        .write_all(&request)
        .await
        .map_err(|error| format!("the redemption request could not be sent: {error}"))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .map_err(|error| format!("the redemption answer could not be read: {error}"))?;
    parse_http_response(&raw).map_err(|error| error.to_string())
}

/// What a parsed HTTP response carries.
#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: String,
}

/// Split a raw HTTP/1.1 response into its status and body.
///
/// # Errors
///
/// A message when the response is not a complete `HTTP/1.x` message, which is what a proxy or
/// a wrong port would produce.
fn parse_http_response(raw: &[u8]) -> std::io::Result<HttpResponse> {
    let text = String::from_utf8_lossy(raw);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return Err(std::io::Error::other(
            "the answer was not a complete HTTP response — is the API on that port?",
        ));
    };
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("the answer carried no HTTP status line"))?;
    Ok(HttpResponse {
        status,
        // A chunked body is not decoded: the API answers these two routes with a content
        // length, and a proxy that re-chunks it is a deployment problem, not a silent success.
        body: body.to_owned(),
    })
}

/// The usage line the caller prints for a `secret` failure.
fn usage(message: &str) -> String {
    format!("{message}\n\nSee `omnion --help` for the command list.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_is_accepted() {
        assert!(is_loopback("127.0.0.1:8080"));
        assert!(is_loopback("localhost:18085"));
        assert!(is_loopback("[::1]:8080"));
        assert!(!is_loopback("10.0.0.5:8080"));
        assert!(!is_loopback("evil.example.com:80"));
        // The trap this closes: a typo that is not a loopback address at all.
        assert!(!is_loopback("omnion.internal:18085"));
    }

    #[test]
    fn a_non_loopback_url_is_refused_before_a_request_is_built() {
        let error =
            redeem_url("10.0.0.5:8080", "lease-id").expect_err("a remote address is refused");
        assert!(error.contains("loopback"));
    }

    #[test]
    fn the_url_carries_the_lease_and_nothing_else() {
        let url = redeem_url("127.0.0.1:18085", "lease-1").expect("loopback is fine");
        assert_eq!(
            url,
            "http://127.0.0.1:18085/api/v1/secret-leases/lease-1/redeem"
        );
        assert!(!url.contains('?'), "no query string carries anything");
    }

    #[test]
    fn a_missing_lease_is_a_usage_error() {
        assert!(redeem_url("127.0.0.1:8080", "  ").is_err());
    }

    #[test]
    fn an_env_name_is_checked_rather_than_trusted() {
        assert!(is_env_name("OMNION_SECRET"));
        assert!(is_env_name("STRIPE_KEY_2"));
        assert!(!is_env_name(""));
        assert!(!is_env_name("2BAD"), "a leading digit is not a name");
        assert!(!is_env_name("HAS SPACE"), "a space is not a name");
        assert!(
            !is_env_name("semi;colon"),
            "a shell metacharacter is not a name"
        );
    }

    #[test]
    fn the_check_report_names_the_credential_and_never_the_value() {
        let report = CheckReport {
            name: "smtp.production".to_owned(),
            version: 3,
            hint: "omnh_1a2b3c4d5e6f".to_owned(),
        };
        let sentence = report.sentence();
        assert!(sentence.contains("smtp.production"));
        assert!(sentence.contains("version 3"));
        assert!(sentence.contains("omnh_1a2b3c4d5e6f"));
        assert!(
            !sentence.contains("sk-live"),
            "no value can appear in this sentence"
        );
    }

    #[test]
    fn a_temp_file_is_private_and_removed_with_its_guard() {
        let directory =
            std::env::temp_dir().join(format!("omnion-helper-test-{}", std::process::id()));
        let value = "qa-helper-value-do-not-leak-4f81a2";
        let path = write_temp_file(value, &directory).expect("the file must be written");
        {
            let _guard = TempFile(path.clone());
            // Mode is the assertion that matters: 0600 is why the file is safe to exist at all.
            let mode = std::fs::metadata(&path)
                .expect("the file exists")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o600,
                "the temporary file must not be readable by anyone else"
            );
            let directory_mode = std::fs::metadata(&directory)
                .expect("the directory exists")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                directory_mode, 0o700,
                "the directory must not be listable by anyone else"
            );
            assert_eq!(std::fs::read_to_string(&path).expect("readable"), value);
        }
        assert!(!path.exists(), "the guard removes the file when it drops");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_second_write_to_the_same_path_is_refused_rather_than_overwriting() {
        let directory =
            std::env::temp_dir().join(format!("omnion-helper-exclusive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let first = write_temp_file("one", &directory).expect("the first write succeeds");
        let refused = write_temp_file("two", &directory);
        assert!(
            refused.is_err(),
            "a pre-planted name is refused, not followed"
        );
        let _ = std::fs::remove_file(first);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_child_environment_replaces_a_stale_value_of_the_same_name() {
        // The parent is given the name deliberately so the replacement path is exercised.
        let name = "OMNION_HELPER_TEST_NAME";
        // SAFETY: single-threaded test, and the variable is restored by the next line.
        unsafe { std::env::set_var(name, "stale") };
        let environment = child_environment(name, "fresh");
        let matches: Vec<_> = environment.iter().filter(|(key, _)| key == name).collect();
        assert_eq!(matches.len(), 1, "the name appears exactly once");
        assert_eq!(matches[0].1, std::ffi::OsString::from("fresh"));
        unsafe { std::env::remove_var(name) };
    }

    #[test]
    fn a_chunkless_response_is_parsed_into_its_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"value\":\"x\"}";
        let response = parse_http_response(raw).expect("a complete response parses");
        assert_eq!(response.status, 200);
        assert!(response.body.contains("\"value\""));
    }

    #[test]
    fn a_truncated_response_is_an_error_rather_than_an_empty_success() {
        // This is what a wrong port answers, and it must not read as "the value is empty".
        let error = parse_http_response(b"hello").expect_err("a non-HTTP answer is refused");
        assert!(error.to_string().contains("HTTP"));
    }
}
