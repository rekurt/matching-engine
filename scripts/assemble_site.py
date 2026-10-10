#!/usr/bin/env python3
"""Include the handbook and preserve links from the former mdBook root."""
import argparse
import html
import json
import shutil
from pathlib import Path
from xml.etree import ElementTree as ET


def assemble(book: Path, site: Path) -> None:
    for file in (book / 'index.html', book / 'quickstart.html', site / 'index.html'):
        if not file.is_file():
            raise ValueError(f'Missing build artifact: {file}')
    if (site / 'docs').exists():
        raise ValueError('The site already contains a docs directory')
    pages = [page.relative_to(book) for page in book.rglob('*.html')
             if page.relative_to(book).as_posix() not in ('index.html', '404.html')]
    for page in pages:
        if (site / page).exists():
            raise ValueError(f'Handbook redirect would replace a site page: {page}')
    namespace = 'http://www.sitemaps.org/schemas/sitemap/0.9'
    ET.register_namespace('', namespace)
    sitemap = ET.parse(site / 'sitemap.xml')
    handbook = ET.parse(book / 'sitemap.xml')
    known = {node.text for node in sitemap.findall(f'.//{{{namespace}}}loc')}
    for entry in handbook.getroot():
        location = entry.find(f'{{{namespace}}}loc')
        if location is not None and location.text not in known:
            sitemap.getroot().append(entry)
            known.add(location.text)
    shutil.copytree(book, site / 'docs')
    sitemap.write(site / 'sitemap.xml', encoding='utf-8', xml_declaration=True)
    for page in pages:
        target = ('../' * (len(page.parts) - 1) or './') + 'docs/' + page.as_posix()
        escaped = html.escape(target, quote=True)
        (site / page).parent.mkdir(parents=True, exist_ok=True)
        (site / page).write_text(
            '<!doctype html><html lang="en"><meta charset="utf-8">'
            '<meta name="viewport" content="width=device-width, initial-scale=1">'
            '<meta name="robots" content="noindex"><title>Handbook page moved</title>'
            f'<meta http-equiv="refresh" content="0;url={escaped}">'
            f'<script>location.replace({json.dumps(target)} + location.search + location.hash)</script>'
            f'<a href="{escaped}">Continue to the handbook</a></html>\n', encoding='utf-8')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--book', type=Path, default=Path('docs/book'))
    parser.add_argument('--site', type=Path, required=True)
    args = parser.parse_args()
    assemble(args.book, args.site)
