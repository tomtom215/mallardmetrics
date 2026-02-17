/// GeoIP lookup stub for Phase 1.
///
/// Phase 4 will integrate MaxMind GeoLite2 for IP → country/city resolution.
/// For now, this module provides the interface that the ingestion handler
/// will use, returning `None` for all lookups.
/// Geographic information resolved from an IP address.
#[derive(Debug, Clone, Default)]
pub struct GeoInfo {
    pub country_code: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
}

/// Look up geographic information for an IP address.
///
/// Returns `GeoInfo` with all fields `None` in Phase 1.
/// Will be implemented with MaxMind GeoLite2 in Phase 4.
pub fn lookup(_ip: &str) -> GeoInfo {
    GeoInfo::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_returns_none() {
        let info = lookup("192.168.1.1");
        assert!(info.country_code.is_none());
        assert!(info.region.is_none());
        assert!(info.city.is_none());
    }

    #[test]
    fn test_geoinfo_default() {
        let info = GeoInfo::default();
        assert!(info.country_code.is_none());
        assert!(info.region.is_none());
        assert!(info.city.is_none());
    }
}
