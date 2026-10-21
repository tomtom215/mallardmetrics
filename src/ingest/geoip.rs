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

    // ── A real database ──────────────────────────────────────────────────
    //
    // Every test above runs without a database, so the decode path (`City`
    // fields, the English-name choice, the empty-record case) was never
    // exercised and a crate upgrade could break it silently. This writes a
    // tiny but genuine MaxMind DB: one IPv4 address mapped to one record.

    /// MMDB string: type 2, length below 29.
    fn mm_str(s: &str) -> Vec<u8> {
        let mut out = vec![0x40 | u8::try_from(s.len()).unwrap()];
        out.extend_from_slice(s.as_bytes());
        out
    }

    /// MMDB map: type 7, fewer than 29 pairs.
    fn mm_map(pairs: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = vec![0xE0 | u8::try_from(pairs.len()).unwrap()];
        for (key, value) in pairs {
            out.extend(mm_str(key));
            out.extend(value);
        }
        out
    }

    /// MMDB array (extended type 11), fewer than 29 items.
    fn mm_array(items: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![u8::try_from(items.len()).unwrap(), 4];
        for item in items {
            out.extend(item);
        }
        out
    }

    /// MMDB unsigned integer of `kind` (5 = u16, 6 = u32, 9 = u64).
    fn mm_uint(kind: u8, value: u64) -> Vec<u8> {
        let bytes: Vec<u8> = value
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        let size = u8::try_from(bytes.len()).unwrap();
        let mut out = if kind > 7 {
            vec![size, kind - 7]
        } else {
            vec![(kind << 5) | size]
        };
        out.extend(bytes);
        out
    }

    /// A City database in which only `address` has a record.
    fn tiny_city_database(address: [u8; 4]) -> Vec<u8> {
        const NODES: u32 = 32;
        let record = mm_map(&[
            (
                "city",
                mm_map(&[("names", mm_map(&[("en", mm_str("Berlin"))]))]),
            ),
            ("country", mm_map(&[("iso_code", mm_str("DE"))])),
            (
                "subdivisions",
                mm_array(&[mm_map(&[(
                    "names",
                    mm_map(&[("en", mm_str("Land Berlin"))]),
                )])]),
            ),
        ]);

        // One node per address bit; the record off the path means "no data".
        let bits = u32::from_be_bytes(address);
        let mut tree = Vec::new();
        for depth in 0..NODES {
            let on_path = if depth + 1 < NODES {
                depth + 1
            } else {
                NODES + 16 // first byte of the data section
            };
            let bit = (bits >> (31 - depth)) & 1;
            let children = if bit == 0 {
                [on_path, NODES]
            } else {
                [NODES, on_path]
            };
            for record in children {
                tree.extend_from_slice(&record.to_be_bytes()[1..]);
            }
        }

        let metadata = mm_map(&[
            ("node_count", mm_uint(6, u64::from(NODES))),
            ("record_size", mm_uint(5, 24)),
            ("ip_version", mm_uint(5, 4)),
            ("database_type", mm_str("GeoLite2-City")),
            ("languages", mm_array(&[mm_str("en")])),
            ("binary_format_major_version", mm_uint(5, 2)),
            ("binary_format_minor_version", mm_uint(5, 0)),
            ("build_epoch", mm_uint(9, 1_700_000_000)),
            ("description", mm_map(&[("en", mm_str("test"))])),
        ]);

        let mut file = tree;
        file.extend([0u8; 16]);
        file.extend(record);
        file.extend(b"\xAB\xCD\xEFMaxMind.com");
        file.extend(metadata);
        file
    }

    #[test]
    fn test_lookup_decodes_a_real_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.mmdb");
        std::fs::write(&path, tiny_city_database([81, 2, 69, 142])).unwrap();

        let reader = GeoIpReader::open(Some(&path));
        assert!(reader.is_loaded());

        let hit = reader.lookup("81.2.69.142");
        assert_eq!(hit.country_code.as_deref(), Some("DE"));
        assert_eq!(hit.region.as_deref(), Some("Land Berlin"));
        assert_eq!(hit.city.as_deref(), Some("Berlin"));

        // An address with no record, one beside it, and text that is no address
        // all degrade to an empty result rather than failing the request.
        for miss in ["8.8.8.8", "81.2.69.143", "2001:db8::1", "nonsense", ""] {
            let info = reader.lookup(miss);
            assert!(info.country_code.is_none(), "{miss:?}");
            assert!(info.region.is_none() && info.city.is_none(), "{miss:?}");
        }
    }

    #[test]
    fn test_a_corrupt_database_disables_geolocation_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.mmdb");
        let mut bytes = tiny_city_database([81, 2, 69, 142]);
        bytes.truncate(bytes.len() / 2);
        std::fs::write(&path, bytes).unwrap();

        let reader = GeoIpReader::open(Some(&path));
        assert!(!reader.is_loaded());
        assert!(reader.lookup("81.2.69.142").country_code.is_none());
    }
}
