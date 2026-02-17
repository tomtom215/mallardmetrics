/// Minimal User-Agent parser for browser and OS detection.
///
/// Phase 1 uses simple string matching. Phase 4 will integrate a full UA parser.
/// Parsed User-Agent information.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct ParsedUserAgent {
    pub browser: Option<String>,
    pub browser_version: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
}

/// Parse a User-Agent string into browser and OS components.
#[allow(dead_code)]
pub fn parse_user_agent(ua: &str) -> ParsedUserAgent {
    ParsedUserAgent {
        browser: detect_browser(ua),
        browser_version: detect_browser_version(ua),
        os: detect_os(ua),
        os_version: detect_os_version(ua),
    }
}

#[allow(dead_code)]
fn detect_browser(ua: &str) -> Option<String> {
    // Order matters: check more specific patterns first
    if ua.contains("Edg/") || ua.contains("Edge/") {
        Some("Edge".to_string())
    } else if ua.contains("OPR/") || ua.contains("Opera") {
        Some("Opera".to_string())
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

#[allow(dead_code)]
fn detect_browser_version(ua: &str) -> Option<String> {
    let patterns = [
        ("Edg/", "Edg/"),
        ("Edge/", "Edge/"),
        ("OPR/", "OPR/"),
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

#[allow(dead_code)]
fn detect_os(ua: &str) -> Option<String> {
    if ua.contains("Windows") {
        Some("Windows".to_string())
    } else if ua.contains("iPhone") || ua.contains("iPad") || ua.contains("iOS") {
        // Check iOS before macOS since iPhone UAs contain "Mac OS X"
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

#[allow(dead_code)]
fn detect_os_version(ua: &str) -> Option<String> {
    if ua.contains("Windows NT") {
        extract_version_after(ua, "Windows NT ")
    } else if ua.contains("iPhone OS") {
        // Check iPhone OS before Mac OS X since iPhone UAs contain both
        extract_version_after(ua, "iPhone OS ").map(|v| v.replace('_', "."))
    } else if ua.contains("Mac OS X") {
        extract_version_after(ua, "Mac OS X ").map(|v| v.replace('_', "."))
    } else if ua.contains("Android") {
        extract_version_after(ua, "Android ")
    } else {
        None
    }
}

#[allow(dead_code)]
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
    }

    #[test]
    fn test_parse_firefox_linux() {
        let ua = "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Firefox"));
        assert_eq!(parsed.browser_version.as_deref(), Some("121.0"));
        assert_eq!(parsed.os.as_deref(), Some("Linux"));
    }

    #[test]
    fn test_parse_safari_macos() {
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Safari/605.1.15";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Safari"));
        assert_eq!(parsed.os.as_deref(), Some("macOS"));
        assert_eq!(parsed.os_version.as_deref(), Some("10.15.7"));
    }

    #[test]
    fn test_parse_edge() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.2210.91";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Edge"));
        assert_eq!(parsed.browser_version.as_deref(), Some("120.0.2210.91"));
    }

    #[test]
    fn test_parse_android() {
        let ua = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.6099.144 Mobile Safari/537.36";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Chrome"));
        assert_eq!(parsed.os.as_deref(), Some("Android"));
        assert_eq!(parsed.os_version.as_deref(), Some("14"));
    }

    #[test]
    fn test_parse_iphone() {
        let ua = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1";
        let parsed = parse_user_agent(ua);
        assert_eq!(parsed.browser.as_deref(), Some("Safari"));
        assert_eq!(parsed.os.as_deref(), Some("iOS"));
        assert_eq!(parsed.os_version.as_deref(), Some("17.2.1"));
    }

    #[test]
    fn test_parse_empty_ua() {
        let parsed = parse_user_agent("");
        assert!(parsed.browser.is_none());
        assert!(parsed.os.is_none());
    }

    #[test]
    fn test_parse_unknown_ua() {
        let parsed = parse_user_agent("SomeBot/1.0");
        assert!(parsed.browser.is_none());
        assert!(parsed.os.is_none());
    }
}
