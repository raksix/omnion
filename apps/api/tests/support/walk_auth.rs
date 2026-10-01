//! The sign-in half of an in-process walk, shared by every suite under `tests/`.
//!
//! # Why this module exists
//!
//! Tick 59 made the session cookie *ambient* authority: anything a browser sends along on its
//! own. A cookie-authenticated **write** therefore has to present a CSRF token as well, and
//! sign-in is the only place that token is issued. Sign-in answers with **two** `Set-Cookie`
//! headers — the session and the token beside it.
//!
//! Twenty suites each carried their own `login()` helper, and twenty helpers took
//! `.split(';').next()` on the *first* `Set-Cookie` only. That expression is correct for one
//! cookie and silently discards every cookie after it, so those suites signed in holding a
//! session with no token. Every write in them was then refused with `csrf_unavailable` — a code
//! whose message names **the server's configuration** (a deployment without
//! `OMNION_CSRF_SECRET`), not the suite's own loss of the token. The failure read as a broken
//! deployment instead of a broken helper, which is why it survived as long as it did: a red
//! suite that blames the product is the most expensive kind of red.
//!
//! Twenty copies of the same helper are twenty places for the next security change to miss, so
//! the correct shape — which `--test media` already had and passes — is lifted here once.
//!
//! # The two rules a caller must not get wrong
//!
//! 1. **Keep every `Set-Cookie`.** [`Session`] is built from all of them, and the CSRF cookie is
//!    asserted to be present so a deployment that stops issuing one fails *at sign-in*, where the
//!    message can name the cause, instead of at the first write, where it cannot.
//! 2. **Send the token in the header too.** The cookie is a decoy the browser replays; the
//!    double-submit pattern requires the value echoed in [`CSRF_HEADER`]. [`Session::apply`] is
//!    the one place that does both, and a suite that hand-rolls its request builder instead
//!    re-opens the defect this module closed.

use axum::http::header;
use serde_json::{Value, json};
use std::sync::OnceLock;

/// Password the accounts these walks create sign in with.
///
/// A test-only value with a test-only name. Naming it here is what keeps the distinction
/// legible: a real credential in a public repository is a different thing entirely.
pub const PASSWORD: &str = "correct horse battery";

/// The header a cookie-authenticated write carries its CSRF token in.
pub const CSRF_HEADER: &str = "x-omnion-csrf";

/// The session cookie's name.
pub const SESSION_COOKIE: &str = "omnion_session";

/// The CSRF cookie's name — the one twenty helpers were dropping.
pub const CSRF_COOKIE: &str = "omnion_csrf";

/// Separator between the session id and the CSRF token inside a [`Session`].
///
/// A unit separator (`0x1f`) rather than a printable character: a session id is a uuid and a
/// token is base64url, so neither can contain it, and no value has to be escaped to be packed.
const SEPARATOR: char = '\u{1f}';

/// A signed-in account: the session cookie and the CSRF token issued beside it.
#[derive(Debug, Clone)]
pub struct Session {
    /// The session cookie's value.
    pub session: String,
    /// The CSRF token issued at sign-in, when the deployment issued one.
    pub csrf: Option<String>,
}

impl Session {
    /// Read both credentials out of every `Set-Cookie` on a sign-in response.
    ///
    /// Panics — with the joined header in the message — when the session cookie is missing. A
    /// sign-in that set no session is a route that changed shape, and a caller that cannot see
    /// the header will read the downstream `unauthenticated` as an authorization decision.
    pub fn from_set_cookies<S: AsRef<str>>(set_cookies: impl IntoIterator<Item = S>) -> Self {
        let header = set_cookies
            .into_iter()
            .map(|cookie| cookie.as_ref().to_owned())
            .collect::<Vec<_>>()
            .join("; ");

        let find = |name: &str| {
            header
                .split(';')
                .map(str::trim)
                .find_map(|cookie| cookie.strip_prefix(&format!("{name}=")))
                .map(str::to_owned)
        };

        let session = find(SESSION_COOKIE)
            .unwrap_or_else(|| panic!("sign-in must set the {SESSION_COOKIE} cookie: {header}"));
        // Asserted rather than tolerated. A deployment that issues no CSRF cookie refuses every
        // cookie-authenticated write with `csrf_unavailable`, and if the suite keeps going it
        // spends its next twenty assertions rediscovering that one fact. Failing here names it.
        assert!(
            header.contains(&format!("{CSRF_COOKIE}=")),
            "sign-in must issue a {CSRF_COOKIE} cookie, or every cookie-authenticated write is \
             refused: {header}"
        );

        Self {
            session,
            csrf: find(CSRF_COOKIE),
        }
    }

    /// The `Cookie` header a browser would send, carrying both cookies.
    pub fn cookie_header(&self) -> String {
        match &self.csrf {
            Some(token) => format!("{SESSION_COOKIE}={}; {CSRF_COOKIE}={token}", self.session),
            None => format!("{SESSION_COOKIE}={}", self.session),
        }
    }

    /// Apply both credentials to a request under construction.
    ///
    /// The CSRF token goes in the header **and** the cookie: the header is what the double-submit
    /// check reads, and the cookie is what the check compares it against. Sending only the
    /// cookie is precisely the ambient-authority case tick 59 closed, and it is refused.
    pub fn apply(&self, builder: axum::http::request::Builder) -> axum::http::request::Builder {
        let builder = builder.header(header::COOKIE, self.cookie_header());
        match &self.csrf {
            Some(token) => builder.header(CSRF_HEADER, token.as_str()),
            None => builder,
        }
    }

