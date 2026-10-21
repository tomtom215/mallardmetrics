//! Optional datacenter / hosting traffic filter.
//!
//! When an ASN database is configured (`asn_db_path`), an event whose client IP
//! resolves to an autonomous system on the hosting list is dropped exactly like
//! bot traffic: answered `202`, never stored, and counted in
//! `mallard_datacenter_filtered_total`.
//!
//! PRIVACY: the address is used transiently, as for GeoIP. It is parsed, looked
//! up in the in-memory database, and discarded. Nothing here stores it,
//! logs it or puts it in an error. The only state kept is one counter.

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Autonomous systems of well-known hosting and cloud providers.
///
/// Each entry was checked on 2026-10-21 against PeeringDB
/// (`https://www.peeringdb.com/api/net?asn__in=...`) and, for the two where
/// PeeringDB carries no type, against the ARIN registration
/// (`https://rdap.arin.net/registry/autnum/<asn>`). The list is deliberately
/// short and conservative: an entry must be an AS whose business is renting
/// compute, and whose address space is not also used for human browsing.
///
/// Deliberately NOT listed, because they carry real people as well as servers:
/// AS8075 (Microsoft: Azure, but also Microsoft's corporate network and
/// consumer services), AS15169 (Google LLC: cloud-adjacent crawlers, but also
/// corporate offices and Google-operated proxies), AS45102 (Alibaba),
/// residential ISPs, mobile carriers and consumer-VPN providers.
pub const DEFAULT_DATACENTER_ASNS: &[u32] = &[
    16509,   // Amazon.com, Inc. (Amazon Web Services)
    14618,   // Amazon.com, Inc. (AMAZON-AES, EC2 us-east-1)
    396_982, // Google LLC (Google Cloud Platform)
    14061,   // DigitalOcean, LLC
    24940,   // Hetzner Online GmbH
    16276,   // OVHcloud
    63949,   // Linode, LLC (Akamai Connected Cloud)
    20473,   // The Constant Company, LLC (Vultr / Choopa)
    12876,   // Scaleway (Online S.A.S.)
    51167,   // Contabo GmbH
    31898,   // Oracle Corporation (Oracle Cloud Infrastructure)
];

/// The datacenter filter: an ASN reader, the ASN set, and the drop counter.
pub struct DatacenterFilter {
    reader: Option<maxminddb::Reader<Vec<u8>>>,
    asns: HashSet<u32>,
    /// Events dropped by this filter since startup.
    pub filtered_total: AtomicU64,
}

impl DatacenterFilter {
    /// A filter that never matches. The default: the feature is opt-in.
    pub fn disabled() -> Self {
        Self {
            reader: None,
            asns: HashSet::new(),
            filtered_total: AtomicU64::new(0),
        }
    }

    /// Open `db_path` and filter on `asns` (the built-in list when empty).
    ///
    /// Unlike GeoIP, a database that cannot be used is an error rather than a
    /// silent downgrade: an operator who turned filtering on would otherwise
    /// believe traffic was being filtered when none was.
    ///
    /// # Errors
    ///
    /// Returns a description when the file cannot be read, is not a MaxMind
    /// database, or is not an ASN database (a City database decodes without
    /// error and yields no ASN for any address).
    pub fn open(db_path: &Path, asns: &[u32]) -> Result<Self, String> {
        let reader = maxminddb::Reader::open_readfile(db_path)
            .map_err(|e| format!("cannot open the ASN database {}: {e}", db_path.display()))?;
        // The crate exposes the metadata only through `Debug`.
        if !format!("{reader:?}").contains("ASN") {
            return Err(format!(
                "{} is not an ASN database (expected GeoLite2-ASN or DB-IP ASN lite)",
                db_path.display()
            ));
        }
        let asns = if asns.is_empty() {
            DEFAULT_DATACENTER_ASNS
        } else {
            asns
        };
        Ok(Self {
            reader: Some(reader),
            asns: asns.iter().copied().collect(),
            filtered_total: AtomicU64::new(0),
        })
    }

