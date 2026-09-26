//! The SMTP client behind the `send_email` action.
//!
//! Deliberately small and dependency-free, like the rest of the platform's infrastructure
//! (the media library signs its own S3 requests for the same reason): a plain-text email is a
//! short line-based conversation, and implementing it here means the platform ships no vendor
//! SDK and no TLS stack it does not need. Development sends through **Mailpit**
//! (`infra/compose/mailpit.yml`), which is what `OMNION_SMTP_HOST`/`OMNION_SMTP_PORT` default
//! to; a production install points the same two settings at its own relay.
//!
//! Protocol: `EHLO` → optional `AUTH PLAIN` → `MAIL FROM` → one `RCPT TO` per recipient →
//! `DATA` → `QUIT`. Every reply is read as a full SMTP response (multi-line replies are
//! followed to their last line) and a non-success code fails with the server's own words.
//! What goes on the wire is checked first: an address or a subject carrying a line break would
//! let a definition inject headers, so both are refused before anything is sent.

use std::time::Duration as StdDuration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Longest accepted email address.
pub const MAX_ADDRESS: usize = 254;

/// Longest accepted subject.
pub const MAX_SUBJECT: usize = 300;

/// Longest accepted body (bytes).
pub const MAX_BODY: usize = 64 * 1024;

/// Where and how the platform sends email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailSettings {
    /// Whether the platform may send at all (`OMNION_MAIL_ENABLED`).
    pub enabled: bool,
    /// SMTP server host.
    pub host: String,
    /// SMTP server port (1025 is Mailpit's plain port).
    pub port: u16,
    /// The address every message is sent from.
    pub from: String,
    /// Username for `AUTH PLAIN`, when the server wants one.
    pub username: Option<String>,
    /// Password for `AUTH PLAIN`, when the server wants one.
    pub password: Option<String>,
    /// How long the whole conversation may take.
    pub timeout: StdDuration,
}

impl MailSettings {
    /// Settings for one server and one sender address.
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16, from: impl Into<String>) -> Self {
        Self {
            enabled: true,
            host: host.into(),
            port,
            from: from.into(),
            username: None,
            password: None,
            timeout: StdDuration::from_secs(10),
        }
    }

    /// Switch sending off (or back on) without losing the rest of the settings.
    #[must_use]
    pub fn with_sending(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Add credentials for a server that authenticates senders.
    #[must_use]
    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    /// Set the conversation timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: StdDuration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// One plain-text message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    /// Recipients, in order.
    pub to: Vec<String>,
    /// Subject line.
    pub subject: String,
    /// Plain-text body.
    pub body: String,
}

impl Email {
    /// A message to one recipient.
    #[must_use]
    pub fn new(to: impl Into<String>, subject: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            to: vec![to.into()],
            subject: subject.into(),
            body: body.into(),
        }
    }
}

/// Why a message could not be sent.
#[derive(Debug, thiserror::Error)]
pub enum MailError {
    /// The platform is configured not to send (`OMNION_MAIL_ENABLED=false`).
    #[error("sending email is switched off (OMNION_MAIL_ENABLED=false)")]
    Disabled,
    /// An address is not a usable email address (or carries a line break).
    #[error("invalid address: {0}")]
    Address(String),
    /// The subject is not usable (blank, too long, or carries a line break).
    #[error("invalid subject: {0}")]
    Subject(String),
    /// The body is larger than the platform sends.
    #[error("the body is larger than {MAX_BODY} bytes")]
    BodyTooLarge,
    /// No recipient was given.
    #[error("the message has no recipient")]
    NoRecipient,
    /// The conversation with the server failed.
    #[error("smtp: {0}")]
    Transport(String),
    /// The server answered an unexpected code.
    #[error("the smtp server answered {code}: {message}")]
    Refused {
        /// The code the server answered with.
        code: u16,
        /// The server's own line.
        message: String,
    },
    /// The server did not answer in time.
    #[error("the smtp server did not answer within {:?}", .0)]
    Timeout(StdDuration),
}

