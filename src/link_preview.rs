//! Link previews for the links you send.
//!
//! Reads the page behind a link in the draft (OpenGraph, Twitter card or
//! `<title>`), cuts a small JPEG thumbnail, and returns what an outgoing
//! message needs to carry the same `extendedTextMessage` metadata a phone
//! would attach. Only the link in the draft is ever fetched, never one that
//! someone else sent.
//!
//! Every hop is checked against private, loopback, link-local and reserved
//! addresses before it is dialled, and the checked address is pinned for the
//! request.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

const MAX_HTML_BYTES: usize = 512 * 1024;
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REDIRECTS: usize = 5;
/// `jpegThumbnail` travels inside the message, so it has to stay small.
const MAX_THUMBNAIL_BYTES: usize = 48 * 1024;
const MAX_TITLE: usize = 200;
const MAX_DESCRIPTION: usize = 400;
const THUMBNAIL_EDGE: u32 = 192;
// Not the updater's agent, which pages can tell apart.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// Metadata for one link, ready to go out with a message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preview {
    pub url: String,
    /// The address as typed, for `matchedText`.
    pub matched: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub thumbnail: Option<Vec<u8>>,
    pub thumbnail_width: u32,
    pub thumbnail_height: u32,
}

impl Preview {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.description.is_none() && self.thumbnail.is_none()
    }
}

/// The first web address in a draft. Needs a scheme or a `www.` prefix, so a
/// bare word is never mistaken for a link.
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

/// Reads the page at `matched`. `None` means no card: unreachable, refused or
/// nothing worth showing.
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

struct Page {
    url: String,
    body: Vec<u8>,
}

async fn fetch_page(url: &str) -> Option<Page> {
    let (final_url, body) = get(url, MAX_HTML_BYTES, "*/*", true).await?;
    Some(Page {
        url: final_url,
        body,
    })
}

async fn fetch_image(url: &str) -> Option<Vec<u8>> {
    let (_, body) = get(url, MAX_IMAGE_BYTES, "image/*", false).await?;
    Some(body)
}

/// GETs `url`, checking every redirect hop. Returns the final address and at
/// most `limit` bytes of body.
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

/// Past `limit` a page is cut (its metadata is at the front) but a picture is
/// refused, since half an image is no image.
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

/// The address this request may dial, if any. With a proxy the proxy resolves
/// the name, so there is nothing to pin; literal private addresses are still
/// refused.
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

fn client_for(
    url: &reqwest::Url,
    address: Option<std::net::SocketAddr>,
) -> Option<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(TIMEOUT)
        // Followed by hand in `get` so each hop is checked.
        .redirect(reqwest::redirect::Policy::none());
    if let Some(proxy) = crate::proxy::reqwest_proxy() {
        builder = builder.proxy(proxy);
    }
    if let (Some(address), Some(host)) = (address, url.host_str()) {
        builder = builder.resolve(host, address);
    }
    builder.build().ok()
}

fn public_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return false;
    }
    // Cloud metadata endpoints answer to names too.
    const REFUSED: &[&str] = &[
        "metadata.google.internal",
        "metadata.goog",
        "instance-data",
        "instance-data.ec2.internal",
    ];
    !REFUSED.contains(&host.as_str())
}

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

#[derive(Debug, Default, PartialEq)]
pub struct Said {
    pub title: Option<String>,
    pub description: Option<String>,
    /// Still relative to the page.
    pub image: Option<String>,
}

/// OpenGraph wins over Twitter card, which wins over plain HTML. `<title>`
/// often carries the site name, so it comes last.
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

fn document_title(html: &str) -> Option<String> {
    let start = find_ignore_case(html, 0, "<title")? + "<title".len();
    let open = start + html[start..].find('>')? + 1;
    let end = find_ignore_case(html, open, "</title>")?;
    Some(html[open..end].to_owned())
}

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

fn clean(text: &str) -> Option<String> {
    let decoded = decode_entities(text);
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

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
            // Unknown reference: keep the `&` as text.
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn thumbnail(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let image = decode_image(bytes)?;
    // Sharpest first, smaller only until it fits.
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

/// Refuses pictures whose header claims an absurd pixel count.
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

    /// Reads a real page over the network. Run by hand:
    /// `cargo test --lib --  --ignored a_real_page`
    #[tokio::test]
    #[ignore]
    async fn a_real_page_is_read_for_its_own_metadata() {
        let preview = fetch("https://zapfast.rocks/").await.expect("preview");
        eprintln!(
            "url={} title={:?} description={:?} thumbnail={:?} bytes {}x{}",
            preview.url,
            preview.title,
            preview.description,
            preview.thumbnail.as_ref().map(Vec::len),
            preview.thumbnail_width,
            preview.thumbnail_height,
        );
        assert!(
            preview.title.is_some() || preview.description.is_some(),
            "the page should describe itself"
        );
        if let Some(thumbnail) = &preview.thumbnail {
            assert!(thumbnail.len() <= MAX_THUMBNAIL_BYTES);
            assert!(thumbnail.starts_with(&[0xff, 0xd8]), "not a JPEG");
        }
    }

    #[tokio::test]
    async fn a_loopback_page_is_not_fetched() {
        assert!(fetch("http://127.0.0.1:9/").await.is_none());
        assert!(fetch("http://localhost:9/").await.is_none());
    }
}
