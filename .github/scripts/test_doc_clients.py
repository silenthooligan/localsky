"""Exercise the downloadable clients against an HTTP contract fixture."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).resolve().parents[2]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.server.seen.append((self.path, self.headers.get('Authorization')))
        path = urlsplit(self.path).path
        status = 200
        if self.server.mode == 'redirect':
            self.send_response(302)
            self.send_header('Location', self.server.other_url+'/credential-trap')
            self.end_headers()
            return
        if self.server.mode == 'error':
            status = 500
            payload = {'code': 'HA_HTTP_STATUS', 'error': 'private provider message'}
        elif path == '/api/v1/info':
            payload = {'service': 'localsky', 'api_version': '2.2.0' if self.server.mode == 'old' else '2.4.0'}
        elif path == '/api/v1/forecast/snapshot':
            payload = {'hourly': [{'time_epoch': self.server.hour+i*3600} for i in range(6)]}
            if self.server.mode == 'malformed':
                payload = {'hourly': None}
        elif path == '/api/v1/forecast/window':
            query = parse_qs(urlsplit(self.path).query)
            self.server.window_query = query
            payload = {'complete': self.server.mode != 'gap', 'age_s': 8000 if self.server.mode == 'stale' else 50,
                       'precip_sum_in': None if self.server.mode == 'missing' else 0.0,
                       'hourly': [{'time_epoch': self.server.hour, 'precip_in': None}]}
        else:
            status, payload = 404, {'error': 'unknown fixture route'}
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.send_header('X-LocalSky-Request-Id', 'fixture-request-42')
        self.end_headers()
        self.wfile.write(body)


class ClientContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        cls.other = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        cls.other.seen = []
        cls.server.other_url = f'http://127.0.0.1:{cls.other.server_port}'
        cls.threads = [threading.Thread(target=s.serve_forever, daemon=True) for s in (cls.server, cls.other)]
        for thread in cls.threads:
            thread.start()
        cls.clients = [[sys.executable, str(ROOT/'docs/src/examples/localsky_client.py')]]
        if shutil.which('node'):
            cls.clients.append(['node', str(ROOT/'docs/src/examples/localsky-client.mjs')])

    @classmethod
    def tearDownClass(cls):
        for server in (cls.server, cls.other):
            server.shutdown()
            server.server_close()
        for thread in cls.threads:
            thread.join(timeout=2)

    def run_client(self, client, mode):
        self.server.mode = mode
        self.server.seen = []
        self.server.hour = int(time.time())//3600*3600
        self.other.seen = []
        env = dict(os.environ, LOCALSKY_URL=f'http://127.0.0.1:{self.server.server_port}', LOCALSKY_TOKEN='fixture-secret')
        result = subprocess.run(client+['--hours', '3'], env=env, capture_output=True, text=True, timeout=20)
        self.assertNotIn('fixture-secret', result.stdout+result.stderr)
        return result

    def test_zero_is_usable_and_bounds_are_inclusive(self):
        for client in self.clients:
            with self.subTest(client=client[0]):
                result = self.run_client(client, 'ok')
                self.assertEqual(result.returncode, 0, result.stderr)
                payload = json.loads(result.stdout)
                self.assertTrue(payload['usable_for_rain_summary'])
                self.assertEqual(payload['forecast']['precip_sum_in'], 0)
                self.assertIsNone(payload['forecast']['hourly'][0]['precip_in'])
                self.assertEqual(int(self.server.window_query['to'][0])-int(self.server.window_query['from'][0]), 7200)
                self.assertEqual(len(self.server.seen), 3)
                self.assertTrue(all(token == 'Bearer fixture-secret' for _, token in self.server.seen))

    def test_missing_stale_and_gapped_data_remain_unusable(self):
        for client in self.clients:
            for mode in ('missing', 'stale', 'gap'):
                with self.subTest(client=client[0], mode=mode):
                    result = self.run_client(client, mode)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertFalse(json.loads(result.stdout)['usable_for_rain_summary'])

    def test_api_errors_preserve_code_and_request_id(self):
        for client in self.clients:
            result = self.run_client(client, 'error')
            self.assertEqual(result.returncode, 1)
            self.assertIn('500', result.stderr)
            self.assertIn('HA_HTTP_STATUS', result.stderr)
            self.assertIn('fixture-request-42', result.stderr)
            self.assertNotIn('private provider message', result.stderr)

    def test_redirect_does_not_send_credentials_to_another_server(self):
        for client in self.clients:
            result = self.run_client(client, 'redirect')
            self.assertEqual(result.returncode, 1)
            self.assertEqual(self.other.seen, [])

    def test_old_api_and_malformed_hourly_stop_before_window(self):
        for client in self.clients:
            for mode in ('old', 'malformed'):
                with self.subTest(client=client[0], mode=mode):
                    result = self.run_client(client, mode)
                    self.assertEqual(result.returncode, 1)
                    self.assertNotIn('Traceback', result.stderr)
                    self.assertFalse(any('/window' in path for path, _ in self.server.seen))


if __name__ == '__main__':
    unittest.main()
