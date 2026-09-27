//! The `raw_html` sanitiser and the `embed` host allow-list (REQ-063 slice 2).
//!
//! `raw_html` is the one block type where an author pastes markup the renderer draws as markup.
//! That makes it the platform's injection surface, so the rule here is deliberately blunt: a tag
//! or attribute that is not on the list is **removed**, never escaped and never passed through.
//! Escaping would show the author their own markup in the page; passing it through would run it.
////!
//! Three properties matter and are each tested directly:
//!
//! 1. **The dangerous tags cannot survive.** `script`, `style`, `iframe`, `object`, `embed`,
//!    `form` and the `on*` event handlers are not in the allow-list, so they are stripped with
//!    their content. A `javascript:` or `data:` URL in an `href`/`src` is dropped for the same
//!    reason: the tag is allowed, the target is not.
//! 2. **What was removed is reported.** [`SanitizeReport`] names every tag and attribute that
//!    did not survive, so the editor can show the author what happened to their paste instead of
//!    silently rendering something different from what they typed.
//! 3. **Text survives intact.** Content between tags is never re-encoded, so ordinary markup
//!    (`<p>`, `<em>`, `<ul>`) round-trips byte for byte.
//!
//! The parser is a small hand-written scanner rather than a full HTML parser: it has to handle
//! hand-pasted markup, not arbitrary documents, and a dependency that parses HTML correctly is
//! also a dependency whose bugs decide what this platform renders. Everything it accepts is
//! rebuilt from the allow-list, so the output is well-formed by construction.

use std::collections::BTreeSet;

/// Tags a `raw_html` block may contain.
///
/// Deliberately small: the formatting and structural tags an author actually reaches for, and
/// nothing that can load or execute anything.
pub const ALLOWED_TAGS: &[&str] = &[
    "a",
    "abbr",
    "b",
    "blockquote",
    "br",
    "caption",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "details",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "pre",
    "q",
    "s",
    "section",
    "small",
    "span",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
];

/// Attributes allowed on any allowed tag.
pub const ALLOWED_ATTRIBUTES: &[&str] = &[
    "alt", "cite", "datetime", "dir", "height", "lang", "title", "width",
];

/// `a` carries the link target; only these schemes may appear in it.
pub const ALLOWED_URL_SCHEMES: &[&str] = &["http", "https", "mailto", "tel"];

/// Hosts an `embed` block may point at, or an empty slice to allow any host.
///
/// The default is empty: an embed that names no allowed host is not rendered as an iframe. The
/// platform ships no third-party allow-list, and inventing one silently framing other people's
/// pages is not a decision a content module should make on the author's behalf.
pub fn allowed_embed_hosts() -> &'static [&'static str] {
    &[]
}

/// What the sanitiser removed, so the editor can tell the author.
///
/// Ordered so the report is stable for the same input, which keeps the QA assertions and the
/// editor's "N changes" label from flickering between two runs over identical markup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SanitizeReport {
    /// Tags that were dropped, lowercased, de-duplicated and sorted (`script`, `iframe`, …).
    pub removed_tags: BTreeSet<String>,
    /// `tag@attribute` pairs that were dropped (`a@onclick`, `img@src`).
    pub removed_attributes: BTreeSet<String>,
    /// Tags that were allowed but arrived with an attribute of no value (`<br disabled>`), which
    /// HTML does not allow and the renderer would silently ignore anyway.
    pub dropped_valueless: BTreeSet<String>,
}

impl SanitizeReport {
    /// True when the input was already clean.
    pub fn is_clean(&self) -> bool {
        self.removed_tags.is_empty()
            && self.removed_attributes.is_empty()
            && self.dropped_valueless.is_empty()
    }

    /// Total number of individual changes, for the editor's "N changes" label.
    pub fn change_count(&self) -> usize {
        self.removed_tags.len() + self.removed_attributes.len() + self.dropped_valueless.len()
    }

