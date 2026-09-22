#!/usr/bin/env python3
"""Read a LocalSky forecast window. Python 3.10+; standard library only."""
import argparse
import json
import os
import sys
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode, urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

MAX_BYTES = 4 * 1024 * 1024


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class LocalSkyError(Exception):
    pass


class LocalSkyClient:
    def __init__(self, url, token=None, timeout=15):
        parsed = urlsplit(url)
        if parsed.scheme not in ("http", "https") or not parsed.hostname:
            raise LocalSkyError("LOCALSKY_URL must be an HTTP or HTTPS instance URL.")
        if parsed.username or parsed.password or parsed.query or parsed.fragment:
            raise LocalSkyError("Keep credentials, query parameters, and fragments out of LOCALSKY_URL.")
        self.base = url.rstrip("/")
        self.token = token
        self.timeout = timeout
        self.opener = build_opener(NoRedirect())

    def get(self, path, query=None):
        # Callers use fixed paths; no arbitrary URL or write operation is exposed.
        if path not in ("/info", "/forecast/snapshot", "/forecast/window"):
            raise LocalSkyError("Unsupported read operation.")
        url = self.base + "/api/v1" + path
        if query:
            url += "?" + urlencode(query)
        headers = {"Accept": "application/json"}
        if self.token:
            headers["Authorization"] = "Bearer " + self.token
        try:
            with self.opener.open(Request(url, headers=headers), timeout=self.timeout) as response:
                body = response.read(MAX_BYTES + 1)
                if len(body) > MAX_BYTES:
                    raise LocalSkyError("Response exceeds the 4 MiB client limit.")
                return json.loads(body)
        except HTTPError as error:
            # Print bounded structured error fields, not raw upstream bodies or tokens.
            try:
                payload = json.loads(error.read(65536))
                code = payload.get("code") or payload.get("diagnostic", {}).get("failure", {}).get("code")
            except (ValueError, AttributeError, TypeError):
                code = None
            request_id = error.headers.get("X-LocalSky-Request-Id", "unavailable")
            raise LocalSkyError(f"GET {path}: HTTP {error.code}; code={code or 'unavailable'}; request_id={request_id}") from error
        except URLError as error:
            raise LocalSkyError(f"GET {path}: connection failed ({type(error.reason).__name__}). Check address, network, and TLS.") from error
        except (ValueError, UnicodeError) as error:
            raise LocalSkyError(f"GET {path}: response is not valid JSON.") from error

    def forecast(self, hours=3):
        if type(hours) is not int or not 1 <= hours <= 48:
            raise LocalSkyError("hours must be an integer from 1 to 48.")
        info = self.get("/info")
        if not isinstance(info, dict) or info.get("service") != "localsky":
            raise LocalSkyError("The configured endpoint did not identify itself as LocalSky.")
        try:
            major, minor, _ = (int(x) for x in info["api_version"].split("."))
        except (KeyError, ValueError, AttributeError):
            raise LocalSkyError("Missing or invalid API contract version.")
        if major != 2 or minor < 3:
            raise LocalSkyError("This example requires LocalSky API 2.3.0 through 2.x.")
        snapshot = self.get("/forecast/snapshot")
        if not isinstance(snapshot, dict) or not isinstance(snapshot.get("hourly"), list):
            raise LocalSkyError("Forecast response is missing its hourly array.")
        now = int(time.time())
        stamps = sorted({row["time_epoch"] for row in snapshot.get("hourly", [])
                         if isinstance(row, dict) and type(row.get("time_epoch")) is int
                         and row["time_epoch"] + 3600 > now})
        if len(stamps) < hours:
            raise LocalSkyError("Not enough current/future forecast rows for the requested window.")
        start = stamps[0]
        end = start + (hours - 1) * 3600
        window = self.get("/forecast/window", {"track": "merged", "from": start, "to": end})
        if not isinstance(window, dict):
            raise LocalSkyError("Forecast window response is not an object.")
        return window


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hours", type=int, default=3, choices=range(1, 49), metavar="1..48")
    parser.add_argument("--max-age-seconds", type=int, default=3600,
                        help="Example consumer freshness limit; choose one for your application.")
    args = parser.parse_args()
    if args.max_age_seconds < 0:
        parser.error("--max-age-seconds must be nonnegative")
    try:
        window = LocalSkyClient(os.environ.get("LOCALSKY_URL", "http://localhost:8090"),
                                os.environ.get("LOCALSKY_TOKEN")).forecast(args.hours)
        age = window.get("age_s")
        usable = (window.get("complete") is True and window.get("precip_sum_in") is not None
                  and isinstance(age, (int, float)) and 0 <= age <= args.max_age_seconds)
        print(json.dumps({"usable_for_rain_summary": usable, "forecast": window}, indent=2))
        return 0 if usable else 2
    except (LocalSkyError, TimeoutError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
