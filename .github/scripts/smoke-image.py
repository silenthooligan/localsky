"""Check the actual native image without networking, persisted data or valves."""
import json
import os
from pathlib import Path
import re
import subprocess
import time
import tomllib


def main():
    image = os.environ['SMOKE_IMAGE']
    expected = tomllib.loads(Path('Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    cid = subprocess.check_output(['docker', 'run', '-d', '--network', 'none',
        '--tmpfs', '/data', '--tmpfs', '/keys', '-e', 'LOCALSKY_DEMO=1',
        '-e', 'LOCALSKY_SMART_DRY_RUN=1', image], text=True).strip()
    if not re.fullmatch(r'[a-f0-9]{64}', cid):
        raise ValueError('Invalid smoke container ID')

    def get(path):
        return subprocess.check_output(['docker', 'exec', cid, 'curl', '-fsS',
            '--max-time', '5', 'http://127.0.0.1:8090' + path], stderr=subprocess.DEVNULL)

    try:
        for _ in range(45):
            try:
                info = json.loads(get('/api/v1/info'))
                health = json.loads(get('/api/v1/health?strict=1'))
                if health['status'] == 'ok':
                    break
            except subprocess.CalledProcessError:
                pass
            time.sleep(2)
        else:
            raise RuntimeError('Native image failed to become healthy within 90 seconds')
        assert info['service_version'] == expected, info
        assert info['build_revision'] == os.environ['GITHUB_SHA'], info
        assert info['demo'] and info['dry_run'] and not health['valves_unclosed']
        snapshot = json.loads(get('/api/v1/irrigation/snapshot'))
        assert all(not zone['running'] for zone in snapshot['zones'])
        routes = ['/', '/irrigation', '/irrigation/decisions', '/zones', '/history', '/docs/']
        pages = {path: get(path).decode() for path in routes}
        assert all('<html' in body.lower() for body in pages.values())
        assets = set(re.findall(r'[/]pkg/[^"\s<>]+\.(?:js|wasm|css)', pages['/']))
        assert len(assets) >= 3 and all(len(get(path)) > 100 for path in assets)
        status = subprocess.check_output(['docker', 'exec', cid, 'cat', '/proc/1/status'], text=True)
        assert re.search(r'^Uid:\s+(\d+)', status, re.M).group(1) == '10001'
        proof = {'source': info['build_revision'], 'version': expected, 'health': health['status'],
                 'routes': routes, 'assets': sorted(assets), 'uid': 10001, 'network': 'none'}
        Path('runtime-proof.json').write_text(json.dumps(proof, indent=2), encoding='utf-8')
        print(json.dumps(proof))
    except Exception:
        subprocess.run(['docker', 'logs', '--tail', '80', cid], check=False)
        raise
    finally:
        subprocess.run(['docker', 'rm', '-f', cid], check=True)


if __name__ == '__main__':
    main()
