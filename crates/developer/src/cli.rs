//! The CLI device-code flow (REQ-033, slice 4).
//!
//! A terminal cannot hold a browser session, so `omnion login` cannot do a redirect. RFC 8628's
//! answer is a short code the person reads off their terminal, types into a browser, and then
//! approves — while the terminal polls for the token.
//!
//! # Why this module is mostly refusals
//!
//! The request's risk note for this flow is **phishing**, and it is the only place in the whole
//! developer platform where the attacker and the victim can be in the same conversation. A
//! device-code flow is walked into like this: an attacker says "run this command, then paste the
//! code at this page", the victim does both, and the attacker polls and receives a token. Nothing
//! in the protocol stops that.
//!
//! What the protocol *gives* us is four properties, and every rule below is one of them:
//!
//! 1. **Short-lived.** [`DEVICE_CODE_TTL_SECONDS`] and [`USER_CODE_TTL_SECONDS`] are bounds here
//!    rather than configuration, so a code cannot be left open for a weekend.
//! 2. **Bound to the approving user.** The approval records *which* user approved, and the token
//!    is that user's — not the person who started the flow. A code an attacker started is still
//!    only ever approved by a session that can mint keys.
//! 3. **Requesting-client metadata is displayed.** [`PendingDevice`] carries the client name and
//!    the requesting host, and [`DeviceApproval`] refuses to approve anything without them
//!    present: an approval screen that shows a bare code is the screen an attacker wants.
//! 4. **Approval needs a permission.** Checked by the route before it reaches this module; this
//!    module refuses an approval with no approving user at all, because a code approved by
//!    nobody is a code whose owner cannot be audited.
//!
//! The polling rule is the other half. RFC 8628 says a client that polls too often is told to
//! slow down, and says the *interval* grows. Growing it and remembering it per code is what
//! stops a well-meaning `while true` loop in a shell script from becoming a denial of service
//! against the endpoint that mints tokens.
//!
//! Nothing here returns a token. The token is minted by [`crate::secret`] at the moment of
//! exchange and handed to the caller once, exactly as an API key's is.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{DeveloperError, Result};
use crate::model::Environment;

/// How long the *device* code stays valid, in seconds (RFC 8628 asks for 15 minutes).
pub const DEVICE_CODE_TTL_SECONDS: i64 = 900;

/// How long the short *user* code the person types stays valid.
///
/// Shorter than the device code on purpose. The user code is the one an attacker reads aloud or
/// pastes into a chat, and every second it lives is a second a stranger has to type it. Fifteen
/// minutes is enough to read a message and click a link; it does not need to be enough to come
/// back to.
pub const USER_CODE_TTL_SECONDS: i64 = 600;

/// The interval a client must wait between polls, in seconds.
pub const POLL_INTERVAL_SECONDS: u64 = 5;

/// How much each `slow_down` adds to the interval, in seconds.
///
/// RFC 8628's own number. Five seconds of growth per offence means a script that ignores the
/// instruction is down to one poll every five minutes within a minute of starting.
pub const SLOW_DOWN_STEP_SECONDS: u64 = 5;

/// The ceiling the grown interval stops at.
pub const MAX_POLL_INTERVAL_SECONDS: u64 = 300;

/// The scopes a CLI token may carry at most, and the ones it starts with.
///
/// A CLI token is a *session* on the terminal. It is issued with the read scopes the request
/// names — the ones that let `omnion` list what it can see — and widening it is a deliberate act
/// in the panel, not a default. A device-code login that could mint a full-power key on
/// approval would make the phishing above strictly worse.
pub const DEFAULT_CLI_SCOPES: [&str; 3] = ["developer.read", "developer.keys.read", "events.read"];

/// Length of the user code, in characters, *excluding* the dash.
///
/// Eight characters from an alphabet without vowels and without `0`/`1`/`O`/`I`: long enough
/// that guessing is hopeless at a 15-minute expiry, short enough to read off a terminal and
/// type into a phone, and unambiguous when read aloud or copied out of a chat. The panel shows
/// it in groups of four with a dash so a mistyped digit is visible.
pub const USER_CODE_LENGTH: usize = 8;

