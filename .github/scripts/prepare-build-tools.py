"""Install checksum-pinned Leptos tools before compiling, with bounded recovery.

Only archives are cached. Every use checks their digest, extracts fresh files,
and checks the native executable version. Tokens never enter Docker's context.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[2]


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def download(url, output, *, asset_api=False):
    # curl does not forward Authorization to a different host on redirects.
    # Feed credentials via stdin so command lines and exceptions cannot expose them.
    config = ''
    if asset_api and os.environ.get('GH_TOKEN'):
        token = os.environ['GH_TOKEN']
        if not re.fullmatch(r'[A-Za-z0-9_]+', token):
            raise RuntimeError('Invalid GitHub token format')
        config = f'header = "Authorization: Bearer {token}"\n'
    command = ['curl', '--config', '-', '--fail', '--silent', '--show-error',
               '--location', '--proto', '=https', '--proto-redir', '=https',
               '--connect-timeout', '15', '--max-time', '90', '--retry', '2',
               '--retry-delay', '2', '--retry-max-time', '180',
               '--output', str(output), '--write-out', '%{http_code}']
    if asset_api:
        command += ['--header', 'Accept: application/octet-stream']
    result = subprocess.run(command + [url], input=config, text=True, capture_output=True)
    return result.returncode, result.stdout.strip()


def archive_for(tool, arch, cache, fetch=download):
    spec = tool['archives'][arch]
    path = cache / (spec['sha256'] + '.tar.gz')
    if path.exists():
        if digest(path) == spec['sha256']:
            print(f"Verified cached {tool['name']} {tool['version']} ({arch})", flush=True)
            return path
        print(f"Discarding corrupt cache entry for {tool['name']} ({arch})", flush=True)
        path.unlink()
    urls = [
        (f"https://github.com/{tool['repository']}/releases/download/{tool['tag']}/{spec['name']}", False),
        (f"https://api.github.com/repos/{tool['repository']}/releases/assets/{spec['asset_id']}", True),
    ]
    failures = []
    partial = path.with_suffix('.partial')
    try:
        for url, api in urls:
            code, status = fetch(url, partial, asset_api=api)
            if code:
                failures.append(f"{'asset API' if api else 'release URL'}: HTTP {status}, curl {code}")
                print(f"{tool['name']} download failed: {failures[-1]}", flush=True)
                continue
            actual = digest(partial)
            if actual != spec['sha256']:
                raise RuntimeError(f"{tool['name']} ({arch}) checksum mismatch: expected "
                                   f"{spec['sha256']}, received {actual}; refusing to install")
            partial.replace(path)
            return path
        raise RuntimeError(f"Cannot download {tool['name']} {tool['version']} ({arch}): "
                           + '; '.join(failures))
    finally:
        partial.unlink(missing_ok=True)


def validate_lock(manifest, lock):
    versions = {p['version'] for p in lock['package'] if p['name'] == 'wasm-bindgen'}
    expected = next(t['version'] for t in manifest['tools'] if t['name'] == 'wasm-bindgen')
    if versions != {expected}:
        raise RuntimeError(f"Cargo.lock wasm-bindgen {sorted(versions)} does not match build-tools.json "
                           f"{expected}; update both architecture assets and checksums together")


def docker_recipe(original, manifest):
    # Reuse the repository's complete build recipe; add only verified build tools.
    # Fail explicitly if its insertion points change, instead of silently falling
    # back to cargo-leptos's unverified, single-attempt automatic downloads.
    sass = next(t['version'] for t in manifest['tools'] if t['name'] == 'dart-sass')
    marker = f'ENV LEPTOS_SASS_VERSION={sass}\n'
    install = 'RUN cargo binstall cargo-leptos --version "^0.3" -y'
    if original.count(marker) != 1 or original.count(install) != 1:
        raise RuntimeError('Dockerfile toolchain changed; update the verified tool insertion points')
    paths = ':'.join('/opt/localsky-build-tools/' + t['name'] +
                     (('/' + str(Path(t['binary']).parent).replace('\\', '/'))
                      if str(Path(t['binary']).parent) != '.' else '') for t in manifest['tools'])
    return original.replace(install, f'RUN cargo binstall cargo-leptos --version "={manifest["cargo_leptos"]}" -y').replace(
        marker, marker + 'COPY .ci-tools /opt/localsky-build-tools\n' + f'ENV PATH="{paths}:${{PATH}}"\n')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--dockerfile', action='store_true')
    args = parser.parse_args()
    arch = platform.machine()
    if platform.system() != 'Linux' or arch not in ('x86_64', 'aarch64'):
        raise RuntimeError(f'Unsupported build runner: {platform.system()} / {arch}')
    manifest = json.loads((ROOT / '.github/build-tools.json').read_text(encoding='utf-8'))
    validate_lock(manifest, tomllib.loads((ROOT / 'Cargo.lock').read_text(encoding='utf-8')))
    cache = Path.home() / '.cache/localsky-build-tools'
    cache.mkdir(parents=True, exist_ok=True)
    destination = ROOT / '.ci-tools'
    if destination.exists():
        raise RuntimeError('Expected a fresh .ci-tools directory; extracted executables must not be cached')
    destination.mkdir()
    proof = []
    for tool in manifest['tools']:
        spec = tool['archives'][arch]
        archive = archive_for(tool, arch, cache)
        with tempfile.TemporaryDirectory(dir=destination) as temp:
            with tarfile.open(archive) as bundle:
                bundle.extractall(temp, filter='data')
            shutil.move(str(Path(temp) / spec['directory']), str(destination / tool['name']))
        binary = destination / tool['name'] / tool['binary']
        version = subprocess.check_output([str(binary), '--version'], text=True).strip()
        if not re.search(r'(?<![\d.])' + re.escape(tool['version']) + r'(?![\d.])', version):
            raise RuntimeError(f"Unexpected {tool['name']} version: {version}")
        print(version, flush=True)
        if os.environ.get('GITHUB_PATH'):
            with open(os.environ['GITHUB_PATH'], 'a', encoding='utf-8') as stream:
                stream.write(str(binary.parent) + '\n')
        proof.append({'tool': tool['name'], 'version': version, 'sha256': spec['sha256'], 'architecture': arch})
    (ROOT / 'build-tools-proof.json').write_text(json.dumps(proof, indent=2), encoding='utf-8')
    if args.dockerfile:
        (ROOT / '.ci-Dockerfile').write_text(docker_recipe(
            (ROOT / 'Dockerfile').read_text(encoding='utf-8'), manifest), encoding='utf-8')


if __name__ == '__main__':
    main()
