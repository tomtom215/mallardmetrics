#!/usr/bin/env python3
"""Seed DuckDB with 30 days of realistic demo analytics data.

This script populates the mallard.duckdb database with synthetic but
realistic web analytics events for dashboard testing and demo screenshots.

Usage:
    pip3 install duckdb
    python3 .claude/seed-demo-data.py [DB_PATH]

Default DB_PATH: /tmp/mallard-demo-data/mallard.duckdb

The script generates ~7,000 events across 30 days for 500 visitors with:
- Recurring visitors across multiple weeks (for retention cohorts)
- Multi-step user journeys (for funnel analysis)
- Diverse browsers, OS, countries, referrer sources
- Realistic growth trend with weekend dips
- Revenue events on checkout pages
- Custom signup events

To start the server with this data:
    mkdir -p /tmp/mallard-demo-data/events
    python3 .claude/seed-demo-data.py
    MALLARD_DATA_DIR=/tmp/mallard-demo-data \
    MALLARD_ADMIN_PASSWORD=demodemo123 \
    MALLARD_HOST=127.0.0.1 \
    MALLARD_PORT=8000 \
    cargo run

Then open http://127.0.0.1:8000, log in with "demodemo123",
enter "demo.example.com" as site_id, select "Last 30 days", click Load.

Note: The DuckDB behavioral extension must be installed for sessions,
funnel, retention, sequences, and flow endpoints to return data.
Install it with: INSTALL behavioral FROM community; LOAD behavioral;
"""
import duckdb, random, sys, hashlib
from datetime import datetime, timedelta

DB_PATH = sys.argv[1] if len(sys.argv) > 1 else "/tmp/mallard-demo-data/mallard.duckdb"
SITE_ID = "demo.example.com"
END_DATE = datetime(2026, 3, 10)
START_DATE = END_DATE - timedelta(days=29)

PAGES = [
    "/", "/features", "/pricing", "/about", "/blog",
    "/blog/getting-started", "/blog/analytics-tips", "/blog/privacy-first",
    "/docs", "/docs/installation", "/docs/api", "/docs/configuration",
    "/signup", "/login", "/dashboard", "/settings", "/checkout", "/thank-you",
]
FUNNEL_PATHS = ["/", "/features", "/pricing", "/signup"]
CHECKOUT_FUNNEL = ["/pricing", "/signup", "/checkout", "/thank-you"]

BROWSERS = [
    ("Chrome", "122.0"), ("Chrome", "121.0"),
    ("Firefox", "123.0"), ("Firefox", "122.0"),
    ("Safari", "17.3"), ("Edge", "122.0"),
]
OSES = [
    ("Windows", "11"), ("Windows", "10"),
    ("macOS", "14.3"), ("Linux", "6.7"),
    ("iOS", "17.3"), ("Android", "14"),
]
DEVICE_TYPES = ["desktop", "desktop", "desktop", "mobile", "mobile", "tablet"]

COUNTRIES = [
    ("US", "California", "San Francisco"),
    ("US", "New York", "New York"),
    ("US", "Texas", "Austin"),
    ("GB", "England", "London"),
    ("DE", "Bavaria", "Munich"),
    ("FR", "Ile-de-France", "Paris"),
    ("CA", "Ontario", "Toronto"),
    ("AU", "NSW", "Sydney"),
    ("JP", "Tokyo", "Tokyo"),
    ("BR", "SP", "Sao Paulo"),
    ("IN", "Maharashtra", "Mumbai"),
    ("NL", "North Holland", "Amsterdam"),
]

REFERRERS = [
    ("https://www.google.com/search?q=web+analytics", "Google"),
    ("https://twitter.com/mallardmetrics", "Twitter"),
    ("https://news.ycombinator.com/item?id=12345", "Hacker News"),
    ("https://www.reddit.com/r/selfhosted/", "Reddit"),
    ("https://github.com/mallardmetrics", "GitHub"),
    (None, None),  # Direct traffic
    (None, None),  # Direct traffic (weighted)
]

SCREENS = ["1920", "1440", "1366", "1280", "768", "414", "390", "375"]

random.seed(42)  # Reproducible results

# Generate 500 visitor profiles with varying return frequencies
visitors = []
for i in range(500):
    vid = hashlib.sha256(f"visitor-{i}".encode()).hexdigest()[:32]
    browser, bver = random.choice(BROWSERS)
    os_name, osver = random.choice(OSES)

    # Return frequency distribution:
    # 30% visit once, 40% visit 2-5 times, 20% visit 5-15 times, 10% visit 15-30 times
    r = random.random()
    if r < 0.30:
        visit_days = 1
    elif r < 0.70:
        visit_days = random.randint(2, 5)
    elif r < 0.90:
        visit_days = random.randint(5, 15)
    else:
        visit_days = random.randint(15, 30)

    # Bias towards recent days for growth trend
    all_days = list(range(30))
    weights = [1 + (d / 10) for d in all_days]
    chosen_days = sorted(set(random.choices(all_days, weights=weights, k=min(visit_days, 30))))

    visitors.append({
        "id": vid, "browser": browser, "browser_version": bver,
        "os": os_name, "os_version": osver,
        "device_type": random.choice(DEVICE_TYPES),
        "country": random.choice(COUNTRIES),
        "screen_size": random.choice(SCREENS),
        "visit_days": chosen_days,
    })

