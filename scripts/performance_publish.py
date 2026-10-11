"""Idempotent GitHub publication and retirement of superseded scoreboards."""

from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import subprocess
import urllib.request

ROLES = ("perceus", "memory", "runtime")


def read(path):
    return json.loads(Path(path).read_text())


def save(path, value):
    path = Path(path)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def digest(text):
    return hashlib.sha256(text.encode()).hexdigest()


class Github:
    def request(self, path, payload=None, method=None):
        command = ["gh", "api", path]
        if payload is not None:
            command += ["--method", method or "PATCH", "--input", "-"]
        try:
            response = subprocess.check_output(command, input=json.dumps(payload) if payload is not None else None, text=True)
        except subprocess.CalledProcessError as error:
            raise RuntimeError(f"GitHub request {path} failed: {error.output}") from error
        return json.loads(response)

    def pages(self, path):
        page = 1
        while True:
            separator = "&" if "?" in path else "?"
            rows = self.request(f"{path}{separator}per_page=100&page={page}")
            yield from rows
            if len(rows) < 100:
                return
            page += 1

    def verify_archive(self, identity, files):
        actual = self.request("gists/" + identity)
        for name, expected in files.items():
            file = actual["files"][name]
            if file.get("truncated"):
                with urllib.request.urlopen(file["raw_url"], timeout=120) as response:
                    content = response.read().decode()
            else:
                content = file["content"]
            if digest(content) != digest(expected["content"]):
                raise ValueError(f"published archive differs: {name}")


def issue_marker(role):
    return f"<!-- fai-performance-tracker:{role} -->"


def bodies(run, config, numbers):
    links = " · ".join(f"[{role}](https://github.com/{config['repository']}/issues/{numbers[role]})" for role in ROLES)
    result = {}
    for role in ROLES:
        text = (run / "reports" / config["trackers"][role]["report"]).read_text()
        body = text + "\n**Related trackers:** " + links + "\n"
        if len(body.encode()) > 65536:
            raise ValueError("generated GitHub body exceeds the size limit")
        result[role] = body
    return result


def archive_files(run, config, migration):
    files = {"snapshot.json": {"content": (run / "snapshot.json").read_text()},
             "results.json": {"content": (run / "reports/results.json").read_text()},
             "tracker-config.json": {"content": json.dumps(config, indent=2)}}
    for role in ROLES:
        name = config["trackers"][role]["report"]
        files[name] = {"content": (run / "reports" / name).read_text()}
    for path in sorted((run / "raw").glob("*")):
        if path.is_file():
            files["raw-" + path.name] = {"content": path.read_text()}
    provenance = {}
    for folder in ["artifacts", "build-records", "attempts"]:
        for path in sorted((run / folder).rglob("*")):
            if path.is_file() and path.suffix in {".json", ".jsonl", ".txt", ".fai", ".rs", ".ml", ".ir", ".stdout", ".stderr"}:
                provenance[path.relative_to(run).as_posix()] = path.read_text()
    files["artifact-provenance.json"] = {"content": json.dumps(provenance, indent=2)}
    if migration:
        files["superseded-issues.json"] = {"content": json.dumps(migration, indent=2)}
    # Gists reject empty or whitespace-only files. Keep their exact contents in
    # a JSON map so a successful, silent command remains distinguishable from a
    # missing log and can be reconstructed without adding bytes to the log.
    blank = {name: value["content"] for name, value in files.items() if not value["content"].strip()}
    if blank:
        for name in blank:
            del files[name]
        files["blank-files.json"] = {"content": json.dumps(blank, indent=2)}
    return files


