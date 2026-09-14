"""Publish release notes only after this revision's CI and image scan pass."""
import json
import os
import time
import urllib.request

deadline = time.monotonic() + 65 * 60
pending = {"ci.yml", "docker.yml"}
while pending:
    for workflow in sorted(pending):
        url = (
            f"https://api.github.com/repos/{os.environ['RELEASE_REPO']}/actions/"
            f"workflows/{workflow}/runs?head_sha={os.environ['RELEASE_SHA']}"
            "&event=push&per_page=20"
        )
        request = urllib.request.Request(url, headers={
            "Authorization": "Bearer " + os.environ["GH_TOKEN"],
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        })
        with urllib.request.urlopen(request, timeout=30) as response:
            runs = json.load(response)["workflow_runs"]
        runs = [r for r in runs if r["head_sha"] == os.environ["RELEASE_SHA"]]
        if not runs:
            continue
        run = max(runs, key=lambda r: r["id"])
        if run["status"] != "completed":
            continue
        if run["conclusion"] != "success":
            raise SystemExit(f"{workflow} did not pass: {run['html_url']}")
        print(f"Passed {workflow}: {run['html_url']}", flush=True)
        pending.remove(workflow)
    if pending:
        if time.monotonic() >= deadline:
            raise SystemExit("Timed out waiting for: " + ", ".join(sorted(pending)))
        print("Waiting for: " + ", ".join(sorted(pending)), flush=True)
        time.sleep(20)
