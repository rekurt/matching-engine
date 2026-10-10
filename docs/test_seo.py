import json
import re
import unittest
from seo import BASE, decorate

class SeoTests(unittest.TestCase):
    source = '<html><head><title>Go client - matching-engine</title><meta name="description" content="old"></head><body><h1>Content</h1></body></html>'

    def test_nested_and_idempotent(self):
        result, url = decorate(self.source, 'clients/go.html')
        self.assertEqual(url, BASE + 'clients/go.html')
        self.assertIn('<body><h1>Content</h1></body>', result)
        self.assertNotIn('content="old"', result)
        self.assertEqual(decorate(result, 'clients/go.html')[0].strip(), result.strip())
        schema = json.loads(re.search(r'data-seo="matching-engine">(.*?)</script>', result).group(1))
        self.assertEqual(schema['url'], url)
        self.assertEqual(result.count('rel="canonical"'), 1)

    def test_menu_title_preserves_style_without_extra_heading(self):
        source = self.source.replace('<body>', '<body><h1 class="menu-title">matching-engine</h1>')
        result, _ = decorate(source, 'index.html')
        self.assertIn('<p class="menu-title">matching-engine</p>', result)
        self.assertEqual(result.count('<h1'), 1)

    def test_home_duplicate(self):
        for name in ('index.html', 'introduction.html'):
            result, url = decorate(self.source, name)
            self.assertIn('href="' + BASE + '"', result)
            self.assertEqual(url, BASE if name == 'index.html' else None)

    def test_excluded_pages(self):
        for name in ('print.html', '404.html'):
            result, url = decorate(self.source, name)
            self.assertIsNone(url)
            self.assertIn('noindex, follow', result)

if __name__ == '__main__':
    unittest.main()