/// Send one message.
pub async fn send(settings: &MailSettings, email: &Email) -> Result<(), MailError> {
    if !settings.enabled {
        return Err(MailError::Disabled);
    }

    let from = validate_address(&settings.from)?;
    let subject = validate_subject(&email.subject)?;

    if email.to.is_empty() {
        return Err(MailError::NoRecipient);
    }
    let mut recipients = Vec::with_capacity(email.to.len());
    for raw in &email.to {
        recipients.push(validate_address(raw)?);
    }

    if email.body.len() > MAX_BODY {
        return Err(MailError::BodyTooLarge);
    }

    let payload = build_message(&from, &recipients, &subject, &email.body);
    let conversation = converse(settings, &from, &recipients, &payload);

    match tokio::time::timeout(settings.timeout, conversation).await {
        Ok(outcome) => outcome,
        Err(_) => Err(MailError::Timeout(settings.timeout)),
    }
}

/// The whole SMTP conversation.
async fn converse(
    settings: &MailSettings,
    from: &str,
    recipients: &[String],
    payload: &str,
) -> Result<(), MailError> {
    let stream = TcpStream::connect((settings.host.as_str(), settings.port))
        .await
        .map_err(|err| {
            MailError::Transport(format!(
                "connect {}:{}: {err}",
                settings.host, settings.port
            ))
        })?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Greeting.
    expect(&mut reader, 220).await?;

    // EHLO, then the capability list the server reports (multi-line).
    write_line(&mut writer, "EHLO omnion").await?;
    let capabilities = expect(&mut reader, 250).await?;

    if let Some(username) = &settings.username {
        let password = settings.password.clone().unwrap_or_default();
        let token = base64(format!("\0{username}\0{password}").as_bytes());
        write_line(&mut writer, &format!("AUTH PLAIN {token}")).await?;
        expect(&mut reader, 235).await?;
    } else if mentions_auth(&capabilities) {
        // Not fatal: most development servers accept unauthenticated local mail. It is worth a
        // line in the log, because a production relay that wants AUTH will refuse MAIL FROM.
        tracing::debug!("the smtp server advertises AUTH but no credentials are configured");
    }

    write_line(&mut writer, &format!("MAIL FROM:<{from}>")).await?;
    expect(&mut reader, 250).await?;

    for recipient in recipients {
        write_line(&mut writer, &format!("RCPT TO:<{recipient}>")).await?;
        expect_any(&mut reader, &[250, 251]).await?;
    }

    write_line(&mut writer, "DATA").await?;
    expect(&mut reader, 354).await?;

    writer
        .write_all(payload.as_bytes())
        .await
        .map_err(|err| MailError::Transport(format!("write body: {err}")))?;
    writer
        .write_all(b".\r\n")
        .await
        .map_err(|err| MailError::Transport(format!("write end of data: {err}")))?;
    expect(&mut reader, 250).await?;

    // QUIT is politeness: the message is accepted at this point, so a failure here is not a
    // failure of the send.
    if write_line(&mut writer, "QUIT").await.is_ok() {
        let _ = expect(&mut reader, 221).await;
    }

    Ok(())
}

/// `true` when a capability list mentions AUTH.
fn mentions_auth(capabilities: &str) -> bool {
    capabilities
        .lines()
        .any(|line| line.to_ascii_uppercase().contains("AUTH"))
}

/// Write one command line.
async fn write_line(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    line: &str,
) -> Result<(), MailError> {
    writer
        .write_all(format!("{line}\r\n").as_bytes())
        .await
        .map_err(|err| MailError::Transport(format!("write {line:?}: {err}")))
}

/// Read one (possibly multi-line) reply and require a code.
async fn expect(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    code: u16,
) -> Result<String, MailError> {
    expect_any(reader, &[code]).await
}

/// Read one reply and require one of the codes.
async fn expect_any(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    codes: &[u16],
) -> Result<String, MailError> {
    let (code, text) = read_reply(reader).await?;
    if !codes.contains(&code) {
        return Err(MailError::Refused {
            code,
            message: text,
        });
    }
    Ok(text)
}

