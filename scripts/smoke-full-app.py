#!/usr/bin/env python3
"""Bounded offscreen integration of the production daemon, UI and a local Drive."""

import argparse
from contextlib import ExitStack
from email.parser import BytesParser
from email.policy import default
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading
import time
from urllib.parse import parse_qs, urlsplit
import uuid


ROOT = Path(__file__).resolve().parents[1]
TOKEN = "hidden-smoke-synthetic-token"
INITIAL = b"initial remote content\n"
LOCAL = b"updated locally\n"
REMOTE = b"updated remotely\n"


class FakeDrive:
    def __init__(self):
        self.lock = threading.RLock()
        self.files = {}
        self.changes = []
        self.requests = []
        self.errors = []
        self.fail_media = False
        self.generated = 0
        self.put("seed", INITIAL)

    def put(self, file_id, content):
        revision = len(self.changes) + 1
        metadata = {
            "id": file_id, "name": "smoke.txt", "parents": ["root"],
            "mimeType": "text/plain", "trashed": False,
            "modifiedTime": f"2026-01-01T00:00:{revision:02d}.000Z",
            "md5Checksum": hashlib.md5(content).hexdigest(), "size": str(len(content)),
        }
        self.files[file_id] = (metadata, content)
        self.changed(metadata)

    def changed(self, metadata):
        self.changes.append({
            "fileId": metadata["id"], "removed": False, "file": dict(metadata),
        })

    def active(self, content):
        with self.lock:
            return next((
                file_id for file_id, (metadata, body) in self.files.items()
                if not metadata["trashed"] and metadata["name"] == "smoke.txt" and body == content
            ), None)

    def handler(self):
        drive = self

        class Handler(BaseHTTPRequestHandler):
            def setup(self):
                super().setup()
                self.connection.settimeout(2)

            def log_message(self, *_):
                pass

            def reply(self, body, status=200):
                if not isinstance(body, bytes):
                    body = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                self.dispatch()

            def do_POST(self):
                self.dispatch()

            def do_PATCH(self):
                self.dispatch()

            def dispatch(self):
                with drive.lock:
                    try:
                        self.route()
                    except Exception as error:
                        drive.errors.append(f"{self.command} {self.path}: {error}")
                        self.reply({"error": "unexpected fake Drive request"}, 500)

            def route(self):
                if self.headers.get("Authorization") != f"Bearer {TOKEN}":
                    raise ValueError("missing synthetic authorization")
                url = urlsplit(self.path)
                query = parse_qs(url.query)
                path = url.path.removeprefix("/drive/v3")
                drive.requests.append(f"{self.command} {url.path}")
                if self.command == "GET":
                    if path == "/changes/startPageToken":
                        return self.reply({"startPageToken": str(len(drive.changes))})
                    if path == "/changes":
                        cursor = int(query["pageToken"][0])
                        return self.reply({
                            "changes": drive.changes[cursor:],
                            "newStartPageToken": str(len(drive.changes)),
                        })
                    if path == "/files/generateIds":
                        drive.generated += 1
                        return self.reply({"ids": [f"uploaded-{drive.generated}"]})
                    if path == "/files":
                        if "'root' in parents" not in query["q"][0]:
                            raise ValueError("expected root folder listing")
                        return self.reply({"files": [
                            metadata for metadata, _ in drive.files.values()
                            if not metadata["trashed"]
                        ]})
                    if path.startswith("/files/"):
                        entry = drive.files.get(path.split("/")[-1])
                        if entry is None:
                            return self.reply({"error": "not found"}, 404)
                        metadata, content = entry
                        if query.get("alt") == ["media"]:
                            if drive.fail_media:
                                return self.reply({"error": "synthetic download failure"}, 503)
                            return self.reply(content)
                        return self.reply(metadata)
                length = int(self.headers.get("Content-Length", "0"))
                if not 0 < length < 65536:
                    raise ValueError("unexpected request size")
                body = self.rfile.read(length)
                if self.command == "POST" and url.path == "/upload/drive/v3/files":
                    message = BytesParser(policy=default).parsebytes(
                        f"Content-Type: {self.headers['Content-Type']}\r\n\r\n".encode() + body
                    )
                    parts = list(message.iter_parts())
                    metadata = json.loads(parts[0].get_payload(decode=True))
                    if metadata["name"] != "smoke.txt" or metadata["parents"] != ["root"]:
                        raise ValueError("unexpected uploaded name or parent")
                    drive.put(metadata["id"], parts[1].get_payload(decode=True))
                    return self.reply(drive.files[metadata["id"]][0])
                if self.command == "PATCH" and path.startswith("/files/"):
                    update = json.loads(body)
                    if not (update == {"trashed": True} or (
                        set(update) == {"name"}
                        and update["name"].startswith("smoke.txt.remote-history-")
                    )):
                        raise ValueError("expected explicit trash or history rename")
                    metadata, _ = drive.files[path.split("/")[-1]]
                    metadata.update(update)
                    drive.changed(metadata)
                    return self.reply(metadata)
                raise ValueError("unsupported route")

        return Handler


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


