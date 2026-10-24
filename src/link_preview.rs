//! Link previews the reader's own machine fetches.
//!
//! WhatsApp attaches preview metadata to a message when the *sender's* device
//! reads the page, and ZapFast has always drawn that. A link sent from here has
//! no such metadata, so no client shows a card for it — the sender is the one
//! who would have to fetch the page.
//!
//! This module does that fetch: it reads a page's OpenGraph or Twitter card
//! tags (or its document title), builds a small JPEG thumbnail, and hands the
//! result to the sender so an outgoing message carries the same
//! `extendedTextMessage` metadata a phone would have sent.
//!
//! Two rules keep it from becoming a liability:
//!
//! * A message is only fetched for the link the reader is about to send, from
//!   the composer, never for links other people sent.
//! * Every hop is checked against private, loopback, link-local and reserved
//!   addresses before it is dialled, and the resolved address is pinned for the
//!   request, so a link cannot make this machine probe its own network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// Longest page body read for its metadata: a preview lives in the first
/// kilobytes of `<head>`.
const MAX_HTML_BYTES: usize = 512 * 1024;
/// Longest image read for the thumbnail. Bigger pictures are skipped.
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// A page that takes longer than this shows no preview.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Redirects followed, each one checked again before it is dialled.
const MAX_REDIRECTS: usize = 5;
/// The JPEG sent in `jpegThumbnail` must stay small: it travels inside the
/// stanza. WhatsApp's own thumbnails sit in this range.
const MAX_THUMBNAIL_BYTES: usize = 48 * 1024;
/// Longest title and description kept. The card is two lines tall.
const MAX_TITLE: usize = 200;
const MAX_DESCRIPTION: usize = 400;
/// Longest edge of the generated thumbnail.
const THUMBNAIL_EDGE: u32 = 192;
/// Not the updater's agent: a page can tell who asked.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// Metadata for one link, ready to ride on an outgoing message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preview {
    /// The address to open, after redirects.
    pub url: String,
    /// The address as the reader wrote it, for `matchedText`.
    pub matched: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// JPEG thumbnail for `jpegThumbnail`, and the size the card should draw.
    pub thumbnail: Option<Vec<u8>>,
    pub thumbnail_width: u32,
    pub thumbnail_height: u32,
}

impl Preview {
    /// Whether the page said anything worth a card.
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.description.is_none() && self.thumbnail.is_none()
    }
}

/// The first web address in a draft, if it has one.
///
/// Only an address with a scheme or a `www.` prefix is picked up: guessing at
/// bare words is how a preview ends up under text that was never a link.
pub fn first_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|token| {
            token.trim_matches(|c: char| matches!(c, '<' | '>' | '"' | '\'' | '(' | '[' | '{'))
        })
        .map(|token| {
            token.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\''])
        })
        .find_map(|token| {
            if let Some(rest) = token
                .strip_prefix("https://")
                .or_else(|| token.strip_prefix("http://"))
            {
                (!rest.is_empty() && rest.contains('.')).then(|| token.to_owned())
            } else if let Some(rest) = token.strip_prefix("www.")
                && rest.contains('.')
                && !rest.ends_with('.')
            {
                Some(format!("https://{token}"))
            } else {
                None
            }
        })
}

/// Reads the page at `matched` and returns what it says about itself.
///
/// `None` means no card: unreachable, refused, nothing to show, or a page
/// that only served a picture too big to read. A missing preview is never an
/// error the reader has to dismiss.
pub async fn fetch(matched: &str) -> Option<Preview> {
    let target = crate::safety::preview_url(matched)?;
    let page = fetch_page(&target).await?;
    let said = parse(&String::from_utf8_lossy(&page.body));
    let mut preview = Preview {
        url: page.url,
        matched: matched.trim().to_owned(),
        title: said.title,
        description: said.description,
        ..Preview::default()
    };
    if let Some(source) = said.image
        && let Some(bytes) = fetch_image(&source).await
        && let Some((jpeg, width, height)) = thumbnail(&bytes)
    {
        preview.thumbnail = Some(jpeg);
        preview.thumbnail_width = width;
        preview.thumbnail_height = height;
    }
    (!preview.is_empty()).then_some(preview)
}