/// Read one SMTP reply: the first line's code, then its continuation lines.
async fn read_reply(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Result<(u16, String), MailError> {
    let mut collected = String::new();
    let mut code = 0_u16;

    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|err| MailError::Transport(format!("read reply: {err}")))?;
        if read == 0 {
            return Err(MailError::Transport(
                "the smtp server closed the connection".to_owned(),
            ));
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.len() < 3 || !trimmed.as_bytes()[..3].iter().all(u8::is_ascii_digit) {
            return Err(MailError::Transport(format!(
                "the smtp server sent a line that is not a reply: {trimmed:?}"
            )));
        }

        if code == 0 {
            code = trimmed[..3].parse().unwrap_or_default();
        }
        collected.push_str(trimmed.get(4..).unwrap_or_default());

        // "250-text" is a continuation; "250 text" is the last line of the reply.
        let last = trimmed.as_bytes().get(3) == Some(&b' ');
        if last {
            break;
        }
        collected.push('\n');
    }

    Ok((code, collected))
}

/// Build the RFC 5322 message: headers, blank line, body with CRLF endings.
fn build_message(from: &str, recipients: &[String], subject: &str, body: &str) -> String {
    let mut message = String::with_capacity(body.len() + 256);
    message.push_str(&format!("From: {from}\r\n"));
    message.push_str(&format!("To: {}\r\n", recipients.join(", ")));
    message.push_str(&format!("Subject: {subject}\r\n"));
    message.push_str(&format!("Date: {}\r\n", http_date()));
    message.push_str(&format!(
        "Message-ID: <{}@omnion>\r\n",
        uuid::Uuid::new_v4()
    ));
    message.push_str("MIME-Version: 1.0\r\n");
    message.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    message.push_str("Content-Transfer-Encoding: 8bit\r\n");
    message.push_str("\r\n");
    message.push_str(&dot_stuff(body));
    message
}

/// Normalize a body for the DATA command: CRLF endings, one CRLF per line, dots escaped.
///
/// A line that begins with a dot would otherwise end the DATA block; SMTP escapes it by doubling
/// the dot, and the receiver removes one again. The result always ends with a CRLF, which is the
/// line the terminating `.` follows.
#[must_use]
pub fn dot_stuff(body: &str) -> String {
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let lines = normalized.strip_suffix('\n').unwrap_or(&normalized);
    let mut out = String::with_capacity(normalized.len() + 8);

    for line in lines.split('\n') {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
        out.push_str("\r\n");
    }

    out
}

/// An RFC 5322 date for the `Date:` header.
fn http_date() -> String {
    use time::format_description::well_known::Rfc2822;
    time::OffsetDateTime::now_utc()
        .format(&Rfc2822)
        .unwrap_or_else(|_| "Thu, 01 Jan 1970 00:00:00 +0000".to_owned())
}

/// Check an address before it reaches a command line.
///
/// The rules are the ones that matter on the wire: one `@`, no whitespace, no line break, a
/// dotted domain, and a bounded length. Header/command injection is impossible because a
/// CR or LF anywhere is refused outright.
pub fn validate_address(raw: &str) -> Result<String, MailError> {
    let address = raw.trim().to_owned();
    if address.is_empty() || address.len() > MAX_ADDRESS {
        return Err(MailError::Address(format!(
            "{raw:?} is not an address of at most {MAX_ADDRESS} characters"
        )));
    }
    if address.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(MailError::Address(format!("{raw:?} carries whitespace")));
    }

    let mut parts = address.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(MailError::Address(format!("{raw:?} needs exactly one @")));
    };
    // A development relay answers to `localhost`; anything else wants a dotted domain, which is
    // what a relay needs to route the message onward.
    let routable = domain.contains('.') || domain.eq_ignore_ascii_case("localhost");
    if local.is_empty() || domain.is_empty() || !routable {
        return Err(MailError::Address(format!(
            "{raw:?} needs a local part and a dotted domain"
        )));
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    if !local.chars().all(allowed)
        || !domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(MailError::Address(format!(
            "{raw:?} carries characters an address cannot carry"
        )));
    }

    Ok(address)
}