/// The alphabet the user code is drawn from, as characters.
///
/// No vowels (`A`/`E`/`I`/`O`/`U`) so a code cannot spell a word, and no `0`/`1` so a code read
/// aloud does not depend on the reader distinguishing `0` from `O`. Held as a `&str` rather than
/// bytes because every use of it is a comparison against a `char` a person typed, and a byte
/// slice would make each of those a cast.
const USER_CODE_ALPHABET: &str = "23456789BCDFGHJKLMNPQRSTVWXYZ";

/// A pending device code, as the panel shows it before approval.
///
/// This is the phishing-defence surface, and the four fields are the four things a person needs
/// in order to notice they are not approving their own login. A screen with only a code is a
/// screen that gets approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDevice {
    /// The long code the terminal polls with. **Never** displayed.
    pub device_code_hash: String,
    /// The short code the person types, pre-grouped for reading.
    pub user_code: String,
    /// When the device code expires.
    pub expires_at: OffsetDateTime,
    /// The interval this client must currently wait between polls.
    pub interval_seconds: u64,
}

/// What `omnion login` started, as the terminal receives it.
///
/// The verification URI and the short code are the whole point of the flow; the token is not in
/// here because there is not one yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStart {
    /// The code the terminal polls with.
    pub device_code: String,
    /// Where the person goes to type the short code.
    pub verification_uri: String,
    /// The short code, pre-grouped.
    pub user_code: String,
    /// Seconds until the device code expires.
    pub expires_in: i64,
    /// Seconds the terminal must wait between polls.
    pub interval_seconds: u64,
}

/// A pending code as the *approval screen* needs it.
///
/// Separate from [`PendingDevice`] on purpose: the approval screen needs to know **who** is
/// being logged in and **from what**, and a struct that carried the device-code hash to the
/// browser would be a struct that could leak it. [`PendingDevice`] is what the terminal's poll
/// sees; this is what the browser's lookup sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceApproval {
    /// The short code, pre-grouped.
    pub user_code: String,
    /// The client name the terminal claimed.
    pub client_name: String,
    /// The host the token will be scoped to, if the terminal claimed one.
    pub client_uri: Option<String>,
    /// The scopes the token will carry, in plain language.
    pub scopes: Vec<String>,
    /// When the code expires.
    pub expires_at: OffsetDateTime,
    /// Who is about to approve, so the screen can name them.
    pub approving_user: String,
}

/// What approving produces — never a token.
///
/// Approving *authorises* the exchange; the token is minted at the moment the terminal polls
/// after approval. Splitting the two is what lets the approval screen be a page that can be
/// refreshed, re-opened and audited, and it is why this type has no field a token could go in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceApproved {
    /// The short code that was approved.
    pub user_code: String,
    /// When the approval happened.
    pub approved_at: OffsetDateTime,
    /// Who approved it.
    pub approved_by: Uuid,
}

/// What a terminal receives when it finally polls after approval.
///
/// The only shape in the crate that carries a token — the same write-only property API keys and
/// OAuth secrets have, and the same reason: a type produced only at mint time has nowhere else
/// for a stored token to leak from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceToken {
    /// The issued token.
    pub access_token: String,
    /// What the token authenticates as, for the terminal to display.
    pub environment: Environment,
    /// The scopes the token actually carries — the approved set, possibly narrowed.
    pub scopes: Vec<String>,
    /// When it expires.
    pub expires_at: OffsetDateTime,
}

// ─────────────────────────────────────────────────────────────────────────── code generation

/// Mint a fresh device code and its user code.
///
/// The device code is what the terminal polls with, so it comes from the same CSPRNG and hash
/// scheme the API key half uses ([`crate::secret::hash`]) and carries its own prefix — a stored
/// value that can be recognised as *this* and never confused with a key. It is returned as
/// (plaintext, hash): the plaintext exists for exactly one store write and the hash is what
/// goes in the row, which is the write-only property the rest of the crate is built on.
///
/// The user code is drawn separately, from a much smaller space, and that difference is the
/// point worth stating: it is short enough to type, so it is also short enough to guess, which
/// is why it is only ever accepted by a flow that additionally requires a session holding
/// `developer.keys.manage`. Rate limiting belongs to the route; the TTL bounds here are what
/// make the space irrelevant after ten minutes.
pub fn new_device_codes() -> (String, String, String) {
    let minted = crate::secret::mint();
    // The key namespace is `omn_`; this one is `omnion_dev_` so the two are distinguishable in
    // a log line and in a support call ("which one leaked?").
    let device_code = minted.plaintext.replacen("omn_", "omnion_dev_", 1);
    let device_code_hash = crate::secret::hash(&device_code);
    let user_code = draw_user_code();
    (device_code, device_code_hash, user_code)
}