/// One fetched document, with the address that finally served it.
struct Page {
    url: String,
    body: Vec<u8>,
}

/// Fetches a page, following redirects one checked hop at a time.
async fn fetch_page(url: &str) -> Option<Page> {
    let (final_url, body) = get(url, MAX_HTML_BYTES, "*/*", true).await?;
    Some(Page {
        url: final_url,
        body,
    })
}

/// Fetches the page's picture, or nothing when it is too big or not an image.
async fn fetch_image(url: &str) -> Option<Vec<u8>> {
    let (_, body) = get(url, MAX_IMAGE_BYTES, "image/*", false).await?;
    Some(body)
}

/// GETs `url` with every hop checked, returning the address that answered and
/// at most `limit` bytes of the body.
async fn get(url: &str, limit: usize, accept: &str, truncate: bool) -> Option<(String, Vec<u8>)> {
    let mut current = reqwest::Url::parse(url).ok()?;
    for _ in 0..=MAX_REDIRECTS {
        let address = dialable(&current).await?;
        let client = client_for(&current, address)?;
        let response = client
            .get(current.clone())
            .header(reqwest::header::ACCEPT, accept)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .ok()?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)?
                .to_str()
                .ok()?;
            current = current.join(location).ok()?;
            continue;
        }
        if !response.status().is_success() {
            return None;
        }
        if let Some(length) = response.content_length()
            && length > limit as u64
            && !truncate
        {
            return None;
        }
        let body = read_capped(response, limit, truncate).await?;
        return Some((current.to_string(), body));
    }
    None
}

/// Reads a body up to `limit` bytes. Past the cap a page is cut (its metadata
/// is at the front) while a picture is refused (half an image is no image).
async fn read_capped(
    mut response: reqwest::Response,
    limit: usize,
    truncate: bool,
) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > limit {
            if !truncate {
                return None;
            }
            body.extend_from_slice(&chunk[..limit - body.len()]);
            return Some(body);
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

/// The address this request is allowed to dial, if any.
///
/// With a proxy configured the proxy resolves the name, so a local lookup
/// would only fail or lie; a literal address is still refused when it is
/// private, and a proxy the reader chose is trusted with the rest.
async fn dialable(url: &reqwest::Url) -> Option<Option<std::net::SocketAddr>> {
    match url.scheme() {
        "http" | "https" => {}
        _ => return None,
    }
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return public_ip(ip).then_some(Some(std::net::SocketAddr::new(ip, port)));
    }
    if !public_hostname(host) {
        return None;
    }
    if crate::proxy::reqwest_proxy().is_some() {
        return Some(None);
    }
    let addrs: Vec<std::net::SocketAddr> =
        tokio::net::lookup_host((host, port)).await.ok()?.collect();
    if addrs.is_empty() || !addrs.iter().all(|addr| public_ip(addr.ip())) {
        return None;
    }
    Some(addrs.first().copied())
}

/// A client for one hop, pinned to the address that was checked.
fn client_for(
    url: &reqwest::Url,
    address: Option<std::net::SocketAddr>,
) -> Option<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(TIMEOUT)
        // Redirects are followed here, so each hop is checked before it is
        // dialled; letting reqwest follow them would skip that check.
        .redirect(reqwest::redirect::Policy::none());
    if let Some(proxy) = crate::proxy::reqwest_proxy() {
        builder = builder.proxy(proxy);
    }
    if let (Some(address), Some(host)) = (address, url.host_str()) {
        builder = builder.resolve(host, address);
    }
    builder.build().ok()
}

/// Names that never leave the machine.
fn public_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return false;
    }
    // Cloud metadata endpoints answer to names as well as addresses.
    const REFUSED: &[&str] = &[
        "metadata.google.internal",
        "metadata.goog",
        "instance-data",
        "instance-data.ec2.internal",
    ];
    !REFUSED.contains(&host.as_str())
}