    /// The JSON body a sign-in call posts.
    pub fn login_body(email: &str) -> Value {
        json!({ "email": email, "password": PASSWORD })
    }
}

// ---------------------------------------------------------------------------------------------
// Packed credentials
// ---------------------------------------------------------------------------------------------

/// Pack a session and its token into the one string a walk passes around.
///
/// The separator is [`SEPARATOR`]. A walk's `request()` signature takes a single `token`
/// argument, and twenty suites share that shape; giving them a *pair* would mean twenty
/// signature changes for one security fix. Packing it keeps the call sites untouched — which is
/// what makes this migration to the shared helper mechanical instead of a rewrite.
impl Session {
    /// Pack into the form [`pack`] reads.
    pub fn pack(&self) -> String {
        format!(
            "{}{SEPARATOR}{}",
            self.session,
            self.csrf.as_deref().unwrap_or_default()
        )
    }
}

/// The packed form [`Session::pack`] writes: the whole cookie header plus the token.
pub fn pack(session: &Session) -> String {
    session.pack()
}

/// Unpack a credential into the session id and the CSRF token, when it carries one.
///
/// A **bare** session id — what a caller with no token passes, and what every pre-tick-59 suite
/// already held — is accepted as a session with no token. That is not leniency for its own sake:
/// it is what lets a suite that only ever reads keep working unchanged, and a read is refused by
/// permission, not by the double-submit check, so there is no token to present.
pub fn unpack(credential: &str) -> Session {
    match credential.split_once(SEPARATOR) {
        Some((session, csrf)) => Session {
            session: session.to_owned(),
            csrf: (!csrf.is_empty()).then(|| csrf.to_owned()),
        },
        None => Session {
            session: credential.to_owned(),
            csrf: None,
        },
    }
}

/// Attach a packed credential to a request under construction.
///
/// The one line a suite needs after deleting its own cookie handling: the session and the token
/// travel together, and neither can be forgotten because there is nothing to forget — the
/// credential is unpacked and both halves are set here.
pub fn apply_credential(
    credential: &str,
    builder: axum::http::request::Builder,
) -> axum::http::request::Builder {
    unpack(credential).apply(builder)
}

// ---------------------------------------------------------------------------------------------
// Fixture configuration
// ---------------------------------------------------------------------------------------------

/// The CSRF secret a walk's own state signs its tokens with.
///
/// A test-only value with a test-only name: it is the fixture's *own* secret, and nothing the
/// suite stores is protected by anything but the walls of the process. Naming it here is what
/// keeps the distinction legible — a real secret would be a credential in a public repository.
pub const CSRF_SECRET: &str = "csrf-walk-suite-key-material-not-a-real-secret";

/// Give a walk's state a CSRF secret, so sign-in issues a token at all.
///
/// This is the *other* half of the failure, and it is invisible until the first is fixed. With no
/// secret configured the deployment is doing exactly what it is told — refusing
/// cookie-authenticated writes, because the double-submit check has nothing to compare against —
/// and the code that comes back, `csrf_unavailable`, is correct. What makes it a defect is that
/// twenty suites constructed a `Config` that never set this, so their own sign-in never issued a
/// token and every write they attempted was guaranteed to fail.
///
/// The distinction worth keeping: a **refusal** here is the product working. The bug is in the
/// suites, which were asserting a deployment that cannot exist and calling its answer a defect.
pub fn with_csrf_secret(config: &mut omnion_core::config::Config) {
    use omnion_core::config::CsrfSecret;
    config.csrf = CsrfSecret::new(Some(CSRF_SECRET.to_owned()));
}

/// Give this process a sign-in budget large enough for the walks in it.
///
/// The limiter is a **process-wide** cell that `router()` fills from the *stored* document, and
/// the stored `sign_in` scope is ten requests per five minutes. A suite that signs in three
/// accounts per walk and runs fifteen walks is refused at the eleventh sign-in, and every walk
/// after it dies on a line that has nothing to do with what it was testing — a failure that
/// names a rate limit on a suite that was never testing rate limits.
///
/// Raising the ceiling here does not weaken the limiter suite: that suite installs and asserts
/// its own numbers, and whichever fixture installs first wins the cell — so a suite that needs
/// the shipped policy is asserting the policy rather than sharing a budget with other tests.
/// Only `sign_in` is raised; the other ceilings stay as a deployment ships them, so no suite
/// here can be the reason a genuinely over-budget request stops being refused.
///
/// `OnceLock` because the limiter is global: installing per walk would be a race, not a reset,
/// and the cell has no reset of its own.
///
/// # Not a claim about a defect
///
/// Both suites this tick needed (`media_shares`, `media_retention`) ran green with the *shipped*
/// `sign_in` budget, so nothing in this tick proves the cell is a problem in them. It is here
/// for the suites that do sign in many times, and it is the shape `--test media` already uses —
/// lifted, not invented.
static RATE_BUDGET: OnceLock<()> = OnceLock::new();

/// Give this process a sign-in budget large enough for the walks in it.
pub fn give_the_process_its_own_sign_in_budget<F>(install: F)
where
    F: FnOnce() + Sync,
{
    let _ = RATE_BUDGET.get_or_init(|| install());
}
