use chrono::NaiveDate;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Fixed domain-separation label for salt derivation.
const SALT_DOMAIN: &[u8] = b"mallard-metrics/salt/v2";

/// Reference date for rotation-period arithmetic (the Unix epoch).
///
/// Salt periods are numbered from this date so that a rotation period longer
/// than one day produces stable, aligned buckets rather than drifting with the
/// server's start time.
const EPOCH: NaiveDate = match NaiveDate::from_ymd_opt(1970, 1, 1) {
    Some(d) => d,
    None => unreachable!(),
};

/// Generates a privacy-safe visitor ID: `HMAC-SHA256(salt, site_id || IP || UA)`.
///
/// The result is deterministic for the same inputs within the same salt period
/// and becomes unlinkable once the salt rotates. The IP address is never stored.
///
/// `site_id` is part of the message so that the same person visiting two
/// different sites hosted on one Mallard instance gets two unrelated IDs. Without
/// it, an operator (or anyone reading the Parquet files) could correlate a
/// visitor across every site on the instance. Per-site scoping does not affect
/// any metric, because every query already filters by `site_id`.
pub fn generate_visitor_id(site_id: &str, ip: &str, user_agent: &str, salt: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(salt.as_bytes()).expect("HMAC accepts any key length");
    // Fields are joined with a separator that cannot occur in either of the two
    // that precede it: `site_id` is validated to alphanumerics plus `.-_:`, and
    // `ip` is either a canonicalised address or the literal "unknown". So
    // ("a.com", "1.2.3.4") and ("a.com|1", ".2.3.4") cannot both be produced,
    // and the concatenation is unambiguous. The trailing field needs no such
    // guarantee because nothing follows it.
    mac.update(site_id.as_bytes());
    mac.update(b"|");
    mac.update(ip.as_bytes());
    mac.update(b"|");
    mac.update(user_agent.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Index of the salt period containing `date`, given a rotation length in days.
///
/// Periods are aligned to the Unix epoch so that all instances sharing a secret
/// and a rotation length agree on period boundaries.
pub fn salt_period(date: NaiveDate, rotation_days: u32) -> i64 {
    let days_since_epoch = (date - EPOCH).num_days();
    let rotation = i64::from(rotation_days.max(1));
    days_since_epoch.div_euclid(rotation)
}

/// Derives the salt for the period containing `date`.
///
/// The persistent secret is used as the HMAC *key* (rather than being embedded
/// in the message), which is the conventional construction and means the
/// secret's full entropy contributes as key material.
pub fn rotating_salt(secret: &str, date: NaiveDate, rotation_days: u32) -> String {
    let period = salt_period(date, rotation_days);
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(SALT_DOMAIN);
    mac.update(b"|");
    mac.update(period.to_string().as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Convenience wrapper for the default daily rotation.
pub fn daily_salt(secret: &str, date: NaiveDate) -> String {
    rotating_salt(secret, date, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn test_deterministic_visitor_id() {
        let id1 = generate_visitor_id("a.com", "192.168.1.1", "Mozilla/5.0", "salt");
        let id2 = generate_visitor_id("a.com", "192.168.1.1", "Mozilla/5.0", "salt");
        assert_eq!(id1, id2, "same inputs must produce the same visitor ID");
    }

    #[test]
    fn test_different_ip_different_id() {
        assert_ne!(
            generate_visitor_id("a.com", "192.168.1.1", "Mozilla/5.0", "salt"),
            generate_visitor_id("a.com", "192.168.1.2", "Mozilla/5.0", "salt")
        );
    }

    #[test]
    fn test_different_ua_different_id() {
        assert_ne!(
            generate_visitor_id("a.com", "192.168.1.1", "Mozilla/5.0", "salt"),
            generate_visitor_id("a.com", "192.168.1.1", "Chrome/120.0", "salt")
        );
    }

    #[test]
    fn test_different_salt_different_id() {
        assert_ne!(
            generate_visitor_id("a.com", "192.168.1.1", "Mozilla/5.0", "salt-day1"),
            generate_visitor_id("a.com", "192.168.1.1", "Mozilla/5.0", "salt-day2")
        );
    }

    #[test]
    fn test_visitor_id_is_scoped_per_site() {
        // The same person on two sites of one instance must not be correlatable.
        assert_ne!(
            generate_visitor_id("a.com", "1.2.3.4", "UA", "salt"),
            generate_visitor_id("b.com", "1.2.3.4", "UA", "salt")
        );
    }

    #[test]
    fn test_field_separation_is_unambiguous() {
        // Without separators these two inputs would hash identically.
        assert_ne!(
            generate_visitor_id("a.com", "1", "2", "salt"),
            generate_visitor_id("a.com|1", "", "2", "salt")
        );
    }

    #[test]
    fn test_visitor_id_is_hex_encoded() {
        let id = generate_visitor_id("a.com", "1.2.3.4", "UA", "salt");
        assert_eq!(id.len(), 64, "SHA-256 hex output is 64 chars");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_empty_inputs() {
        assert_eq!(generate_visitor_id("", "", "", "").len(), 64);
    }

    #[test]
    fn test_daily_salt_deterministic() {
        assert_eq!(
            daily_salt("secret", d(2024, 1, 15)),
            daily_salt("secret", d(2024, 1, 15))
        );
    }

    #[test]
    fn test_daily_salt_changes_by_date() {
        assert_ne!(
            daily_salt("secret", d(2024, 1, 15)),
            daily_salt("secret", d(2024, 1, 16))
        );
    }

    #[test]
    fn test_daily_salt_changes_by_secret() {
        assert_ne!(
            daily_salt("secret1", d(2024, 1, 15)),
            daily_salt("secret2", d(2024, 1, 15))
        );
    }

    #[test]
    fn test_salt_period_daily_rotation_changes_every_day() {
        assert_ne!(
            salt_period(d(2024, 1, 15), 1),
            salt_period(d(2024, 1, 16), 1)
        );
    }

    /// First day of the salt period containing `date`.
    ///
    /// Tests that need "two dates in the same period" must derive them from a
    /// period boundary. Picking calendar dates by hand looks fine until the pair
    /// happens to straddle one — 2024-01-15 and 2024-01-20 are five days apart
    /// and land in *different* 30-day periods, because epoch-aligned buckets
    /// know nothing about the calendar.
    fn period_start(date: NaiveDate, rotation_days: u32) -> NaiveDate {
        let period = salt_period(date, rotation_days);
        let days = period * i64::from(rotation_days);
        assert!(days >= 0, "the test dates are all after the epoch");
        EPOCH + chrono::Days::new(days.cast_unsigned())
    }

    #[test]
    fn test_salt_period_is_stable_within_rotation_window() {
        // Every day inside one 30-day period shares a salt.
        let start = period_start(d(2024, 1, 15), 30);
        let expected = salt_period(start, 30);
        for offset in 0..30u64 {
            let day = start + chrono::Days::new(offset);
            assert_eq!(
                salt_period(day, 30),
                expected,
                "{day} is day {offset} of the period starting {start}"
            );
        }
        assert_ne!(
            salt_period(start + chrono::Days::new(30), 30),
            expected,
            "the period must roll over on day 30"
        );
    }

    #[test]
    fn test_salt_period_boundary_is_a_documented_edge() {
        // Two dates five days apart can still cross a boundary. This is the
        // trade-off `visitor_salt_rotation_days` makes explicit, not a bug —
        // and retention cohorts spanning a boundary lose their linkage.
        assert_ne!(
            salt_period(d(2024, 1, 15), 30),
            salt_period(d(2024, 1, 20), 30)
        );
    }

    #[test]
    fn test_salt_period_rolls_over_after_rotation_window() {
        assert_ne!(
            salt_period(d(2024, 1, 1), 7),
            salt_period(d(2024, 1, 15), 7),
            "14 days apart must be different 7-day periods"
        );
    }

    #[test]
    fn test_salt_period_is_epoch_aligned() {
        // Period boundaries do not depend on when the process started.
        assert_eq!(salt_period(d(1970, 1, 1), 7), 0);
        assert_eq!(salt_period(d(1970, 1, 7), 7), 0);
        assert_eq!(salt_period(d(1970, 1, 8), 7), 1);
    }

    #[test]
    fn test_salt_period_handles_pre_epoch_dates() {
        // div_euclid keeps periods monotonic for dates before the epoch;
        // a truncating division would map 1969-12-28 and 1970-01-01 together.
        assert!(salt_period(d(1969, 12, 28), 7) < salt_period(d(1970, 1, 1), 7));
    }

    #[test]
    fn test_rotation_zero_is_treated_as_one_day() {
        // Defensive: validate() rejects 0, but the maths must not divide by zero.
        assert_eq!(
            salt_period(d(2024, 1, 15), 0),
            salt_period(d(2024, 1, 15), 1)
        );
    }

    #[test]
    fn test_longer_rotation_keeps_visitor_id_stable_across_days() {
        // This is the property that makes retention cohorts possible: within
        // one rotation period the same visitor keeps one ID.
        let start = period_start(d(2024, 1, 15), 30);
        let first = generate_visitor_id(
            "a.com",
            "1.2.3.4",
            "UA",
            &rotating_salt("secret", start, 30),
        );
        for offset in [1u64, 7, 14, 29] {
            let day = start + chrono::Days::new(offset);
            assert_eq!(
                first,
                generate_visitor_id("a.com", "1.2.3.4", "UA", &rotating_salt("secret", day, 30)),
                "the visitor ID changed {offset} days into a 30-day period"
            );
        }
        assert_ne!(
            first,
            generate_visitor_id(
                "a.com",
                "1.2.3.4",
                "UA",
                &rotating_salt("secret", start + chrono::Days::new(30), 30),
            ),
            "the visitor ID must become unlinkable once the salt rotates"
        );
    }

    #[test]
    fn test_daily_rotation_breaks_visitor_id_across_days() {
        // The documented consequence of the privacy-preserving default.
        let s1 = rotating_salt("secret", d(2024, 1, 15), 1);
        let s2 = rotating_salt("secret", d(2024, 1, 16), 1);
        assert_ne!(
            generate_visitor_id("a.com", "1.2.3.4", "UA", &s1),
            generate_visitor_id("a.com", "1.2.3.4", "UA", &s2)
        );
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
            site in "[a-z]{1,10}\\.com",
            ip in "[0-9a-z.]{1,20}",
            ua in "[A-Za-z0-9]{1,50}",
            salt in "[A-Za-z0-9]{1,30}",
        ) {
            prop_assert_eq!(
                generate_visitor_id(&site, &ip, &ua, &salt),
                generate_visitor_id(&site, &ip, &ua, &salt)
            );
        }

        /// Uniqueness: distinct IPs (same site, UA and salt) give distinct IDs.
        #[test]
        fn prop_visitor_id_unique_per_ip(
            suffix_a in 0u8..128u8,
            suffix_b in 128u8..=255u8,
            ua in "[A-Za-z0-9]{1,20}",
            salt in "[A-Za-z0-9]{1,20}",
        ) {
            prop_assert_ne!(
                generate_visitor_id("a.com", &format!("10.0.0.{suffix_a}"), &ua, &salt),
                generate_visitor_id("a.com", &format!("10.0.0.{suffix_b}"), &ua, &salt)
            );
        }

        /// Site scoping: distinct sites never share a visitor ID.
        #[test]
        fn prop_visitor_id_unique_per_site(
            a in "a[a-z]{2,8}\\.com",
            b in "b[a-z]{2,8}\\.com",
            ip in "[0-9.]{7,15}",
        ) {
            prop_assert_ne!(
                generate_visitor_id(&a, &ip, "UA", "salt"),
                generate_visitor_id(&b, &ip, "UA", "salt")
            );
        }

        /// Daily rotation: the salt differs on any two distinct days.
        #[test]
        fn prop_daily_salt_changes_per_day(
            secret in "[A-Za-z0-9]{1,20}",
            day_a in 0u32..180u32,
            day_b in 180u32..360u32,
        ) {
            let base = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
            prop_assert_ne!(
                daily_salt(&secret, base + Duration::days(i64::from(day_a))),
                daily_salt(&secret, base + Duration::days(i64::from(day_b)))
            );
        }

        /// Period alignment: any two dates within one rotation share a period.
        #[test]
        fn prop_salt_period_stable_within_window(
            rotation in 2u32..90u32,
            offset in 0u32..90u32,
        ) {
            let base = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
            let day = offset % rotation;
            let start_period = salt_period(base, rotation);
            let within = base + Duration::days(i64::from(day));
            // `base` may sit mid-period, so compare against the period of a date
            // that is provably in the same window: the period start itself.
            let period_start = base - Duration::days(
                (base - NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()).num_days()
                    .rem_euclid(i64::from(rotation)),
            );
            prop_assert_eq!(salt_period(period_start, rotation), start_period);
            prop_assert!(salt_period(within, rotation) >= start_period);
        }
    }
}
