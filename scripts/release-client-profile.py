#!/usr/bin/env python3
"""Generate real-client acceptance configuration through Kinetix's profile API."""

import json
import os
import pathlib
import re
import sys
import urllib.error
import urllib.request


client, model, proxy_base, destination = sys.argv[1:]
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


def endpoint_root(url):
    value = url.rstrip("/")
    return value[:-3] if value.endswith("/v1") else value


def shell_quote(value):
    return "'" + value.replace("'", "'\\''") + "'"


def rewrite_client_base_url(files, public_base_url, proxy_base):
    proxy_root = proxy_base.rstrip("/")
    proxy_v1 = proxy_root + "/v1"
    public_root = endpoint_root(public_base_url)

    if client == "pi":
        models = json.loads(files["models.json"])
        models["providers"]["kinetix"]["baseUrl"] = proxy_v1
        files["models.json"] = json.dumps(models, indent=2) + "\n"
    elif client == "claude_code":
        name = "kinetix-claude.sh"
        old = "export ANTHROPIC_BASE_URL=" + shell_quote(public_root)
        new = "export ANTHROPIC_BASE_URL=" + shell_quote(proxy_root)
        if old not in files[name]:
            raise ValueError("generated Claude profile has no expected base URL")
        files[name] = files[name].replace(old, new, 1)
    elif client == "codex":
        name = "config.toml"
        files[name], replacements = re.subn(
            r"(?m)^base_url = .*?$",
            "base_url = " + json.dumps(proxy_v1),
            files[name],
            count=1,
        )
        if replacements != 1:
            raise ValueError("generated Codex profile has no provider base URL")
    elif client == "open_code":
        config = json.loads(files["opencode.json"])
        config["provider"]["kinetix"]["options"]["baseURL"] = proxy_v1
        files["opencode.json"] = json.dumps(config, indent=2) + "\n"


try:
    keys = request(f"{base}/admin/api/keys")["keys"]
except Exception as error:
    raise SystemExit(f"could not load Kinetix keys for {client} acceptance: {error}")

profile = None
for key in keys:
    if key.get("status") != "active":
        continue
    try:
        profile = request(
            f"{base}/admin/api/client-profiles/generate",
            {
                "key_id": key["id"],
                "client": client,
                "model": model,
                "api_key": api_key,
            },
        )
        break
    except urllib.error.HTTPError as error:
        if error.code in (400, 403):
            continue
        raise SystemExit(f"could not generate the {client} acceptance profile from Kinetix")
    except Exception:
        raise SystemExit(f"could not generate the {client} acceptance profile from Kinetix")

if profile is None:
    raise SystemExit(f"no active Kinetix key grants this {client} acceptance model")

try:
    files = {entry["filename"]: entry["content"] for entry in profile["files"]}
    rewrite_client_base_url(files, profile["public_base_url"], proxy_base)
except (KeyError, TypeError, ValueError) as error:
    raise SystemExit(f"Kinetix returned an incomplete {client} acceptance profile: {error}")

output = pathlib.Path(destination)
output.mkdir(parents=True, exist_ok=True)
for filename, content in files.items():
    (output / filename).write_text(content)
