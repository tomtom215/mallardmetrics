//! Google Analytics 4 BigQuery export, as newline-delimited JSON.
//!
//! # Source format
//!
//! Google documents the daily `events_YYYYMMDD` tables of the GA4 BigQuery
//! export at <https://support.google.com/analytics/answer/7029846> (checked
//! 2026-10-21). One table row is one event. `bq extract --destination_format=NEWLINE_DELIMITED_JSON`
//! or `EXPORT DATA ... format = 'JSON'` writes each row as one JSON object whose
//! field names are the column names, with `RECORD`s as nested objects and
//! `REPEATED` fields (`event_params`) as arrays.
//!
//! The fields read here, and nothing else:
//!
//! | GA4 field | Used for |
//! |---|---|
//! | `event_timestamp` (INT64, UTC microseconds) | `timestamp` |
//! | `event_name` | `event_name` (`page_view` becomes `pageview`) |
//! | `platform` | rows that are not `WEB` are skipped |
//! | `event_params[key=page_location].value.string_value` | `pathname`, `hostname`, UTM parameters |
//! | `event_params[key=page_referrer].value.string_value` | `referrer`, `referrer_source` |
//! | `event_params[key=ga_session_id].value.int_value` | grouping events into one pseudonymous visitor per GA4 session |
//! | `event_params[key=currency].value.string_value`, `ecommerce.purchase_revenue` | revenue of `purchase` events |
//! | `device.category`, `device.operating_system(_version)`, `device.web_info.browser(_version)`, `device.web_info.hostname` | the matching columns |
//! | `geo.region`, `geo.city` | `region`, `city`, when the instance's `geoip_precision` allows |
//! | `user_pseudo_id` | **only** as HMAC input for the pseudonymous visitor id; never stored |
//!
//! `user_id`, `device.advertising_id`, `device.vendor_id`, `user_properties`,
//! the remaining `event_params`, `items` and every campaign click identifier
//! are not read.
//!
//! # What is not verified
//!
//! The field names are Google's. Two encodings are inferred rather than read
//! from a document: BigQuery's JSON export writes `INT64` as a quoted string
//! (both spellings are accepted here), and `currency` is the parameter name
//! the GA4 recommended `purchase` event documents. `geo.country` is a country
//! *name* and Mallard stores an ISO 3166 code, so `country_code` is left empty
//! rather than guessed.

use super::{Parsed, Policy};
use crate::ingest::buffer::Event;
use crate::ingest::handler::{
    extract_referrer_source, normalize_currency, parse_utm_params, round_to_hour,
    sanitize_pathname, sanitize_string, strip_url_query_and_fragment,
};
use chrono::DateTime;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Prefix shared by every visitor id this importer writes.
pub const VISITOR_PREFIX: &str = "imp:ga4:";

/// Automatic events that record GA4's own bookkeeping, not something a visitor
/// did. Skipped unless the operator asks for everything.
const BOOKKEEPING_EVENTS: &[&str] = &["session_start", "first_visit", "user_engagement"];

/// Earliest accepted `event_timestamp`: 2010-01-01 UTC, in microseconds. GA4
/// itself is newer; anything older is a unit mistake (seconds or milliseconds).
const MIN_TIMESTAMP_MICROS: i64 = 1_262_304_000_000_000;

/// How far past "now" a timestamp may be before it is rejected.
const FUTURE_SLACK_SECS: i64 = 86_400;

/// Largest revenue the `DECIMAL(12,2)` column holds.
const MAX_REVENUE_AMOUNT: f64 = 9_999_999_999.99;

/// Longest strings stored, matching the ingest endpoint.
const MAX_NAME_LEN: usize = 256;
const MAX_REFERRER_LEN: usize = 2048;

/// A string field that GA4 left empty or marked unknown is absent.
fn text(value: &Value) -> Option<&str> {
    let s = value.as_str()?.trim();
    (!s.is_empty() && s != "(not set)" && s != "(none)").then_some(s)
}

/// BigQuery's JSON export writes `INT64` as a string; a hand-made file may use
/// a number. Accept both, refuse everything else.
fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The `event_params` entry named `key`, as its `value` record.
fn param<'a>(params: &'a [Value], key: &str) -> Option<&'a Value> {
    params
        .iter()
        .find(|p| p.get("key").and_then(Value::as_str) == Some(key))
        .and_then(|p| p.get("value"))
}

