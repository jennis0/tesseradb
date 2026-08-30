"""A small projected corpus for the phase-F basemap capture.

Every Nth GeoNames row: entity id, longitude, latitude, country code. Nothing is projected here —
the point of the corpus is that the *build* projects it, from `lon`/`lat` in degrees.
"""
import io
import sys
import zipfile

import pyarrow as pa
import pyarrow.parquet as pq

SRC = "/mnt/nas/joe/tessera/datasets/geonames/2026-08-27/allCountries.zip"
OUT = sys.argv[1]
STRIDE = int(sys.argv[2]) if len(sys.argv) > 2 else 60

ids, lons, lats, countries = [], [], [], []
with zipfile.ZipFile(SRC) as z:
    with z.open("allCountries.txt") as f:
        for i, raw in enumerate(io.TextIOWrapper(f, encoding="utf-8")):
            if i % STRIDE:
                continue
            c = raw.split("\t")
            try:
                lat, lon = float(c[4]), float(c[5])
            except (IndexError, ValueError):
                continue
            ids.append(len(ids))
            lons.append(lon)
            lats.append(lat)
            countries.append(c[8] or "public")

t = pa.table(
    {
        "entity_id": pa.array(ids, pa.uint64()),
        "lon": pa.array(lons, pa.float64()),
        "lat": pa.array(lats, pa.float64()),
        "country": pa.array(countries, pa.string()),
    }
)
pq.write_table(t, OUT)
print(f"{len(ids):,} points -> {OUT}")
print(f"lon [{min(lons)}, {max(lons)}]  lat [{min(lats)}, {max(lats)}]")
print(f"countries: {len(set(countries)):,}")
