# Fetching the Full PeeringDB Database (Language-Agnostic)

This document explains how to fetch the complete PeeringDB database via HTTP API. Use this to implement a sync client in any programming language.

---

## TL;DR - Fastest Full Fetch

For a complete database dump, fetch pre-built JSON files from the CDN:

```
GET https://public.peeringdb.com/{resource}-0.json
```

Example:
```bash
curl https://public.peeringdb.com/net-0.json
```

---

## Data Sources

### 1. CDN Cache (Recommended for Full Sync)

**Base URL:** `https://public.peeringdb.com`

**Format:** `/{resource}-0.json`

| Pros | Cons |
|------|------|
| Fastest download speeds | Updated periodically (not real-time) |
| No rate limiting | No filtering/query parameters |
| No authentication needed | Public data only (no `poc` contacts) |

**Example URLs:**
```
https://public.peeringdb.com/org-0.json
https://public.peeringdb.com/net-0.json
https://public.peeringdb.com/fac-0.json
https://public.peeringdb.com/ix-0.json
```

### 2. Live API (For Incremental/Filtered/Private Data)

**Base URL:** `https://www.peeringdb.com/api`

**Format:** `/{resource}`

| Pros | Cons |
|------|------|
| Real-time data | Rate limited |
| Supports query parameters | Slower for bulk fetches |
| Supports authentication | May require API key |

---

## Resources to Fetch

Fetch these 13 resources in order (respects foreign key dependencies):

| # | Resource | Endpoint | Description |
|---|----------|----------|-------------|
| 1 | `org` | `/api/org` | Organizations |
| 2 | `campus` | `/api/campus` | Campus locations |
| 3 | `fac` | `/api/fac` | Data center facilities |
| 4 | `net` | `/api/net` | Networks (ASNs) |
| 5 | `ix` | `/api/ix` | Internet exchanges |
| 6 | `carrier` | `/api/carrier` | Carrier companies |
| 7 | `carrierfac` | `/api/carrierfac` | Carrier ↔ Facility links |
| 8 | `ixfac` | `/api/ixfac` | IX ↔ Facility links |
| 9 | `ixlan` | `/api/ixlan` | IX LAN segments |
| 10 | `ixpfx` | `/api/ixpfx` | IX LAN prefixes |
| 11 | `netfac` | `/api/netfac` | Network ↔ Facility links |
| 12 | `netixlan` | `/api/netixlan` | Network ↔ IX connections |
| 13 | `poc` | `/api/poc` | Network contacts (requires auth) |

---

## Response Format

All endpoints return JSON with this structure:

```json
{
  "meta": {},
  "data": [
    {
      "id": 1,
      "status": "ok",
      "created": "2010-01-01T00:00:00Z",
      "updated": "2024-06-15T12:30:00Z",
      ...resource-specific fields...
    },
    ...
  ]
}
```

**Key fields present in all resources:**
- `id` - Unique identifier (integer)
- `status` - Object status: `"ok"`, `"pending"`, `"deleted"`
- `created` - ISO 8601 timestamp
- `updated` - ISO 8601 timestamp (used for incremental sync)

---

## Full Sync Algorithm

### Step 1: Fetch All Resources

```
FOR each resource IN [org, campus, fac, net, ix, carrier, carrierfac, ixfac, ixlan, ixpfx, netfac, netixlan, poc]:
    response = HTTP GET https://public.peeringdb.com/{resource}-0.json
    data = JSON_PARSE(response.body)["data"]

    FOR each object IN data:
        INSERT object INTO local_database

    RECORD max(updated) timestamp for this resource
```

### Step 2: Store Last Sync Timestamp

After fetching each resource, store the maximum `updated` timestamp:

```
last_sync[resource] = MAX(object.updated for all objects)
```

### Step 3: Incremental Sync (Later)

For subsequent syncs, only fetch objects modified after your last sync:

```
GET https://www.peeringdb.com/api/{resource}?since={unix_timestamp}
```