    /// Whether filtering is active.
    pub const fn is_enabled(&self) -> bool {
        self.reader.is_some()
    }

    /// Number of ASNs on the list (0 when disabled).
    pub fn asn_count(&self) -> usize {
        self.asns.len()
    }

    /// Does `ip` belong to a listed hosting ASN?
    ///
    /// Anything that cannot be resolved (disabled, unparsable text, an address
    /// absent from the database, a decode error, an IPv6 address in an IPv4-only
    /// database) is "no": the filter only ever drops on positive evidence.
    pub fn is_datacenter(&self, ip: &str) -> bool {
        let Some(reader) = &self.reader else {
            return false;
        };
        let Ok(addr) = ip.parse::<IpAddr>() else {
            return false;
        };
        let Ok(result) = reader.lookup(addr.to_canonical()) else {
            return false;
        };
        let Ok(Some(record)) = result.decode::<maxminddb::geoip2::Asn>() else {
            return false;
        };
        record
            .autonomous_system_number
            .is_some_and(|asn| self.asns.contains(&asn))
    }

    /// Count one dropped event.
    pub fn record(&self) {
        self.filtered_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Events dropped since startup.
    pub fn filtered(&self) -> u64 {
        self.filtered_total.load(Ordering::Relaxed)
    }
}

/// Parse `"16509, AS14061"` style lists. Returns the numbers and the tokens
/// that were not an ASN.
pub fn parse_asn_list(raw: &str) -> (Vec<u32>, Vec<String>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let digits = token
            .strip_prefix("AS")
            .or_else(|| token.strip_prefix("as"))
            .unwrap_or(token);
        match digits.parse::<u32>() {
            Ok(n) if n > 0 => ok.push(n),
            _ => bad.push(token.to_string()),
        }
    }
    (ok, bad)
}

/// A genuine, tiny MaxMind DB writer for tests (see `geoip.rs` for the
/// single-record City variant this generalises). Never part of a release build.
#[cfg(any(test, feature = "testing"))]
#[allow(clippy::missing_panics_doc, clippy::must_use_candidate)]
pub mod test_db {

    /// String of up to 284 bytes (sizes of 29 and over use an extra byte).
    fn mm_str(s: &str) -> Vec<u8> {
        let mut out = if s.len() < 29 {
            vec![0x40 | u8::try_from(s.len()).unwrap()]
        } else {
            vec![0x5D, u8::try_from(s.len() - 29).unwrap()]
        };
        out.extend_from_slice(s.as_bytes());
        out
    }

    fn mm_map(pairs: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = vec![0xE0 | u8::try_from(pairs.len()).unwrap()];
        for (key, value) in pairs {
            out.extend(mm_str(key));
            out.extend(value);
        }
        out
    }

    fn mm_array(items: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![u8::try_from(items.len()).unwrap(), 4];
        for item in items {
            out.extend(item);
        }
        out
    }

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

    /// Bits of an address, most significant first.
    pub fn v4_bits(a: [u8; 4], prefix: usize) -> Vec<bool> {
        let n = u32::from_be_bytes(a);
        (0..prefix).map(|i| (n >> (31 - i)) & 1 == 1).collect()
    }

    /// An IPv4 address inside an IPv6 tree: 96 zero bits, then the IPv4 bits.
    pub fn v4_in_v6_bits(a: [u8; 4], prefix: usize) -> Vec<bool> {
        let mut bits = vec![false; 96];
        bits.extend(v4_bits(a, prefix));
        bits
    }

    pub fn v6_bits(a: std::net::Ipv6Addr, prefix: usize) -> Vec<bool> {
        let n = u128::from_be_bytes(a.octets());
        (0..prefix).map(|i| (n >> (127 - i)) & 1 == 1).collect()
    }

    #[derive(Clone, Copy)]
    enum Child {
        Empty,
        Node(usize),
        Data(usize),
    }