# Connect and create table
conn = duckdb.connect(DB_PATH)
conn.execute("""
CREATE TABLE IF NOT EXISTS events (
    site_id VARCHAR NOT NULL, visitor_id VARCHAR NOT NULL, timestamp TIMESTAMP NOT NULL,
    event_name VARCHAR NOT NULL, pathname VARCHAR NOT NULL, hostname VARCHAR,
    referrer VARCHAR, referrer_source VARCHAR, utm_source VARCHAR, utm_medium VARCHAR,
    utm_campaign VARCHAR, utm_content VARCHAR, utm_term VARCHAR, browser VARCHAR,
    browser_version VARCHAR, os VARCHAR, os_version VARCHAR, device_type VARCHAR,
    screen_size VARCHAR, country_code VARCHAR(2), region VARCHAR, city VARCHAR,
    props VARCHAR, revenue_amount DECIMAL(12,2), revenue_currency VARCHAR(3))
""")
conn.execute(f"DELETE FROM events WHERE site_id = '{SITE_ID}'")

batch = []
for visitor in visitors:
    for day_offset in visitor["visit_days"]:
        date = START_DATE + timedelta(days=day_offset)
        # Weekend dip: skip 30% of weekend visits
        if date.weekday() >= 5 and random.random() < 0.3:
            continue

        num_pages = random.choices([1, 2, 3, 4, 5, 6, 7, 8],
                                    weights=[15, 25, 25, 15, 10, 5, 3, 2])[0]
        ref, ref_source = random.choice(REFERRERS)

        # UTM parameters on 20% of referred traffic
        utm_s = utm_m = utm_c = utm_co = utm_t = None
        if ref and random.random() < 0.2:
            utm_s = random.choice(["google", "twitter", "newsletter", "partner"])
            utm_m = random.choice(["cpc", "social", "email", "referral"])
            utm_c = random.choice(["spring2026", "launch", "brand"])

        # Build page sequence
        pages = []
        if random.random() < 0.15:
            pages.extend(FUNNEL_PATHS[:random.randint(2, 4)])
        elif random.random() < 0.08:
            pages.extend(CHECKOUT_FUNNEL[:random.randint(2, 4)])
        else:
            pages.append(random.choices(PAGES[:5], weights=[40, 15, 15, 10, 20])[0])
            for _ in range(num_pages - 1):
                pages.append(random.choice(PAGES))

        hour = random.randint(6, 23)
        minute = random.randint(0, 59)
        cc, rg, ct = visitor["country"]

        for pi, page in enumerate(pages):
            ts = date.replace(hour=hour, minute=minute, second=random.randint(0, 59)) + \
                 timedelta(seconds=pi * random.randint(10, 120))

            props = rev_amt = rev_cur = None
            if page == "/thank-you":
                rev_amt = round(random.choice([29.0, 49.0, 99.0, 199.0]), 2)
                rev_cur = "USD"
                props = '{"plan":"' + random.choice(["starter", "pro", "enterprise"]) + '"}'

            # Custom signup event
            if page == "/signup" and random.random() < 0.6:
                batch.append((
                    SITE_ID, visitor["id"], ts.strftime("%Y-%m-%d %H:%M:%S"),
                    "signup", page, SITE_ID, ref, ref_source,
                    utm_s, utm_m, utm_c, utm_co, utm_t,
                    visitor["browser"], visitor["browser_version"],
                    visitor["os"], visitor["os_version"],
                    visitor["device_type"], visitor["screen_size"],
                    cc, rg, ct, '{"method":"email"}', None, None,
                ))

            batch.append((
                SITE_ID, visitor["id"], ts.strftime("%Y-%m-%d %H:%M:%S"),
                "pageview", page, SITE_ID,
                ref if pi == 0 else None,
                ref_source if pi == 0 else None,
                utm_s if pi == 0 else None,
                utm_m if pi == 0 else None,
                utm_c if pi == 0 else None,
                utm_co if pi == 0 else None,
                utm_t if pi == 0 else None,
                visitor["browser"], visitor["browser_version"],
                visitor["os"], visitor["os_version"],
                visitor["device_type"], visitor["screen_size"],
                cc, rg, ct, props, rev_amt, rev_cur,
            ))

print(f"Inserting {len(batch)} events...")
conn.executemany(
    "INSERT INTO events VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
    batch,
)
count = conn.execute(
    f"SELECT COUNT(*) FROM events WHERE site_id = '{SITE_ID}'"
).fetchone()[0]
uv = conn.execute(
    f"SELECT COUNT(DISTINCT visitor_id) FROM events WHERE site_id = '{SITE_ID}'"
).fetchone()[0]
date_range = conn.execute(
    f"SELECT MIN(timestamp)::DATE, MAX(timestamp)::DATE FROM events WHERE site_id = '{SITE_ID}'"
).fetchone()
print(f"Done: {count} events, {uv} unique visitors, {date_range[0]} to {date_range[1]}")
conn.close()
