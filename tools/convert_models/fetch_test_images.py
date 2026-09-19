"""Download a few openly licensed test photos from Wikimedia Commons (dev-time, run once).

Writes tools/fixtures/images/<name>.jpg (1024 px wide) and tools/fixtures/images/SOURCES.md with the
author, licence and page of every photo. Only CC0, public domain and CC BY photos are used.
"""

import json
import re
import urllib.parse
import urllib.request
from pathlib import Path

OUT = Path(__file__).resolve().parents[2] / "tools" / "fixtures" / "images"
API = "https://commons.wikimedia.org/w/api.php"
UA = {"User-Agent": "zoologist-test-fixtures/0.1 (dev tool)"}
OK_LICENCES = re.compile(r"^(CC0|Public domain|PD|CC BY \d)", re.I)

WANTED = {
    "deer": "white-tailed deer trail camera",
    "ringtail_night_ir": "deer infrared trail camera night",  # turned out to be a ringtail
    "fox": "red fox Vulpes vulpes grass",
    "ringtail_night": "raccoon trail camera",  # turned out to be a ringtail
    "coyote": "coyote camera trap",
    "turkey": "wild turkey Meleagris gallopavo",
    "bird_feeder": "bird feeder cardinal",
    "cat": "domestic cat garden",
    "person": "person walking sidewalk",
    "car": "car parked driveway house",
}


def get(url):
    with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=30) as r:
        return r.read()


def search(query):
    params = {
        "action": "query", "format": "json", "generator": "search", "gsrnamespace": "6",
        "gsrsearch": f"{query} filetype:bitmap", "gsrlimit": "20", "prop": "imageinfo",
        "iiprop": "url|extmetadata|mime", "iiurlwidth": "1024",
    }
    data = json.loads(get(API + "?" + urllib.parse.urlencode(params)))
    pages = sorted(data.get("query", {}).get("pages", {}).values(), key=lambda p: p.get("index", 0))
    for page in pages:
        info = page["imageinfo"][0]
        meta = info.get("extmetadata", {})
        licence = meta.get("LicenseShortName", {}).get("value", "")
        if info.get("mime") != "image/jpeg" or not OK_LICENCES.match(licence):
            continue
        artist = re.sub("<[^>]+>", "", meta.get("Artist", {}).get("value", "unknown")).strip()
        return info["thumburl"], page["title"], info["descriptionurl"], licence, artist
    return None


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    lines = ["# Test photo sources", "", "Downloaded from Wikimedia Commons by "
             "`tools/convert_models/fetch_test_images.py` (1024 px wide versions).", "",
             "| File | Photo | Author | Licence |", "|---|---|---|---|"]
    for name, query in WANTED.items():
        found = search(query)
        if not found:
            print(f"{name}: nothing suitable for {query!r}")
            continue
        url, title, page, licence, artist = found
        (OUT / f"{name}.jpg").write_bytes(get(url))
        lines.append(f"| {name}.jpg | [{title}]({page}) | {artist} | {licence} |")
        print(f"{name}: {title} ({licence})")
    (OUT / "SOURCES.md").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