/// Check a subject before it reaches a header line.
pub fn validate_subject(raw: &str) -> Result<String, MailError> {
    let subject = raw.trim();
    if subject.is_empty() {
        return Err(MailError::Subject("the subject is empty".to_owned()));
    }
    if subject.chars().count() > MAX_SUBJECT {
        return Err(MailError::Subject(format!(
            "a subject is at most {MAX_SUBJECT} characters"
        )));
    }
    if subject.contains(['\r', '\n']) {
        return Err(MailError::Subject(
            "a subject cannot carry a line break".to_owned(),
        ));
    }

    Ok(subject.to_owned())
}

/// Standard base64, for the `AUTH PLAIN` token.
#[must_use]
pub fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map(u32::from).unwrap_or(0);
        let b2 = chunk.get(2).copied().map(u32::from).unwrap_or(0);
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn addresses_are_checked_before_they_reach_the_wire() {
        assert_eq!(
            validate_address("ada@example.com").expect("valid"),
            "ada@example.com"
        );
        assert_eq!(
            validate_address("  editor+tag@mail.example.co.uk ").expect("valid"),
            "editor+tag@mail.example.co.uk"
        );

        for broken in [
            "",
            "ada",
            "@example.com",
            "ada@",
            "ada@example",
            "ada@exa mple.com",
            "ada@example.com\r\nBcc: someone@example.com",
            "ada\n@example.com",
            "a\"b@example.com",
        ] {
            let error = validate_address(broken).expect_err("refused");
            assert!(matches!(error, MailError::Address(_)), "{broken:?}");
        }
    }

    #[test]
    fn a_subject_is_checked_before_it_reaches_the_header() {
        assert_eq!(
            validate_subject(" Published: home ").expect("valid"),
            "Published: home"
        );
        assert!(validate_subject("").is_err());
        assert!(validate_subject("Hi\r\nBcc: someone@example.com").is_err());
        assert!(validate_subject(&"a".repeat(MAX_SUBJECT + 1)).is_err());
    }

    #[test]
    fn the_body_is_normalized_and_dots_are_escaped() {
        let stuffed = dot_stuff("first\n.second\r\nthird\n");
        assert_eq!(stuffed, "first\r\n..second\r\nthird\r\n");
        assert!(
            stuffed.ends_with("\r\n"),
            "the DATA block ends with a CRLF line"
        );
    }

    #[test]
    fn the_message_carries_the_headers_a_receiver_expects() {
        let message = build_message(
            "omnion@localhost",
            &["ada@example.com".to_owned()],
            "Published: home",
            "home is live.",
        );
        assert!(message.starts_with("From: omnion@localhost\r\n"));
        assert!(message.contains("\r\nTo: ada@example.com\r\n"));
        assert!(message.contains("\r\nSubject: Published: home\r\n"));
        assert!(message.contains("\r\nMIME-Version: 1.0\r\n"));
        assert!(message.contains("\r\nContent-Type: text/plain; charset=utf-8\r\n"));
        assert!(message.contains("\r\n\r\nhome is live.\r\n"));
    }

    #[test]
    fn base64_matches_the_standard() {
        // RFC 4648 test vectors.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"\0user\0pass"), "AHVzZXIAcGFzcw==");
    }

    #[tokio::test]
    async fn a_message_travels_a_real_smtp_conversation() {
        // A sink that speaks just enough SMTP: greeting, EHLO, AUTH, MAIL/RCPT, DATA, QUIT.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("address").port();
        let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);

            writer.write_all(b"220 sink ready\r\n").await.ok();

            let mut commands: Vec<String> = Vec::new();
            let mut data = String::new();
            let mut line = String::new();

            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                let command = line.trim_end().to_owned();
                let upper = command.to_ascii_uppercase();
                commands.push(command);

                if upper.starts_with("EHLO") {
                    writer
                        .write_all(b"250-sink\r\n250-SIZE 102400\r\n250 AUTH PLAIN\r\n")
                        .await
                        .ok();
                } else if upper.starts_with("AUTH") {
                    writer.write_all(b"235 authenticated\r\n").await.ok();
                } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
                    writer.write_all(b"250 ok\r\n").await.ok();
                } else if upper == "DATA" {
                    writer.write_all(b"354 go ahead\r\n").await.ok();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            break;
                        }
                        if line == ".\r\n" {
                            break;
                        }
                        data.push_str(&line);
                    }
                    writer.write_all(b"250 queued\r\n").await.ok();
                } else if upper == "QUIT" {
                    writer.write_all(b"221 bye\r\n").await.ok();
                    break;
                }
            }

            let _ = sent_tx.send((commands, data));
        });

        let settings = MailSettings::new("127.0.0.1", port, "omnion@localhost")
            .with_credentials("omnion", "secret")
            .with_timeout(StdDuration::from_secs(5));
        let email = Email::new(
            "ada@example.com",
            "Published: home",
            "home is live.\n.leading dot line",
        );

        send(&settings, &email)
            .await
            .expect("the sink accepts the message");

        let (commands, data) = sent_rx.await.expect("the sink reports what it read");
        assert!(
            commands
                .iter()
                .any(|c| c == &format!("AUTH PLAIN {}", base64(b"\0omnion\0secret"))),
            "the credentials travel as a PLAIN token: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c == "MAIL FROM:<omnion@localhost>"),
            "{commands:?}"
        );
        assert!(data.contains("Subject: Published: home\r\n"), "{data}");
        assert!(data.contains("To: ada@example.com\r\n"), "{data}");
        assert!(
            data.contains("..leading dot line\r\n"),
            "the dot is escaped: {data}"
        );
        assert!(data.ends_with("\r\n"), "{data}");
        server.await.ok();
    }

    #[tokio::test]
    async fn a_server_that_refuses_reports_its_own_words() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("address").port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();

            writer.write_all(b"220 sink ready\r\n").await.ok();
            reader.read_line(&mut line).await.ok();
            writer.write_all(b"250 sink\r\n").await.ok();
            line.clear();
            reader.read_line(&mut line).await.ok();
            writer
                .write_all(b"550 sender address rejected\r\n")
                .await
                .ok();
        });

        let settings = MailSettings::new("127.0.0.1", port, "omnion@localhost")
            .with_timeout(StdDuration::from_secs(5));
        let error = send(&settings, &Email::new("ada@example.com", "Hi", "Hello"))
            .await
            .expect_err("the server refuses");
        match error {
            MailError::Refused { code, message } => {
                assert_eq!(code, 550);
                assert!(message.contains("sender address rejected"), "{message}");
            }
            other => panic!("expected a refusal, got {other}"),
        }
        server.await.ok();
    }

    #[tokio::test]
    async fn an_unreachable_server_is_a_transport_failure() {
        // Port 1 on loopback is not listening; the failure must be reported, not swallowed.
        let settings = MailSettings::new("127.0.0.1", 1, "omnion@localhost")
            .with_timeout(StdDuration::from_millis(500));
        let error = send(&settings, &Email::new("ada@example.com", "Hi", "Hello"))
            .await
            .expect_err("nothing is listening");
        assert!(matches!(error, MailError::Transport(_)), "{error}");
    }

    #[tokio::test]
    async fn a_message_without_a_recipient_or_with_a_bad_one_is_refused_before_connecting() {
        let settings = MailSettings::new("127.0.0.1", 1, "omnion@localhost");
        let mut email = Email::new("ada@example.com", "Hi", "Hello");
        email.to.clear();
        assert!(matches!(
            send(&settings, &email).await.expect_err("no recipient"),
            MailError::NoRecipient
        ));

        let email = Email::new("not-an-address", "Hi", "Hello");
        assert!(matches!(
            send(&settings, &email).await.expect_err("bad recipient"),
            MailError::Address(_)
        ));
    }

    #[tokio::test]
    async fn a_switched_off_platform_does_not_connect_at_all() {
        let settings = MailSettings::new("127.0.0.1", 1, "omnion@localhost").with_sending(false);
        let error = send(&settings, &Email::new("ada@example.com", "Hi", "Hello"))
            .await
            .expect_err("sending is switched off");
        assert!(matches!(error, MailError::Disabled), "{error}");
        assert!(
            error.to_string().contains("OMNION_MAIL_ENABLED"),
            "the message names the switch: {error}"
        );
    }
}
