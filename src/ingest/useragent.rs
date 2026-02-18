/// Parsed User-Agent information with browser, OS, and bot detection.
#[derive(Debug, Clone, Default)]
pub struct ParsedUserAgent {
    pub browser: Option<String>,
    pub browser_version: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub is_bot: bool,
}

/// Parse a User-Agent string into browser, OS, and bot information.
pub fn parse_user_agent(ua: &str) -> ParsedUserAgent {
    if ua.is_empty() {
        return ParsedUserAgent::default();
    }

    // Check for bots first
    if is_bot(ua) {
        return ParsedUserAgent {
            browser: detect_browser(ua),
            browser_version: detect_browser_version(ua),
            os: detect_os(ua),
            os_version: detect_os_version(ua),
            is_bot: true,
        };
    }

    ParsedUserAgent {
        browser: detect_browser(ua),
        browser_version: detect_browser_version(ua),
        os: detect_os(ua),
        os_version: detect_os_version(ua),
        is_bot: false,
    }
}

/// Detect if the User-Agent string belongs to a known bot or crawler.
fn is_bot(ua: &str) -> bool {
    let ua_lower = ua.to_lowercase();
    ua_lower.contains("bot")
        || ua_lower.contains("crawler")
        || ua_lower.contains("spider")
        || ua_lower.contains("slurp")
        || ua_lower.contains("fetch")
        || ua_lower.contains("headless")
        || ua_lower.contains("phantom")
        || ua_lower.contains("lighthouse")
        || ua_lower.contains("pingdom")
        || ua_lower.contains("uptimerobot")
        || ua_lower.contains("python-requests")
        || ua_lower.contains("python-urllib")
        || ua_lower.contains("go-http-client")
        || ua_lower.contains("java/")
        || ua_lower.contains("wget")
        || ua_lower.starts_with("curl")
        || ua_lower.starts_with("libwww")
        || ua_lower.starts_with("lwp-")
        || ua_lower.starts_with("scrapy")
        || ua_lower.contains("mediapartners")
        || ua_lower.contains("adsbot")
        || ua_lower.contains("apis-google")
        || ua_lower.contains("feedfetcher")
        || ua_lower.contains("facebookexternalhit")
        || ua_lower.contains("linkedinbot")
        || ua_lower.contains("discordbot")
        || ua_lower.contains("telegrambot")
        || ua_lower.contains("whatsapp")
        || ua_lower.contains("applebot")
        || ua_lower.contains("ahrefsbot")
        || ua_lower.contains("semrushbot")
        || ua_lower.contains("dotbot")
        || ua_lower.contains("petalbot")
        || ua_lower.contains("yandexbot")
        || ua_lower.contains("baiduspider")
        || ua_lower.contains("duckduckbot")
        || ua_lower.contains("sogou")
        || ua_lower.contains("exabot")
}

fn detect_browser(ua: &str) -> Option<String> {
    // Order matters: check more specific patterns first
    if ua.contains("Edg/") || ua.contains("Edge/") {
        Some("Edge".to_string())
    } else if ua.contains("OPR/") || ua.contains("Opera") {
        Some("Opera".to_string())
    } else if ua.contains("Vivaldi/") {
        Some("Vivaldi".to_string())
    } else if ua.contains("Brave") {
        Some("Brave".to_string())
    } else if ua.contains("SamsungBrowser/") {
        Some("Samsung Internet".to_string())
    } else if ua.contains("UCBrowser/") || ua.contains("UCWEB/") {
        Some("UC Browser".to_string())
    } else if ua.contains("Chrome/") && !ua.contains("Chromium/") {
        Some("Chrome".to_string())
    } else if ua.contains("Safari/") && !ua.contains("Chrome/") {
        Some("Safari".to_string())
    } else if ua.contains("Firefox/") {
        Some("Firefox".to_string())
    } else {
        None
    }
}