def publish(run, config, args, render):
    # Validation is the first operation, before any GitHub write or migration.
    render(run)
    manifest = read(run / "snapshot.json")
    if manifest["status"] != "complete" or manifest["repository"] != config["repository"]:
        raise ValueError("publication needs a complete snapshot for this repository")
    snapshot_hash = digest((run / "snapshot.json").read_text())
    path = run / "publication.json"
    receipt = read(path) if path.exists() else {"schemaVersion": 1, "snapshotSha256": snapshot_hash, "issues": {}, "retired": []}
    if receipt["snapshotSha256"] != snapshot_hash:
        raise ValueError("a published snapshot is immutable; create a new run")
    api = Github()
    repo = "repos/" + config["repository"]
    migration_path = run / "migration-before.json"
    migration = []
    if args.migrate:
        migration = read(migration_path) if migration_path.exists() else [api.request(f"{repo}/issues/{n}") for n in config["supersededIssues"]]
        if any("pull_request" in issue for issue in migration):
            raise ValueError("migration contains a pull request")
        save(migration_path, migration)
    if "archiveId" not in receipt:
        files = archive_files(run, config, migration)
        archive = api.request("gists", {"description": f"Fai performance snapshot {manifest['runId']} ({manifest['measuredRevision'][:12]})", "public": config.get("archivePublic", False), "files": files}, "POST")
        receipt.update(archiveId=archive["id"], archiveHasMigration=bool(migration))
        save(path, receipt)
    if not receipt.get("archiveValidated"):
        files = archive_files(run, config, migration if receipt.get("archiveHasMigration") else [])
        api.verify_archive(receipt["archiveId"], files)
        archive = api.request("gists/" + receipt["archiveId"])
        receipt.update(archiveValidated=True, archiveUrl=archive["html_url"] + "/" + archive["history"][0]["version"], migrationArchived=receipt.get("archiveHasMigration", False))
        save(path, receipt)
    if args.migrate and not receipt.get("migrationArchived"):
        files = {"superseded-issues.json": {"content": json.dumps(migration, indent=2)}}
        api.request("gists/" + receipt["archiveId"], {"files": files})
        api.verify_archive(receipt["archiveId"], files)
        archive = api.request("gists/" + receipt["archiveId"])
        receipt["archiveUrl"] = archive["html_url"] + "/" + archive["history"][0]["version"]
    if args.migrate:
        receipt["migrationArchived"] = True
        save(path, receipt)
    render(run, receipt["archiveUrl"])

    numbers = {}
    discovered = None
    for role in ROLES:
        tracker = config["trackers"][role]
        number = receipt["issues"].get(role, {}).get("number") or tracker.get("issue")
        if number:
            issue = api.request(f"{repo}/issues/{number}")
            if issue_marker(role) not in (issue.get("body") or "") or "pull_request" in issue or issue["state"] != "open":
                raise ValueError(f"configured {role} issue is not an open managed tracker")
        else:
            if discovered is None:
                discovered = [row for row in api.pages(repo + "/issues?state=open") if "pull_request" not in row]
            matches = [issue for issue in discovered if issue_marker(role) in (issue.get("body") or "")]
            if len(matches) > 1:
                raise ValueError(f"multiple managed {role} issues; configure the intended issue ID")
            if matches:
                issue = matches[0]
            elif args.create:
                issue = api.request(repo + "/issues", {"title": tracker["title"], "body": (run / "reports" / tracker["report"]).read_text(), "labels": config["labels"]}, "POST")
            else:
                raise ValueError(f"no {role} tracker; use --create for the initial publication")
            number = issue["number"]
        numbers[role] = number
        receipt["issues"].setdefault(role, {})["number"] = number
        save(path, receipt)
    if len(set(numbers.values())) != 3 or set(numbers.values()).intersection(config["supersededIssues"]):
        raise ValueError("tracker IDs must be distinct from one another and superseded issues")

    expected = bodies(run, config, numbers)
    for role in ROLES:
        number = numbers[role]
        old = api.request(f"{repo}/issues/{number}")
        payload = {"title": config["trackers"][role]["title"], "body": expected[role]}
        if old["title"] != payload["title"] or old["body"] != payload["body"]:
            api.request(f"{repo}/issues/{number}", payload)
        receipt["issues"][role]["bodySha256"] = digest(expected[role])
        save(path, receipt)
    for role in ROLES:
        actual = api.request(f"{repo}/issues/{numbers[role]}")
        if actual["body"] != expected[role] or actual["state"] != "open":
            raise ValueError("tracker publication verification failed")

    for role, number in numbers.items():
        marker = f"<!-- fai-performance-refresh:{manifest['runId']} -->"
        comments = api.pages(f"{repo}/issues/{number}/comments")
        if not any(marker in comment["body"] for comment in comments):
            api.request(f"{repo}/issues/{number}/comments", {"body": marker + f"\nUpdated the main body from validated snapshot `{manifest['runId']}`, measured at `{manifest['measuredRevision']}`. [Versioned report and raw evidence]({receipt['archiveUrl']})."}, "POST")
    if args.record_ids:
        for role, number in numbers.items():
            config["trackers"][role]["issue"] = number
        save(args.config, config)

    # Old trackers close only after all three new bodies have been read back.
    if args.migrate:
        references = "\n".join(f"- {config['trackers'][role]['title']}: #{numbers[role]}" for role in ROLES)
        marker = "<!-- fai-performance-superseded -->"
        for original in migration:
            number = original["number"]
            issue = api.request(f"{repo}/issues/{number}")
            if issue["state"] == "closed":
                if original["state"] == "open" and number not in receipt["retired"]:
                    receipt["retired"].append(number)
                    save(path, receipt)
                continue
            if issue["body"] != original["body"] or issue["title"] != original["title"]:
                raise ValueError(f"#{number} changed during migration; review it before closing")
            if not any(marker in comment["body"] for comment in api.pages(f"{repo}/issues/{number}/comments")):
                api.request(f"{repo}/issues/{number}/comments", {"body": marker + "\nTracking is consolidated into the generated current-state reports below. Remaining gaps and validation work continue there; this issue's measurements and discussion are preserved as history.\n\n" + references + f"\n\n[Archived checkpoint and previous issue bodies]({receipt['archiveUrl']})."}, "POST")
            api.request(f"{repo}/issues/{number}", {"state": "closed", "state_reason": "not_planned"})
            if api.request(f"{repo}/issues/{number}")["state"] != "closed":
                raise ValueError(f"failed to retire #{number}")
            receipt["retired"].append(number)
            save(path, receipt)
    receipt.update(status="published", completedAt=datetime.now(timezone.utc).isoformat())
    save(path, receipt)
    print("Published and verified: " + ", ".join(f"{role} #{number}" for role, number in numbers.items()))
