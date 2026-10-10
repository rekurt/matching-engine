import tempfile
import unittest
from pathlib import Path

from assemble_site import assemble


class SiteAssemblyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.book = Path(self.temp.name) / 'book'
        self.site = Path(self.temp.name) / 'site'
        self.book.mkdir()
        self.site.mkdir()
        for name in ('index.html', 'quickstart.html', 'protocol.html', 'book.js'):
            (self.book / name).write_text('handbook ' + name)
        (self.book / 'clients').mkdir()
        (self.book / 'clients/go.html').write_text('Go client')
        (self.site / 'index.html').write_text('landing')
        self.sitemap = '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"><url><loc>https://rekurt.github.io/matching-engine/</loc></url></urlset>'
        (self.site / 'sitemap.xml').write_text(self.sitemap)
        (self.book / 'sitemap.xml').write_text(self.sitemap.replace('/</loc>', '/docs/quickstart.html</loc>'))

    def test_book_and_old_links_preserve_landing(self):
        assemble(self.book, self.site)
        self.assertEqual((self.site / 'index.html').read_text(), 'landing')
        self.assertEqual((self.site / 'docs/book.js').read_text(), 'handbook book.js')
        self.assertEqual((self.site / 'docs/protocol.html').read_text(), 'handbook protocol.html')
        redirect = (self.site / 'protocol.html').read_text()
        self.assertIn('./docs/protocol.html', redirect)
        self.assertIn('location.search + location.hash', redirect)
        sitemap = (self.site / 'sitemap.xml').read_text()
        self.assertIn('https://rekurt.github.io/matching-engine/</loc>', sitemap)
        self.assertIn('https://rekurt.github.io/matching-engine/docs/quickstart.html</loc>', sitemap)

    def test_nested_old_links_resolve_relative_to_the_old_directory(self):
        assemble(self.book, self.site)
        redirect = (self.site / 'clients/go.html').read_text()
        self.assertIn('../docs/clients/go.html', redirect)
        self.assertIn('location.search + location.hash', redirect)
        self.assertEqual((self.site / 'docs/clients/go.html').read_text(), 'Go client')

    def test_incomplete_build_does_not_change_site(self):
        (self.book / 'quickstart.html').unlink()
        with self.assertRaises(ValueError):
            assemble(self.book, self.site)
        self.assertFalse((self.site / 'docs').exists())

    def test_collision_does_not_replace_existing_page(self):
        (self.site / 'protocol.html').write_text('existing page')
        with self.assertRaises(ValueError):
            assemble(self.book, self.site)
        self.assertFalse((self.site / 'docs').exists())
        self.assertEqual((self.site / 'protocol.html').read_text(), 'existing page')


if __name__ == '__main__':
    unittest.main()