---

## API Query Parameters

When using the live API (`www.peeringdb.com/api`):

| Parameter | Description | Example |
|-----------|-------------|---------|
| `since` | Unix timestamp - only return objects updated after this time | `?since=1704067200` |
| `id` | Filter by ID | `?id=1` |
| `depth` | Include related objects (0-2) | `?depth=1` |
| `limit` | Max results per page | `?limit=100` |
| `skip` | Offset for pagination | `?skip=100` |

**Incremental sync example:**
```
GET https://www.peeringdb.com/api/net?since=1704067200
```
Returns only networks updated after Jan 1, 2024 00:00:00 UTC.

---

## Authentication

### API Key (Recommended)

```
GET /api/net
Authorization: Api-Key YOUR_API_KEY
```

### Basic Auth (Alternative)

```
GET /api/net
Authorization: Basic BASE64(username:password)
```

### When Authentication is Required

- **Public data (org, fac, net, ix, etc.):** No auth required
- **Private data (poc contacts, private IX fields):** API key required

---

## Rate Limiting

The live API enforces rate limits:

**Response:** `HTTP 429 Too Many Requests`

**Handling strategy:**
```
IF response.status == 429:
    wait_seconds = MIN(2 ^ retry_count, 60)
    SLEEP(wait_seconds)
    retry_count++
    RETRY request
```

The CDN cache (`public.peeringdb.com`) has no rate limiting.

---

## Example: Full Fetch in Pseudocode

```
RESOURCES = ["org", "campus", "fac", "net", "ix", "carrier",
             "carrierfac", "ixfac", "ixlan", "ixpfx",
             "netfac", "netixlan"]

FUNCTION fetch_full_database():
    FOR resource IN RESOURCES:
        url = "https://public.peeringdb.com/" + resource + "-0.json"
        response = HTTP_GET(url)

        IF response.status != 200:
            FAIL("Could not fetch " + resource)

        json = JSON_PARSE(response.body)
        objects = json["data"]

        max_updated = 0
        FOR obj IN objects:
            SAVE_TO_DATABASE(resource, obj)
            IF obj["updated"] > max_updated:
                max_updated = obj["updated"]

        SAVE_LAST_SYNC(resource, max_updated)

        PRINT("Fetched " + LENGTH(objects) + " " + resource + " objects")
```

---

## Example: Fetch with curl

```bash
# Fetch all networks from CDN
curl -o net.json https://public.peeringdb.com/net-0.json

# Fetch all facilities from CDN
curl -o fac.json https://public.peeringdb.com/fac-0.json

# Fetch from live API with auth
curl -H "Authorization: Api-Key YOUR_KEY" \
     https://www.peeringdb.com/api/net

# Incremental fetch (changes since timestamp)
curl "https://www.peeringdb.com/api/net?since=1704067200"
```

---

## Example: Full Fetch Script (Bash)

```bash
#!/bin/bash

CDN_URL="https://public.peeringdb.com"
RESOURCES="org campus fac net ix carrier carrierfac ixfac ixlan ixpfx netfac netixlan"
OUTPUT_DIR="./peeringdb_data"

mkdir -p "$OUTPUT_DIR"

for resource in $RESOURCES; do
    echo "Fetching $resource..."
    curl -s -o "$OUTPUT_DIR/${resource}.json" "$CDN_URL/${resource}-0.json"

    count=$(jq '.data | length' "$OUTPUT_DIR/${resource}.json")
    echo "  Downloaded $count $resource objects"
done

echo "Done! Data saved to $OUTPUT_DIR/"
```

---

## Data Schema Examples

### Network (`net`)