/// Whether an address is one this machine may reach on the open internet.
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Carrier-grade NAT.
        || (a == 100 && (64..128).contains(&b))
        // Reserved, benchmarking, and 6to4 relay anycast.
        || (a == 192 && b == 0)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 192 && b == 88 && ip.octets()[2] == 99)
        || a >= 240)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4);
    }
    let segments = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Unique local.
        || (segments[0] & 0xfe00) == 0xfc00
        // Link local.
        || (segments[0] & 0xffc0) == 0xfe80
        // Documentation.
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

/// What a page says about itself.
#[derive(Debug, Default, PartialEq)]
pub struct Said {
    pub title: Option<String>,
    pub description: Option<String>,
    /// The picture's address, still relative to the page.
    pub image: Option<String>,
}

/// Reads OpenGraph, then Twitter card, then plain HTML metadata.
///
/// The first source that has a field wins: a page that names its title for
/// social cards is describing the card, while `<title>` may carry the site's
/// own decoration.
pub fn parse(html: &str) -> Said {
    let mut og_title = None;
    let mut og_description = None;
    let mut og_image = None;
    let mut twitter_title = None;
    let mut twitter_description = None;
    let mut twitter_image = None;
    let mut meta_description = None;
    for (key, content) in meta_tags(html) {
        let content = clean(&content);
        let content = content.as_deref();
        let Some(content) = content else { continue };
        match key.as_str() {
            "og:title" => og_title = og_title.or_else(|| Some(content.to_owned())),
            "og:description" => {
                og_description = og_description.or_else(|| Some(content.to_owned()))
            }
            "og:image" | "og:image:url" | "og:image:secure_url" => {
                og_image = og_image.or_else(|| Some(content.to_owned()))
            }
            "twitter:title" => twitter_title = twitter_title.or_else(|| Some(content.to_owned())),
            "twitter:description" => {
                twitter_description = twitter_description.or_else(|| Some(content.to_owned()))
            }
            "twitter:image" | "twitter:image:src" => {
                twitter_image = twitter_image.or_else(|| Some(content.to_owned()))
            }
            "description" => {
                meta_description = meta_description.or_else(|| Some(content.to_owned()))
            }
            _ => {}
        }
    }
    Said {
        title: og_title
            .or(twitter_title)
            .or_else(|| document_title(html).and_then(|title| clean(&title)))
            .map(|title| clip(title, MAX_TITLE)),
        description: og_description
            .or(twitter_description)
            .or(meta_description)
            .map(|description| clip(description, MAX_DESCRIPTION)),
        image: og_image.or(twitter_image),
    }
}

/// Every `<meta>` tag's key and content, key lowercased.
fn meta_tags(html: &str) -> Vec<(String, String)> {
    let mut tags = Vec::new();
    let mut at = 0;
    while let Some(found) = find_ignore_case(html, at, "<meta") {
        let start = found + "<meta".len();
        let Some(end) = html[start..].find('>') else {
            break;
        };
        let mut key = None;
        let mut content = None;
        for (name, value) in attributes(&html[start..start + end]) {
            if name.eq_ignore_ascii_case("property") || name.eq_ignore_ascii_case("name") {
                key.get_or_insert(value);
            } else if name.eq_ignore_ascii_case("content") {
                content.get_or_insert(value);
            }
        }
        if let (Some(key), Some(content)) = (key, content) {
            tags.push((key.trim().to_ascii_lowercase(), content));
        }
        at = start + end + 1;
    }
    tags
}

/// The document's own title, undecorated.
fn document_title(html: &str) -> Option<String> {
    let start = find_ignore_case(html, 0, "<title")? + "<title".len();
    let open = start + html[start..].find('>')? + 1;
    let end = find_ignore_case(html, open, "</title>")?;
    Some(html[open..end].to_owned())
}