fn param_text<'a>(params: &'a [Value], key: &str) -> Option<&'a str> {
    param(params, key)?.get("string_value").and_then(text)
}

fn param_int(params: &[Value], key: &str) -> Option<i64> {
    let value = param(params, key)?;
    value
        .get("int_value")
        .and_then(integer)
        .or_else(|| value.get("string_value").and_then(integer))
}

/// `a.b.c` into nested objects.
fn path<'a>(row: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().try_fold(row, |v, k| v.get(*k))
}

/// The host of an absolute URL, lowercased, without credentials or port.
fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    // `[::1]:8080` keeps its brackets; `example.com:8080` loses the port.
    let host = if host.starts_with('[') {
        host.split(']').next().map(|h| format!("{h}]"))?
    } else {
        host.split(':').next()?.to_string()
    };
    let host = sanitize_string(&host.to_ascii_lowercase(), MAX_NAME_LEN);
    (!host.is_empty()).then_some(host)
}

/// `HMAC-SHA256(key, parts joined by 0x1f)`, first 16 bytes as hex.
fn pseudonym(key: &[u8], parts: &[&str]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    for part in parts {
        mac.update(part.as_bytes());
        mac.update(&[0x1f]);
    }
    hex::encode(&mac.finalize().into_bytes()[..16])
}

/// The event's instant, from `event_timestamp`.
fn instant_of(row: &Value, now_secs: i64) -> Result<(i64, DateTime<chrono::Utc>), String> {
    let micros = row
        .get("event_timestamp")
        .ok_or_else(|| "missing event_timestamp".to_string())
        .and_then(|v| {
            integer(v).ok_or_else(|| {
                "event_timestamp is not an integer (expected UTC microseconds)".to_string()
            })
        })?;
    if micros < MIN_TIMESTAMP_MICROS {
        return Err(format!(
            "event_timestamp {micros} is before 2010-01-01; GA4 writes UTC microseconds, not seconds or milliseconds"
        ));
    }
    if micros / 1_000_000 > now_secs + FUTURE_SLACK_SECS {
        return Err(format!("event_timestamp {micros} is in the future"));
    }
    let instant = DateTime::from_timestamp(
        micros.div_euclid(1_000_000),
        u32::try_from(micros.rem_euclid(1_000_000) * 1000).unwrap_or(0),
    )
    .ok_or_else(|| format!("event_timestamp {micros} is out of range"))?;
    Ok((micros, instant))
}

/// The pseudonymous visitor id of one row.
///
/// One id per GA4 session (`user_pseudo_id` + `ga_session_id`), so Mallard can
/// rebuild the session; one id per event when the row has no usable session
/// identity or the instance forbids linking events. Both are keyed hashes: the
/// source identifiers are never stored.
fn visitor_id_of(
    row: &Value,
    params: &[Value],
    ctx: &Ctx<'_>,
    micros: i64,
    raw_name: &str,
    location: &str,
) -> String {
    let pseudo_id = row.get("user_pseudo_id").and_then(text);
    let session = param_int(params, "ga_session_id");
    if let (Some(user), Some(session), false) = (pseudo_id, session, ctx.policy.suppress_visitor_id)
    {
        return format!(
            "{VISITOR_PREFIX}s:{}",
            pseudonym(
                ctx.key,
                &["session", ctx.site_id, user, &session.to_string()]
            )
        );
    }
    // Derived from the whole row so that re-importing the file derives the
    // same id.
    let batch = row
        .get("batch_event_index")
        .and_then(integer)
        .unwrap_or_default()
        .to_string();
    let bundle = row
        .get("event_bundle_sequence_id")
        .and_then(integer)
        .unwrap_or_default()
        .to_string();
    format!(
        "{VISITOR_PREFIX}e:{}",
        pseudonym(
            ctx.key,
            &[
                "event",
                ctx.site_id,
                pseudo_id.unwrap_or(""),
                &micros.to_string(),
                raw_name,
                location,
                &batch,
                &bundle,
            ],
        )
    )
}