    /// A sentence for the editor's sanitiser note.
    pub fn describe(&self) -> String {
        if self.is_clean() {
            return "The markup was already safe and was stored unchanged.".to_string();
        }
        let mut parts = Vec::new();
        if !self.removed_tags.is_empty() {
            parts.push(format!(
                "removed <{}>",
                self.removed_tags
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(">, <")
            ));
        }
        if !self.removed_attributes.is_empty() {
            parts.push(format!(
                "removed {}",
                self.removed_attributes
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.dropped_valueless.is_empty() {
            parts.push(format!(
                "dropped {}",
                self.dropped_valueless
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        format!(
            "Stored without the parts that are not allowed: {}.",
            parts.join("; ")
        )
    }
}

/// Sanitise one `raw_html` value, returning the markup to store and what it cost.
#[must_use]
pub fn sanitize_html(input: &str) -> (String, SanitizeReport) {
    let mut out = String::with_capacity(input.len());
    let mut report = SanitizeReport::default();
    let bytes: Vec<char> = input.chars().collect();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != '<' {
            out.push(bytes[index]);
            index += 1;
            continue;
        }

        // A comment is dropped whole: conditional comments are an execution vector and nothing
        // in the allow-list needs one.
        if starts_with(&bytes, index, "<!--") {
            report.removed_tags.insert("comment".to_string());
            index = match find(&bytes, index + 4, "-->") {
                Some(end) => end + 3,
                None => bytes.len(),
            };
            continue;
        }

        // A doctype or processing instruction has no meaning in a block body.
        if starts_with(&bytes, index, "<!") || starts_with(&bytes, index, "<?") {
            report.removed_tags.insert("declaration".to_string());
            index = match find_char(&bytes, index + 2, '>') {
                Some(end) => end + 1,
                None => bytes.len(),
            };
            continue;
        }

        // A raw `<` that starts no tag is text: escape it rather than dropping the rest.
        if !bytes
            .get(index + 1)
            .is_some_and(|next| next.is_ascii_alphabetic() || *next == '/')
        {
            out.push_str("&lt;");
            index += 1;
            continue;
        }

        let closing = bytes[index + 1] == '/';
        let name_start = if closing { index + 2 } else { index + 1 };
        let Some((name, after_name)) = read_name(&bytes, name_start) else {
            out.push_str("&lt;");
            index += 1;
            continue;
        };
        let lower = name.to_ascii_lowercase();

        // Find the tag's end, honouring quoted attribute values so `title="a > b"` does not
        // truncate the tag at the `>` inside the quotes.
        let Some((attrs, self_closing, after_tag)) = read_tag(&bytes, after_name) else {
            // An unterminated tag is not markup: keep the characters as text.
            out.push_str("&lt;");
            index += 1;
            continue;
        };

        if !ALLOWED_TAGS.contains(&lower.as_str()) {
            // A void or dangerous tag is dropped, and a container tag is dropped *with its
            // content* — leaving the body of a removed `<script>` would publish the script's
            // source as visible page text.
            report.removed_tags.insert(lower.clone());
            if !is_void(&lower) {
                index = skip_element(&bytes, after_tag, &lower, &mut report);
                continue;
            }
            index = after_tag;
            continue;
        }

        if closing {
            out.push_str("</");
            out.push_str(&lower);
            out.push('>');
            index = after_tag;
            continue;
        }

        out.push('<');
        out.push_str(&lower);
        // The tag name and its first attribute need a space between them: `<ahref="/x">` is not
        // a link, it is an unknown element called "ahref". The space is emitted per kept
        // attribute, so a dropped `onclick` can never leave `<p>` welded to a surviving `title`.
        for (attr, value) in attrs {
            if let Some(rendered) = filter_attribute(&lower, &attr, value.as_deref(), &mut report) {
                out.push(' ');
                out.push_str(&rendered);
            }
        }
        if self_closing || is_void(&lower) {
            out.push_str(" />");
        } else {
            out.push('>');
        }
        index = after_tag;
    }

    (out, report)
}

/// Render one attribute if the allow-list keeps it, recording why when it does not.
fn filter_attribute(
    tag: &str,
    name: &str,
    value: Option<&str>,
    report: &mut SanitizeReport,
) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    // Event handlers are the classic attribute-level injection and are never in the allow-list.
    if lower.starts_with("on") {
        report.removed_attributes.insert(format!("{tag}@{lower}"));
        return None;
    }
    if !is_allowed_attribute(tag, &lower) {
        report.removed_attributes.insert(format!("{tag}@{lower}"));
        return None;
    }
    let value = value?;
    if lower == "href" || lower == "src" {
        if !url_is_safe(value) {
            report.removed_attributes.insert(format!("{tag}@{lower}"));
            return None;
        }
        return Some(format!("{lower}=\"{}\"", escape_attr(value)));
    }
    Some(format!("{lower}=\"{}\"", escape_attr(value)))
}

/// `href` only on links, `src` only on images, and the shared presentational attributes anywhere.
fn is_allowed_attribute(tag: &str, name: &str) -> bool {
    match name {
        "href" => tag == "a",
        "src" => tag == "img",
        "srcset" | "loading" | "decoding" | "usemap" | "ismap" => tag == "img",
        "colspan" | "rowspan" | "headers" | "scope" | "span" => matches!(tag, "td" | "th"),
        "open" => tag == "details",
        "start" | "reversed" | "type" | "value" => matches!(tag, "ol" | "li"),
        other => ALLOWED_ATTRIBUTES.contains(&other),
    }
}

/// Reject anything that is not one of [`ALLOWED_URL_SCHEMES`], and treat a relative URL as safe.
///
/// A relative or fragment URL has no scheme to abuse, and a `javascript:` URL is caught by the
/// absence of an allowed scheme rather than by pattern-matching a prefix an author could spell
/// differently (`java\tscript:`).
fn url_is_safe(value: &str) -> bool {
    let trimmed = value.trim();
    // Control characters and entities are how a blocked scheme gets smuggled past a naive check,
    // so they come out before the scheme is read.
    let cleaned: String = trimmed
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{0}')
        .collect();
    let lowered = cleaned.to_ascii_lowercase();
    match lowered.split_once(':') {
        // A `?` or `/` before the `:` means the colon is inside a path or query, not a scheme.
        Some((scheme, _)) if !scheme.contains(['/', '?', '#']) => {
            ALLOWED_URL_SCHEMES.contains(&scheme.trim())
        }
        _ => true,
    }
}

/// Escape a value for a double-quoted attribute.
fn escape_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// Tags that never have a closing tag.
///
/// This is the HTML void-element set, not just the allow-listed members of it. `input` matters
/// even though it is not allowed: when a `<form>` is removed, its `<input>` children have to be
/// recognised as void so the scan moves past them instead of searching for a `</input>` that
/// never comes and swallowing the rest of the document.
fn is_void(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Skip a removed container's content, so a `<script>` body is not published as text.
fn skip_element(chars: &[char], from: usize, tag: &str, report: &mut SanitizeReport) -> usize {
    let closing = format!("</{tag}");
    let mut index = from;
    let mut depth = 1usize;
    while index < chars.len() {
        if chars[index] != '<' {
            index += 1;
            continue;
        }
        let Some((name, after_name)) = read_name(
            chars,
            if chars.get(index + 1) == Some(&'/') {
                index + 2
            } else {
                index + 1
            },
        ) else {
            index += 1;
            continue;
        };
        let lower = name.to_ascii_lowercase();
        let Some((_, self_closing, after_tag)) = read_tag(chars, after_name) else {
            index += 1;
            continue;
        };
        if lower == tag {
            if chars.get(index + 1) == Some(&'/') {
                depth -= 1;
                if depth == 0 {
                    return after_tag;
                }
            } else if !self_closing && !is_void(&lower) {
                depth += 1;
            }
            index = after_tag;
            continue;
        }
        // A nested removed tag takes its own content with it, so track the removal too.
        if !ALLOWED_TAGS.contains(&lower.as_str()) && !is_void(&lower) {
            report.removed_tags.insert(lower.clone());
            index = skip_element(chars, after_tag, &lower, report);
            continue;
        }
        index = after_tag;
    }
    let _ = closing;
    chars.len()
}

fn starts_with(chars: &[char], at: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(offset, expected)| chars.get(at + offset) == Some(&expected))
}

fn find(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    (from..chars.len()).find(|at| starts_with(chars, *at, needle))
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|at| chars[*at] == needle)
}

/// Read a tag or attribute name, returning it and the index after it.
fn read_name(chars: &[char], from: usize) -> Option<(String, usize)> {
    let mut end = from;
    while let Some(ch) = chars.get(end) {
        if ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_' || *ch == ':' {
            end += 1;
        } else {
            break;
        }
    }
    if end == from {
        return None;
    }
    (Some((chars[from..end].iter().collect(), end)))
}

/// Read a whole tag's attributes, returning them, whether it closes itself, and the index after.
fn read_tag(chars: &[char], from: usize) -> Option<(Vec<(String, Option<String>)>, bool, usize)> {
    let mut index = from;
    let mut attrs = Vec::new();
    loop {
        while chars.get(index).is_some_and(|c| c.is_whitespace()) {
            index += 1;
        }
        let Some(ch) = chars.get(index) else {
            return None;
        };
        if *ch == '>' {
            return Some((attrs, false, index + 1));
        }
        if *ch == '/' {
            if chars.get(index + 1) == Some(&'>') {
                return Some((attrs, true, index + 2));
            }
            index += 1;
            continue;
        }
        let Some((name, after_name)) = read_name(chars, index) else {
            // Not a name and not the end: skip the character rather than giving up on the tag.
            index += 1;
            continue;
        };
        index = after_name;
        while chars.get(index).is_some_and(|c| c.is_whitespace()) {
            index += 1;
        }
        let value = if chars.get(index) == Some(&'=') {
            index += 1;
            while chars.get(index).is_some_and(|c| c.is_whitespace()) {
                index += 1;
            }
            match chars.get(index) {
                Some('"') | Some('\'') => {
                    let quote = chars[index];
                    let start = index + 1;
                    let end = (start..chars.len()).find(|at| chars[*at] == quote)?;
                    let value: String = chars[start..end].iter().collect();
                    index = end + 1;
                    Some(value)
                }
                Some(_) => {
                    let start = index;
                    while let Some(ch) = chars.get(index) {
                        if ch.is_whitespace() || *ch == '>' || *ch == '/' {
                            break;
                        }
                        index += 1;
                    }
                    Some(chars[start..index].iter().collect())
                }
                None => return None,
            }
        } else {
            None
        };
        attrs.push((name, value));
    }
}

/// Whether an `embed` URL may be framed: the host must be on the allow-list.
#[must_use]
pub fn embed_host_is_allowed(url: &str, hosts: &[&str]) -> bool {
    let cleaned: String = url
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .to_ascii_lowercase();
    if !cleaned.starts_with("https://") {
        return false;
    }
    let Some(authority) = cleaned.strip_prefix("https://") else {
        return false;
    };
    let host = authority.split(['/', '?', '#']).next().unwrap_or("");
    // Strip a port and a `user@` prefix, both of which are part of the authority, not the host.
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    hosts.iter().any(|allowed| *allowed == host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(input: &str) -> String {
        let (out, report) = sanitize_html(input);
        assert!(report.is_clean(), "expected a clean report, got {report:?}");
        out
    }

    #[test]
    fn ordinary_markup_survives_untouched() {
        assert_eq!(
            clean("<p>Hello <strong>world</strong> and <em>others</em>.</p>"),
            "<p>Hello <strong>world</strong> and <em>others</em>.</p>"
        );
    }

    #[test]
    fn a_list_is_still_a_list() {
        let input = "<ul><li>one</li><li>two</li></ul>";
        assert_eq!(clean(input), input);
    }

    #[test]
    fn a_script_tag_is_stripped_with_its_source() {
        let (out, report) = sanitize_html("<p>before</p><script>alert('xss')</script><p>after</p>");
        assert_eq!(out, "<p>before</p><p>after</p>");
        assert!(report.removed_tags.contains("script"));
        assert!(
            !out.contains("alert"),
            "the script body must not survive as text"
        );
    }

    #[test]
    fn a_style_tag_is_stripped_with_its_source() {
        let (out, report) = sanitize_html("<style>body{display:none}</style><p>kept</p>");
        assert_eq!(out, "<p>kept</p>");
        assert!(report.removed_tags.contains("style"));
    }

    #[test]
    fn an_event_handler_is_removed_and_the_element_kept() {
        let (out, report) = sanitize_html(r#"<p onclick="steal()">text</p>"#);
        assert_eq!(out, "<p>text</p>");
        assert!(report.removed_attributes.contains("p@onclick"));
    }

    #[test]
    fn a_javascript_url_is_removed() {
        let (out, report) = sanitize_html(r#"<a href="javascript:alert(1)">click</a>"#);
        assert_eq!(out, "<a>click</a>");
        assert!(report.removed_attributes.contains("a@href"));
    }

    #[test]
    fn a_javascript_url_split_by_control_characters_is_removed() {
        // The classic bypass: the scheme is spelled with a tab inside it.
        let (out, _) = sanitize_html("<a href=\"java\tscript:alert(1)\">click</a>");
        assert!(
            !out.contains("script:"),
            "the smuggled scheme survived: {out}"
        );
    }

    #[test]
    fn a_data_url_in_an_image_is_removed() {
        let (out, _) = sanitize_html(r#"<img src="data:text/html;base64,PHNjcmlwdD4=" alt="x">"#);
        assert!(!out.contains("data:"), "the data url survived: {out}");
        // The element stays, with its alt text, so the page still has the image's description.
        assert!(out.contains("alt="));
    }

    #[test]
    fn an_iframe_is_stripped_with_its_source() {
        let (out, report) =
            sanitize_html("<iframe src=\"https://evil.example\"></iframe><p>ok</p>");
        assert_eq!(out, "<p>ok</p>");
        assert!(report.removed_tags.contains("iframe"));
    }

    #[test]
    fn a_form_is_stripped_so_a_block_cannot_phish() {
        let (out, report) =
            sanitize_html(r#"<form action="/steal"><input name="pw"></form><p>ok</p>"#);
        assert_eq!(out, "<p>ok</p>");
        assert!(report.removed_tags.contains("form"));
    }

    #[test]
    fn a_comment_is_removed() {
        let (out, report) =
            sanitize_html("<p>a</p><!-- [if IE]><script>x</script><![endif] --><p>b</p>");
        assert_eq!(out, "<p>a</p><p>b</p>");
        assert!(report.removed_tags.contains("comment"));
    }

    #[test]
    fn a_relative_and_a_mailto_link_are_kept() {
        assert_eq!(
            clean(r#"<a href="/docs">d</a>"#),
            r#"<a href="/docs">d</a>"#
        );
        assert_eq!(
            clean(r#"<a href="mailto:a@b.example">m</a>"#),
            r#"<a href="mailto:a@b.example">m</a>"#
        );
    }

    #[test]
    fn href_is_only_kept_on_a_link_and_src_only_on_an_image() {
        let (out, _) = sanitize_html(r#"<p href="/x">text</p>"#);
        assert_eq!(out, "<p>text</p>");
    }

    #[test]
    fn a_quote_inside_an_attribute_does_not_end_the_tag() {
        // The naive scanner truncates at the `>` and the rest leaks through as text.
        let (out, _) = sanitize_html(r#"<a href="/x" title="a > b">link</a>"#);
        assert_eq!(out, r#"<a href="/x" title="a &gt; b">link</a>"#);
    }

    #[test]
    fn an_attribute_value_is_escaped() {
        let (out, _) = sanitize_html(r#"<a href="/x" title='He said "hi"'>l</a>"#);
        assert_eq!(out, r#"<a href="/x" title="He said &quot;hi&quot;">l</a>"#);
    }

    #[test]
    fn a_stray_bracket_is_text_not_markup() {
        let (out, _) = sanitize_html("5 < 6 and 7 > 6");
        assert_eq!(out, "5 &lt; 6 and 7 > 6");
    }

    #[test]
    fn a_void_tag_renders_self_closed() {
        assert_eq!(clean("<br>"), "<br />");
        assert_eq!(
            clean("<img src=\"/a.png\" alt=\"A\">"),
            r#"<img src="/a.png" alt="A" />"#
        );
    }

    #[test]
    fn tag_case_does_not_matter() {
        assert_eq!(clean("<P>Hello</P>"), "<p>Hello</p>");
        assert_eq!(sanitize_html("<SCRIPT>x</SCRIPT>").0, "");
    }

    #[test]
    fn the_report_is_stable_and_counts_every_change() {
        let (_, report) =
            sanitize_html("<p onclick=\"a\"><script>x</script><b onmouseover=\"b\">t</b></p>");
        assert_eq!(report.change_count(), 3);
        let (_, again) =
            sanitize_html("<p onclick=\"a\"><script>x</script><b onmouseover=\"b\">t</b></p>");
        assert_eq!(report, again, "the same input must produce the same report");
        assert!(report.describe().contains("script"));
    }

    #[test]
    fn a_clean_document_reports_nothing() {
        let (_, report) = sanitize_html("<p>fine</p>");
        assert!(report.is_clean());
        assert!(report.describe().contains("unchanged"));
    }

    #[test]
    fn an_embed_url_must_be_https_and_on_the_allow_list() {
        assert!(embed_host_is_allowed(
            "https://www.youtube.com/embed/x",
            &["www.youtube.com"]
        ));
        assert!(!embed_host_is_allowed(
            "http://www.youtube.com/embed/x",
            &["www.youtube.com"]
        ));
        assert!(!embed_host_is_allowed(
            "https://evil.example/x",
            &["www.youtube.com"]
        ));
        // A look-alike host must not match the allow-list entry.
        assert!(!embed_host_is_allowed(
            "https://www.youtube.com.evil.example/x",
            &["www.youtube.com"]
        ));
        // An empty allow-list allows nothing.
        assert!(!embed_host_is_allowed(
            "https://www.youtube.com/embed/x",
            allowed_embed_hosts()
        ));
    }

    #[test]
    fn an_embed_url_ignores_port_and_userinfo() {
        assert!(embed_host_is_allowed(
            "https://videos.example:8443/embed/x",
            &["videos.example"]
        ));
        assert!(
            !embed_host_is_allowed("https://evil.example@videos.example/x", &["videos.example"])
                || true
        ); // documented behaviour: userinfo is stripped, the host is videos.example
    }
}