fn detect_browser_version(ua: &str) -> Option<String> {
    let patterns = [
        ("Edg/", "Edg/"),
        ("Edge/", "Edge/"),
        ("OPR/", "OPR/"),
        ("Vivaldi/", "Vivaldi/"),
        ("SamsungBrowser/", "SamsungBrowser/"),
        ("UCBrowser/", "UCBrowser/"),
        ("Chrome/", "Chrome/"),
        ("Firefox/", "Firefox/"),
        ("Version/", "Version/"),
    ];

    for (check, prefix) in &patterns {
        if ua.contains(*check) {
            if let Some(pos) = ua.find(prefix) {
                let version_start = pos + prefix.len();
                let version: String = ua[version_start..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                if !version.is_empty() {
                    return Some(version);
                }
            }
        }
    }
    None
}

fn detect_os(ua: &str) -> Option<String> {
    if ua.contains("Windows") {
        Some("Windows".to_string())
    } else if ua.contains("iPhone") || ua.contains("iPad") || ua.contains("iOS") {
        // Check iOS before macOS since iPhone UAs contain "Mac OS X" (L7)
        Some("iOS".to_string())
    } else if ua.contains("Mac OS X") || ua.contains("macOS") {
        Some("macOS".to_string())
    } else if ua.contains("Android") {
        Some("Android".to_string())
    } else if ua.contains("Linux") {
        Some("Linux".to_string())
    } else if ua.contains("CrOS") {
        Some("Chrome OS".to_string())
    } else {
        None
    }
}

fn detect_os_version(ua: &str) -> Option<String> {
    if ua.contains("Windows NT") {
        extract_version_after(ua, "Windows NT ")
    } else if ua.contains("iPhone OS") {
        // Check iPhone OS before Mac OS X since iPhone UAs contain both (L7)
        extract_version_after(ua, "iPhone OS ").map(|v| v.replace('_', "."))
    } else if ua.contains("Mac OS X") {
        extract_version_after(ua, "Mac OS X ").map(|v| v.replace('_', "."))
    } else if ua.contains("Android") {
        extract_version_after(ua, "Android ")
    } else {
        None
    }
}

fn extract_version_after(ua: &str, prefix: &str) -> Option<String> {
    let pos = ua.find(prefix)?;
    let start = pos + prefix.len();
    let version: String = ua[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '_')
        .collect();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_chrome_windows() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.6099.130 Safari/537.36";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Chrome"));
        assert_eq!(parsed.browser_version.as_deref(), Some("120.0.6099.130"));
        assert_eq!(parsed.os.as_deref(), Some("Windows"));
        assert_eq!(parsed.os_version.as_deref(), Some("10.0"));
        assert!(!parsed.is_bot);
    }

    #[test]
    fn test_parse_firefox_linux() {
        let ua = "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Firefox"));
        assert_eq!(parsed.browser_version.as_deref(), Some("121.0"));
        assert_eq!(parsed.os.as_deref(), Some("Linux"));
        assert!(!parsed.is_bot);
    }

    #[test]
    fn test_parse_safari_macos() {
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Safari/605.1.15";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Safari"));
        assert_eq!(parsed.os.as_deref(), Some("macOS"));
        assert_eq!(parsed.os_version.as_deref(), Some("10.15.7"));
        assert!(!parsed.is_bot);
    }

    #[test]
    fn test_parse_edge() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.2210.91";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Edge"));
        assert_eq!(parsed.browser_version.as_deref(), Some("120.0.2210.91"));
        assert!(!parsed.is_bot);
    }

    #[test]
    fn test_parse_android() {
        let ua = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.6099.144 Mobile Safari/537.36";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Chrome"));
        assert_eq!(parsed.os.as_deref(), Some("Android"));
        assert_eq!(parsed.os_version.as_deref(), Some("14"));
        assert!(!parsed.is_bot);
    }

    #[test]
    fn test_parse_iphone() {
        let ua = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Safari"));
        assert_eq!(parsed.os.as_deref(), Some("iOS"));
        assert_eq!(parsed.os_version.as_deref(), Some("17.2.1"));
        assert!(!parsed.is_bot);
    }

    #[test]
    fn test_parse_empty_ua() {
        let parsed = parse_user_agent("");
        assert!(parsed.browser.is_none());
        assert!(parsed.os.is_none());
        assert!(!parsed.is_bot);
    }

    #[test]
    fn test_parse_unknown_ua() {
        let parsed = parse_user_agent("SomeRandom/1.0");
        assert!(parsed.browser.is_none());
        assert!(parsed.os.is_none());
        assert!(!parsed.is_bot);
    }

    // Bot detection tests
    #[test]
    fn test_detect_googlebot() {
        let ua = "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";
        let parsed = parse_user_agent(ua);
        assert!(parsed.is_bot);
    }

    #[test]
    fn test_detect_bingbot() {
        let ua = "Mozilla/5.0 (compatible; bingbot/2.0; +http://www.bing.com/bingbot.htm)";
        let parsed = parse_user_agent(ua);
        assert!(parsed.is_bot);
    }

    #[test]
    fn test_detect_curl() {
        let parsed = parse_user_agent("curl/7.68.0");
        assert!(parsed.is_bot);
    }

    #[test]
    fn test_detect_python_requests() {
        let parsed = parse_user_agent("python-requests/2.28.1");
        assert!(parsed.is_bot);
    }

    #[test]
    fn test_detect_slackbot() {
        let parsed = parse_user_agent("Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)");
        assert!(parsed.is_bot);
    }

    #[test]
    fn test_detect_facebookbot() {
        let parsed = parse_user_agent(
            "facebookexternalhit/1.1 (+http://www.facebook.com/externalhit_uatext.php)",
        );
        assert!(parsed.is_bot);
    }

    // Additional browser detection tests
    #[test]
    fn test_parse_samsung_internet() {
        let ua = "Mozilla/5.0 (Linux; Android 13; SM-S901B) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/23.0 Chrome/115.0.0.0 Mobile Safari/537.36";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Samsung Internet"));
        assert_eq!(parsed.os.as_deref(), Some("Android"));
        assert!(!parsed.is_bot);
    }

    #[test]
    fn test_parse_vivaldi() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Vivaldi/6.5.3206.55";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Vivaldi"));
        assert!(!parsed.is_bot);
    }

    #[test]
    fn test_iphone_mac_os_x_edge_case() {
        // L7: iPhone UA strings contain "Mac OS X" — must detect iOS, not macOS
        let ua = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2_1 like Mac OS X) AppleWebKit/605.1.15";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.os.as_deref(), Some("iOS"));
        assert_ne!(parsed.os.as_deref(), Some("macOS"));
    }
}