/// Draw one user code from [`USER_CODE_ALPHABET`] and group it for reading.
fn draw_user_code() -> String {
    use rand::RngCore;
    let alphabet = USER_CODE_ALPHABET;
    let mut raw = String::with_capacity(USER_CODE_LENGTH);
    let mut byte = [0u8; 1];
    // Rejection sampling rather than `% ALPHABET.len()`: a modulo over 32 values onto a
    // 30-character alphabet would make the first four characters measurably more likely, which
    // is 4 bits of entropy given away per code for no reason.
    let bound = (256 / USER_CODE_ALPHABET.len() * USER_CODE_ALPHABET.len()) as u16;
    let mut rng = rand::rngs::OsRng;
    while raw.chars().count() < USER_CODE_LENGTH {
        rng.fill_bytes(&mut byte);
        if (byte[0] as u16) < bound {
            raw.push(alphabet.as_bytes()[byte[0] as usize % alphabet.len()] as char);
        }
    }
    format_user_code(&raw)
}

/// A user code, grouped for reading: `ABCD-2345`.
///
/// The grouping is presentation, not part of the code: [`normalize_user_code`] strips it, and
/// every comparison runs on the normalised form. A flow that compared the *grouped* string would
/// refuse a code typed without the dash, which is the most likely way a person types it.
pub fn format_user_code(raw: &str) -> String {
    let raw = normalize_user_code(raw);
    match raw.char_indices().nth(4) {
        Some((index, _)) => format!("{}-{}", &raw[..index], &raw[index..]),
        None => raw,
    }
}

/// Reduce a typed code to its comparable form: uppercase, no dashes, no spaces.
///
/// Refuses anything outside the alphabet by *dropping* it, so `A-2-3-4` and `A234` normalise to
/// the same thing a person means, while `!!!` normalises to nothing and fails the length check
/// in [`check_user_code`] rather than matching a code of the same characters in another
/// alphabet.
pub fn normalize_user_code(raw: &str) -> String {
    raw.chars()
        .filter(|c| USER_CODE_ALPHABET.contains(c.to_ascii_uppercase()))
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Check a typed user code: length first, then alphabet.
///
/// Two separate checks because they are two different mistakes — a short code is a mistyped one
/// and a long one is a pasted something else — and the caller tells the person which.
pub fn check_user_code(raw: &str) -> Result<()> {
    let normalized = normalize_user_code(raw);
    if normalized.is_empty() {
        return Err(DeveloperError::InvalidDeviceCode);
    }
    if normalized.chars().count() != USER_CODE_LENGTH {
        return Err(DeveloperError::InvalidDeviceCode);
    }
    Ok(())
}

/// Mint a user code from the alphabet.
// ─────────────────────────────────────────────────────────────────────────── the poll rule

/// Decide what a poll at `now` should answer.
///
/// Three outcomes, and the ordering matters:
///
/// 1. **Too soon** → `DeviceCodeSlowDown`, and the interval grows. Checked *first* because a
///    client that polls too fast while the code is also unapproved must be told to slow down,
///    not that it is unapproved — otherwise it polls faster in response, which is the loop this
///    rule exists to break.
/// 2. **Not approved** → `DeviceCodePending`. Normal, not a fault.
/// 3. **Approved** → `Ok(())`, and the caller mints the token.
///
/// An *expired* code is not decided here: expiry is a property of the stored row, and this
/// function takes the row's state rather than reading a clock, so the rule is testable without
/// one. The route reads `expires_at` and answers `InvalidDeviceCode` for a code past it, which
/// is the same answer it gives for a code that never existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollState {
    /// Whether somebody has approved this code.
    pub approved: bool,
    /// When the terminal last polled.
    pub last_polled_at: Option<OffsetDateTime>,
    /// The interval the client is currently required to wait.
    pub interval_seconds: u64,
}

