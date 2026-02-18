use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

/// Geographic information resolved from an IP address.
#[derive(Debug, Clone, Default)]
pub struct GeoInfo {
    pub country_code: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
}

/// Thread-safe wrapper around the MaxMind GeoLite2 database reader.
/// When the database is not available, all lookups return `GeoInfo::default()`.
pub struct GeoIpReader {
    reader: Option<Arc<maxminddb::Reader<Vec<u8>>>>,
}

impl GeoIpReader {
    /// Open a MaxMind .mmdb database file.
    ///
    /// Returns a reader that gracefully degrades: if the path is `None`,
    /// the file doesn't exist, or it fails to open, all lookups return `None`.
    pub fn open(path: Option<&Path>) -> Self {
        let reader = path.and_then(|p| {
            if !p.exists() {
                tracing::warn!(path = %p.display(), "GeoIP database not found, geolocation disabled");
                return None;
            }
            match maxminddb::Reader::open_readfile(p) {
                Ok(r) => {
                    tracing::info!(path = %p.display(), "GeoIP database loaded");
                    Some(Arc::new(r))
                }
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e, "Failed to open GeoIP database, geolocation disabled");
                    None
                }
            }
        });
        Self { reader }
    }

    /// Returns `true` if a GeoIP database is loaded.
    pub const fn is_loaded(&self) -> bool {
        self.reader.is_some()
    }

    /// Look up geographic information for an IP address.
    ///
    /// PRIVACY: The IP address is passed by reference, used only for the lookup,
    /// and never stored or logged. Only the resolved geographic fields are returned.
    pub fn lookup(&self, ip: &str) -> GeoInfo {
        let Some(reader) = &self.reader else {
            return GeoInfo::default();
        };

        let Ok(addr) = ip.parse::<IpAddr>() else {
            return GeoInfo::default();
        };

        let Ok(lookup_result) = reader.lookup(addr) else {
            return GeoInfo::default();
        };

        let Ok(Some(city)) = lookup_result.decode::<maxminddb::geoip2::City>() else {
            return GeoInfo::default();
        };

        let country_code = city.country.iso_code.map(String::from);

        let region = city
            .subdivisions
            .first()
            .and_then(|s| s.names.english)
            .map(String::from);

        let city_name = city.city.names.english.map(String::from);

        GeoInfo {
            country_code,
            region,
            city: city_name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_returns_none_without_reader() {
        let reader = GeoIpReader::open(None);
        let info = reader.lookup("192.168.1.1");
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

    #[test]
    fn test_reader_no_db_path() {
        let reader = GeoIpReader::open(None);
        let info = reader.lookup("8.8.8.8");
        assert!(info.country_code.is_none());
        assert!(info.region.is_none());
        assert!(info.city.is_none());
    }

    #[test]
    fn test_reader_missing_db_file() {
        let reader = GeoIpReader::open(Some(Path::new("/nonexistent/GeoLite2.mmdb")));
        let info = reader.lookup("8.8.8.8");
        assert!(info.country_code.is_none());
    }

    #[test]
    fn test_reader_invalid_ip() {
        let reader = GeoIpReader::open(None);
        let info = reader.lookup("not-an-ip");
        assert!(info.country_code.is_none());
    }

    #[test]
    fn test_reader_empty_ip() {
        let reader = GeoIpReader::open(None);
        let info = reader.lookup("");
        assert!(info.country_code.is_none());
    }

    #[test]
    fn test_is_loaded_without_db() {
        let reader = GeoIpReader::open(None);
        assert!(!reader.is_loaded());
    }

    #[test]
    fn test_is_loaded_with_missing_file() {
        let reader = GeoIpReader::open(Some(Path::new("/nonexistent/GeoLite2.mmdb")));
        assert!(!reader.is_loaded());
    }
}