/// Revenue of a `purchase`: an amount with no currency cannot be summed with
/// anything, so one without the other is dropped.
fn revenue_of(row: &Value, params: &[Value], raw_name: &str) -> (Option<f64>, Option<String>) {
    if raw_name != "purchase" {
        return (None, None);
    }
    let amount = path(row, &["ecommerce", "purchase_revenue"])
        .and_then(float)
        .filter(|a| a.is_finite() && a.abs() <= MAX_REVENUE_AMOUNT);
    let currency = param_text(params, "currency").and_then(normalize_currency);
    match (amount, currency) {
        (Some(a), Some(c)) => (Some(a), Some(c)),
        _ => (None, None),
    }
}

/// The fixed inputs of every line of one import.
struct Ctx<'a> {
    site_id: &'a str,
    policy: &'a Policy,
    key: &'a [u8],
}

/// Turn one export line into an event, a deliberate skip, or an error message.
///
/// # Errors
///
/// A message naming what is wrong with the row; the caller adds the line number.
pub fn parse_line(
    line: &str,
    site_id: &str,
    policy: &Policy,
    key: &[u8],
    include_all_events: bool,
    now_secs: i64,
) -> Result<Parsed, String> {
    let ctx = Ctx {
        site_id,
        policy,
        key,
    };
    let row: Value = serde_json::from_str(line).map_err(|e| format!("not valid JSON: {e}"))?;
    if !row.is_object() {
        return Err("expected one JSON object per line".to_string());
    }
    let (micros, instant) = instant_of(&row, now_secs)?;
    let raw_name = row
        .get("event_name")
        .and_then(text)
        .ok_or_else(|| "missing event_name".to_string())?;
    let params: &[Value] = match row.get("event_params") {
        None | Some(Value::Null) => &[],
        Some(Value::Array(items)) => items,
        Some(_) => return Err("event_params is not an array".to_string()),
    };

    if let Some(platform) = row.get("platform").and_then(text)
        && !platform.eq_ignore_ascii_case("web")
    {
        return Ok(Parsed::Skip(format!("platform {platform} is not WEB")));
    }
    if !include_all_events && BOOKKEEPING_EVENTS.contains(&raw_name) {
        return Ok(Parsed::Skip(format!(
            "{raw_name} is GA4 bookkeeping (use --all-events to keep it)"
        )));
    }
    let Some(location) = param_text(params, "page_location") else {
        return Ok(Parsed::Skip(format!("{raw_name} has no page_location")));
    };

    let utm = parse_utm_params(location);
    let referrer_raw = param_text(params, "page_referrer");
    let device = |keys: &[&str]| path(&row, keys).and_then(text).map(str::to_string);
    let (region, city) = match policy.geoip_precision.as_str() {
        "city" => (device(&["geo", "region"]), device(&["geo", "city"])),
        "region" => (device(&["geo", "region"]), None),
        _ => (None, None),
    };
    let (revenue_amount, revenue_currency) = revenue_of(&row, params, raw_name);
    let keep = |suppressed: bool, keys: &[&str]| if suppressed { None } else { device(keys) };

    Ok(Parsed::Event(Box::new(Event {
        site_id: site_id.to_string(),
        visitor_id: visitor_id_of(&row, params, &ctx, micros, raw_name, location),
        timestamp: if policy.round_timestamps {
            round_to_hour(instant)
        } else {
            instant.naive_utc()
        },
        event_name: if raw_name == "page_view" {
            "pageview".to_string()
        } else {
            sanitize_string(raw_name, MAX_NAME_LEN)
        },
        pathname: sanitize_pathname(location),
        hostname: host_of(location).or_else(|| device(&["device", "web_info", "hostname"])),
        referrer: referrer_raw.map(|r| {
            let r = if policy.strip_referrer_query {
                strip_url_query_and_fragment(r)
            } else {
                r
            };
            sanitize_string(r, MAX_REFERRER_LEN)
        }),
        referrer_source: referrer_raw.and_then(extract_referrer_source),
        utm_source: utm.source,
        utm_medium: utm.medium,
        utm_campaign: utm.campaign,
        utm_content: utm.content,
        utm_term: utm.term,
        browser: device(&["device", "web_info", "browser"]),
        browser_version: keep(
            policy.suppress_browser_version,
            &["device", "web_info", "browser_version"],
        ),
        // GA4 reports the Mac platform name; Mallard's own parser says macOS.
        os: device(&["device", "operating_system"]).map(|os| {
            if os == "Macintosh" {
                "macOS".to_string()
            } else {
                os
            }
        }),
        os_version: keep(
            policy.suppress_os_version,
            &["device", "operating_system_version"],
        ),
        device_type: keep(policy.suppress_screen_size, &["device", "category"])
            .map(|c| c.to_ascii_lowercase()),
        screen_size: None,
        country_code: None,
        region,
        city,
        props: None,
        revenue_amount,
        revenue_currency,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;
    const KEY: &[u8] = b"0123456789abcdef-test-key";

    fn policy() -> Policy {
        Policy::default()
    }

    fn event(line: &str) -> Event {
        match parse_line(line, "example.com", &policy(), KEY, false, NOW).unwrap() {
            Parsed::Event(e) => *e,
            Parsed::Skip(why) => panic!("skipped: {why}"),
        }
    }

    const PAGE_VIEW: &str = r#"{"event_date":"20260314","event_timestamp":"1773489600123456","event_name":"page_view","event_params":[{"key":"page_location","value":{"string_value":"https://www.example.com/pricing/?utm_source=news&utm_medium=email&token=secret"}},{"key":"page_referrer","value":{"string_value":"https://www.google.com/search?q=private"}},{"key":"ga_session_id","value":{"int_value":"1773489000"}}],"user_pseudo_id":"1234567890.1773400000","user_id":"customer-42","platform":"WEB","device":{"category":"desktop","operating_system":"Macintosh","operating_system_version":"14.4","web_info":{"browser":"Chrome","browser_version":"122.0","hostname":"www.example.com"},"advertising_id":"AAAA"},"geo":{"country":"Germany","region":"Berlin","city":"Berlin"}}"#;

    #[test]
    fn maps_a_page_view() {
        let e = event(PAGE_VIEW);
        assert_eq!(e.event_name, "pageview");
        assert_eq!(e.pathname, "/pricing");
        assert_eq!(e.hostname.as_deref(), Some("www.example.com"));
        assert_eq!(e.timestamp.to_string(), "2026-03-14 12:00:00.123456");
        assert_eq!(e.utm_source.as_deref(), Some("news"));
        assert_eq!(e.utm_medium.as_deref(), Some("email"));
        assert_eq!(e.referrer_source.as_deref(), Some("Google"));
        assert_eq!(e.browser.as_deref(), Some("Chrome"));
        assert_eq!(e.os.as_deref(), Some("macOS"));
        assert_eq!(e.device_type.as_deref(), Some("desktop"));
        assert_eq!(e.region.as_deref(), Some("Berlin"));
        assert_eq!(e.country_code, None, "GA4 gives a country name, not a code");
        assert!(e.visitor_id.starts_with("imp:ga4:s:"));
        assert!(e.props.is_none());
    }

    #[test]
    fn drops_every_identifier_and_query_string() {
        // The referrer keeps its query unless strip_referrer_query is on, as on
        // the live ingest path; `privacy_policy_is_applied` covers that.
        let e = event(PAGE_VIEW);
        let stored = format!("{e:?}");
        for secret in [
            "customer-42",
            "1234567890.1773400000",
            "AAAA",
            "token=secret",
        ] {
            assert!(!stored.contains(secret), "{secret} leaked into {stored}");
        }
    }

    #[test]
    fn one_ga4_session_is_one_visitor_and_the_key_matters() {
        let a = event(PAGE_VIEW);
        let b = event(&PAGE_VIEW.replace("1773489600123456", "1773489700000000"));
        assert_eq!(a.visitor_id, b.visitor_id);
        let other_session = event(&PAGE_VIEW.replace("1773489000", "1773999999"));
        assert_ne!(a.visitor_id, other_session.visitor_id);
        let other_key = match parse_line(
            PAGE_VIEW,
            "example.com",
            &policy(),
            b"another-key-entirely",
            false,
            NOW,
        )
        .unwrap()
        {
            Parsed::Event(e) => e,
            Parsed::Skip(_) => unreachable!(),
        };
        assert_ne!(a.visitor_id, other_key.visitor_id);
    }

    #[test]
    fn suppress_visitor_id_makes_each_event_its_own_visitor() {
        let strict = Policy {
            suppress_visitor_id: true,
            ..Policy::default()
        };
        let parse =
            |line: &str| match parse_line(line, "example.com", &strict, KEY, false, NOW).unwrap() {
                Parsed::Event(e) => e.visitor_id,
                Parsed::Skip(_) => unreachable!(),
            };
        let a = parse(PAGE_VIEW);
        let b = parse(&PAGE_VIEW.replace("1773489600123456", "1773489700000000"));
        assert!(a.starts_with("imp:ga4:e:"));
        assert_ne!(a, b);
        assert_eq!(a, parse(PAGE_VIEW), "the id must be reproducible");
    }

    #[test]
    fn privacy_policy_is_applied() {
        let strict = Policy {
            round_timestamps: true,
            strip_referrer_query: true,
            suppress_browser_version: true,
            suppress_os_version: true,
            suppress_screen_size: true,
            geoip_precision: "none".to_string(),
            suppress_visitor_id: false,
        };
        let Parsed::Event(e) =
            parse_line(PAGE_VIEW, "example.com", &strict, KEY, false, NOW).unwrap()
        else {
            panic!("skipped")
        };
        assert_eq!(e.timestamp.to_string(), "2026-03-14 12:00:00");
        assert_eq!(e.referrer.as_deref(), Some("https://www.google.com/search"));
        assert!(e.browser_version.is_none() && e.os_version.is_none());
        assert!(e.device_type.is_none() && e.screen_size.is_none());
        assert!(e.region.is_none() && e.city.is_none());
    }

    #[test]
    fn purchase_carries_revenue_only_with_a_currency() {
        let purchase = r#"{"event_timestamp":1773489600000000,"event_name":"purchase","event_params":[{"key":"page_location","value":{"string_value":"https://example.com/thanks"}},{"key":"currency","value":{"string_value":"eur"}}],"ecommerce":{"purchase_revenue":19.99,"transaction_id":"T-1"}}"#;
        let e = event(purchase);
        assert_eq!(e.event_name, "purchase");
        assert_eq!(e.revenue_amount, Some(19.99));
        assert_eq!(e.revenue_currency.as_deref(), Some("EUR"));
        assert!(e.visitor_id.starts_with("imp:ga4:e:"), "no user or session");
        assert!(!format!("{e:?}").contains("T-1"));

        let no_currency =
            purchase.replace(r#",{"key":"currency","value":{"string_value":"eur"}}"#, "");
        let e = event(&no_currency);
        assert_eq!((e.revenue_amount, e.revenue_currency), (None, None));
    }

    #[test]
    fn skips_are_deliberate_and_explained() {
        let skip =
            |line: &str, all: bool| match parse_line(line, "example.com", &policy(), KEY, all, NOW)
                .unwrap()
            {
                Parsed::Skip(why) => why,
                Parsed::Event(_) => panic!("expected a skip"),
            };
        let engagement = PAGE_VIEW.replace("page_view", "user_engagement");
        assert!(skip(&engagement, false).contains("bookkeeping"));
        assert!(matches!(
            parse_line(&engagement, "example.com", &policy(), KEY, true, NOW).unwrap(),
            Parsed::Event(_)
        ));
        assert!(skip(&PAGE_VIEW.replace("\"WEB\"", "\"IOS\""), false).contains("IOS"));
        let no_location = PAGE_VIEW.replace("page_location", "something_else");
        assert!(skip(&no_location, false).contains("no page_location"));
    }

    #[test]
    fn malformed_rows_name_the_problem() {
        let err =
            |line: &str| parse_line(line, "example.com", &policy(), KEY, false, NOW).unwrap_err();
        assert!(err("not json").contains("not valid JSON"));
        assert!(err("[1,2]").contains("one JSON object"));
        assert!(err(r#"{"event_name":"x"}"#).contains("missing event_timestamp"));
        assert!(err(r#"{"event_timestamp":"abc","event_name":"x"}"#).contains("not an integer"));
        assert!(err(r#"{"event_timestamp":1773489600,"event_name":"x"}"#).contains("before 2010"));
        assert!(err(r#"{"event_timestamp":9999999999999999,"event_name":"x"}"#).contains("future"));
        assert!(err(r#"{"event_timestamp":1773489600000000}"#).contains("missing event_name"));
        assert!(
            err(r#"{"event_timestamp":1773489600000000,"event_name":"x","event_params":{}}"#)
                .contains("event_params is not an array")
        );
    }

    #[test]
    fn host_parsing() {
        assert_eq!(
            host_of("https://User:pw@Example.COM:8443/a?b#c").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("http://[::1]:3000/x").as_deref(), Some("[::1]"));
        assert_eq!(host_of("/relative"), None);
    }
}
