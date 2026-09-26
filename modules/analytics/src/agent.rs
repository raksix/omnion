//! Reading a user agent: was it a person, and on what?
//!
//! Deliberately a *heuristic*, not a device database: the platform stores the four coarse
//! answers a report can act on (device class, operating system, browser, bot) and nothing that
//! would turn an analytics row into a fingerprint. Unknown agents answer `other` rather than
//! guessing, and an agent shorter than a real one is treated as a bot — the honest reading of
//! "something that is not a browser spoke to us".

/// Substrings that mark an automated agent. Extend deliberately: every entry here is traffic a
/// site stops counting, so a false positive costs a real pageview.
const BOT_MARKERS: &[&str] = &[
    "bot",
    "crawler",
    "spider",
    "crawl",
    "slurp",
    "facebookexternalhit",
    "whatsapp",
    "telegrambot",
    "slackbot",
    "discordbot",
    "embedly",
    "link preview",
    "pinterest",
    "bitlybot",
    "skypeuripreview",
    "nuzzel",
    "vkshare",
    "curl/",
    "wget",
    "python-requests",
    "python-urllib",
    "httpx/",
    "go-http-client",
    "okhttp",
    "java/",
    "libwww",
    "headlesschrome",
    "phantomjs",
    "lighthouse",
    "pingdom",
    "uptimerobot",
    "statuscake",
    "datadog",
    "semrush",
    "ahrefs",
    "mj12bot",
    "dotbot",
    "petalbot",
    "bytespider",
    "yandex",
    "baiduspider",
    "sogou",
    "exabot",
    "archive.org_bot",
];

/// `true` when the agent is not a person's browser.
///
/// An empty or implausibly short agent answers `true`: no browser sends one, and counting it
/// would mean counting scrapers with a one-line patch.
#[must_use]
pub fn is_bot(user_agent: &str) -> bool {
    let agent = user_agent.trim();
    if agent.len() < 12 {
        return true;
    }

    let lower = agent.to_ascii_lowercase();
    BOT_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Coarse device class stored on the visit.
///
/// `mobile` keeps its old meaning on purpose: a phone that asks for the desktop site is still a
/// phone, and `Mobi` in the agent is how it says so.
#[must_use]
pub fn device_type(user_agent: &str) -> &'static str {
    let agent = user_agent.to_ascii_lowercase();
    if agent.is_empty() {
        return "other";
    }
    if agent.contains("ipad") || agent.contains("tablet") || agent.contains("kindle") {
        return "tablet";
    }
    if agent.contains("android") && !agent.contains("mobile") {
        return "tablet";
    }
    if agent.contains("mobi") || agent.contains("iphone") || agent.contains("ipod") {
        return "mobile";
    }
    "desktop"
}

/// Operating system family, as coarse as the device class.
#[must_use]
pub fn os(user_agent: &str) -> &'static str {
    let agent = user_agent.to_ascii_lowercase();
    if agent.contains("windows") {
        return "windows";
    }
    if agent.contains("android") {
        return "android";
    }
    if agent.contains("iphone") || agent.contains("ipad") || agent.contains("ipod") {
        return "ios";
    }
    if agent.contains("mac os") || agent.contains("macintosh") {
        return "macos";
    }
    if agent.contains("linux") || agent.contains("x11") {
        return "linux";
    }
    "other"
}

/// Browser family; order matters, because most engines claim to be the others.
#[must_use]
pub fn browser(user_agent: &str) -> &'static str {
    let agent = user_agent.to_ascii_lowercase();
    if agent.contains("edg/") || agent.contains("edge/") || agent.contains("edgios") {
        return "edge";
    }
    if agent.contains("opr/") || agent.contains("opera") {
        return "opera";
    }
    if agent.contains("firefox") || agent.contains("fxios") {
        return "firefox";
    }
    if agent.contains("chrome") || agent.contains("crios") {
        return "chrome";
    }
    if agent.contains("safari") {
        return "safari";
    }
    "other"
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
    const IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
                          AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 \
                          Safari/604.1";

    #[test]
    fn people_are_not_bots_and_crawlers_are() {
        assert!(!is_bot(CHROME));
        assert!(!is_bot(IPHONE));
        assert!(is_bot("Googlebot/2.1 (+http://www.google.com/bot.html)"));
        assert!(is_bot("curl/8.5.0"));
        assert!(is_bot("python-requests/2.31.0"));
        assert!(is_bot("HeadlessChrome/126.0.0.0"));
        assert!(is_bot(""), "an empty agent is not a person");
        assert!(is_bot("x"), "a one-letter agent is not a person");
    }

    #[test]
    fn devices_are_read_from_the_agents_people_actually_send() {
        assert_eq!(device_type(CHROME), "desktop");
        assert_eq!(device_type(IPHONE), "mobile");
        assert_eq!(
            device_type(
                "Mozilla/5.0 (Linux; Android 13; SM-X200) AppleWebKit/537.36 Safari/537.36"
            ),
            "tablet",
            "an Android without `Mobi` is a tablet"
        );
        assert_eq!(device_type(""), "other");
    }

    #[test]
    fn systems_and_browsers_keep_their_own_names() {
        assert_eq!(os(CHROME), "windows");
        assert_eq!(os(IPHONE), "ios");
        assert_eq!(browser(CHROME), "chrome");
        assert_eq!(browser(IPHONE), "safari");
        assert_eq!(
            browser(
                "Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 Chrome/126.0 Safari/537.36 Edg/126.0"
            ),
            "edge",
            "Edge claims to be Chrome and is checked first"
        );
        assert_eq!(browser("Mozilla/5.0 Firefox/127.0"), "firefox");
        assert_eq!(browser("Some agent nobody has heard of"), "other");
    }
}
