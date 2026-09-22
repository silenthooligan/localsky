"""Build the public guide on a Linux runner with a verified mdBook binary."""
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[2]


def main():
    spec = importlib.util.spec_from_file_location('build_tools', ROOT/'.github/scripts/prepare-build-tools.py')
    tools = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(tools)
    tool = {'name': 'mdbook', 'repository': 'rust-lang/mdBook', 'tag': 'v0.5.3', 'version': '0.5.3', 'archives': {'x86_64': {
        'name': 'mdbook-v0.5.3-x86_64-unknown-linux-gnu.tar.gz', 'asset_id': 424597729,
        'sha256': 'e2fd508a4fac06cbaa9f88b97d27bdc3b55a08946304ca845879fe26a3699e11'}}}
    with tempfile.TemporaryDirectory() as temp:
        archive = tools.archive_for(tool, 'x86_64', Path(temp))
        with tarfile.open(archive) as bundle:
            bundle.extractall(temp, filter='data')
        binary = Path(temp)/'mdbook'
        subprocess.run([str(binary), '--version'], check=True)
        tokens = {
            'LOCALSKY_VERSION': tomllib.loads((ROOT/'Cargo.toml').read_text())['package']['version'],
            'LOCALSKY_API_VERSION': re.search(r'pub const API_VERSION: &str = "([^"]+)"', (ROOT/'src/api/info.rs').read_text()).group(1),
            'LOCALSKY_DB_MIGRATIONS': str(len(list((ROOT/'src/persistence/migrations').glob('M*.sql')))),
            'LOCALSKY_SKIP_RULES': str((ROOT/'src/gates_catalog.rs').read_text().count('        (\n')),
        }
        for page in (ROOT/'docs/src').glob('*.md'):
            text = page.read_text(encoding='utf-8')
            for key, value in tokens.items():
                text = text.replace('{{'+key+'}}', value)
            if '{{LOCALSKY_' in text:
                raise RuntimeError(f'Unresolved documentation token in {page.name}')
            page.write_text(text, encoding='utf-8')
        subprocess.run([str(binary), 'build', 'docs'], cwd=ROOT, check=True)
        revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
        (ROOT/'docs/book/build-info.json').write_text(json.dumps({
            'revision': revision, 'service_version': tokens['LOCALSKY_VERSION'],
            'api_version': tokens['LOCALSKY_API_VERSION'], 'mdbook': '0.5.3',
        }, indent=2)+'\n', encoding='utf-8')


if __name__ == '__main__':
    main()