/// Attribute name/value pairs from the inside of a tag.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let mut attributes = Vec::new();
    let bytes = tag.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'/') {
            at += 1;
        }
        let name_start = at;
        while at < bytes.len()
            && !bytes[at].is_ascii_whitespace()
            && bytes[at] != b'='
            && bytes[at] != b'/'
            && bytes[at] != b'>'
        {
            at += 1;
        }
        if at == name_start {
            break;
        }
        let name = tag[name_start..at].to_owned();
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at >= bytes.len() || bytes[at] != b'=' {
            attributes.push((name, String::new()));
            continue;
        }
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let value;
        if at < bytes.len() && (bytes[at] == b'"' || bytes[at] == b'\'') {
            let quote = bytes[at];
            at += 1;
            let value_start = at;
            while at < bytes.len() && bytes[at] != quote {
                at += 1;
            }
            value = tag[value_start..at].to_owned();
            at += 1;
        } else {
            let value_start = at;
            while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'>' {
                at += 1;
            }
            value = tag[value_start..at].to_owned();
        }
        attributes.push((name, value));
    }
    attributes
}

/// Position of `needle` in `haystack` at or after `from`, ASCII case-blind.
fn find_ignore_case(haystack: &str, from: usize, needle: &str) -> Option<usize> {
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    if from >= hay.len() || needle.is_empty() || hay.len() - from < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&at| {
        hay[at..at + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Trimmed, whitespace-collapsed text with its character references decoded.
fn clean(text: &str) -> Option<String> {
    let decoded = decode_entities(text);
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

/// Cuts text to `max` characters on a word boundary when there is one.
fn clip(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let mut clipped: String = text.chars().take(max).collect();
    if let Some(space) = clipped.rfind(' ') {
        clipped.truncate(space);
    }
    clipped.push('…');
    clipped
}

/// Decodes the character references a page puts in its own metadata.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';') else { break };
        let reference = &rest[1..semi];
        let decoded = match reference {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "rsquo" => Some('’'),
            "lsquo" => Some('‘'),
            "ldquo" => Some('“'),
            "rdquo" => Some('”'),
            _ => reference.strip_prefix('#').and_then(|number| {
                let code = match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => number.parse::<u32>().ok()?,
                };
                char::from_u32(code)
            }),
        };
        match decoded {
            Some(character) => {
                out.push(character);
                rest = &rest[semi + 1..];
            }
            // Not a reference this reader knows: keep the ampersand and move
            // on, so prose with a bare `&` survives.
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Resizes a picture down to a small JPEG that fits in `jpegThumbnail`.
fn thumbnail(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let image = decode_image(bytes)?;
    // Try the sharpest encoding first and give ground only as far as the
    // wire budget demands.
    for (edge, quality) in [
        (THUMBNAIL_EDGE, 75),
        (THUMBNAIL_EDGE, 60),
        (144, 55),
        (96, 50),
    ] {
        let resized = image
            .resize(edge, edge, image::imageops::FilterType::Triangle)
            .to_rgb8();
        let (width, height) = resized.dimensions();
        let mut jpeg = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality);
        if encoder.encode_image(&resized).is_err() {
            return None;
        }
        if jpeg.len() <= MAX_THUMBNAIL_BYTES {
            return Some((jpeg, width, height));
        }
    }
    None
}

/// Decodes a picture, refusing one whose header claims more pixels than this
/// machine should allocate for a preview.
fn decode_image(bytes: &[u8]) -> Option<image::DynamicImage> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    if u64::from(width) * u64::from(height) > 40_000_000 {
        return None;
    }
    image::load_from_memory(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_address_is_found() {
        assert_eq!(
            first_url("look at https://zapfast.rocks/benchmarks/"),
            Some("https://zapfast.rocks/benchmarks/".into())
        );
        assert_eq!(
            first_url("see (https://example.com/a)."),
            Some("https://example.com/a".into())
        );
        assert_eq!(
            first_url("www.example.com is the site"),
            Some("https://www.example.com".into())
        );
    }

    #[test]
    fn prose_is_not_a_link() {
        assert_eq!(first_url("hello world"), None);
        assert_eq!(first_url("https://"), None);
        assert_eq!(first_url("mailto:someone@example.com"), None);
        assert_eq!(first_url("h t t p s"), None);
    }

    #[test]
    fn open_graph_wins_over_the_document_title() {
        let html = r#"
            <html><head>
            <title>Site — Some Page</title>
            <meta property="og:title" content="The Card Title">
            <meta property="og:description" content="What the card says.">
            <meta property="og:image" content="/cover.png">
            </head></html>"#;
        let said = parse(html);
        assert_eq!(said.title.as_deref(), Some("The Card Title"));
        assert_eq!(said.description.as_deref(), Some("What the card says."));
        assert_eq!(said.image.as_deref(), Some("/cover.png"));
    }

    #[test]
    fn twitter_cards_stand_in_for_open_graph() {
        let html = r#"<meta name="twitter:title" content="Tweet title">
                      <meta name="twitter:description" content="Tweet text">
                      <meta name="twitter:image" content="https://example.com/i.jpg">"#;
        let said = parse(html);
        assert_eq!(said.title.as_deref(), Some("Tweet title"));
        assert_eq!(said.description.as_deref(), Some("Tweet text"));
        assert_eq!(said.image.as_deref(), Some("https://example.com/i.jpg"));
    }

    #[test]
    fn the_document_title_is_the_last_resort() {
        let said = parse("<html><head><title>Just a page</title></head></html>");
        assert_eq!(said.title.as_deref(), Some("Just a page"));
        assert_eq!(said.description, None);
    }

    #[test]
    fn attributes_are_read_in_any_order() {
        let html = r#"<meta content='One' property='og:title'>
                      <meta content="Two" name="og:description">"#;
        let said = parse(html);
        assert_eq!(said.title.as_deref(), Some("One"));
        assert_eq!(said.description.as_deref(), Some("Two"));
    }

    #[test]
    fn character_references_are_decoded() {
        let said =
            parse(r#"<meta property="og:title" content="Fish &amp; Chips &mdash; &#65;&#x42;">"#);
        assert_eq!(said.title.as_deref(), Some("Fish & Chips — AB"));
    }

    #[test]
    fn an_ampersand_that_is_not_a_reference_survives() {
        let said = parse(r#"<meta property="og:title" content="A & B">"#);
        assert_eq!(said.title.as_deref(), Some("A & B"));
    }

    #[test]
    fn blank_metadata_falls_through() {
        let html = r#"<meta property="og:title" content="   ">
                      <title>Real title</title>"#;
        assert_eq!(parse(html).title.as_deref(), Some("Real title"));
    }

    #[test]
    fn long_metadata_is_clipped() {
        let long = "word ".repeat(100);
        let clipped = clip(long, 40);
        assert!(clipped.chars().count() <= 41, "{clipped}");
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn private_addresses_are_refused() {
        for address in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "255.255.255.255",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(!public_ip(ip), "{address} should be refused");
        }
        for address in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111"] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(public_ip(ip), "{address} should be allowed");
        }
    }

    #[test]
    fn local_names_are_refused() {
        assert!(!public_hostname("localhost"));
        assert!(!public_hostname("printer.local"));
        assert!(!public_hostname("metadata.google.internal"));
        assert!(public_hostname("zapfast.rocks"));
    }

    #[test]
    fn a_non_web_scheme_is_never_fetched() {
        assert_eq!(crate::safety::preview_url("file:///etc/passwd"), None);
        assert_eq!(crate::safety::preview_url("javascript:alert(1)"), None);
    }

    #[tokio::test]
    async fn a_loopback_page_is_not_fetched() {
        assert!(fetch("http://127.0.0.1:9/").await.is_none());
        assert!(fetch("http://localhost:9/").await.is_none());
    }
}
