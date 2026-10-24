#!/usr/bin/python3
"""Bounded, PID-scoped semantic Qt UI control using the system GI bindings."""

import argparse
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import time


class UIError(Exception):
    pass


class NotFound(UIError):
    pass


def positive_float(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be finite and greater than zero")
    return number


def positive_int(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return number


def load_atspi(call_timeout_ms):
    try:
        import gi
        gi.require_version("Atspi", "2.0")
        from gi.repository import Atspi
    except (ImportError, ValueError) as exc:
        raise UIError(
            "System Python GI/AT-SPI bindings are unavailable. Use /usr/bin/python3. "
            "On Debian/Ubuntu ask the user to run: "
            "sudo apt install python3-gi gir1.2-atspi-2.0 at-spi2-core. "
            "This helper never installs packages."
        ) from exc
    if not hasattr(Atspi, "set_timeout"):
        raise UIError("AT-SPI lacks set_timeout; a bounded IPC client is required.")
    Atspi.set_timeout(call_timeout_ms, call_timeout_ms)
    return Atspi


class UI:
    def __init__(self, atspi, pid, max_nodes=1000, max_depth=40):
        self.atspi = atspi
        self.pid = pid
        self.max_nodes = max_nodes
        self.max_depth = max_depth

    def application(self):
        desktop = self.atspi.get_desktop(0)
        if desktop is None:
            raise UIError("No AT-SPI desktop. Run in the graphical user's D-Bus session.")
        desktop.clear_cache()
        count = desktop.get_child_count()
        if count > self.max_nodes:
            raise UIError("Desktop exceeds --max-nodes; refusing incomplete PID selection.")
        matches = []
        for index in range(count):
            app = desktop.get_child_at_index(index)
            if app is None:
                continue
            app.clear_cache()
            if app.get_process_id() == self.pid:
                matches.append(app)
        if not matches:
            raise NotFound(
                f"No accessible application for exact PID {self.pid}. "
                "Check it is alive, belongs to this graphical session, and was launched "
                "with QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1. An offscreen Qt process "
                "is not a replacement for a desktop AT-SPI test."
            )
        if len(matches) != 1:
            raise UIError(f"Multiple AT-SPI applications advertise PID {self.pid}; refusing.")
        return matches[0]

    def states(self, node):
        state_set = node.get_state_set()
        return {
            name: state_set.contains(getattr(self.atspi.StateType, name.upper()))
            for name in ("showing", "visible", "enabled", "sensitive", "focused")
        }

    def walk(self, root):
        """Visit a bounded tree; never silently truncate a selector search."""
        stack = [(root, (), 0)]
        visited = 0
        while stack:
            node, ancestors, depth = stack.pop()
            visited += 1
            if visited > self.max_nodes:
                raise UIError("Tree exceeds --max-nodes; refusing an incomplete search.")
            if node is None:
                raise UIError("Accessible tree changed during traversal; retry.")
            node.clear_cache()
            name = node.get_name() or ""
            role = node.get_role_name() or ""
            states = self.states(node)
            yield node, {
                "name": name,
                "role": role,
                "ancestors": list(ancestors),
                "depth": depth,
                "states": states,
            }
            count = node.get_child_count()
            if count < 0:
                raise UIError("Accessible child count is unavailable; retry.")
            if count and depth >= self.max_depth:
                raise UIError("Tree exceeds --max-depth; refusing an incomplete search.")
            if visited + len(stack) + count > self.max_nodes:
                raise UIError("Tree exceeds --max-nodes; refusing an incomplete search.")
            for index in reversed(range(count)):
                child = node.get_child_at_index(index)
                stack.append((child, ancestors + (name,), depth + 1))

    def matches(self, root, names, role=None, ancestor=None, include_hidden=False):
        found = []
        for node, info in self.walk(root):
            if info["name"] not in names:
                continue
            if role and info["role"] != role:
                continue
            if ancestor and ancestor not in info["ancestors"]:
                continue
            if not include_hidden and not info["states"]["showing"]:
                continue
            found.append((node, info))
        return found

    def select(self, root, args):
        found = self.matches(
            root, args["name"], args.get("role"), args.get("ancestor"),
            args.get("include_hidden", False),
        )
        if not found:
            raise NotFound(f"No matching accessible control: {args['name']!r}")
        if len(found) != 1:
            raise UIError(
                f"Ambiguous selector ({len(found)} matches); specify --role/--ancestor. "
                "No action was taken."
            )
        return found[0]

    def details(self, node, info):
        action = node.get_action_iface()
        return {
            **info,
            "actions": [
                action.get_action_name(index)
                for index in range(action.get_n_actions())
            ] if action is not None else [],
            "editable": node.get_editable_text_iface() is not None,
        }

    def act(self, node, info, command, args):
        if not info["states"]["showing"]:
            raise UIError("Refusing to act on a control that is not showing.")
        if not info["states"]["enabled"] or not info["states"]["sensitive"]:
            raise UIError("Refusing to act on a disabled or insensitive control.")
        if command == "edit":
            editable = node.get_editable_text_iface()
            if editable is None:
                raise UIError("Control does not expose the AT-SPI EditableText interface.")
            if not editable.set_text_contents(args["text"]):
                raise UIError("AT-SPI EditableText rejected set_text_contents.")
            return {**info, "operation": "set_text_contents", "success": True}
        action = node.get_action_iface()
        if action is None:
            raise UIError("Control does not expose the AT-SPI Action interface.")
        names = [action.get_action_name(i) for i in range(action.get_n_actions())]
        if command == "invoke":
            candidates = [i for i, name in enumerate(names) if name == args["action"]]
        else:
            candidates = [
                i for i, name in enumerate(names)
                if name.casefold() in {"press", "click", "activate"}
            ]
        semantic_names = {names[i].casefold() for i in candidates}
        if len(semantic_names) != 1:
            raise UIError(
                f"No unique semantic action; available actions: {names!r}. "
                "Use invoke --action with an exact advertised action name."
            )
        index = candidates[0]
        if not action.do_action(index):
            raise UIError(f"AT-SPI action {names[index]!r} returned false.")
        return {**info, "operation": names[index], "success": True}


def worker(args, atspi=None):
    atspi = atspi if atspi is not None else load_atspi(args["call_timeout_ms"])
    ui = UI(atspi, args.get("pid"), args["max_nodes"], args["max_depth"])
    command = args["command"]
    if command == "check" and not args.get("pid"):
        desktop = atspi.get_desktop(0)
        if desktop is None:
            raise UIError("No AT-SPI desktop in this D-Bus session.")
        desktop.clear_cache()
        return {
            "atspi": True,
            "desktop_children": desktop.get_child_count(),
            "session_type": os.environ.get("XDG_SESSION_TYPE", "unknown"),
            "pid_verified": False,
            "note": "Use --pid PID check to verify your app is exposed.",
        }
    if command == "wait":
        deadline = time.monotonic() + args["wait_seconds"]
        while True:
            try:
                node, info = ui.select(ui.application(), args)
                return ui.details(node, info)
            except NotFound:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise NotFound(
                        f"Wait expired after {args['wait_seconds']}s for {args['name']!r} "
                        f"in exact PID {args['pid']}."
                    ) from None
                time.sleep(min(args["poll_seconds"], remaining))
    app = ui.application()
    if command == "check":
        return {"atspi": True, "pid_verified": True, "pid": args["pid"],
                "application": app.get_name()}
    if command == "tree":
        return {"pid": args["pid"], "nodes": [info for _, info in ui.walk(app)]}
    node, info = ui.select(app, args)
    if command == "find":
        return ui.details(node, info)
    return ui.act(node, info, command, args)


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--pid", type=positive_int, help="Exact owned application's PID")
    result.add_argument("--timeout", type=positive_float, default=15,
                        help="Hard wall-clock worker budget in seconds (default: 15)")
    result.add_argument("--call-timeout-ms", type=positive_int, default=1500,
                        help="AT-SPI normal/startup IPC timeout (default: 1500)")
    result.add_argument("--max-nodes", type=positive_int, default=1000)
    result.add_argument("--max-depth", type=positive_int, default=40)
    commands = result.add_subparsers(dest="command", required=True)
    commands.add_parser("check", help="Check GI/AT-SPI and, optionally, exact PID exposure")
    commands.add_parser("tree", help="Print accessible names, roles, states and ancestry")
    for command in ("find", "wait", "press", "invoke", "edit"):
        sub = commands.add_parser(command)
        sub.add_argument("--name", action="append", required=True,
                         help="Exact accessible name; repeat for explicit spelling variants")
        sub.add_argument("--role", help="Exact AT-SPI role name from tree")
        sub.add_argument("--ancestor", help="Exact ancestor accessible name from tree")
        if command in ("find", "wait"):
            sub.add_argument("--include-hidden", action="store_true")
        if command == "wait":
            sub.add_argument("--wait-seconds", type=positive_float, default=8)
            sub.add_argument("--poll-seconds", type=positive_float, default=0.25)
        if command == "invoke":
            sub.add_argument("--action", required=True)
        if command == "edit":
            sub.add_argument("--text", required=True)
    return result


def run_bounded(args):
    """A disposable worker bounds GI startup, discovery and all synchronous IPC."""
    try:
        completed = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), "--worker", json.dumps(args)],
            capture_output=True, text=True, timeout=args["timeout"], check=False,
        )
    except subprocess.TimeoutExpired:
        print(json.dumps({
            "error": f"Overall deadline ({args['timeout']}s) expired. Worker killed; "
                     "the UI was not killed. An issued action may already have completed; "
                     "inspect state before retrying."
        }), file=sys.stderr)
        return 124
    if completed.stdout:
        print(completed.stdout, end="")
    if completed.stderr:
        print(completed.stderr, end="", file=sys.stderr)
    return completed.returncode


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--worker":
        try:
            print(json.dumps(worker(json.loads(sys.argv[2])), ensure_ascii=False))
            return 0
        except Exception as exc:
            print(json.dumps({"error": str(exc), "type": type(exc).__name__}),
                  file=sys.stderr)
            return 2
    cli = parser()
    args = vars(cli.parse_args())
    if args["command"] != "check" and args["pid"] is None:
        cli.error("--pid is required for every command except check")
    if args["command"] == "wait" and args["wait_seconds"] >= args["timeout"]:
        cli.error("--wait-seconds must be less than --timeout (leave time for IPC)")
    return run_bounded(args)


if __name__ == "__main__":
    sys.exit(main())
