use axum::http::HeaderMap;

/// Parsed User-Agent information.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedUserAgent {
    pub browser: Option<String>,
    pub browser_version: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub is_bot: bool,
}

/// User-Agent Client Hints sent by Chromium-based browsers.
///
/// Chrome froze the legacy `User-Agent` string years ago: it now reports a
/// fixed major version and a fixed platform version, so parsing it alone gives
/// increasingly wrong answers. When the low-entropy hint headers are present
/// they are authoritative.
///
/// Only the low-entropy hints are read — the ones browsers send by default.
/// The high-entropy ones (full version, model, exact platform version) require
/// an explicit `Accept-CH` opt-in and would raise the fingerprinting surface,
/// which is the opposite of this project's purpose.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientHints {
    /// Brand list from `Sec-CH-UA`, as (brand, major version) pairs.
    pub brands: Vec<(String, String)>,
    /// `Sec-CH-UA-Mobile`: `?1` → true, `?0` → false.
    pub is_mobile: Option<bool>,
    /// `Sec-CH-UA-Platform`, e.g. `Windows`, `macOS`, `Android`.
    pub platform: Option<String>,
}

/// Brand names browsers inject purely to break naive parsers.
///
/// Chromium's GREASE brands are random-looking strings such as
/// `"Not/A)Brand"` or `" Not;A Brand"`, deliberately varied between releases.
fn is_grease_brand(brand: &str) -> bool {
    let lowered = brand.to_ascii_lowercase();
    lowered.contains("not") && lowered.contains("brand")
}

/// Longest hint header read. A real `Sec-CH-UA` is under 200 bytes; anything
/// longer is not a browser, and header size is not capped by the body limit.
const MAX_HINT_HEADER_LEN: usize = 512;

/// Most brand entries read from `Sec-CH-UA`. Browsers send three to five.
const MAX_BRANDS: usize = 8;

/// Browser brands a hint may name.
///
/// Hints come from the client, so an unrestricted brand is an arbitrary string
/// written into the `browser` column: a header-sized label, a made-up row in
/// the breakdown, or a spreadsheet formula in the CSV export. Anything not
/// listed here is ignored and the User-Agent string decides instead.
const KNOWN_BRANDS: &[&str] = &[
    "Chromium",
    "Google Chrome",
    "Microsoft Edge",
    "Opera",
    "Opera GX",
    "Brave",
    "Vivaldi",
    "Yandex",
    "Samsung Internet",
    "Android WebView",
    "DuckDuckGo",
    "Whale",
];

impl ClientHints {
    /// Read the low-entropy client hints from a request's headers.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let brands = headers
            .get("sec-ch-ua")
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= MAX_HINT_HEADER_LEN)
            .map(parse_brand_list)
            .unwrap_or_default();

        let is_mobile = headers
            .get("sec-ch-ua-mobile")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| match v.trim() {
                "?1" => Some(true),
                "?0" => Some(false),
                _ => None,
            });

        let platform = headers
            .get("sec-ch-ua-platform")
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= MAX_HINT_HEADER_LEN)
            .and_then(|v| normalize_platform(v.trim().trim_matches('"')));

        Self {
            brands,
            is_mobile,
            platform,
        }
    }

    /// True when no usable hint was supplied.
    pub const fn is_empty(&self) -> bool {
        self.brands.is_empty() && self.is_mobile.is_none() && self.platform.is_none()
    }

    /// Best browser guess from the brand list, ignoring GREASE entries.
    ///
    /// Chromium sends several brands at once; the most specific one wins.
    fn browser(&self) -> Option<(String, String)> {
        // `min_by_key` keeps the first brand of the best rank, and the brand
        // list is ordered by the browser itself, so ties resolve the way the
        // browser presented them.
        let chosen = self
            .brands
            .iter()
            .filter(|(brand, _)| !is_grease_brand(brand))
            .min_by_key(|(brand, _)| brand_specificity(brand))?;
        Some((normalize_brand(&chosen.0), chosen.1.clone()))
    }
}