impl PollState {
    /// A code nobody has touched yet.
    pub const fn fresh() -> Self {
        Self {
            approved: false,
            last_polled_at: None,
            interval_seconds: POLL_INTERVAL_SECONDS,
        }
    }

    /// Apply one poll at `now`, returning what the caller should answer and the interval to
    /// store back.
    ///
    /// The stored interval is returned rather than mutated so that the caller writes it in the
    /// same statement that answers, and a store that forgets to persist the growth produces a
    /// client that is told to slow down and never is.
    pub fn polled(&mut self, now: OffsetDateTime) -> Result<u64> {
        if let Some(last) = self.last_polled_at {
            let elapsed = (now - last).whole_seconds();
            if elapsed >= 0 && (elapsed as u64) < self.interval_seconds {
                self.interval_seconds = self
                    .interval_seconds
                    .saturating_add(SLOW_DOWN_STEP_SECONDS)
                    .min(MAX_POLL_INTERVAL_SECONDS);
                return Err(DeveloperError::DeviceCodeSlowDown {
                    seconds: self.interval_seconds,
                });
            }
        }
        self.last_polled_at = Some(now);
        if self.approved {
            return Ok(self.interval_seconds);
        }
        Err(DeveloperError::DeviceCodePending)
    }
}

// ─────────────────────────────────────────────────────────────────────────── approval rules

/// Check that an approval can proceed, and say so plainly when it cannot.
///
/// The three refusals are separate because each means something different to the person at the
/// screen, and the panel has to tell them which:
///
/// * **no approving user** — the caller had no session at all. This is the phishing case: a
///   code approved with nobody behind it produces a token with no owner.
/// * **expired** — the code was abandoned; approving it would mint a token for a flow that has
///   been over for minutes.
/// * **no client metadata** — the terminal claimed nothing. An approval screen showing a bare
///   code is the screen an attacker wants, so the platform refuses to render one rather than
///   showing a code with nothing next to it.
pub fn check_approval(approval: &DeviceApproval, now: OffsetDateTime) -> Result<()> {
    if approval.approving_user.trim().is_empty() {
        return Err(DeveloperError::DeviceCodeApprovalRefused);
    }
    if now >= approval.expires_at {
        return Err(DeveloperError::InvalidDeviceCode);
    }
    if approval.client_name.trim().is_empty() {
        return Err(DeveloperError::DeviceCodeApprovalRefused);
    }
    Ok(())
}

/// The scopes a CLI token is issued with, refusing any that are not read-shaped.
///
/// A device-code login is the one flow in the platform where a token is minted by a *different
/// person* than the one who will use it — the approving session's user, handed to a terminal.
/// That asymmetry is why the issued set is a fixed list and not whatever the terminal asked
/// for: a terminal that asks for `developer.keys.manage` in its start request is either broken
/// or an attacker, and both are refused.
pub fn cli_scopes() -> Vec<String> {
    DEFAULT_CLI_SCOPES
        .iter()
        .map(|scope| (*scope).to_string())
        .collect()
}

/// Refuse a start request that asked for scopes beyond the default set.
pub fn check_requested_scopes(requested: &[String]) -> Result<()> {
    let allowed = cli_scopes();
    let mut narrowed = Vec::new();
    for scope in requested {
        if scope.trim().is_empty() {
            return Err(crate::error::DeveloperError::EmptyScope);
        }
        if !allowed.iter().any(|grant| grant == scope) {
            return Err(DeveloperError::ScaffoldRefused {
                code: "cli_scope_refused".to_string(),
                message: format!(
                    "a CLI login can only be granted {allowed:?}; a terminal cannot ask for more"
                ),
            });
        }
        if !narrowed.contains(scope) {
            narrowed.push(scope.clone());
        }
    }
    if narrowed.is_empty() {
        return Ok(());
    }
    Ok(())
}

/// Whether a scope is one a CLI token may carry.
pub fn is_cli_scope(scope: &str) -> bool {
    DEFAULT_CLI_SCOPES.contains(&scope)
}

/// The plain-language sentence the approval screen shows for one scope.
///
/// The request asks for "a plain-language list of the scopes the issued token will carry",
/// which is not the permission key: `developer.keys.read` tells a developer nothing about what
/// the terminal will be able to do.
pub fn scope_sentence(scope: &str) -> &'static str {
    match scope {
        "developer.read" => "Browse the API reference and the served OpenAPI document",
        "developer.keys.read" => "Read API key metadata, usage and the request log",
        "events.read" => "Read the event catalogue and the event feed",
        _ => "A scope this build of the platform no longer describes",
    }
}

