"""Submit the pages that changed since the last deploy to IndexNow, run after the deploy.

Usage: indexnow.py <site dir> <manifest> <page url> <key>

The manifest maps every page of the last deploy to its hash. Without one, every page goes out once.
Pages marked noindex and alias redirects are left out. A failed submission only warns and keeps the old
manifest, so the next deploy sends the same pages again.
"""

import hashlib
import json
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path

ENDPOINT = "https://api.indexnow.org/indexnow"
# IndexNow takes at most 10,000 URLs per request
BATCH = 10_000


def pages(root):
    out = {}
    # The markdown copies and the llms files are in the sitemap too
    extra = [*root.rglob("*.md"), *root.glob("llms*.txt")]
    for path in extra:
        out[path.relative_to(root).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()

    for path in sorted(root.rglob("index.html")):
        html = path.read_bytes()
        # A noindex page asks search engines to leave it out, and an alias only redirects to its page
        if b"noindex" in html[:4096] or re.search(rb'http-equiv="?refresh', html[:4096]):
            continue

        out[path.relative_to(root).as_posix()] = hashlib.sha256(html).hexdigest()
    return out


def url(site, path):
    return site + path.removesuffix("index.html")


def submit(site, key, urls):
    host = site.split("/")[2]
    for i in range(0, len(urls), BATCH):
        body = json.dumps({"host": host, "key": key, "keyLocation": f"{site}{key}.txt", "urlList": urls[i:i + BATCH]}).encode()
        request = urllib.request.Request(ENDPOINT, data=body, headers={"Content-Type": "application/json; charset=utf-8"})
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                print(f"IndexNow accepted {len(urls[i:i + BATCH])} URLs, HTTP {response.status}")
        except urllib.error.HTTPError as error:
            print(f"::warning::IndexNow refused the submission, HTTP {error.code}: {error.read().decode(errors='replace')[:200]}")
            return False
        except urllib.error.URLError as error:
            print(f"::warning::IndexNow was not reached: {error.reason}")
            return False

    return True


def main():
    root, manifest, page_url, key = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3], sys.argv[4]
    # The page URL can be http while the site is served over https
    site = "https://" + page_url.split("://", 1)[-1]
    if not site.endswith("/"):
        site += "/"

    new = pages(root)
    old = json.loads(manifest.read_text()) if manifest.exists() else None
    if old is None:
        changed = sorted(new)
        print(f"No page list from the last deploy, announcing all {len(changed)} pages")
    else:
        # Changed and new pages, and deleted ones, so search engines drop them
        changed = sorted(p for p in new.keys() | old.keys() if new.get(p) != old.get(p))
        print(f"{len(changed)} pages changed since the last deploy")

    urls = [url(site, p) for p in changed]
    for u in urls:
        print(f"  {u}")

    if urls and not submit(site, key, urls):
        return

    manifest.write_text(json.dumps(new))


if __name__ == "__main__":
    main()