/// Ordering key for brand preference — lower is more specific.
///
/// Every Chromium-based browser advertises `"Chromium"` alongside its own
/// brand, and Chrome advertises both `"Chromium"` and `"Google Chrome"`. Only
/// the most specific brand carries the browser's real major version: Chrome
/// freezes the version it reports as `"Chromium"` (and in its legacy
/// User-Agent), which is the entire reason the hint exists.
fn brand_specificity(brand: &str) -> u8 {
    match brand.to_ascii_lowercase().as_str() {
        // The generic engine brand: always the last resort.
        "chromium" => 2,
        // More specific than the engine, less specific than a derived product.
        "google chrome" => 1,
        // A product brand such as Edge, Opera or Brave.
        _ => 0,
    }
}

/// Map a client-hint brand to the name used elsewhere in the product.
fn normalize_brand(brand: &str) -> String {
    match brand.to_ascii_lowercase().as_str() {
        "google chrome" | "chromium" => "Chrome".to_string(),
        "microsoft edge" => "Edge".to_string(),
        "opera" | "opera gx" => "Opera".to_string(),
        "yandex" => "Yandex Browser".to_string(),
        _ => brand.to_string(),
    }
}

/// A hint version reduced to digits and dots, at most 16 bytes, or empty.
fn sanitize_hint_version(raw: &str) -> String {
    let version = raw.trim().trim_matches('"').trim();
    let valid = !version.is_empty()
        && version.len() <= 16
        && version.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && version.bytes().next().is_some_and(|b| b.is_ascii_digit());
    if valid {
        version.to_string()
    } else {
        String::new()
    }
}

/// Parse a `Sec-CH-UA` header into (brand, version) pairs.
///
/// Format: `"Chromium";v="120", "Not(A:Brand";v="24", "Google Chrome";v="120"`
///
/// Only brands in [`KNOWN_BRANDS`] survive, spelled as the list spells them, and
/// versions are digits and dots; nothing the client typed is retained verbatim.
fn parse_brand_list(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in raw.split(',').take(MAX_BRANDS) {
        let mut parts = entry.splitn(2, ';');
        let Some(brand) = parts.next() else { continue };
        let brand = brand.trim().trim_matches('"').trim();
        let Some(known) = KNOWN_BRANDS
            .iter()
            .find(|known| known.eq_ignore_ascii_case(brand))
        else {
            continue;
        };
        let version = parts
            .next()
            .and_then(|v| v.trim().strip_prefix("v="))
            .map(sanitize_hint_version)
            .unwrap_or_default();
        out.push(((*known).to_string(), version));
    }
    out
}

/// Parse a User-Agent string.
pub fn parse_user_agent(ua: &str) -> ParsedUserAgent {
    parse_user_agent_with_hints(ua, &ClientHints::default())
}

/// Parse a User-Agent string, preferring Client Hints where they are present.
pub fn parse_user_agent_with_hints(ua: &str, hints: &ClientHints) -> ParsedUserAgent {
    if ua.is_empty() && hints.is_empty() {
        return ParsedUserAgent::default();
    }

    // One lowercase allocation, reused by every check below. The previous
    // implementation lowercased the string for bot detection and then re-scanned
    // the original a further twenty-odd times for browser and OS matching — on
    // the hottest path in the program.
    let lower = ua.to_ascii_lowercase();

    let mut parsed = ParsedUserAgent {
        browser: detect_browser(ua),
        browser_version: detect_browser_version(ua),
        os: detect_os(ua),
        os_version: detect_os_version(ua),
        is_bot: is_bot(&lower),
    };

    // Client hints override the frozen UA string where available.
    if let Some((brand, version)) = hints.browser() {
        parsed.browser = Some(brand);
        if !version.is_empty() {
            parsed.browser_version = Some(version);
        }
    }
    if let Some(platform) = &hints.platform
        && let Some(os) = normalize_platform(platform)
    {
        // A version read from the User-Agent belongs to the OS the User-Agent
        // named. If the hint says something else, pairing the two would record
        // an OS that never existed ("macOS 10").
        if parsed.os.as_deref() != Some(os.as_str()) {
            parsed.os_version = None;
        }
        parsed.os = Some(os);
    }

    parsed
}

