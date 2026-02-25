use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Generates a privacy-safe visitor ID by computing HMAC-SHA256(IP || UA, daily_salt).
///
/// The resulting hash is deterministic for the same inputs within the same day,
/// but changes when the daily salt rotates. IP addresses are never stored.
pub fn generate_visitor_id(ip: &str, user_agent: &str, daily_salt: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(daily_salt.as_bytes()).expect("HMAC accepts any key length");
    mac.update(ip.as_bytes());
    mac.update(b"|");
    mac.update(user_agent.as_bytes());
    let result = mac.finalize();
    hex::encode(result.into_bytes())
}

/// Generates the daily salt for a given date.
///
/// In production, this should use a persistent secret combined with the date.
/// The secret should be loaded from configuration, not hardcoded.
pub fn daily_salt(secret: &str, date: chrono::NaiveDate) -> String {
    let input = format!("{secret}:{date}");
    let mut mac =
        HmacSha256::new_from_slice(b"mallard-metrics-salt").expect("HMAC accepts any key length");
    mac.update(input.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn test_deterministic_visitor_id() {
        let id1 = generate_visitor_id("192.168.1.1", "Mozilla/5.0", "salt-2024-01-15");
        let id2 = generate_visitor_id("192.168.1.1", "Mozilla/5.0", "salt-2024-01-15");
        assert_eq!(id1, id2, "Same inputs must produce same visitor ID");
    }

    #[test]
    fn test_different_ip_different_id() {
        let id1 = generate_visitor_id("192.168.1.1", "Mozilla/5.0", "salt");
        let id2 = generate_visitor_id("192.168.1.2", "Mozilla/5.0", "salt");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_different_ua_different_id() {
        let id1 = generate_visitor_id("192.168.1.1", "Mozilla/5.0", "salt");
        let id2 = generate_visitor_id("192.168.1.1", "Chrome/120.0", "salt");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_different_salt_different_id() {
        let id1 = generate_visitor_id("192.168.1.1", "Mozilla/5.0", "salt-day1");
        let id2 = generate_visitor_id("192.168.1.1", "Mozilla/5.0", "salt-day2");
        assert_ne!(id1, id2, "Different daily salts must produce different IDs");
    }

    #[test]
    fn test_visitor_id_is_hex_encoded() {
        let id = generate_visitor_id("1.2.3.4", "UA", "salt");
        assert_eq!(id.len(), 64, "SHA-256 hex output is 64 chars");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_empty_inputs() {
        let id = generate_visitor_id("", "", "");
        assert_eq!(id.len(), 64);
    }

    #[test]
    fn test_daily_salt_deterministic() {
        let date = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let s1 = daily_salt("secret", date);
        let s2 = daily_salt("secret", date);
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_daily_salt_changes_by_date() {
        let d1 = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let s1 = daily_salt("secret", d1);
        let s2 = daily_salt("secret", d2);
        assert_ne!(s1, s2);
    }

    #[test]
    fn test_daily_salt_changes_by_secret() {
        let date = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let s1 = daily_salt("secret1", date);
        let s2 = daily_salt("secret2", date);
        assert_ne!(s1, s2);
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use chrono::{Duration, NaiveDate};
    use proptest::prelude::*;

    proptest! {
        /// Determinism: identical inputs always yield the same visitor ID.
        #[test]
        fn prop_visitor_id_deterministic(
            ip in "[0-9a-z.]{1,20}",
            ua in "[A-Za-z0-9]{1,50}",
            salt in "[A-Za-z0-9]{1,30}",
        ) {
            let id1 = generate_visitor_id(&ip, &ua, &salt);
            let id2 = generate_visitor_id(&ip, &ua, &salt);
            prop_assert_eq!(id1, id2);
        }

        /// Uniqueness: distinct IP addresses (same UA and salt) produce distinct visitor IDs.
        ///
        /// Uses non-overlapping suffix ranges to guarantee the two IPs are always different.
        #[test]
        fn prop_visitor_id_unique_per_ip(
            suffix_a in 0u8..128u8,
            suffix_b in 128u8..=255u8,
            ua in "[A-Za-z0-9]{1,20}",
            salt in "[A-Za-z0-9]{1,20}",
        ) {
            let ip_a = format!("10.0.0.{suffix_a}");
            let ip_b = format!("10.0.0.{suffix_b}");
            let id_a = generate_visitor_id(&ip_a, &ua, &salt);
            let id_b = generate_visitor_id(&ip_b, &ua, &salt);
            prop_assert_ne!(id_a, id_b);
        }

        /// Daily rotation: the same IP+UA always yields a different salt on different days.
        ///
        /// day_a is in [0, 180) and day_b is in [180, 360), so they always differ.
        #[test]
        fn prop_daily_salt_changes_per_day(
            secret in "[A-Za-z0-9]{1,20}",
            day_a in 0u32..180u32,
            day_b in 180u32..360u32,
        ) {
            let base = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
            let d_a = base + Duration::days(i64::from(day_a));
            let d_b = base + Duration::days(i64::from(day_b));
            let s_a = daily_salt(&secret, d_a);
            let s_b = daily_salt(&secret, d_b);
            prop_assert_ne!(s_a, s_b);
        }
    }
}
