#!/usr/bin/env python3
"""Build pi-acceptance.json from real-Pi release acceptance artifacts and gate on it.

Reads the summary.tsv rows and per-case evidence written by
scripts/release-client-acceptance.sh (proxy trace, client log, verify log, and
the non-secret Pi profile extract). Exits 1 when a required Pi capability fails,
or when a capability that passed in the optional baseline artifact no longer
passes.
"""

import argparse
import collections
import json
import pathlib
import sys

INFERENCE_PATHS = {"/v1/chat/completions", "/v1/messages", "/v1/responses"}
SCHEMA = "kinetix.pi-acceptance.v1"


def read_jsonl(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def feature(name, required, passed, detail):
    if passed is None:
        result = "not_applicable" if not required else "fail"
    else:
        result = "pass" if passed else "fail"
    return {"name": name, "required": required, "result": result, "detail": detail}


def evaluate_case(artifacts, scenario, model, summary_status):
    case_id = f"pi-{scenario}"
    trace = read_jsonl(artifacts / f"{case_id}.trace.jsonl")
    inference = [row for row in trace if row.get("path", "").split("?", 1)[0] in INFERENCE_PATHS]
    log = (artifacts / f"{case_id}.jsonl")
    log_text = log.read_text(errors="replace") if log.exists() else ""
    verify = artifacts / f"{case_id}.verify.txt"
    verify_text = verify.read_text(errors="replace") if verify.exists() else ""
    profile_path = artifacts / f"{case_id}.pi-profile.json"
    profile = json.loads(profile_path.read_text()) if profile_path.exists() else {}

    streaming = bool(inference) and all(
        row.get("request_stream") is True
        and (row.get("response_content_type") or "").split(";", 1)[0].strip().lower()
        == "text/event-stream"
        for row in inference
    )
    tool_ids = {tool_id for row in inference for tool_id in row.get("tool_call_ids", [])}
    result_refs = sum(int(row.get("request_tool_results", 0)) for row in inference)

    # Every tool result Pi sends must answer a tool call id Kinetix returned
    # earlier in the session.
    seen = set()
    answered = []
    unknown = []
    for row in inference:
        for result_id in row.get("request_tool_result_ids", []):
            (answered if result_id in seen else unknown).append(result_id)
        seen.update(row.get("tool_call_ids", []))

    images = sum(int(row.get("request_images", 0)) for row in inference)
    controls = sorted({c for row in inference for c in row.get("request_reasoning_control", [])})
    replayed = sum(int(row.get("request_reasoning_replay", 0)) for row in inference)
    sessions = collections.Counter(
        value
        for row in inference
        for value in row.get("session_headers", {}).values()
        if value
    )
    wants_images = "image" in profile.get("input", [])
    wants_reasoning = profile.get("reasoning") is True

    features = [
        feature("streaming", True, streaming, f"{len(inference)} inference request(s)"),
        feature(
            "multi_turn_tools",
            True,
            len(tool_ids) >= 2 and result_refs >= 2,
            f"{len(tool_ids)} tool call(s), {result_refs} tool result(s)",
        ),
        feature(
            "tool_continuation",
            True,
            bool(answered) and not unknown,
            f"{len(answered)} result(s) matched prior call ids, {len(unknown)} unknown",
        ),
        feature(
            "grounded_tool_results",
            True,
            "alpha-sentinel" in log_text and "beta-sentinel" in log_text,
            "both file sentinels reported",
        ),
        feature(
            "images",
            wants_images,
            (images > 0) if wants_images else None,
            f"{images} image part(s); profile input={profile.get('input')}",
        ),
        feature(
            "reasoning",
            wants_reasoning,
            bool(controls) if wants_reasoning else None,
            f"controls={controls}; profile reasoning={profile.get('reasoning')}",
        ),
        feature(
            "reasoning_replay",
            False,
            (replayed > 0) if replayed else None,
            f"{replayed} assistant turn(s) replayed reasoning",
        ),
    ]
    if scenario == "fallback":
        features.append(
            feature(
                "fallback",
                True,
                any(row.get("fallback") == "1" for row in inference),
                "X-Kinetix-Fallback: 1 observed",
            )
        )
    if scenario == "affinity":
        features.append(
            feature(
                "sticky_affinity",
                True,
                any(count >= 2 for count in sessions.values()) and "affinity ok" in verify_text,
                "stable session header and one final target",
            )
        )

    passed = summary_status == "PASS" and all(
        f["result"] == "pass" for f in features if f["required"]
    )
    return {
        "scenario": scenario,
        "model": model,
        "transport": profile.get("api", "unknown"),
        "summary_status": summary_status,
        "features": features,
        "result": "pass" if passed else "fail",
    }


def regressions(report, baseline):
    previous = {}
    for case in baseline.get("cases", []):
        for item in case.get("features", []):
            if item.get("result") == "pass":
                previous[(case.get("scenario"), item.get("name"))] = True
    found = []
    for case in report["cases"]:
        current = {item["name"]: item["result"] for item in case["features"]}
        for (scenario, name) in previous:
            if scenario == case["scenario"] and current.get(name) != "pass":
                found.append(f"{scenario}/{name}")
    return sorted(found)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifacts", required=True, type=pathlib.Path)
    parser.add_argument("--pi-version", required=True)
    parser.add_argument("--kinetix-version", required=True)
    parser.add_argument("--baseline", type=pathlib.Path)
    args = parser.parse_args()

    rows = []
    summary = args.artifacts / "summary.tsv"
    for line in summary.read_text().splitlines()[1:]:
        cols = line.split("\t")
        if len(cols) >= 4 and cols[0] == "pi":
            rows.append(cols)
    if not rows:
        raise SystemExit("no Pi cases in summary.tsv")

    report = {
        "schema": SCHEMA,
        "pi_version": args.pi_version,
        "kinetix_version": args.kinetix_version,
        "cases": [evaluate_case(args.artifacts, c[1], c[2], c[3]) for c in rows],
    }
    report["regressions"] = (
        regressions(report, json.loads(args.baseline.read_text())) if args.baseline else []
    )
    report["result"] = (
        "pass"
        if all(case["result"] == "pass" for case in report["cases"]) and not report["regressions"]
        else "fail"
    )
    out = args.artifacts / "pi-acceptance.json"
    out.write_text(json.dumps(report, indent=2) + "\n")

    for case in report["cases"]:
        failed = [f["name"] for f in case["features"] if f["required"] and f["result"] != "pass"]
        print(f"pi {case['scenario']}: {case['result']}" + (f" (failed: {', '.join(failed)})" if failed else ""))
    if report["regressions"]:
        print("regressed since baseline: " + ", ".join(report["regressions"]))
    print(f"wrote {out}")
    return 0 if report["result"] == "pass" else 1


if __name__ == "__main__":
    sys.exit(main())
