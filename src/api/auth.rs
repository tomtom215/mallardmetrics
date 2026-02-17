/// Authentication middleware stub for Phase 1.
///
/// Phase 4 will implement full authentication with username/password (bcrypt/argon2)
/// and API key management.
///
/// For Phase 1, the dashboard is open (no auth required).
/// The ingestion endpoint uses origin checking only.
/// Validate that the request origin is allowed for event ingestion.
pub fn validate_origin(origin: Option<&str>, allowed_sites: &[String]) -> bool {
    if allowed_sites.is_empty() {
        return true; // No restrictions configured
    }

    origin.is_none_or(|origin| {
        let host = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))
            .unwrap_or(origin);
        allowed_sites.iter().any(|s| host.starts_with(s.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_origin_no_restrictions() {
        assert!(validate_origin(Some("https://anything.com"), &[]));
    }

    #[test]
    fn test_validate_origin_allowed() {
        let sites = vec!["example.com".to_string()];
        assert!(validate_origin(Some("https://example.com"), &sites));
    }

    #[test]
    fn test_validate_origin_not_allowed() {
        let sites = vec!["example.com".to_string()];
        assert!(!validate_origin(Some("https://evil.com"), &sites));
    }

    #[test]
    fn test_validate_origin_no_header() {
        let sites = vec!["example.com".to_string()];
        assert!(validate_origin(None, &sites));
    }

    #[test]
    fn test_validate_origin_http() {
        let sites = vec!["example.com".to_string()];
        assert!(validate_origin(Some("http://example.com"), &sites));
    }
}
