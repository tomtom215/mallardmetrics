# Privacy Architecture & Data Processing Documentation

> **Legal disclaimer:** This document describes the technical data-processing architecture of
> Mallard Metrics and provides general information about applicable privacy regulations. It is
> **not legal advice**. Every operator deploying Mallard Metrics bears independent responsibility
> for complying with data protection law in their jurisdiction. Consult a qualified data
> protection attorney before deploying this software to process data from real users.

---

## Table of Contents

1. [What Data Is Processed](#what-data-is-processed)
2. [What Is and Is Not Stored](#what-is-and-is-not-stored)
3. [Visitor Identification Architecture](#visitor-identification-architecture)
4. [GDPR Analysis](#gdpr-analysis)
5. [ePrivacy Directive (Cookie Law)](#eprivacy-directive-cookie-law)
6. [CCPA Analysis](#ccpa-analysis)
7. [The GitHub Pages / Live Demo Question](#the-github-pages--live-demo-question)
8. [What Operators Must Do](#what-operators-must-do)
9. [What Mallard Metrics Does Not Do](#what-mallard-metrics-does-not-do)
10. [Comparison with Other Analytics Tools](#comparison-with-other-analytics-tools)
11. [Definitions and Primary Sources](#definitions-and-primary-sources)

---

## What Data Is Processed

Every HTTP request to `POST /api/event` (or `GET /api/event` for pixel tracking) causes the
server to process the following data. "Process" under GDPR means any operation performed on
data, including reading it from a request header — storage is not required for processing to
occur.

### Ephemerally processed (held in RAM only, never written to disk)

| Data item | Source | Purpose | Lifetime |
|---|---|---|---|
| IP address | `X-Forwarded-For` / `X-Real-IP` / socket | GeoIP lookup + visitor ID derivation | Single request (~µs); dropped when handler returns |
| Raw User-Agent string | `User-Agent` request header | UA parsing + bot detection + visitor ID derivation | Single request (~µs); dropped when handler returns |

**IP addresses are never logged, never written to the event buffer, and never written to
DuckDB or Parquet.** The relevant code is in `src/ingest/handler.rs` and
`src/ingest/visitor_id.rs`.

### Persistently stored (written to DuckDB and Parquet)

| Field | Type | Example value | Retained |
|---|---|---|---|
| `visitor_id` | 64-char hex string | `a3f2…c9d1` | Until partition deleted |
| `timestamp` | UTC datetime | `2024-01-15T14:23:01` | Until partition deleted |
| `event_name` | String | `pageview` | Until partition deleted |
| `site_id` | String | `example.com` | Until partition deleted |
| `pathname` | String | `/pricing` | Until partition deleted |
| `hostname` | String (optional) | `example.com` | Until partition deleted |
| `referrer` | String (optional) | `https://google.com/search?q=…` | Until partition deleted |
| `referrer_source` | String (optional) | `Google` | Until partition deleted |
| `utm_source` | String (optional) | `newsletter` | Until partition deleted |
| `utm_medium` | String (optional) | `email` | Until partition deleted |
| `utm_campaign` | String (optional) | `spring-sale` | Until partition deleted |
| `utm_content` | String (optional) | `hero-cta` | Until partition deleted |
| `utm_term` | String (optional) | `analytics software` | Until partition deleted |
| `browser` | String (optional) | `Chrome` | Until partition deleted |
| `browser_version` | String (optional) | `120.0` | Until partition deleted |
| `os` | String (optional) | `Windows` | Until partition deleted |
| `os_version` | String (optional) | `10.0` | Until partition deleted |
| `device_type` | String (optional) | `desktop` | Until partition deleted |
| `screen_size` | String (optional) | `1920x1080` | Until partition deleted |
| `country_code` | String (optional) | `DE` | Until partition deleted |
| `region` | String (optional) | `Bavaria` | Until partition deleted |
| `city` | String (optional) | `Munich` | Until partition deleted |
| `props` | JSON string (optional) | `{"plan":"pro"}` | Until partition deleted |
| `revenue_amount` | Float (optional) | `49.99` | Until partition deleted |
| `revenue_currency` | String (optional) | `EUR` | Until partition deleted |

Retention period is controlled by `MALLARD_RETENTION_DAYS` (default: 0 = unlimited).

---

## What Is and Is Not Stored

### Accurate characterisation

| Claim | Accurate? | Nuance |
|---|---|---|
| IP addresses are not stored | **Yes** | Not written anywhere to disk. Processed in RAM per-request only. |
| Raw User-Agent strings are not stored | **Yes** | Only four parsed fields (browser name, version, OS name, version) are stored. |
| Visitor IDs are pseudonymous, not anonymous | **Yes** | See [Visitor Identification Architecture](#visitor-identification-architecture). |
| Geographic data is stored | **Yes** | `country_code`, `region`, `city` are derived from the IP and stored permanently. |
| Referrer URLs are stored | **Yes** | May include search queries or campaign parameters sent by the browser. |
| Custom `props` are stored | **Yes** | Operators control what custom properties are collected via the tracking script. |

### What "no PII storage" means and does not mean

The phrase "no PII storage" as commonly used in analytics marketing means: **no directly
identifying personal data** (name, email, full IP address, raw User-Agent string) is written
to disk. That characterisation is accurate for Mallard Metrics.

However, under GDPR, **pseudonymous data is still personal data** (GDPR Recital 26; Art. 4(5)).
The stored visitor ID hash is pseudonymous personal data. The stored geographic data was derived
from an IP address (personal data). GDPR therefore applies to the stored data, not only to the
ephemeral processing.

---

## Visitor Identification Architecture

### Algorithm (two-step HMAC-SHA256)

```
Step 1 — Daily salt derivation:
  Input:  MALLARD_SECRET + ":" + UTC_DATE (e.g. "my-secret:2024-01-15")
  Key:    Literal constant "mallard-metrics-salt"
  Output: daily_salt (64-char hex)

Step 2 — Visitor ID derivation:
  Input:  IP_ADDRESS + "|" + USER_AGENT
  Key:    daily_salt (from Step 1)
  Output: visitor_id (64-char hex, stored in Parquet)
```

Source: `src/ingest/visitor_id.rs:10–30`.

### Privacy properties

| Property | Guaranteed | Notes |
|---|---|---|
| IP not stored | Yes | Only the hash output is retained |
| Different visitors produce different IDs | Yes | Property-tested (`visitor_id.rs:127–138`) |
| Same visitor produces same ID within a day | Yes | Enables deduplication without cookies |
| Same visitor produces different IDs across days | Yes | Salt changes at UTC midnight |
| ID cannot be reversed to recover the IP | Practically yes | HMAC-SHA256 is a one-way function; brute-force impractical |

### Is the visitor ID "anonymous" under GDPR?

**No.** GDPR Recital 26 draws a clear distinction: data is anonymous only when it is
"impossible" — or would require "disproportionate" effort — for "any person" to identify the
individual. The visitor ID is **pseudonymous** (Art. 4(5)) because:

1. The operator holds `MALLARD_SECRET`. With the secret and a target date, they can
   regenerate the daily salt.
2. If an operator also had access to network logs containing the original IP addresses, they
   could in principle correlate those IPs to visitor IDs for the same calendar day.
3. Pseudonymous data therefore remains personal data subject to GDPR (Recital 26, confirmed
   in EDPB Opinion 05/2022 on anonymisation techniques).

The design substantially reduces re-identification risk, but does not eliminate GDPR
applicability.

---

## GDPR Analysis

### Does GDPR apply to Mallard Metrics deployments?

**Yes, if the deployment processes data of individuals in the EU/EEA.** GDPR applies to any
controller or processor established in the EU/EEA, or any controller/processor that processes
personal data of EU/EEA data subjects regardless of where the controller is located
(GDPR Art. 3 — territorial scope).

A self-hosted instance receiving traffic from EU users is subject to GDPR regardless of where
the server is located.

### Is processing IP addresses subject to GDPR?

**Yes.** The Court of Justice of the EU ruled in *Breyer v. Bundesrepublik Deutschland*
(Case C‑582/14, 19 October 2016) that dynamic IP addresses constitute personal data for a
website operator who has the legal means — such as seeking disclosure from the ISP — to
identify the individual. Subsequent EDPB guidance reaffirms this.

Mallard Metrics processes (but does not store) IP addresses. That temporary processing still
constitutes processing of personal data under GDPR Art. 4(2).

### What lawful basis applies?

GDPR Art. 6(1) requires a lawful basis for every processing activity. For web analytics, the
most commonly applicable bases are:

#### Legitimate Interests (Art. 6(1)(f)) — most likely applicable

Operators may rely on legitimate interests when:
- There is a genuine legitimate interest (e.g., understanding how a website is used to improve it)
- The processing is necessary for that interest
- The interest is not overridden by the data subject's rights and freedoms

Under legitimate interests, **consent is not required** and no cookie/consent banner is needed,
but operators **must**:

1. Complete a **Legitimate Interests Assessment (LIA)** documenting the balancing test
2. Publish a **privacy notice** (Art. 13/14) disclosing:
   - Categories of data processed
   - Purposes and legal basis
   - Retention periods
   - Data subject rights (access, erasure, objection)
   - Contact information for the data controller
3. **Honour objections** from data subjects (Art. 21 right to object)

The EDPB's Guidelines 06/2020 on targeting of social media users and Guidelines 02/2019 on
processing of personal data under Article 6(1)(b) GDPR provide relevant context, though
they address different scenarios. The core balancing test is described in EDPB Opinion 06/2014
on legitimate interests.

#### Consent (Art. 6(1)(a)) — an alternative

Operators may instead rely on freely-given, specific, informed, unambiguous consent. This
generally requires a consent banner. Once consent is withdrawn, processing must stop.

#### Which basis is recommended?

For a self-hosted analytics tool with privacy-preserving design, **legitimate interests is
typically the appropriate basis**. The EDPB has acknowledged that privacy-respecting analytics
can qualify. However, the specific balancing test must be performed by the operator for their
deployment context.

### Data subject rights under GDPR

Regardless of lawful basis, operators must be able to respond to:

| Right | Article | Implication for Mallard Metrics |
|---|---|---|
| Right of access | Art. 15 | Operator must be able to produce data for a given visitor |
| Right to erasure | Art. 17 | Operator must be able to delete records for a visitor |
| Right to restriction | Art. 18 | Operator must be able to restrict processing |
| Right to data portability | Art. 20 | Applies only to consent-based or contract-based processing |
| Right to object | Art. 21 | Especially relevant under legitimate interests |

**Note on erasure:** Because the visitor ID is a pseudonymous hash — not a name, email, or
stored IP — responding to erasure requests requires the operator to know which visitor ID hash
corresponds to the requesting individual. Without the original IP and UA, the operator may be
unable to correlate stored records to a specific individual. Operators should document this
limitation in their privacy notice.

### Data transfers outside the EU/EEA

If the Mallard Metrics instance is hosted outside the EU/EEA, GDPR Chapter V applies to
the data transfer. Applicable mechanisms include Standard Contractual Clauses (SCCs) or
adequacy decisions. Operators must assess this for their specific hosting arrangement.

### Record of Processing Activities (Art. 30)

Organisations with 250 or more employees, or whose processing is not occasional, or that
process special-category data, must maintain a record of processing activities. Analytics
is typically recurring and should be included in the Art. 30 record.

---

## ePrivacy Directive (Cookie Law)

The ePrivacy Directive (2002/58/EC), as implemented in national law across EU member states,
requires prior **consent** for:

> "the storing of information, or the gaining of access to information already stored,
> in the terminal equipment of a subscriber or user"
> — Art. 5(3), ePrivacy Directive

Mallard Metrics **does not set cookies** and **does not access terminal storage** on the
visitor's device. The visitor ID is derived server-side from request data; no information is
written to or read from the browser.

**Consequence:** Art. 5(3) ePrivacy does not require consent for Mallard Metrics' visitor
identification mechanism.

However, this does not exempt the operator from GDPR (see above). The ePrivacy analysis and
the GDPR analysis are separate inquiries.

### National law variation

ePrivacy is implemented differently in each member state. Germany's TTDSG §25, France's CNIL
guidelines, and the UK's PECR each have their own specific requirements. Some national
authorities have issued specific guidance on cookie-free analytics. Operators deploying in
specific jurisdictions should consult applicable national guidance.

---

## CCPA Analysis

The California Consumer Privacy Act (Cal. Civ. Code § 1798.100 et seq.), as amended by CPRA,
applies to **for-profit businesses** that meet at least one threshold:

- Annual gross revenue exceeding $25 million
- Buy, sell, receive, or share personal information of 100,000 or more California consumers
  or households per year
- Derive 50% or more of annual revenue from selling or sharing personal information

A small open-source project or individual deploying Mallard Metrics may fall below all three
thresholds. Operators must assess their own applicability.

### If CCPA/CPRA applies

Under CCPA § 1798.140(v)(1), "personal information" includes:

- IP addresses (explicitly listed)
- Unique identifiers — the visitor ID hash qualifies as a "unique identifier" under § 1798.140(ae)
- Geolocation data (country, region, city stored by Mallard Metrics)
- Inferences drawn from personal information to create a profile (aggregate analytics)

**Obligations if CCPA applies:**

1. Disclose categories of personal information collected in a privacy policy
2. Disclose the purposes for which personal information is used
3. Provide a "Do Not Sell or Share My Personal Information" link if applicable
4. Honor rights to know, delete, correct, and opt-out
5. Maintain records of consumer requests

Mallard Metrics does not "sell" personal information (no data leaves the self-hosted instance),
but the "share" definition in CPRA may cover cross-context behavioural advertising — which
Mallard Metrics does not perform.

---

## The GitHub Pages / Live Demo Question

Embedding Mallard Metrics as a live demo on a public site introduces concrete legal obligations:

### What happens technically

When a visitor loads your GitHub Pages site with an embedded Mallard Metrics demo:
- Their browser may send requests to the Mallard Metrics instance
- The instance temporarily processes their IP address and User-Agent
- Their approximate geographic location (country, region, city) is stored in Parquet
- A pseudonymous visitor ID is generated and stored

This is real personal data processing, not simulation.

### GDPR obligations this triggers

| Obligation | Source | Notes |
|---|---|---|
| Lawful basis | Art. 6 | Legitimate interests requires LIA; consent requires banner |
| Privacy notice | Art. 13 | Must be provided at time of data collection |
| Data processor agreement | Art. 28 | If GitHub acts as a processor on your behalf |
| Record of processing | Art. 30 | May apply depending on organisation size |

### Recommended approaches for a public demo

**Option A — Synthetic data (recommended):** Pre-populate the demo instance with synthetic,
generated event data. No real visitor data is processed. Add a banner stating: "This demo uses
synthetic data; no visitor information is collected." This eliminates GDPR obligations for the
demo instance.

**Option B — Consent-gated iframe:** Show a consent notice before loading the iframe. Only
embed after explicit consent. This requires a consent management mechanism.

**Option C — Privacy notice + legitimate interests:** Publish a privacy notice covering the demo,
complete a Legitimate Interests Assessment, and ensure the demo instance's data processing is
disclosed to visitors. Higher compliance burden than Option A.

**Option D — Read-only dashboard, no tracking script:** Host a Mallard Metrics dashboard showing
pre-loaded synthetic data without deploying the tracking script to GitHub Pages visitors.

---

## What Operators Must Do

This section summarises the minimum obligations for a legally compliant deployment.

### Before going live

- [ ] **Legal review:** Have a qualified data protection attorney review your deployment for your
  jurisdiction and user base
- [ ] **Privacy notice:** Publish a privacy notice on your site disclosing the data processing
  described in the [Persistently stored](#persistently-stored) table above
- [ ] **Lawful basis:** Document your chosen lawful basis (legitimate interests is typical)
- [ ] **Legitimate Interests Assessment:** If using legitimate interests, complete and document
  a balancing test (template available from the ICO and CNIL)
- [ ] **Art. 30 record:** Include analytics in your record of processing activities if required
- [ ] **Data processor agreements:** If using a hosting provider, ensure an appropriate DPA is in
  place

### Configuration

- [ ] Set `MALLARD_RETENTION_DAYS` to the shortest period that meets your analytics needs
- [ ] Do not collect custom `props` containing PII (names, emails, user IDs that are direct
  identifiers) without explicit legal basis and privacy notice disclosure
- [ ] Enable `MALLARD_SECURE_COOKIES=true` in production (TLS deployments)
- [ ] Set `MALLARD_SECRET` to a strong random value and protect it as a secret

### Ongoing

- [ ] Maintain a process for responding to data subject rights requests
- [ ] Monitor applicable supervisory authority guidance for updates to cookie-free analytics
  interpretations
- [ ] Review retention periods periodically

---

## What Mallard Metrics Does Not Do

The following activities are **not performed** by Mallard Metrics, regardless of configuration:

- Does not set HTTP cookies
- Does not read or write browser `localStorage` or `sessionStorage`
- Does not use browser fingerprinting techniques that access device APIs (WebGL, Canvas, AudioContext)
- Does not store raw IP addresses anywhere on disk
- Does not store raw User-Agent strings anywhere on disk
- Does not transmit collected data to any third party
- Does not perform cross-site tracking
- Does not serve advertising
- Does not sell data

---

## Comparison with Other Analytics Tools

| Feature | Mallard Metrics | Google Analytics 4 | Plausible Analytics | Fathom Analytics |
|---|---|---|---|---|
| Self-hosted | Yes | No | Yes (paid) / No (cloud) | No |
| Cookies set | No | Yes (session + persistent) | No | No |
| IP stored | No | Yes (anonymised) | No | No |
| Raw UA stored | No | Yes | No | No |
| Geo stored | Country/region/city | Country/region/city | Country only | Country only |
| Visitor ID type | Daily-rotating HMAC hash | Persistent cookie-based | Daily-rotating hash | Daily-rotating hash |
| GDPR applicability | Yes (pseudonymous data) | Yes (personal data) | Yes (pseudonymous data) | Yes (pseudonymous data) |
| Consent needed (ePrivacy) | No (no terminal storage access) | Yes (cookies) | No | No |
| Consent needed (GDPR) | Depends on lawful basis | Yes (or other basis) | Depends on lawful basis | Depends on lawful basis |
| Data controller | You (self-hosted operator) | Google | Plausible or you | Fathom |

---

## Definitions and Primary Sources

All legal citations below refer to publicly available primary sources.

### GDPR

| Term | Definition | Source |
|---|---|---|
| Personal data | "Any information relating to an identified or identifiable natural person" | GDPR Art. 4(1) |
| Processing | "Any operation… performed on personal data, whether or not by automated means" — includes collection, storage, retrieval, and use | GDPR Art. 4(2) |
| Pseudonymisation | "Processing of personal data in such a manner that the personal data can no longer be attributed to a specific data subject without the use of additional information" | GDPR Art. 4(5) |
| Pseudonymised data is personal data | "The principles of data protection should… not apply to anonymous information… [This] does not therefore concern the processing of such anonymous information, including for statistical or research purposes." The contrapositive: pseudonymous data does apply. | GDPR Recital 26 |
| Lawful basis | Processing is lawful only if at least one listed condition is met | GDPR Art. 6(1) |
| Legitimate interests | "Processing is necessary for the purposes of the legitimate interests pursued by the controller or by a third party, except where such interests are overridden by the interests or fundamental rights and freedoms of the data subject" | GDPR Art. 6(1)(f) |
| Transparency | Controller must provide specific information at time of collection | GDPR Art. 13 |
| Right to erasure | Data subject may request erasure in defined circumstances | GDPR Art. 17 |
| Right to object | Data subject may object to processing under legitimate interests | GDPR Art. 21 |

### Case law and regulatory guidance

| Document | Relevance |
|---|---|
| CJEU Case C‑582/14, *Breyer v. Bundesrepublik Deutschland* (19 Oct 2016) | Dynamic IP addresses are personal data for website operators |
| EDPB Opinion 05/2022 on the European Commission's Draft Decision pursuant to Art. 25(6) GDPR for the United States | Reaffirms pseudonymous data is personal data |
| EDPB Guidelines 06/2020 on targeting of social media users | Elaborates on legitimate interests balancing test |
| EDPB Guidelines 2/2019 on the processing of personal data under Article 6(1)(b) GDPR | Scope of necessity test for lawful bases |
| Article 29 Working Party Opinion 06/2014 on legitimate interests | Detailed three-part test for legitimate interests |
| ePrivacy Directive 2002/58/EC, Art. 5(3) | Terminal storage consent requirement ("cookie law") |
| ICO Guidance on legitimate interests | Practical LIA template (UK-focused, broadly applicable) — ico.org.uk |
| CNIL guidance on analytics without consent | France-specific guidance; one of the more permissive interpretations for privacy-preserving analytics — cnil.fr |

### CCPA

| Term | Source |
|---|---|
| Definition of "personal information" (includes IP addresses and unique identifiers) | Cal. Civ. Code § 1798.140(v)(1) |
| Definition of "unique identifier" | Cal. Civ. Code § 1798.140(ae) |
| Business thresholds for CCPA applicability | Cal. Civ. Code § 1798.140(d) |

---

*This document was last reviewed on 2026-03-03. Privacy law evolves; operators should verify
currency of regulatory guidance before relying on it.*
