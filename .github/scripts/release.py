"""Release validation, image promotion and idempotent publication.

The release workflow owns the dependency graph. This helper never substitutes
another revision's checks or treats a missing/failed check as success.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time
import tomllib


def version_key(tag):
    match = re.fullmatch(r'v(\d+)\.(\d+)\.(\d+)(?:-(alpha|beta|rc)(?:[.-]?(\d+))?)?', tag)
    if not match:
        raise ValueError(f'Unsupported release tag: {tag}')
    major, minor, patch, kind, number = match.groups()
    return (int(major), int(minor), int(patch),
            {'alpha': 0, 'beta': 1, 'rc': 2, None: 3}[kind], int(number or 0))


def api(path, *, missing_ok=False):
    for attempt in range(3):
        result = subprocess.run(['gh', 'api', path], capture_output=True, text=True)
        if result.returncode == 0:
            return json.loads(result.stdout)
        if missing_ok and '(HTTP 404)' in result.stderr:
            return None
        if not re.search(r'HTTP (429|500|502|503|504)', result.stderr) or attempt == 2:
            raise RuntimeError(f'GitHub API request failed: {path}: {result.stderr.strip()}')
        time.sleep(2 ** attempt)
    raise AssertionError('unreachable')


def validate(root=Path('.'), *, verify_only=False):
    ref = os.environ['GITHUB_REF']
    if not verify_only and not ref.startswith('refs/tags/'):
        raise ValueError('Release requires a version tag; dispatch with --ref vX.Y.Z')
    package = tomllib.loads((root / 'Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    tag = 'v' + package if verify_only else ref.removeprefix('refs/tags/')
    version_key(tag)
    if tag != 'v' + package:
        raise ValueError(f'Release tag {tag} differs from Cargo.toml version {package}')
    sha = os.environ['GITHUB_SHA'] if verify_only else subprocess.check_output(
        ['git', 'rev-parse', f'{tag}^{{commit}}'], text=True).strip()
    head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    if sha != head or sha != os.environ['GITHUB_SHA']:
        raise ValueError(f'Release source mismatch: tag={sha}, checkout={head}, event={os.environ["GITHUB_SHA"]}')
    changelog = (root / 'CHANGELOG.md').read_text(encoding='utf-8')
    section = re.search(r'^## \[' + re.escape(package) + r'\][^\n]*\n(.*?)(?=^## \[|\Z)',
                        changelog, re.M | re.S)
    if not section or not section[1].strip():
        raise ValueError(f'Missing CHANGELOG.md entry for {package}')
    return tag, section[1].strip()


def promotion_tags(tag, latest):
    key = version_key(tag)
    tags = [tag[1:]]
    # Alpha/rc are previews. Betas retain the established Latest policy.
    if key[3] in (0, 2):
        return tags
    if latest is None or key >= version_key(latest):
        tags += [f'{key[0]}.{key[1]}', 'latest']
    return tags


def image_refs(directory, repository, sha):
    refs = []
    for arch in ('amd64', 'arm64'):
        proof = json.loads((directory / f'{arch}.json').read_text(encoding='utf-8'))
        if proof.get('architecture') != arch or proof.get('source') != sha:
            raise ValueError(f'Image proof does not match {arch} / {sha}')
        if not re.fullmatch(r'sha256:[a-f0-9]{64}', proof.get('digest', '')):
            raise ValueError(f'Invalid {arch} image digest')
        refs.append(f'ghcr.io/{repository}@{proof["digest"]}')
    return refs


def promote(directory):
    tag, _ = validate()
    repo = os.environ['GITHUB_REPOSITORY']
    refs = image_refs(directory, repo, os.environ['GITHUB_SHA'])
    latest = api(f'repos/{repo}/releases/latest', missing_ok=True)
    tags = promotion_tags(tag, latest['tag_name'] if latest else None)
    ghcr = f'ghcr.io/{repo}'
    command = ['docker', 'buildx', 'imagetools', 'create']
    for item in tags:
        command += ['--tag', f'{ghcr}:{item}']
    subprocess.run(command + refs, check=True)
    inspected = subprocess.check_output(['docker', 'buildx', 'imagetools', 'inspect',
                                         f'{ghcr}:{tag[1:]}', '--format', '{{json .Manifest}}'], text=True)
    manifest = json.loads(inspected)
    digest = manifest['digest']
    if not re.fullmatch(r'sha256:[a-f0-9]{64}', digest):
        raise ValueError('Invalid published manifest digest')
    proof = {'source': os.environ['GITHUB_SHA'], 'tag': tag, 'digest': digest, 'tags': tags, 'images': refs}
    Path('release-images.json').write_text(json.dumps(proof, indent=2), encoding='utf-8')
    if os.environ.get('DOCKERHUB_USERNAME') and os.environ.get('DOCKERHUB_TOKEN'):
        username = os.environ['DOCKERHUB_USERNAME']
        subprocess.run(['docker', 'login', '--username', username, '--password-stdin'],
                       input=os.environ['DOCKERHUB_TOKEN'], text=True, check=True)
        mirror = ['docker', 'buildx', 'imagetools', 'create']
        for item in tags:
            mirror += ['--tag', f'docker.io/{username}/localsky:{item}']
        subprocess.run(mirror + [f'{ghcr}@{digest}'], check=True)
        for item in tags:
            remote = subprocess.check_output(['docker', 'buildx', 'imagetools', 'inspect',
                f'docker.io/{username}/localsky:{item}', '--format', '{{json .Manifest}}'], text=True)
            if json.loads(remote)['digest'] != digest:
                raise ValueError(f'Docker Hub {item} digest differs from GHCR')
    print(json.dumps(proof), flush=True)


def publish():
    tag, notes = validate()
    repo = os.environ['GITHUB_REPOSITORY']
    existing = api(f'repos/{repo}/releases/tags/{tag}', missing_ok=True)
    preview = version_key(tag)[3] in (0, 2)
    if existing:
        if existing['draft'] or existing['prerelease'] != preview:
            raise ValueError(f'Existing release {tag} has unexpected draft/prerelease state')
        print(f'Release already published; preserving its notes and Latest status: {existing["html_url"]}')
        return
    latest = api(f'repos/{repo}/releases/latest', missing_ok=True)
    tags = promotion_tags(tag, latest['tag_name'] if latest else None)
    body = (f'## Run it\n\n```bash\ndocker run -d -p 8090:8090 -v localsky-data:/data '
            f'ghcr.io/{repo}:{tag[1:]}\n```\n\n'
            'Multi-arch image (amd64 + arm64). Quick start: https://localsky.io/docs/getting-started\n\n'
            + notes + f'\n\nBuild, native startup and image scan results: '
            f'https://github.com/{repo}/actions/runs/{os.environ["GITHUB_RUN_ID"]}\n')
    path = Path('release-notes.md')
    path.write_text(body, encoding='utf-8')
    command = ['gh', 'release', 'create', tag, '--repo', repo, '--verify-tag',
               '--title', tag, '--notes-file', str(path)]
    command += ['--prerelease'] if preview else ['--latest' if 'latest' in tags else '--latest=false']
    subprocess.run(command, check=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('operation', choices=['validate', 'promote', 'publish'])
    parser.add_argument('--digests', type=Path, default=Path('/tmp/digests'))
    parser.add_argument('--verify-only', action='store_true')
    args = parser.parse_args()
    if args.verify_only and args.operation != 'validate':
        parser.error('--verify-only is only valid for validation; it cannot authorize publication')
    if args.operation == 'validate':
        tag = validate(verify_only=args.verify_only)[0]
        existing = None if args.verify_only else api(
            f'repos/{os.environ["GITHUB_REPOSITORY"]}/releases/tags/{tag}', missing_ok=True)
        if existing and (existing['draft'] or existing['prerelease'] != (version_key(tag)[3] in (0, 2))):
            raise ValueError(f'Existing release {tag} has unexpected draft/prerelease state')
        if os.environ.get('GITHUB_OUTPUT'):
            with open(os.environ['GITHUB_OUTPUT'], 'a', encoding='utf-8') as output:
                output.write(f'published={str(existing is not None).lower()}\n')
        print(f'{tag}: ' + ('verification only; no publication' if args.verify_only else
                           'already published; no rebuild or retag' if existing else 'ready for checks'))
    elif args.operation == 'promote':
        promote(args.digests)
    else:
        publish()