    /// Build a database of `database_type` with `ip_version` 4 or 6, mapping each
    /// bit path to an ASN record.
    pub fn asn_database(
        database_type: &str,
        ip_version: u8,
        entries: &[(Vec<bool>, u32)],
    ) -> Vec<u8> {
        let mut data = Vec::new();
        let mut offsets = Vec::new();
        for (_, asn) in entries {
            offsets.push(data.len());
            data.extend(mm_map(&[
                ("autonomous_system_number", mm_uint(6, u64::from(*asn))),
                (
                    "autonomous_system_organization",
                    mm_str("Test Hosting Organization Inc."),
                ),
            ]));
        }

        let mut nodes: Vec<[Child; 2]> = vec![[Child::Empty; 2]];
        for (index, (bits, _)) in entries.iter().enumerate() {
            let mut at = 0;
            for (depth, bit) in bits.iter().enumerate() {
                let side = usize::from(*bit);
                if depth + 1 == bits.len() {
                    nodes[at][side] = Child::Data(offsets[index]);
                } else {
                    at = if let Child::Node(next) = nodes[at][side] {
                        next
                    } else {
                        nodes.push([Child::Empty; 2]);
                        let next = nodes.len() - 1;
                        nodes[at][side] = Child::Node(next);
                        next
                    };
                }
            }
        }

        let count = nodes.len();
        let mut tree = Vec::new();
        for pair in &nodes {
            for child in pair {
                let value = match child {
                    Child::Empty => count,
                    Child::Node(n) => *n,
                    Child::Data(offset) => count + 16 + offset,
                };
                tree.extend_from_slice(&u32::try_from(value).unwrap().to_be_bytes()[1..]);
            }
        }

        let metadata = mm_map(&[
            ("node_count", mm_uint(6, count as u64)),
            ("record_size", mm_uint(5, 24)),
            ("ip_version", mm_uint(5, u64::from(ip_version))),
            ("database_type", mm_str(database_type)),
            ("languages", mm_array(&[mm_str("en")])),
            ("binary_format_major_version", mm_uint(5, 2)),
            ("binary_format_minor_version", mm_uint(5, 0)),
            ("build_epoch", mm_uint(9, 1_700_000_000)),
            ("description", mm_map(&[("en", mm_str("test"))])),
        ]);

        let mut file = tree;
        file.extend([0u8; 16]);
        file.extend(data);
        file.extend(b"\xAB\xCD\xEFMaxMind.com");
        file.extend(metadata);
        file
    }
}

#[cfg(test)]
mod tests {
    use super::test_db::*;
    use super::*;

    const AWS: [u8; 4] = [3, 5, 140, 2];
    const HOME: [u8; 4] = [81, 2, 69, 142];

    fn v4_database() -> Vec<u8> {
        asn_database(
            "GeoLite2-ASN",
            4,
            &[
                (v4_bits(AWS, 32), 16509),
                (v4_bits(HOME, 32), 3320), // Deutsche Telekom: residential
            ],
        )
    }

    fn write(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asn.mmdb");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    fn open(bytes: &[u8], asns: &[u32]) -> (tempfile::TempDir, DatacenterFilter) {
        let (dir, path) = write(bytes);
        let filter = DatacenterFilter::open(&path, asns).unwrap();
        (dir, filter)
    }

    #[test]
    fn test_default_list_is_unique_and_excludes_ambiguous_asns() {
        let unique: HashSet<_> = DEFAULT_DATACENTER_ASNS.iter().collect();
        assert_eq!(unique.len(), DEFAULT_DATACENTER_ASNS.len());
        assert_eq!(DEFAULT_DATACENTER_ASNS.len(), 11);
        // The ambiguous ones must stay out (see the doc comment).
        for excluded in [8075, 15169, 45102] {
            assert!(!DEFAULT_DATACENTER_ASNS.contains(&excluded));
        }
    }

    #[test]
    fn test_disabled_filter_never_matches() {
        let filter = DatacenterFilter::disabled();
        assert!(!filter.is_enabled());
        assert!(!filter.is_datacenter("3.5.140.2"));
        assert_eq!(filter.asn_count(), 0);
    }

