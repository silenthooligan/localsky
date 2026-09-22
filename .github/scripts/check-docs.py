"""Validate rendered documentation links and fragment targets."""
import argparse
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit


class Page(HTMLParser):
    def __init__(self, text):
        super().__init__()
        self.ids = set()
        self.links = []
        self.feed(text)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if 'id' in attrs:
            self.ids.add(attrs['id'])
        if tag == 'a' and attrs.get('name'):
            self.ids.add(attrs['name'])
        for key in ('href', 'src'):
            if key in attrs:
                self.links.append(attrs[key])


def check(root):
    root = root.resolve()
    pages = {p.resolve(): Page(p.read_text(encoding='utf-8')) for p in root.rglob('*.html')}
    failures = []
    for path, page in pages.items():
        if path.name == 'print.html':
            continue  # mdBook rewrites chapter fragments for its combined print page.
        for link in page.links:
            url = urlsplit(link)
            if url.scheme or url.netloc or link.startswith('//'):
                continue
            if url.path.startswith('/') and not url.path.startswith('/docs/'):
                continue  # Site navigation outside this book.
            name = unquote(url.path)
            target = (root/name[6:] if name.startswith('/docs/') else path.parent/name).resolve() if name else path
            if target.is_dir():
                target /= 'index.html'
            if not target.exists():
                failures.append(f'{path.name}: missing {link}')
            elif url.fragment and target in pages and unquote(url.fragment) not in pages[target].ids:
                failures.append(f'{path.name}: missing fragment {link}')
    for failure in sorted(set(failures)):
        print(failure)
    print(f'Checked {len(pages)} rendered pages; {len(set(failures))} broken links.')
    return bool(failures)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('book', type=Path, nargs='?', default=Path('docs/book'))
    raise SystemExit(check(parser.parse_args().book))
