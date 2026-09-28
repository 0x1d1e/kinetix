#!/usr/bin/env python3
"""Use Kinetix's profile renderer for the real-Pi release acceptance run."""

import json
import os
import pathlib
import sys
import urllib.error
import urllib.request


model, proxy_base, destination = sys.argv[1:]
base = os.environ["KINETIX_BASE"].rstrip("/")
admin_token = os.environ["KINETIX_ADMIN_TOKEN"]
api_key = os.environ["KINETIX_KEY"]


def request(url, payload=None):
    body = None if payload is None else json.dumps(payload).encode()
    headers = {"x-kinetix-admin-token": admin_token}
    if body is not None:
        headers["content-type"] = "application/json"
    req = urllib.request.Request(url, data=body, headers=headers)
    with urllib.request.urlopen(req, timeout=30) as response:
        return json.loads(response.read())


try:
    keys = request(f"{base}/admin/api/keys")["keys"]
except Exception as error:
    raise SystemExit(f"could not load Kinetix keys for Pi acceptance: {error}")

profile = None
for key in keys:
    if key.get("status") != "active":
        continue
    try:
        profile = request(
            f"{base}/admin/api/client-profiles/generate",
            {
                "key_id": key["id"],
                "client": "pi",
                "model": model,
                "api_key": api_key,
            },
        )
        break
    except urllib.error.HTTPError as error:
        if error.code in (400, 403):
            continue
        raise SystemExit("could not generate the Pi acceptance profile from Kinetix")
    except Exception:
        raise SystemExit("could not generate the Pi acceptance profile from Kinetix")

if profile is None:
    raise SystemExit("no active Kinetix key grants this Pi acceptance model")

files = {entry["filename"]: entry["content"] for entry in profile["files"]}
try:
    models = json.loads(files["models.json"])
    settings = files["settings.json"]
    provider = models["providers"]["kinetix"]
    provider["baseUrl"] = proxy_base.rstrip("/") + "/v1"
except (KeyError, TypeError, ValueError):
    raise SystemExit("Kinetix returned an incomplete Pi acceptance profile")

output = pathlib.Path(destination)
(output / "models.json").write_text(json.dumps(models, indent=2) + "\n")
(output / "settings.json").write_text(settings)