/// Map a `Sec-CH-UA-Platform` value to the OS names used elsewhere.
///
/// Only platforms the product reports elsewhere are accepted; any other value
/// is dropped rather than stored, so the hint cannot add rows to the OS
/// breakdown.
fn normalize_platform(platform: &str) -> Option<String> {
    Some(
        match platform.to_ascii_lowercase().as_str() {
            "windows" => "Windows",
            "macos" => "macOS",
            "android" => "Android",
            "ios" => "iOS",
            "linux" => "Linux",
            "chrome os" | "chromeos" | "chromium os" => "Chrome OS",
            _ => return None,
        }
        .to_string(),
    )
}

/// Substrings that identify automated traffic.
///
/// Deliberately conservative. Broad substrings misclassify real people: the old
/// list matched `"fetch"` (any UA mentioning it) and `"whatsapp"` (the WhatsApp
/// in-app browser, which is a person reading a page), and the bare `"bot"`
/// matched device names such as the Cubot phone range.
const BOT_MARKERS: &[&str] = &[
    "bot/",
    "bot;",
    "bot)",
    " bot ",
    "googlebot",
    "bingbot",
    "yandexbot",
    "duckduckbot",
    "baiduspider",
    "applebot",
    "ahrefsbot",
    "semrushbot",
    "dotbot",
    "petalbot",
    "mj12bot",
    "seznambot",
    "linkedinbot",
    "discordbot",
    "telegrambot",
    "twitterbot",
    "slackbot",
    "facebookexternalhit",
    "crawler",
    "spider",
    "slurp",
    "headlesschrome",
    "phantomjs",
    "lighthouse",
    "pingdom",
    "uptimerobot",
    "statuscake",
    "gtmetrix",
    "pagespeed",
    "python-requests",
    "python-urllib",
    "aiohttp",
    "go-http-client",
    "okhttp",
    "java/",
    "jakarta",
    "axios/",
    "node-fetch",
    "got (",
    "wget",
    "libwww-perl",
    "mediapartners",
    "adsbot",
    "apis-google",
    "feedfetcher",
    "feedburner",
    "sogou",
    "exabot",
    "archive.org_bot",
    "ia_archiver",
    "screaming frog",
    "storebot",
    "googleother",
    "bingpreview",
    "meta-external",
    "postmanruntime",
    "bytespider",
    "chatgpt-user",
];

/// Prefixes that identify command-line and library clients.
const BOT_PREFIXES: &[&str] = &["curl/", "wget/", "libwww", "lwp-", "scrapy", "http_request"];

/// Detect automated traffic from an already-lowercased User-Agent.
fn is_bot(ua_lower: &str) -> bool {
    if ua_lower.is_empty() {
        // An absent User-Agent is almost never a real browser, but it is also
        // not proof of a crawler, so it is left to the operator's other filters.
        return false;
    }
    BOT_PREFIXES.iter().any(|p| ua_lower.starts_with(p))
        || BOT_MARKERS.iter().any(|m| ua_lower.contains(m))
        // Trailing "bot" with no following token, e.g. "SomeCrawlerBot".
        || ua_lower.ends_with("bot")
}

fn detect_browser(ua: &str) -> Option<String> {
    // Order matters: every Chromium derivative also advertises "Chrome/".
    if ua.contains("Edg/") || ua.contains("Edge/") || ua.contains("EdgA/") || ua.contains("EdgiOS/")
    {
        Some("Edge".to_string())
    } else if ua.contains("OPR/") || ua.contains("OPT/") || ua.contains("Opera") {
        Some("Opera".to_string())
    } else if ua.contains("Vivaldi/") {
        Some("Vivaldi".to_string())
    } else if ua.contains("Brave/") {
        Some("Brave".to_string())
    } else if ua.contains("SamsungBrowser/") {
        Some("Samsung Internet".to_string())
    } else if ua.contains("YaBrowser/") {
        Some("Yandex Browser".to_string())
    } else if ua.contains("UCBrowser/") || ua.contains("UCWEB/") {
        Some("UC Browser".to_string())
    } else if ua.contains("DuckDuckGo/") {
        Some("DuckDuckGo".to_string())
    } else if ua.contains("Chromium/") {
        Some("Chromium".to_string())
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        // Chrome on iOS wraps WebKit and says CriOS, not Chrome.
        Some("Chrome".to_string())
    } else if ua.contains("FxiOS/") || ua.contains("Firefox/") {
        Some("Firefox".to_string())
    } else if ua.contains("Safari/") {
        Some("Safari".to_string())
    } else {
        None
    }
}

