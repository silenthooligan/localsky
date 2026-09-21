"""Regression tests for the failures that previously required manual releases."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


tools = load('build_tools', 'prepare-build-tools.py')
release = load('release', 'release.py')


class ToolDownloads(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.cache = Path(self.temp.name)
        self.payload = b'verified archive bytes'
        self.sha = hashlib.sha256(self.payload).hexdigest()
        self.tool = {'name': 'test', 'version': '1', 'repository': 'owner/repo', 'tag': 'v1',
                     'archives': {'aarch64': {'name': 'tool.tgz', 'asset_id': 7, 'sha256': self.sha}}}

    def test_504_recovers_through_asset_api(self):
        calls = []
        def fetch(url, output, *, asset_api):
            calls.append((url, asset_api))
            if not asset_api:
                output.write_bytes(b'incomplete')
                return 22, '504'
            output.write_bytes(self.payload)
            return 0, '200'
        path = tools.archive_for(self.tool, 'aarch64', self.cache, fetch)
        self.assertEqual(path.read_bytes(), self.payload)
        self.assertEqual([api for _, api in calls], [False, True])
        self.assertFalse(path.with_suffix('.partial').exists())

    def test_cache_is_rechecked_and_corruption_replaced(self):
        path = self.cache / (self.sha + '.tar.gz')
        path.write_bytes(b'corrupt cached bytes')
        def fetch(url, output, **kwargs):
            output.write_bytes(self.payload)
            return 0, '200'
        self.assertEqual(tools.archive_for(self.tool, 'aarch64', self.cache, fetch).read_bytes(), self.payload)
        def never_fetch(*args, **kwargs):
            self.fail('Verified cache should not need the network')
        self.assertEqual(tools.archive_for(self.tool, 'aarch64', self.cache, never_fetch), path)

    def test_successful_download_with_wrong_hash_is_rejected(self):
        def fetch(url, output, **kwargs):
            output.write_bytes(b'tampered')
            return 0, '200'
        with self.assertRaisesRegex(RuntimeError, 'checksum mismatch'):
            tools.archive_for(self.tool, 'aarch64', self.cache, fetch)
        self.assertEqual(list(self.cache.iterdir()), [])

    def test_all_failed_paths_report_tool_architecture_status_and_transport(self):
        def fetch(url, output, *, asset_api):
            return (22, '403') if asset_api else (28, '504')
        with self.assertRaisesRegex(RuntimeError, r'test 1 \(aarch64\).*HTTP 504, curl 28.*HTTP 403, curl 22'):
            tools.archive_for(self.tool, 'aarch64', self.cache, fetch)

    def test_lock_version_drift_stops_before_install(self):
        manifest = {'tools': [{'name': 'wasm-bindgen', 'version': '0.2.126'}]}
        with self.assertRaisesRegex(RuntimeError, 'update both architecture'):
            tools.validate_lock(manifest, {'package': [{'name': 'wasm-bindgen', 'version': '0.2.127'}]})

    def test_actual_recipe_changes_only_tool_installation(self):
        manifest = json.loads((tools.ROOT / '.github/build-tools.json').read_text(encoding='utf-8'))
        original = (tools.ROOT / 'Dockerfile').read_text(encoding='utf-8')
        recipe = tools.docker_recipe(original, manifest)
        self.assertIn('--version "=0.3.9"', recipe)
        self.assertIn('COPY .ci-tools /opt/localsky-build-tools', recipe)
        self.assertIn('/opt/localsky-build-tools/binaryen/bin', recipe)
        self.assertEqual(recipe.split('# ── Runtime ──')[1], original.split('# ── Runtime ──')[1])
        with self.assertRaisesRegex(RuntimeError, 'insertion points'):
            tools.docker_recipe(original.replace('LEPTOS_SASS_VERSION=1.99.0', 'LEPTOS_SASS_VERSION=2.0.0'), manifest)

    def test_credentials_are_only_sent_to_api_via_stdin(self):
        result = subprocess.CompletedProcess([], 22, '504', 'not printed')
        with patch.dict(os.environ, {'GH_TOKEN': 'ghs_test_secret'}), patch.object(tools.subprocess, 'run', return_value=result) as run:
            tools.download('https://github.com/owner/repo/releases/download/v1/file', self.cache / 'x')
            self.assertEqual(run.call_args.kwargs['input'], '')
            tools.download('https://api.github.com/repos/owner/repo/releases/assets/1', self.cache / 'x', asset_api=True)
            self.assertNotIn('ghs_test_secret', str(run.call_args.args))
            self.assertIn('ghs_test_secret', run.call_args.kwargs['input'])


class Publication(unittest.TestCase):
    def test_older_retry_cannot_downgrade_latest(self):
        self.assertEqual(release.promotion_tags('v0.9.2', 'v0.9.3'), ['0.9.2'])
        self.assertEqual(release.promotion_tags('v0.9.10', 'v0.9.9'), ['0.9.10', '0.9', 'latest'])
        self.assertEqual(release.promotion_tags('v0.9.3', None), ['0.9.3', '0.9', 'latest'])

    def test_preview_and_beta_policy(self):
        self.assertEqual(release.promotion_tags('v1.0.0-rc.1', 'v0.9.2'), ['1.0.0-rc.1'])
        self.assertEqual(release.promotion_tags('v1.0.0-alpha', None), ['1.0.0-alpha'])
        self.assertEqual(release.promotion_tags('v1.0.0-beta.1', 'v0.9.2'), ['1.0.0-beta.1', '1.0', 'latest'])
        self.assertEqual(release.promotion_tags('v1.0.0-beta.1', 'v1.0.0'), ['1.0.0-beta.1'])

    def test_invalid_tag(self):
        for tag in ['main', 'vnext', 'v0.9.2;echo secret']:
            with self.assertRaises(ValueError):
                release.version_key(tag)

    def test_api_outage_is_not_mistaken_for_missing_release(self):
        failed = subprocess.CompletedProcess([], 1, '', 'gh: Bad Gateway (HTTP 502)')
        with patch.object(release.subprocess, 'run', return_value=failed) as run, patch.object(release.time, 'sleep'):
            with self.assertRaisesRegex(RuntimeError, 'HTTP 502'):
                release.api('repos/test/app/releases/latest', missing_ok=True)
            self.assertEqual(run.call_count, 3)

    def test_api_retry_recovers_without_skipping_check(self):
        failed = subprocess.CompletedProcess([], 1, '', 'gh: Bad Gateway (HTTP 502)')
        passed = subprocess.CompletedProcess([], 0, '{"tag_name":"v0.9.2"}', '')
        with patch.object(release.subprocess, 'run', side_effect=[failed, passed]), patch.object(release.time, 'sleep'):
            self.assertEqual(release.api('repos/test/app/releases/latest'), {'tag_name': 'v0.9.2'})

    def test_404_is_the_only_absence(self):
        missing = subprocess.CompletedProcess([], 1, '', 'gh: Not Found (HTTP 404)')
        with patch.object(release.subprocess, 'run', return_value=missing):
            self.assertIsNone(release.api('repos/test/app/releases/latest', missing_ok=True))
            with self.assertRaises(RuntimeError):
                release.api('repos/test/app/releases/latest')

    def test_already_published_retry_does_not_mutate_release(self):
        existing = {'draft': False, 'prerelease': False, 'html_url': 'https://github.com/test/app/releases/tag/v0.9.2'}
        with patch.dict(os.environ, {'GITHUB_REPOSITORY': 'test/app'}), \
             patch.object(release, 'validate', return_value=('v0.9.2', 'notes')), \
             patch.object(release, 'api', return_value=existing), patch.object(release.subprocess, 'run') as run:
            release.publish()
            run.assert_not_called()

    def test_partial_rerun_after_manual_recovery_does_not_retag_images(self):
        existing = {'draft': False, 'prerelease': False, 'html_url': 'https://github.com/test/app/releases/tag/v0.9.2'}
        with patch.dict(os.environ, {'GITHUB_REPOSITORY': 'test/app', 'GITHUB_SHA': 'a' * 40}), \
             patch.object(release, 'validate', return_value=('v0.9.2', 'notes')), \
             patch.object(release, 'api', return_value=existing), \
             patch.object(release.Path, 'write_text') as write, patch.object(release.subprocess, 'run') as run:
            release.promote(Path('absent-digests'))
            run.assert_not_called()
            self.assertTrue(json.loads(write.call_args.args[0])['already_published'])

    def test_incomplete_mirror_credentials_fail_before_any_registry_write(self):
        with patch.dict(os.environ, {'GITHUB_REPOSITORY': 'test/app', 'DOCKERHUB_TOKEN': 'test', 'DOCKERHUB_USERNAME': ''}), \
             patch.object(release, 'validate', return_value=('v0.9.2', 'notes')), \
             patch.object(release, 'api', return_value=None), patch.object(release.subprocess, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'no DOCKERHUB_USERNAME'):
                release.promote(Path('absent-digests'))
            run.assert_not_called()

    def test_draft_is_not_silently_accepted_as_published(self):
        with patch.dict(os.environ, {'GITHUB_REPOSITORY': 'test/app'}), \
             patch.object(release, 'validate', return_value=('v0.9.2', 'notes')), \
             patch.object(release, 'api', return_value={'draft': True, 'prerelease': False}):
            with self.assertRaisesRegex(ValueError, 'draft/prerelease'):
                release.publish()

    def test_both_architectures_must_match_release_sha(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            sha = 'a' * 40
            for arch in ('amd64', 'arm64'):
                (root / f'{arch}.json').write_text(json.dumps({'source': sha, 'architecture': arch, 'digest': 'sha256:' + 'b' * 64}))
            self.assertEqual(len(release.image_refs(root, 'test/app', sha)), 2)
            (root / 'arm64.json').write_text(json.dumps({'source': 'wrong revision', 'architecture': 'arm64', 'digest': 'sha256:' + 'b' * 64}))
            with self.assertRaisesRegex(ValueError, 'Image proof does not match'):
                release.image_refs(root, 'test/app', sha)
            (root / 'arm64.json').unlink()
            with self.assertRaises(FileNotFoundError):
                release.image_refs(root, 'test/app', sha)

    def test_validate_rejects_branch_version_drift_and_wrong_sha(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'Cargo.toml').write_text('[package]\nversion = "0.9.3"\n')
            (root / 'CHANGELOG.md').write_text('## [0.9.3]\nA fix.\n')
            with patch.dict(os.environ, {'GITHUB_REF': 'refs/heads/main'}):
                with self.assertRaisesRegex(ValueError, 'requires a version tag'):
                    release.validate(root)
            with patch.dict(os.environ, {'GITHUB_REF': 'refs/tags/v0.9.2'}):
                with self.assertRaisesRegex(ValueError, 'differs from Cargo.toml'):
                    release.validate(root)
            with patch.dict(os.environ, {'GITHUB_REF': 'refs/tags/v0.9.3', 'GITHUB_SHA': 'a' * 40}), \
                 patch.object(release.subprocess, 'check_output', side_effect=['a' * 40, 'b' * 40]):
                with self.assertRaisesRegex(ValueError, 'source mismatch'):
                    release.validate(root)

    def test_branch_rehearsal_validates_source_without_using_existing_version_tag(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'Cargo.toml').write_text('[package]\nversion = "0.9.2"\n')
            (root / 'CHANGELOG.md').write_text('## [0.9.2]\nThe existing release notes.\n')
            with patch.dict(os.environ, {'GITHUB_REF': 'refs/heads/ci-fix', 'GITHUB_SHA': 'a' * 40}), \
                 patch.object(release.subprocess, 'check_output', return_value='a' * 40) as git:
                self.assertEqual(release.validate(root, verify_only=True), ('v0.9.2', 'The existing release notes.'))
                git.assert_called_once_with(['git', 'rev-parse', 'HEAD'], text=True)

    def test_missing_release_notes_fail_validation(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'Cargo.toml').write_text('[package]\nversion = "0.9.3"\n')
            (root / 'CHANGELOG.md').write_text('## [0.9.2]\nOnly old notes.\n')
            with patch.dict(os.environ, {'GITHUB_REF': 'refs/tags/v0.9.3', 'GITHUB_SHA': 'a' * 40}), \
                 patch.object(release.subprocess, 'check_output', return_value='a' * 40):
                with self.assertRaisesRegex(ValueError, 'Missing CHANGELOG'):
                    release.validate(root)

    def test_first_publication_uses_tag_and_notes_but_old_version_never_sets_latest(self):
        for tag, latest, flag in [('v0.9.3', 'v0.9.2', '--latest'),
                                  ('v0.9.2', 'v0.9.3', '--latest=false')]:
            with self.subTest(tag=tag), \
                 patch.dict(os.environ, {'GITHUB_REPOSITORY': 'test/app', 'GITHUB_RUN_ID': '42'}), \
                 patch.object(release, 'validate', return_value=(tag, 'Specific release notes.')), \
                 patch.object(release, 'api', side_effect=[None, {'tag_name': latest}]), \
                 patch.object(release.Path, 'write_text') as write, patch.object(release.subprocess, 'run') as run:
                release.publish()
                command = run.call_args.args[0]
                self.assertIn(flag, command)
                self.assertIn('--verify-tag', command)
                self.assertIn('--notes-file', command)
                self.assertIn('Specific release notes.', write.call_args.args[0])
                self.assertIn('actions/runs/42', write.call_args.args[0])


if __name__ == '__main__':
    unittest.main()
