"""Write a markdown copy of every docs page, SKILL.md, llms.txt and llms-full.txt, run after `zola build`"""

import re
import sys
import tomllib
from pathlib import Path

SITE = Path(__file__).resolve().parent.parent
REPO = SITE.parent
PUBLIC = Path(sys.argv[1]) if len(sys.argv) > 1 else SITE / "public"
DOCS = SITE / "content" / "docs"


def read(path):
    _, front, body = path.read_text().split("+++", 2)
    return tomllib.loads(front), body.strip()


def page_url(base, name, fragment=""):
    stem = Path(name).stem
    # A fragment points into the HTML page, the markdown copy has no anchors
    if fragment:
        return f"{base}/docs/{stem}/{fragment}" if stem != "_index" else f"{base}/docs/{fragment}"

    return f"{base}/docs.md" if stem == "_index" else f"{base}/docs/{stem}.md"


def convert(body, base):
    body = re.sub(
        r"\(@/docs/([^)#]+\.md)(#[^)]*)?\)",
        lambda m: f"({page_url(base, m.group(1), m.group(2) or '')})",
        body,
    )
    # Site-relative links become absolute, so the copy works outside the site
    return re.sub(r"\]\(/(?!/)", f"]({base}/", body)


def copy(meta, body, html_url, base):
    lines = [f"# {meta['title']}", "", html_url, ""]

    if meta.get("description"):
        lines += [f"> {meta['description']}", ""]

    return "\n".join(lines) + "\n" + convert(body, base) + "\n"


def main():
    config = tomllib.loads((SITE / "config.toml").read_text())
    base = config["base_url"].rstrip("/")
    pages = []

    for path in DOCS.glob("*.md"):
        meta, body = read(path)
        weight = -1 if path.stem == "_index" else meta.get("weight", 999)
        pages.append((weight, path.stem, meta, body))

    pages.sort(key=lambda p: (p[0], p[1]))
    full = []

    for _, stem, meta, body in pages:
        html = f"{base}/docs/" if stem == "_index" else f"{base}/docs/{stem}/"
        out = PUBLIC / "docs.md" if stem == "_index" else PUBLIC / "docs" / f"{stem}.md"
        text = copy(meta, body, html, base)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(text)
        full.append(text)

    skill = REPO / "SKILL.md"
    (PUBLIC / "SKILL.md").write_text(skill.read_text())

    intro = pages[0][3].split("\n\n")[0]
    quickstart = next(body for _, stem, _, body in pages if stem == "quickstart")
    setup = convert(re.search(r"^## Setup\n\n(.*?)\n\n", quickstart, re.S | re.M).group(1), base)
    lines = [f"# {config['title']}", "", f"> {config['description']}", "", intro, "", "## Setup", "", setup, ""]
    lines += ["## Docs", ""]

    for _, stem, meta, _ in pages[1:]:
        lines.append(f"- [{meta['title']}]({page_url(base, stem + '.md')}): {meta.get('description', '')}".rstrip(": "))

    lines += [
        "",
        "## Agents",
        "",
        f"- [SKILL.md]({base}/SKILL.md): how an agent runs shipcheck, reads its result and fixes a failed check",
        "",
        "## Optional",
        "",
        f"- [All docs in one file]({base}/llms-full.txt)",
        "- [Source](https://github.com/gabrielkoerich/shipcheck): the Rust CLI, MIT",
    ]

    (PUBLIC / "llms.txt").write_text("\n".join(lines) + "\n")
    (PUBLIC / "llms-full.txt").write_text("\n".join(full))

    # The markdown copies and agent files join Zola's sitemap, _headers names each copy's HTML page as canonical
    extra = ["docs.md"] + [f"docs/{stem}.md" for _, stem, _, _ in pages[1:]]
    extra += ["llms.txt", "llms-full.txt", "SKILL.md"]
    sitemap = PUBLIC / "sitemap.xml"
    urls = "".join(f"    <url>\n        <loc>{base}/{path}</loc>\n    </url>\n" for path in extra)
    sitemap.write_text(sitemap.read_text().replace("</urlset>", urls + "</urlset>"))
    print(f"wrote {len(pages)} markdown copies, SKILL.md, llms.txt and llms-full.txt into {PUBLIC}")


if __name__ == "__main__":
    main()