fn detect_browser_version(ua: &str) -> Option<String> {
    // Checked in the same order as detect_browser so the version belongs to the
    // browser that was named.
    const PREFIXES: &[&str] = &[
        "Edg/",
        "EdgA/",
        "EdgiOS/",
        "Edge/",
        "OPR/",
        "OPT/",
        "Vivaldi/",
        "Brave/",
        "SamsungBrowser/",
        "YaBrowser/",
        "UCBrowser/",
        "DuckDuckGo/",
        "Chromium/",
        "Chrome/",
        "CriOS/",
        "FxiOS/",
        "Firefox/",
        // Safari reports its marketing version in "Version/"; it must come last
        // because Chromium on Android also emits a "Version/" token.
        "Version/",
    ];

    for prefix in PREFIXES {
        if let Some(pos) = ua.find(prefix) {
            let version: String = ua[pos + prefix.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if !version.is_empty() {
                return Some(version.trim_end_matches('.').to_string());
            }
        }
    }
    None
}

fn detect_os(ua: &str) -> Option<String> {
    if ua.contains("Windows") {
        Some("Windows".to_string())
    } else if ua.contains("iPhone") || ua.contains("iPad") || ua.contains("iPod") {
        // Checked before macOS: iOS UAs also contain "Mac OS X".
        Some("iOS".to_string())
    } else if ua.contains("Mac OS X") || ua.contains("macOS") {
        Some("macOS".to_string())
    } else if ua.contains("Android") {
        // Checked before Linux: Android UAs contain "Linux; Android".
        Some("Android".to_string())
    } else if ua.contains("CrOS") {
        Some("Chrome OS".to_string())
    } else if ua.contains("Linux") || ua.contains("X11") {
        Some("Linux".to_string())
    } else {
        None
    }
}

/// Map a Windows NT kernel version to its marketing name.
///
/// `Windows NT 10.0` is what both Windows 10 and Windows 11 report, so the two
/// cannot be told apart from the UA string alone; both are reported as "10".
fn windows_marketing_version(nt_version: &str) -> String {
    match nt_version {
        "10.0" => "10".to_string(),
        "6.3" => "8.1".to_string(),
        "6.2" => "8".to_string(),
        "6.1" => "7".to_string(),
        other => other.to_string(),
    }
}

fn detect_os_version(ua: &str) -> Option<String> {
    if ua.contains("Windows NT") {
        extract_version_after(ua, "Windows NT ").map(|v| windows_marketing_version(&v))
    } else if ua.contains("iPhone OS") {
        extract_version_after(ua, "iPhone OS ").map(|v| v.replace('_', "."))
    } else if ua.contains("CPU OS") {
        extract_version_after(ua, "CPU OS ").map(|v| v.replace('_', "."))
    } else if ua.contains("Mac OS X") {
        // Every current macOS browser reports the frozen 10.15.x here whatever
        // the real release, so the value says nothing and would put every Mac
        // visitor in one "10.15.7" row.
        extract_version_after(ua, "Mac OS X ")
            .map(|v| v.replace('_', "."))
            .filter(|v| v != "10.15.7" && v != "10.15")
    } else if ua.contains("Android") {
        extract_version_after(ua, "Android ")
    } else {
        None
    }
}

fn extract_version_after(ua: &str, prefix: &str) -> Option<String> {
    let pos = ua.find(prefix)?;
    let version: String = ua[pos + prefix.len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '_')
        .collect();
    if version.is_empty() {
        None
    } else {
        Some(version.trim_end_matches(['.', '_']).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHROME_WIN: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/120.0.0.0 Safari/537.36";
    const FIREFOX_LINUX: &str =
        "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";
    const SAFARI_IOS: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 \
         (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1";
    const EDGE_WIN: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/120.0.0.0 Safari/537.36 Edg/120.0.2210.61";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    // ── Browser detection ────────────────────────────────────────────────

    #[test]
    fn test_detect_common_browsers() {
        assert_eq!(
            parse_user_agent(CHROME_WIN).browser.as_deref(),
            Some("Chrome")
        );
        assert_eq!(
            parse_user_agent(FIREFOX_LINUX).browser.as_deref(),
            Some("Firefox")
        );
        assert_eq!(
            parse_user_agent(SAFARI_IOS).browser.as_deref(),
            Some("Safari")
        );
        assert_eq!(parse_user_agent(EDGE_WIN).browser.as_deref(), Some("Edge"));
    }

    #[test]
    fn test_chromium_is_distinguished_from_chrome() {
        let chromium =
            "Mozilla/5.0 (X11; Linux x86_64) Chromium/120.0.0.0 Chrome/120.0.0.0 Safari/537.36";
        assert_eq!(
            parse_user_agent(chromium).browser.as_deref(),
            Some("Chromium")
        );
    }

    #[test]
    fn test_browser_versions() {
        assert_eq!(
            parse_user_agent(CHROME_WIN).browser_version.as_deref(),
            Some("120.0.0.0")
        );
        assert_eq!(
            parse_user_agent(FIREFOX_LINUX).browser_version.as_deref(),
            Some("121.0")
        );
        assert_eq!(
            parse_user_agent(EDGE_WIN).browser_version.as_deref(),
            Some("120.0.2210.61")
        );
        assert_eq!(
            parse_user_agent(SAFARI_IOS).browser_version.as_deref(),
            Some("17.2")
        );
    }

    // ── OS detection ─────────────────────────────────────────────────────

    #[test]
    fn test_detect_operating_systems() {
        assert_eq!(parse_user_agent(CHROME_WIN).os.as_deref(), Some("Windows"));
        assert_eq!(parse_user_agent(FIREFOX_LINUX).os.as_deref(), Some("Linux"));
        assert_eq!(parse_user_agent(SAFARI_IOS).os.as_deref(), Some("iOS"));
    }

    #[test]
    fn test_android_is_not_reported_as_linux() {
        let android =
            "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Chrome/120.0.0.0";
        let parsed = parse_user_agent(android);
        assert_eq!(parsed.os.as_deref(), Some("Android"));
        assert_eq!(parsed.os_version.as_deref(), Some("14"));
    }

    #[test]
    fn test_windows_nt_version_is_mapped_to_a_marketing_name() {
        assert_eq!(
            parse_user_agent(CHROME_WIN).os_version.as_deref(),
            Some("10")
        );
        let win7 = "Mozilla/5.0 (Windows NT 6.1; Win64; x64) Chrome/109.0.0.0";
        assert_eq!(parse_user_agent(win7).os_version.as_deref(), Some("7"));
    }

    #[test]
    fn test_ios_version_underscores_become_dots() {
        assert_eq!(
            parse_user_agent(SAFARI_IOS).os_version.as_deref(),
            Some("17.2")
        );
    }

    #[test]
    fn test_ipad_reports_ios() {
        let ipad = "Mozilla/5.0 (iPad; CPU OS 17_2 like Mac OS X) AppleWebKit/605.1.15 Version/17.2 Safari/604.1";
        let parsed = parse_user_agent(ipad);
        assert_eq!(parsed.os.as_deref(), Some("iOS"));
        assert_eq!(parsed.os_version.as_deref(), Some("17.2"));
    }

    #[test]
    fn test_chrome_os() {
        let cros = "Mozilla/5.0 (X11; CrOS x86_64 14541.0.0) AppleWebKit/537.36 Chrome/120.0.0.0";
        assert_eq!(parse_user_agent(cros).os.as_deref(), Some("Chrome OS"));
    }

    // ── Bot detection ────────────────────────────────────────────────────

    #[test]
    fn test_known_bots_are_detected() {
        let bots = [
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            "Mozilla/5.0 (compatible; bingbot/2.0; +http://www.bing.com/bingbot.htm)",
            "curl/8.4.0",
            "python-requests/2.31.0",
            "Go-http-client/2.0",
            "Mozilla/5.0 (compatible; AhrefsBot/7.0; +http://ahrefs.com/robot/)",
            "facebookexternalhit/1.1",
            "Mozilla/5.0 (X11; Linux x86_64) HeadlessChrome/120.0.0.0",
            "Wget/1.21.4",
        ];
        for ua in bots {
            assert!(
                parse_user_agent(ua).is_bot,
                "{ua} must be detected as a bot"
            );
        }
    }

    #[test]
    fn test_real_browsers_are_not_flagged_as_bots() {
        for ua in [CHROME_WIN, FIREFOX_LINUX, SAFARI_IOS, EDGE_WIN] {
            assert!(!parse_user_agent(ua).is_bot, "{ua} must not be a bot");
        }
    }

    #[test]
    fn test_device_names_containing_bot_are_not_flagged() {
        // Regression: a bare `contains("bot")` matched the Cubot phone range.
        let cubot = "Mozilla/5.0 (Linux; Android 12; CUBOT NOTE 20) AppleWebKit/537.36 Chrome/108.0.0.0 Mobile Safari/537.36";
        assert!(!parse_user_agent(cubot).is_bot);
    }

    #[test]
    fn test_in_app_browsers_are_not_flagged() {
        // Regression: `contains("whatsapp")` classified WhatsApp's in-app
        // browser — a real person reading a page — as a crawler.
        let whatsapp = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148 [FBAN/WhatsApp]";
        assert!(!parse_user_agent(whatsapp).is_bot);
    }

    #[test]
    fn test_fetch_substring_does_not_flag_a_browser() {
        // Regression: `contains("fetch")` was broad enough to catch real UAs.
        let ua = "Mozilla/5.0 (Windows NT 10.0) Chrome/120.0 SomeFetcherExtension/1.0";
        assert!(!parse_user_agent(ua).is_bot);
    }

    #[test]
    fn test_empty_user_agent() {
        let parsed = parse_user_agent("");
        assert_eq!(parsed, ParsedUserAgent::default());
        assert!(!parsed.is_bot);
    }

    // ── Client Hints ─────────────────────────────────────────────────────

    #[test]
    fn test_client_hints_parsing() {
        let h = headers(&[
            (
                "sec-ch-ua",
                r#""Not_A Brand";v="8", "Chromium";v="120", "Google Chrome";v="120""#,
            ),
            ("sec-ch-ua-mobile", "?0"),
            ("sec-ch-ua-platform", "\"Windows\""),
        ]);
        let hints = ClientHints::from_headers(&h);
        assert_eq!(hints.is_mobile, Some(false));
        assert_eq!(hints.platform.as_deref(), Some("Windows"));
        // GREASE brands are not browsers and are not kept at all.
        assert_eq!(
            hints.brands,
            vec![
                ("Chromium".to_string(), "120".to_string()),
                ("Google Chrome".to_string(), "120".to_string())
            ]
        );
    }

    #[test]
    fn test_grease_brands_are_ignored() {
        let h = headers(&[(
            "sec-ch-ua",
            r#""Not/A)Brand";v="99", "Google Chrome";v="121""#,
        )]);
        let hints = ClientHints::from_headers(&h);
        let (brand, version) = hints.browser().unwrap();
        assert_eq!(brand, "Chrome");
        assert_eq!(version, "121");
    }

    #[test]
    fn test_hints_override_the_frozen_user_agent_version() {
        // Chrome freezes the UA major version; the hint carries the real one.
        let h = headers(&[("sec-ch-ua", r#""Chromium";v="99", "Google Chrome";v="131""#)]);
        let hints = ClientHints::from_headers(&h);
        let parsed = parse_user_agent_with_hints(CHROME_WIN, &hints);
        assert_eq!(parsed.browser.as_deref(), Some("Chrome"));
        assert_eq!(parsed.browser_version.as_deref(), Some("131"));
    }

    #[test]
    fn test_chromium_only_brand_list_still_resolves() {
        // A pure Chromium build advertises no product brand; the generic one
        // must still be used rather than yielding no browser at all.
        let h = headers(&[("sec-ch-ua", r#""Not-A.Brand";v="99", "Chromium";v="122""#)]);
        let hints = ClientHints::from_headers(&h);
        let (brand, version) = hints.browser().unwrap();
        assert_eq!(brand, "Chrome");
        assert_eq!(version, "122");
    }

    #[test]
    fn test_hints_prefer_the_specific_product_brand() {
        let h = headers(&[(
            "sec-ch-ua",
            r#""Not.A/Brand";v="8", "Chromium";v="120", "Microsoft Edge";v="120""#,
        )]);
        let hints = ClientHints::from_headers(&h);
        assert_eq!(hints.browser().unwrap().0, "Edge");
    }

    #[test]
    fn test_hints_platform_overrides_the_user_agent() {
        let h = headers(&[("sec-ch-ua-platform", "\"macOS\"")]);
        let hints = ClientHints::from_headers(&h);
        assert_eq!(
            parse_user_agent_with_hints(CHROME_WIN, &hints)
                .os
                .as_deref(),
            Some("macOS")
        );
    }

    #[test]
    fn test_unknown_platform_hint_is_ignored() {
        let h = headers(&[("sec-ch-ua-platform", "\"Unknown\"")]);
        let hints = ClientHints::from_headers(&h);
        assert_eq!(
            parse_user_agent_with_hints(CHROME_WIN, &hints)
                .os
                .as_deref(),
            Some("Windows"),
            "an unusable hint must not erase the UA-derived value"
        );
    }

    #[test]
    fn test_absent_hints_leave_parsing_unchanged() {
        let hints = ClientHints::from_headers(&HeaderMap::new());
        assert!(hints.is_empty());
        assert_eq!(
            parse_user_agent_with_hints(CHROME_WIN, &hints),
            parse_user_agent(CHROME_WIN)
        );
    }

    #[test]
    fn test_mobile_hint_parsing() {
        assert_eq!(
            ClientHints::from_headers(&headers(&[("sec-ch-ua-mobile", "?1")])).is_mobile,
            Some(true)
        );
        assert_eq!(
            ClientHints::from_headers(&headers(&[("sec-ch-ua-mobile", "garbage")])).is_mobile,
            None
        );
    }

    #[test]
    fn test_hostile_client_hints_cannot_inject_labels_or_bulk() {
        // SI-09 regression: brand, version and platform were stored as sent. A
        // single request could carry a header-sized `browser` string (the body
        // limit does not cover headers), an arbitrary label in the browser
        // breakdown, or a spreadsheet formula for the CSV export.
        let huge = "A".repeat(60_000);
        let hostile = [
            format!(r#""{huge}";v="120""#),
            r#""=HYPERLINK(\"http://evil\")";v="1""#.to_string(),
            r#""Chromium";v="1""#.to_string() + &format!(r#", "Google Chrome";v="{huge}""#),
            // Within the size limit: a known brand with a hostile version.
            r#""Google Chrome";v="=1+1""#.to_string(),
        ];
        for header in &hostile {
            let h = headers(&[
                ("sec-ch-ua", header),
                ("sec-ch-ua-platform", &format!("\"=cmd|{huge}\"")),
            ]);
            let parsed = parse_user_agent_with_hints(CHROME_WIN, &ClientHints::from_headers(&h));
            for value in [
                &parsed.browser,
                &parsed.browser_version,
                &parsed.os,
                &parsed.os_version,
            ]
            .into_iter()
            .flatten()
            {
                assert!(value.len() <= 64, "stored a {}-byte label", value.len());
                assert!(
                    !value.starts_with(['=', '+', '-', '@']),
                    "{value:?} would be read as a formula"
                );
            }
            assert_eq!(
                parsed.os.as_deref(),
                Some("Windows"),
                "a bad platform hint falls back to the UA"
            );
        }
    }

    #[test]
    fn test_unlisted_brands_fall_back_to_the_user_agent() {
        let h = headers(&[("sec-ch-ua", r#""Totally Made Up";v="7""#)]);
        let parsed = parse_user_agent_with_hints(CHROME_WIN, &ClientHints::from_headers(&h));
        assert_eq!(parsed.browser.as_deref(), Some("Chrome"));
        assert_eq!(parsed.browser_version.as_deref(), Some("120.0.0.0"));
    }

    #[test]
    fn test_platform_hint_is_allow_listed_and_canonical() {
        for (raw, expected) in [
            ("\"Chrome OS\"", Some("Chrome OS")),
            ("\"Chromium OS\"", Some("Chrome OS")),
            ("\"iOS\"", Some("iOS")),
            ("\"TempleOS\"", None),
        ] {
            let h = headers(&[("sec-ch-ua-platform", raw)]);
            assert_eq!(
                ClientHints::from_headers(&h).platform.as_deref(),
                expected,
                "{raw}"
            );
        }
    }

    // ── iOS wrappers, frozen versions, bot markers (SI-22) ───────────────

    const CHROME_IOS: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 \
         (KHTML, like Gecko) CriOS/120.0.6099.119 Mobile/15E148 Safari/604.1";
    const EDGE_IOS: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 \
         (KHTML, like Gecko) Version/17.0 EdgiOS/120.0.2210.126 Mobile/15E148 Safari/605.1.15";
    const OPERA_TOUCH_IOS: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 \
         (KHTML, like Gecko) Mobile/15E148 Safari/604.1 OPT/4.0.1";
    const CHROME_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

    #[test]
    fn test_ios_browser_wrappers_are_not_reported_as_safari() {
        // Chrome, Edge and Opera on iOS carry no "Chrome/", "Edg/" or "OPR/"
        // token, so they fell through to "Safari/" with no version.
        for (ua, browser, version) in [
            (CHROME_IOS, "Chrome", "120.0.6099.119"),
            (EDGE_IOS, "Edge", "120.0.2210.126"),
            (OPERA_TOUCH_IOS, "Opera", "4.0.1"),
        ] {
            let parsed = parse_user_agent(ua);
            assert_eq!(parsed.browser.as_deref(), Some(browser), "{ua}");
            assert_eq!(parsed.browser_version.as_deref(), Some(version), "{ua}");
            assert_eq!(parsed.os.as_deref(), Some("iOS"));
            assert_eq!(parsed.os_version.as_deref(), Some("17.2"));
        }
    }

    #[test]
    fn test_frozen_macos_version_is_not_reported() {
        let parsed = parse_user_agent(CHROME_MAC);
        assert_eq!(parsed.os.as_deref(), Some("macOS"));
        assert_eq!(
            parsed.os_version, None,
            "10.15.7 is a constant, not a release"
        );
        // A genuinely different version is kept.
        let big_sur = CHROME_MAC.replace("10_15_7", "11_6");
        assert_eq!(
            parse_user_agent(&big_sur).os_version.as_deref(),
            Some("11.6")
        );
    }

    #[test]
    fn test_more_crawlers_are_recognised_and_browsers_are_not() {
        for bot in [
            "Mozilla/5.0 (Linux; Android 7.0;) AppleWebKit/537.36 (KHTML, like Gecko) Mobile Safari/537.36 (compatible; Storebot-Google/1.0)",
            "Mozilla/5.0 (compatible; GoogleOther)",
            "Mozilla/5.0 (Windows NT 6.1; WOW64) AppleWebKit/534+ (KHTML, like Gecko) BingPreview/1.0b",
            "PostmanRuntime/7.36.0",
            "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; ChatGPT-User/1.0; +https://openai.com/bot)",
        ] {
            assert!(parse_user_agent(bot).is_bot, "{bot}");
        }
        for human in [CHROME_WIN, CHROME_IOS, CHROME_MAC, EDGE_WIN] {
            assert!(!parse_user_agent(human).is_bot, "{human}");
        }
    }

    #[test]
    fn test_a_platform_hint_that_disagrees_with_the_user_agent_drops_the_stale_version() {
        let h = headers(&[("sec-ch-ua-platform", "\"macOS\"")]);
        let parsed = parse_user_agent_with_hints(CHROME_WIN, &ClientHints::from_headers(&h));
        assert_eq!(parsed.os.as_deref(), Some("macOS"));
        assert_eq!(
            parsed.os_version, None,
            "Windows NT 10 is not a macOS version"
        );

        let agreeing = headers(&[("sec-ch-ua-platform", "\"Windows\"")]);
        let parsed = parse_user_agent_with_hints(CHROME_WIN, &ClientHints::from_headers(&agreeing));
        assert_eq!(parsed.os_version.as_deref(), Some("10"));
    }

    #[test]
    fn test_grease_detection() {
        assert!(is_grease_brand("Not/A)Brand"));
        assert!(is_grease_brand(" Not;A Brand"));
        assert!(is_grease_brand("Not_A Brand"));
        assert!(!is_grease_brand("Google Chrome"));
        assert!(!is_grease_brand("Chromium"));
    }
}