    #[test]
    fn test_hit_and_miss_ipv4() {
        let (_dir, filter) = open(&v4_database(), &[]);
        assert!(filter.is_enabled());
        assert!(filter.is_datacenter("3.5.140.2"), "AWS address");
        assert!(!filter.is_datacenter("81.2.69.142"), "residential ASN");
        assert!(!filter.is_datacenter("8.8.8.8"), "absent from the database");
        for junk in ["", "nonsense", "3.5.140", "unknown"] {
            assert!(!filter.is_datacenter(junk), "{junk:?}");
        }
        // IPv6 against an IPv4-only database is a miss, not a failure.
        assert!(!filter.is_datacenter("2001:db8::1"));
    }

    #[test]
    fn test_ipv6_and_ipv4_mapped_in_a_v6_database() {
        let v6: std::net::Ipv6Addr = "2a01:4f8::".parse().unwrap();
        let bytes = asn_database(
            "DBIP-ASN-Lite (compat=GeoLite2-ASN)",
            6,
            &[
                (v6_bits(v6, 32), 24940),        // Hetzner-style /32
                (v4_in_v6_bits(AWS, 32), 16509), // IPv4 inside the v6 tree
                (v4_in_v6_bits(HOME, 32), 3320),
            ],
        );
        let (_dir, filter) = open(&bytes, &[]);
        assert!(filter.is_datacenter("2a01:4f8::1234"));
        assert!(!filter.is_datacenter("2a01:4f9::1"));
        assert!(filter.is_datacenter("3.5.140.2"));
        assert!(
            filter.is_datacenter("::ffff:3.5.140.2"),
            "an IPv4-mapped address is the IPv4 address"
        );
        assert!(!filter.is_datacenter("::ffff:81.2.69.142"));
    }

    #[test]
    fn test_custom_list_replaces_the_default() {
        // Only the residential ASN is listed, so the AWS address passes.
        let (_dir, filter) = open(&v4_database(), &[3320]);
        assert_eq!(filter.asn_count(), 1);
        assert!(filter.is_datacenter("81.2.69.142"));
        assert!(!filter.is_datacenter("3.5.140.2"));
    }

    #[test]
    fn test_missing_corrupt_and_wrong_type_databases_are_errors() {
        let err = DatacenterFilter::open(Path::new("/nonexistent/asn.mmdb"), &[])
            .err()
            .unwrap();
        assert!(err.contains("cannot open"), "{err}");

        let mut bytes = v4_database();
        bytes.truncate(bytes.len() / 2);
        let (_dir, path) = write(&bytes);
        assert!(DatacenterFilter::open(&path, &[]).is_err());

        let city = asn_database("GeoLite2-City", 4, &[(v4_bits(AWS, 32), 16509)]);
        let (_dir2, path2) = write(&city);
        let err = DatacenterFilter::open(&path2, &[]).err().unwrap();
        assert!(err.contains("not an ASN database"), "{err}");
    }

    #[test]
    fn test_error_messages_never_contain_an_address() {
        // Errors name the path and the database, never a visitor.
        let err = DatacenterFilter::open(Path::new("/nonexistent/asn.mmdb"), &[])
            .err()
            .unwrap();
        assert!(!err.contains("3.5.140.2"));
    }

    #[test]
    fn test_counter() {
        let filter = DatacenterFilter::disabled();
        assert_eq!(filter.filtered(), 0);
        filter.record();
        filter.record();
        assert_eq!(filter.filtered(), 2);
    }

    #[test]
    fn test_parse_asn_list() {
        let (ok, bad) = parse_asn_list("16509, AS14061 ,as24940,,0,abc,-1,99999999999");
        assert_eq!(ok, vec![16509, 14061, 24940]);
        assert_eq!(bad, vec!["0", "abc", "-1", "99999999999"]);
        assert_eq!(parse_asn_list(""), (vec![], vec![]));
    }
}
