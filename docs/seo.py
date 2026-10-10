"""Add stable search metadata to the generated mdBook, without changing its body."""
import html
import json
from pathlib import Path
import re
import sys
from xml.etree import ElementTree as ET

BASE = 'https://rekurt.github.io/matching-engine/'
SUMMARY = 'Rust spot matching service with price-time priority, protocol v2, durable recovery, TLS and Go clients. Integration, configuration and deployment documentation.'
DESCRIPTIONS = {
    'quickstart.html': 'Build and run the Rust matching service, configure trading pairs, connect clients and submit your first orders with the matching-engine quick start.',
    'configuration.html': 'Configure matching-engine network listeners, TLS, persistence, shards and operational settings for the Rust spot matching service.',
    'trading-pairs.html': 'Configure trading pairs and instrument constraints for matching-engine. Learn how price and quantity precision affect order validation.',
    'integration.html': 'Integrate clients with matching-engine: sessions, order requests, responses and recovery using the binary wire protocol v2.',
    'clients/go.html': 'Connect Go applications to the Rust matching-engine service. Client setup, protocol v2 requests and integration examples.',
    'clients/python.html': 'Use the Python integration example to connect to matching-engine and exchange requests through its binary wire protocol v2.',
    'protocol.html': 'Matching-engine wire protocol v2 reference: binary framing, sessions, order requests, responses and durable recovery semantics.',
    'architecture.html': 'Explore the matching-engine Rust workspace: order matching, shard routing, persistence and network layers for a spot matching service.',
    'api.html': 'HTTP API reference for matching-engine: operational endpoints and integration details for running the Rust spot matching service.',
    'deployment.html': 'Deploy matching-engine with persistence and TLS. Learn about recovery procedures and operational limits of the Rust spot matching service.',
    'benchmarks.html': 'Run and interpret matching-engine benchmarks. Understand the workload and measurement scope of the Rust spot matching service.',
}

def decorate(source, relative):
    source = re.sub(r'<h1(\s[^>]*class="menu-title"[^>]*)>(.*?)</h1>', r'<p\1>\2</p>', source, flags=re.S)
    homepage = relative in ('index.html', 'introduction.html')
    excluded = relative in ('print.html', '404.html', 'toc.html')
    canonical = BASE if homepage else BASE + relative
    title_match = re.search(r'<title>(.*?)</title>', source, re.S)
    old_title = title_match.group(1) if title_match else 'Contents - matching-engine'
    title = 'matching-engine — Rust spot matching service' if homepage else html.unescape(old_title)
    description = SUMMARY if homepage else DESCRIPTIONS.get(relative, 'Documentation for matching-engine, the Rust spot matching service: ' + title)
    source = re.sub(r'<title>.*?</title>', '<title>' + html.escape(title) + '</title>', source, count=1, flags=re.S)
    # Only replace head metadata; mdBook content and navigation remain untouched.
    head_end = source.index('</head>')
    head, body = source[:head_end], source[head_end:]
    head = re.sub(r'\s*<meta\b[^>]*(?:name|property)=["\'](?:description|robots|og:[^"\']+|twitter:[^"\']+)["\'][^>]*>', '', head, flags=re.I)
    head = re.sub(r'\s*<link\b[^>]*rel=["\']canonical["\'][^>]*>', '', head, flags=re.I)
    head = re.sub(r'\s*<script[^>]*data-seo=["\']matching-engine["\'][^>]*>.*?</script>', '', head, flags=re.S)
    tags = [('name', 'description', description), ('name', 'robots', 'noindex, follow' if excluded else 'index, follow, max-image-preview:large'), ('property', 'og:type', 'website'), ('property', 'og:locale', 'en_US'), ('property', 'og:site_name', 'matching-engine'), ('property', 'og:title', title), ('property', 'og:description', description), ('property', 'og:url', canonical), ('property', 'og:image', BASE + 'assets/social.png'), ('property', 'og:image:width', '1200'), ('property', 'og:image:height', '630'), ('property', 'og:image:alt', 'matching-engine — Rust spot matching service'), ('name', 'twitter:card', 'summary_large_image'), ('name', 'twitter:title', title), ('name', 'twitter:description', description), ('name', 'twitter:image', BASE + 'assets/social.png')]
    metadata = '\n'.join(f'<meta {attr}="{key}" content="{html.escape(value, quote=True)}">' for attr, key, value in tags)
    schema = {'@context': 'https://schema.org', '@type': 'WebPage', '@id': canonical + '#webpage', 'url': canonical, 'name': title, 'description': description, 'inLanguage': 'en', 'about': {'@type': 'SoftwareSourceCode', '@id': BASE + '#software', 'name': 'matching-engine', 'codeRepository': 'https://github.com/rekurt/matching-engine', 'programmingLanguage': 'Rust', 'author': {'@type': 'Person', '@id': 'https://rekurt.github.io/#person', 'name': 'rekurt'}}}
    payload = json.dumps(schema, ensure_ascii=False).replace('<', '\\u003c')
    metadata += f'\n<link rel="canonical" href="{canonical}">\n<script type="application/ld+json" data-seo="matching-engine">{payload}</script>\n'
    return head.rstrip() + '\n' + metadata + body, None if excluded or relative == 'introduction.html' else canonical

def build(directory):
    urls = []
    for path in sorted(directory.rglob('*.html')):
        relative = path.relative_to(directory).as_posix()
        source, url = decorate(path.read_text(), relative)
        path.write_text(source)
        if url:
            urls.append(url)
    if not urls:
        raise ValueError('No generated documentation pages found')
    root = ET.Element('urlset', xmlns='http://www.sitemaps.org/schemas/sitemap/0.9')
    for url in sorted(set(urls)):
        ET.SubElement(ET.SubElement(root, 'url'), 'loc').text = url
    ET.ElementTree(root).write(directory / 'sitemap.xml', encoding='utf-8', xml_declaration=True)
    (directory / 'robots.txt').write_text('User-agent: *\nAllow: /\nSitemap: ' + BASE + 'sitemap.xml\n')
    assert (directory / 'assets/social.png').is_file(), 'Social card is missing'
    print(f'SEO metadata generated for {len(urls)} canonical documentation pages')

if __name__ == '__main__':
    build(Path(sys.argv[1]))