def smoke(args):
    args.artifacts.mkdir(parents=True, exist_ok=True)
    run = args.artifacts.resolve() / f"run-{uuid.uuid4().hex[:12]}"
    run.mkdir(mode=0o700)
    children = []
    drive = FakeDrive()
    started = time.monotonic()
    deadline = started + args.deadline
    stage = "initializing"
    checks = []

    def wait(description, predicate):
        nonlocal stage
        stage = description
        while True:
            for name, child in children:
                if child.poll() is not None:
                    raise RuntimeError(f"{name} exited early ({child.returncode}) during {stage}")
            if drive.errors:
                raise RuntimeError(f"fake Drive rejected request: {drive.errors}")
            if time.monotonic() >= deadline:
                raise RuntimeError(f"deadline exceeded during {stage}")
            if predicate():
                checks.append(description)
                print(f"PASS: {description}", flush=True)
                return
            time.sleep(0.05)

    with ExitStack() as stack:
        def cleanup_report():
            (run / "processes.json").write_text(json.dumps([
                {"name": name, "pid": child.pid, "returncode": child.poll()}
                for name, child in children
            ], indent=2))

        stack.callback(cleanup_report)

        def launch(name, command, env):
            nonlocal stage
            stage = f"starting {name}"
            log = stack.enter_context((run / f"{name}.log").open("w"))
            child = subprocess.Popen(
                command, env=env, cwd=run, stdout=log, stderr=subprocess.STDOUT,
            )
            children.append((name, child))
            stack.callback(stop, child)
            return child

        try:
            for directory in ("config/gdrive-client", "data/gdrive-client", "local", "runtime", "empty"):
                (run / directory).mkdir(parents=True, mode=0o700)
            environment = {
                "PATH": os.defpath, "LANG": "C.UTF-8", "HOME": str(run),
                "XDG_CONFIG_HOME": str(run / "config"), "XDG_DATA_HOME": str(run / "data"),
                "XDG_CACHE_HOME": str(run / "cache"), "XDG_RUNTIME_DIR": str(run / "runtime"),
                "XDG_DATA_DIRS": str(run / "empty"), "XDG_CONFIG_DIRS": str(run / "empty"),
                "GDRIVE_UI_TEST_BUS": "1", "RUST_LOG": "info",
                "QT_QPA_PLATFORM": "offscreen", "QT_QUICK_BACKEND": "software",
                "QT_QUICK_CONTROLS_STYLE": "Fusion", "QT_QPA_PLATFORMTHEME": "generic",
                "QML_DISABLE_DISK_CACHE": "1",
                "GDRIVE_TEST_UI_SNAPSHOT": str(run / "ui.json"),
            }
            server = ThreadingHTTPServer(("127.0.0.1", 0), drive.handler())
            server.daemon_threads = True
            server.block_on_close = False
            server.timeout = 0.1
            stack.callback(server.server_close)
            stopping = threading.Event()

            def serve():
                while not stopping.is_set():
                    server.handle_request()

            thread = threading.Thread(target=serve, daemon=True)
            thread.start()
            stack.callback(thread.join, 3)
            stack.callback(stopping.set)
            environment["GDRIVE_TEST_DRIVE_ORIGIN"] = f"http://127.0.0.1:{server.server_port}"
            (run / "config/gdrive-client/config.toml").write_text(
                'poll_interval_secs = 5\n[[sync_folders]]\n'
                'id = "00000000-0000-0000-0000-000000000001"\n'
                'display_name = "Hidden smoke folder"\nenabled = true\n'
                f'local_path = {json.dumps(str(run / "local"))}\n'
                'owner_user = ""\nowner_group = ""\n'
            )
            (run / "data/gdrive-client/oauth-token.json").write_text(json.dumps({
                "access_token": TOKEN, "refresh_token": None, "expires_at": 4102444800,
            }))
            address = (
                f"unix:abstract=gdrive-smoke-{uuid.uuid4().hex}"
                if sys.platform.startswith("linux") else f"unix:path={run / 'bus'}"
            )
            environment["DBUS_SESSION_BUS_ADDRESS"] = address
            # No service directories: neither desktop services nor the user's
            # installed daemon can be activated by this private bus.
            (run / "bus.conf").write_text(
                f"<busconfig><type>session</type><listen>{address}</listen>"
                "<auth>EXTERNAL</auth><policy context=\"default\">"
                "<allow own=\"*\"/><allow send_destination=\"*\"/>"
                "<allow receive_sender=\"*\"/></policy></busconfig>"
            )
            launch("bus", ["dbus-daemon", "--nofork", f"--config-file={run / 'bus.conf'}"], environment)

            def has_owner(name):
                result = subprocess.run(
                    ["busctl", "--user", "--timeout=1s", "call", "org.freedesktop.DBus",
                     "/org/freedesktop/DBus", "org.freedesktop.DBus", "NameHasOwner", "s", name],
                    env=environment, capture_output=True, text=True, timeout=2,
                )
                return result.returncode == 0 and result.stdout.strip() == "b true"

            wait("private bus ready", lambda: has_owner("org.freedesktop.DBus"))
            launch("daemon", [str(args.bin_dir / "gdrived"), "--test-smoke"], environment)
            wait("real daemon owns private bus name", lambda: has_owner("org.gclient.GDrive1"))
            launch("ui", [str(args.bin_dir / "gdrive-ui"), "--test-smoke"], environment)

            def snapshot():
                path = run / "ui.json"
                return json.loads(path.read_text()) if path.exists() else {}

            def ui_connected():
                state = snapshot()
                return (
                    state.get("rootCount") == 1 and state.get("widgetMenu")
                    and state.get("platformWidgetMenu")
                    and {"Connected to gdrived", "Sign out", "Hidden smoke folder", "syncing"}
                    <= set(state.get("texts", []))
                )

            wait("production QML displays daemon folder/authentication and widget-backed menu", ui_connected)
            wait("closing production Add clears native-picker loading", lambda: (
                snapshot().get("pickerCancelledOnAddClose") is True
            ))
            (run / "ui-connected.json").write_text(json.dumps(snapshot(), indent=2))
            local = run / "local/smoke.txt"
            wait("initial remote download completed", lambda: (
                local.exists() and local.read_bytes() == INITIAL
                and "InitialSyncComplete" in (run / "daemon.log").read_text()
            ))
            local.write_bytes(LOCAL)
            wait("local modification uploaded to fake Drive", lambda: (
                drive.active(LOCAL)
                and "LocalChangeUploaded" in (run / "daemon.log").read_text()
            ))
            with drive.lock:
                drive.put(drive.active(LOCAL), REMOTE)
                drive.fail_media = True
            wait("UI reports injected remote download failure", lambda: (
                snapshot().get("syncState") == "error" and snapshot().get("syncError")
                and any("Sync error\n" in tooltip for tooltip in snapshot().get("tooltips", []))
            ))
            (run / "ui-error.json").write_text(json.dumps(snapshot(), indent=2))
            if local.read_bytes() != LOCAL:
                raise RuntimeError("failed download modified local content")
            with drive.lock:
                drive.fail_media = False
            wait("remote modification downloaded after retry", lambda: local.read_bytes() == REMOTE)
            wait("UI recovers to connected idle without sync errors", lambda: (
                ui_connected() and snapshot().get("syncState") == "idle"
                and snapshot().get("syncError") == ""
                and "g-client - Idle" in snapshot().get("tooltips", [])
            ))
            # A final liveness check is mandatory even if the last predicate
            # happened to become true just before a child aborted.
            wait("all owned processes remain alive", lambda: True)
            result = {
                "success": True, "elapsed_seconds": round(time.monotonic() - started, 2),
                "checks": checks,
            }
        except Exception as error:
            result = {"success": False, "stage": stage, "error": str(error), "checks": checks}
            raise
        finally:
            with drive.lock:
                (run / "requests.json").write_text(json.dumps(drive.requests, indent=2))
                (run / "fake-errors.json").write_text(json.dumps(drive.errors, indent=2))
            (run / "result.json").write_text(json.dumps(result, indent=2))
            print(f"Artifacts: {run}", flush=True)
    # Keep logs/snapshots, but do not accumulate even synthetic token caches.
    for directory in ("config", "data", "local", "runtime", "cache", "empty"):
        shutil.rmtree(run / directory, ignore_errors=True)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--bin-dir", type=Path, default=Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "debug")
    parser.add_argument("--deadline", type=float, default=60)
    args = parser.parse_args()
    if not 5 <= args.deadline <= 120:
        parser.error("--deadline must be between 5 and 120 seconds")
    args.bin_dir = args.bin_dir.resolve()
    try:
        print(json.dumps(smoke(args)))
    except Exception as error:
        parser.exit(1, f"FAIL: {error}\n")


if __name__ == "__main__":
    main()
