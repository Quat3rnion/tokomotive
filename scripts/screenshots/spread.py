#!/usr/bin/env python3
"""Spread demo Ledger timestamps over 24 hours; run only with the Proxy stopped."""
import math
import random
import sqlite3
import time
from env import DATABASE

random.seed(11)
db = sqlite3.connect(DATABASE)
now = int(time.time() * 1000)
start, end = now - 23 * 3600_000 - 40 * 60_000, now - 3 * 60_000
ids = [row[0] for row in db.execute("SELECT id FROM ledger ORDER BY id")]
assert ids, "traffic produced no Ledger Entries"


def weight(timestamp):
    local = time.localtime(timestamp / 1000)
    hour = local.tm_hour + local.tm_min / 60
    return 0.25 + max(0.0, math.sin((hour - 7) / 13 * math.pi)) if 7 <= hour <= 20 else 0.25


timestamps = []
while len(timestamps) < len(ids):
    stamp = random.randint(start, end)
    if random.random() < weight(stamp) / 1.25:
        timestamps.append(stamp)
for row_id, stamp in zip(ids, sorted(timestamps)):
    db.execute("UPDATE ledger SET ts_ms = ? WHERE id = ?", (stamp, row_id))
db.execute("UPDATE virtual_keys SET last_used_at = (SELECT MAX(ts_ms) / 1000 FROM ledger WHERE key_id = virtual_keys.id) WHERE id IN (SELECT DISTINCT key_id FROM ledger)")
for table in ("users", "virtual_keys", "teams"):
    db.execute(f"UPDATE {table} SET created_at = created_at - ?", (26 * 3600,))
db.execute("UPDATE cards SET created_at = created_at - ?", (3 * 86400,))
db.execute("UPDATE cards SET created_at = created_at + 86400 WHERE kind = 'price'")
db.execute("DELETE FROM rollup_hourly")
db.execute("""INSERT INTO rollup_hourly(hour, user_id, model, requests, errors, input, output, cache_read,
    cache_write_5m, cache_write_1h, cost_micros, price_micros, latency_ms_sum, ttft_ms_sum, ttft_count,
    image_input, image_output, service_units)
    SELECT ts_ms / 3600000 * 3600, user_id, COALESCE(m.name, l.model, l.requested_model, ''),
    COUNT(*), SUM(status >= 400), SUM(input), SUM(output), SUM(cache_read), SUM(cache_write_5m),
    SUM(cache_write_1h), SUM(cost_micros), SUM(price_micros), SUM(latency_ms), IFNULL(SUM(ttft_ms), 0),
    COUNT(ttft_ms), SUM(image_input), SUM(image_output), SUM(service_units)
    FROM ledger l LEFT JOIN models m ON m.id = l.model_id
    GROUP BY ts_ms / 3600000 * 3600, user_id, COALESCE(m.name, l.model, l.requested_model, '')""")
db.commit()
totals = db.execute("SELECT (SELECT SUM(price_micros) FROM ledger), (SELECT SUM(price_micros) FROM rollup_hourly), (SELECT COUNT(*) FROM ledger), (SELECT SUM(requests) FROM rollup_hourly)").fetchone()
assert totals[0] == totals[1] and totals[2] == totals[3], totals
db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
db.close()
print("spread Ledger and rebuilt rollups:", totals)