```json
{
  "id": 1,
  "org_id": 10,
  "name": "Example Network",
  "asn": 65000,
  "website": "https://example.com",
  "looking_glass": "https://lg.example.com",
  "route_server": "https://rs.example.com",
  "irr_as_set": "AS-EXAMPLE",
  "info_type": "NSP",
  "info_prefixes4": 1000,
  "info_prefixes6": 500,
  "info_traffic": "100-1000Gbps",
  "info_ratio": "Balanced",
  "info_scope": "Global",
  "info_unicast": true,
  "info_multicast": false,
  "info_ipv6": true,
  "policy_url": "https://example.com/peering",
  "policy_general": "Open",
  "policy_locations": "Required - US",
  "policy_ratio": false,
  "policy_contracts": "Not Required",
  "status": "ok",
  "created": "2010-01-01T00:00:00Z",
  "updated": "2024-06-15T12:30:00Z"
}
```

### Facility (`fac`)

```json
{
  "id": 1,
  "org_id": 5,
  "name": "Example DC",
  "website": "https://dc.example.com",
  "address1": "123 Data Center Way",
  "address2": "",
  "city": "San Francisco",
  "state": "CA",
  "zipcode": "94105",
  "country": "US",
  "latitude": 37.7749,
  "longitude": -122.4194,
  "status": "ok",
  "created": "2010-01-01T00:00:00Z",
  "updated": "2024-06-15T12:30:00Z"
}
```

### Internet Exchange (`ix`)

```json
{
  "id": 1,
  "org_id": 3,
  "name": "Example IX",
  "name_long": "Example Internet Exchange",
  "city": "Amsterdam",
  "country": "NL",
  "region_continent": "Europe",
  "media": "Ethernet",
  "proto_unicast": true,
  "proto_multicast": false,
  "proto_ipv6": true,
  "website": "https://ix.example.com",
  "tech_email": "tech@ix.example.com",
  "tech_phone": "+31 20 123 4567",
  "policy_email": "policy@ix.example.com",
  "policy_phone": "+31 20 123 4568",
  "status": "ok",
  "created": "2010-01-01T00:00:00Z",
  "updated": "2024-06-15T12:30:00Z"
}
```

### Network-IX Connection (`netixlan`)

```json
{
  "id": 12345,
  "net_id": 100,
  "ix_id": 50,
  "ixlan_id": 75,
  "name": "Example IX",
  "asn": 65000,
  "ipaddr4": "192.0.2.100",
  "ipaddr6": "2001:db8::100",
  "speed": 100000,
  "is_rs_peer": true,
  "operational": true,
  "status": "ok",
  "created": "2015-06-01T00:00:00Z",
  "updated": "2024-06-15T12:30:00Z"
}
```

---

## Foreign Key Relationships

When storing data, resolve these relationships:

| Resource | Foreign Key | References |
|----------|-------------|------------|
| `campus` | `org_id` | `org.id` |
| `fac` | `org_id` | `org.id` |
| `net` | `org_id` | `org.id` |
| `ix` | `org_id` | `org.id` |
| `carrier` | `org_id` | `org.id` |
| `carrierfac` | `carrier_id`, `fac_id` | `carrier.id`, `fac.id` |
| `ixfac` | `ix_id`, `fac_id` | `ix.id`, `fac.id` |
| `ixlan` | `ix_id` | `ix.id` |
| `ixpfx` | `ixlan_id` | `ixlan.id` |
| `netfac` | `net_id`, `fac_id` | `net.id`, `fac.id` |
| `netixlan` | `net_id`, `ixlan_id` | `net.id`, `ixlan.id` |
| `poc` | `net_id` | `net.id` |

**Fetch order matters!** Fetch parent resources before children to satisfy foreign keys.

---

## Summary

| Task | URL Pattern |
|------|-------------|
| Full sync (fast) | `https://public.peeringdb.com/{resource}-0.json` |
| Full sync (live) | `https://www.peeringdb.com/api/{resource}` |
| Incremental sync | `https://www.peeringdb.com/api/{resource}?since={timestamp}` |
| Single object | `https://www.peeringdb.com/api/{resource}?id={id}` |
| Private data | Add `Authorization: Api-Key YOUR_KEY` header |