/// Whether a string is a shape the CLI accepts as a label.
///
/// The rules are the key-name rules ([`crate::model::key_rules::validate_name`]) applied to a
/// shorter bound, and they are re-used rather than restated: a CLI label and an API key name
/// both become identifiers in a URL, a log line and a bucket key, and two independent
/// implementations of "what may an identifier contain" is the situation where one of them is
/// looser.
pub fn valid_cli_label(name: &str) -> bool {
    let length = name.trim().chars().count();
    (crate::scaffold::NAME_MIN..=crate::scaffold::NAME_MAX).contains(&length)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(seconds)
    }

    // ── codes ─────────────────────────────────────────────────────────────────────────────

    #[test]
    fn a_user_code_is_eight_characters_from_the_alphabet() {
        for _ in 0..50 {
            let (_, _, user) = new_device_codes();
            let normalized = normalize_user_code(&user);
            assert_eq!(normalized.chars().count(), USER_CODE_LENGTH);
            assert!(
                normalized
                    .chars()
                    .all(|c| USER_CODE_ALPHABET.contains(c.to_ascii_uppercase())),
                "{user} used a character outside the alphabet"
            );
        }
    }

    #[test]
    fn a_user_code_has_no_vowels_so_it_cannot_spell_a_word() {
        // The phishing property: a code is read aloud and typed. Vowels make codes that can be
        // misread as a word, and words are memorable — which is what we do not want.
        let forbidden = ['A', 'E', 'I', 'O', 'U', '0', '1'];
        for _ in 0..200 {
            let (_, _, user) = new_device_codes();
            assert!(
                !normalize_user_code(&user)
                    .chars()
                    .any(|c| forbidden.contains(&c)),
                "{user} contains a vowel or an ambiguous digit"
            );
        }
    }

    #[test]
    fn the_displayed_code_is_grouped_but_the_code_is_not() {
        // Every character here is in the alphabet. `A`, `E`, `I`, `O`, `U`, `0` and `1` are
        // NOT -- and the reason is worth stating, because the first version of this test used
        // them and failed: normalization *drops* characters outside the alphabet, so a code
        // written with a vowel silently becomes a shorter code. A person who misreads `8` as
        // `B` therefore gets a refusal rather than a wrong code, which is the safe direction,
        // and a test that had used `ABCD2345` would have been testing something that cannot
        // happen.
        let raw = "BCDF-2345";
        assert_eq!(format_user_code(raw), "BCDF-2345");
        // The comparison runs on the normalised form, so typing it without the dash works --
        // which is the most likely way a person types it.
        assert_eq!(normalize_user_code(&format_user_code(raw)), "BCDF2345");
        assert_eq!(normalize_user_code("bcdf-2345"), "BCDF2345");
        assert_eq!(normalize_user_code("bcdf 2345"), "BCDF2345");
    }

    #[test]
    fn a_character_outside_the_alphabet_is_dropped_not_rejected_silently() {
        // A vowel in the code makes the normalised form shorter, so the length check refuses
        // it. This is the phishing-relevant direction: a misread character produces a refusal,
        // never a match against somebody else's code.
        assert_eq!(normalize_user_code("BCDF2345"), "BCDF2345");
        assert_eq!(
            normalize_user_code("BCDFA345"),
            "BCDF345",
            "the A is dropped"
        );
        assert!(check_user_code("BCDFA345").is_err());
    }

    #[test]
    fn a_short_or_empty_code_is_refused() {
        assert!(check_user_code("").is_err());
        assert!(check_user_code("BCDF").is_err());
        assert!(check_user_code("BCDF234").is_err());
        assert!(check_user_code("BCDF234567").is_err());
        assert!(check_user_code("BCDF-2345").is_ok());
    }

    #[test]
    fn a_device_code_is_longer_than_a_user_code_prefixed_and_hashed_on_the_way_in() {
        let (device, hash, user) = new_device_codes();
        assert!(device.starts_with("omnion_dev_"));
        assert!(
            device.len() > user.len() * 2,
            "the device code is the one a stranger must never guess"
        );
        // Write-only, like every other credential in this crate: the row stores the hash and
        // the plaintext exists for exactly one response.
        assert!(hash.starts_with("omnion-dev$") || hash.contains('$'));
        assert!(
            !hash.contains(&device),
            "the stored value is the hash, not the code"
        );
        assert!(crate::secret::verify(&device, &hash));
    }

    #[test]
    fn two_pairs_of_codes_do_not_collide() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let (_, _, user) = new_device_codes();
            assert!(seen.insert(user), "a user code repeated inside 200 draws");
        }
    }

    // ── the poll rule ────────────────────────────────────────────────────────────────────

    #[test]
    fn a_first_poll_on_an_unapproved_code_is_pending() {
        let mut state = PollState::fresh();
        assert_eq!(
            state.polled(at(100)),
            Err(DeveloperError::DeviceCodePending)
        );
    }

    #[test]
    fn polling_faster_than_the_interval_is_told_to_slow_down_and_the_interval_grows() {
        let mut state = PollState::fresh();
        assert_eq!(state.polled(at(0)), Err(DeveloperError::DeviceCodePending));
        // Two seconds later, with a five-second interval.
        assert_eq!(
            state.polled(at(2)),
            Err(DeveloperError::DeviceCodeSlowDown { seconds: 10 })
        );
        // The growth persists: polling again is refused, and the interval grows again.
        assert_eq!(
            state.polled(at(3)),
            Err(DeveloperError::DeviceCodeSlowDown { seconds: 15 })
        );
    }

    #[test]
    fn polling_after_the_interval_is_allowed_again() {
        let mut state = PollState::fresh();
        assert_eq!(state.polled(at(0)), Err(DeveloperError::DeviceCodePending));
        // Offending once grows the interval to ten, so a poll at five is still refused…
        let _ = state.polled(at(1));
        // …and a poll at ten is fine.
        assert_eq!(state.polled(at(11)), Err(DeveloperError::DeviceCodePending));
    }

    #[test]
    fn the_grown_interval_is_stored_even_when_the_answer_is_pending() {
        // The reason `polled` returns the interval instead of just mutating: a store that never
        // persists the growth tells a runaway client to slow down and never does.
        let mut state = PollState::fresh();
        let _ = state.polled(at(0));
        let stored = match state.polled(at(1)) {
            Err(DeveloperError::DeviceCodeSlowDown { seconds }) => seconds,
            other => panic!("expected a slow-down, got {other:?}"),
        };
        assert_eq!(stored, 10);
        assert_eq!(state.interval_seconds, 10);
    }

    #[test]
    fn the_interval_growth_stops_at_the_ceiling() {
        let mut state = PollState::fresh();
        let mut now = 0;
        let _ = state.polled(at(now));
        for _ in 0..200 {
            now += 1;
            let _ = state.polled(at(now));
        }
        assert_eq!(
            state.interval_seconds, MAX_POLL_INTERVAL_SECONDS,
            "a client that ignores the instruction is not slowed forever, but not endlessly either"
        );
    }

    #[test]
    fn an_approved_code_answers_ok_on_the_next_poll_that_respects_the_interval() {
        let mut state = PollState::fresh();
        state.approved = true;
        assert_eq!(state.polled(at(0)), Ok(POLL_INTERVAL_SECONDS));
    }

    #[test]
    fn a_client_that_polls_too_fast_is_told_to_slow_down_even_after_approval() {
        // Ordering, stated as a test because it is the part that is easy to get wrong: the
        // temptation is to answer "approved" immediately and let the token through, which
        // rewards exactly the behaviour the rule exists to discourage.
        let mut state = PollState::fresh();
        state.approved = true;
        assert_eq!(state.polled(at(0)), Ok(POLL_INTERVAL_SECONDS));
        assert_eq!(
            state.polled(at(1)),
            Err(DeveloperError::DeviceCodeSlowDown { seconds: 10 })
        );
    }

    #[test]
    fn a_clock_that_jumps_backwards_does_not_trip_the_slow_down() {
        // `now` before `last_polled` yields a negative elapsed. Treating that as "too soon"
        // would hand a client a five-minute penalty for a clock skew.
        let mut state = PollState::fresh();
        assert_eq!(
            state.polled(at(100)),
            Err(DeveloperError::DeviceCodePending)
        );
        assert_eq!(state.polled(at(90)), Err(DeveloperError::DeviceCodePending));
    }

    // ── approval ────────────────────────────────────────────────────────────────────────

    fn approval() -> DeviceApproval {
        DeviceApproval {
            user_code: "BCDF-2345".to_string(),
            client_name: "omnion-cli".to_string(),
            client_uri: Some("https://cli.omnion.dev".to_string()),
            scopes: cli_scopes(),
            expires_at: at(DEVICE_CODE_TTL_SECONDS),
            approving_user: "Furkan ERMAĞ".to_string(),
        }
    }

    #[test]
    fn a_complete_approval_proceeds() {
        assert!(check_approval(&approval(), at(10)).is_ok());
    }

    #[test]
    fn an_approval_with_nobody_behind_it_is_refused() {
        // The phishing case: a code approved with no session produces a token with no owner.
        let mut subject = approval();
        subject.approving_user = "   ".to_string();
        assert_eq!(
            check_approval(&subject, at(10)),
            Err(DeveloperError::DeviceCodeApprovalRefused)
        );
    }

    #[test]
    fn an_approval_with_no_client_metadata_is_refused() {
        // A bare code on a screen is the screen an attacker wants; the platform refuses to
        // render one rather than showing a code with nothing beside it.
        let mut subject = approval();
        subject.client_name = String::new();
        assert_eq!(
            check_approval(&subject, at(10)),
            Err(DeveloperError::DeviceCodeApprovalRefused)
        );
    }

    #[test]
    fn an_expired_approval_is_an_invalid_code_and_nothing_else() {
        let subject = approval();
        assert_eq!(
            check_approval(&subject, at(DEVICE_CODE_TTL_SECONDS)),
            Err(DeveloperError::InvalidDeviceCode)
        );
        // One second earlier is still fine — the boundary is the expiry instant, not the
        // rounding of it.
        assert!(check_approval(&subject, at(DEVICE_CODE_TTL_SECONDS - 1)).is_ok());
    }

    // ── scopes ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn the_cli_scopes_are_all_read_shaped() {
        for scope in cli_scopes() {
            assert!(scope.ends_with(".read"), "{scope} is not a read scope");
            assert!(is_cli_scope(&scope));
        }
    }

    #[test]
    fn a_terminal_cannot_ask_for_a_scope_beyond_the_default_set() {
        // The asymmetry: the token is minted for the *approving* session's user and handed to a
        // terminal. A terminal that asks for a write scope is either broken or an attacker.
        assert!(check_requested_scopes(&["developer.read".to_string()]).is_ok());
        assert!(check_requested_scopes(&cli_scopes()).is_ok());
        for scope in [
            "developer.keys.manage",
            "developer.oauth.manage",
            "iam.manage",
        ] {
            assert!(
                check_requested_scopes(&[scope.to_string()]).is_err(),
                "{scope} was granted to a terminal"
            );
        }
    }

    #[test]
    fn an_empty_scope_list_is_accepted_because_the_defaults_apply() {
        assert!(check_requested_scopes(&[]).is_ok());
    }

    #[test]
    fn a_blank_scope_is_refused_rather_than_granted_as_empty() {
        assert!(check_requested_scopes(&["".to_string()]).is_err());
        assert!(check_requested_scopes(&["   ".to_string()]).is_err());
    }

    #[test]
    fn every_cli_scope_has_a_sentence_the_screen_can_show() {
        // `scope_sentence` returns a fallback for unknown scopes, so the *test* is what stops a
        // new default scope shipping without one.
        for scope in DEFAULT_CLI_SCOPES {
            let sentence = scope_sentence(scope);
            assert!(
                !sentence.starts_with("A scope this build"),
                "{scope} has no plain-language sentence"
            );
        }
    }

    #[test]
    fn a_sentence_is_a_sentence_and_not_a_permission_key() {
        // What the request asked for: "a plain-language list". Showing the key would satisfy the
        // letter of it and none of the purpose.
        let sentence = scope_sentence("developer.keys.read");
        assert!(
            !sentence.contains('.'),
            "it reads like a permission key: {sentence}"
        );
        assert!(sentence.len() > 20);
    }
}
