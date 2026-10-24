"""No desktop, packages, daemon, network, or settings needed."""

import contextlib
import io
import json
import subprocess
import types
import unittest
from unittest.mock import Mock, patch

import atspi_ui as ui
import smoke


class FakeStates:
    def __init__(self, **states):
        self.values = {
            "showing": True, "visible": True, "enabled": True,
            "sensitive": True, "focused": False, **states,
        }

    def contains(self, state):
        return self.values[state]


class Node:
    def __init__(self, name, role="push button", children=(), pid=123, **states):
        self.name = name
        self.role = role
        self.children = list(children)
        self.pid = pid
        self.state_set = FakeStates(**states)
        self.action = None
        self.editable = None
        self.cache_clears = 0

    def clear_cache(self):
        self.cache_clears += 1

    def get_name(self):
        return self.name

    def get_role_name(self):
        return self.role

    def get_process_id(self):
        return self.pid

    def get_state_set(self):
        return self.state_set

    def get_child_count(self):
        return len(self.children)

    def get_child_at_index(self, index):
        return self.children[index]

    def get_action_iface(self):
        return self.action

    def get_editable_text_iface(self):
        return self.editable


def fake_atspi(apps=()):
    states = Mock(**{
        name.upper(): name
        for name in ("showing", "visible", "enabled", "sensitive", "focused")
    })
    return Mock(StateType=states, get_desktop=Mock(return_value=Node("desktop", children=apps)))


def action(*names, accepted=True):
    result = Mock()
    result.get_n_actions.return_value = len(names)
    result.get_action_name.side_effect = lambda i: names[i]
    result.do_action.return_value = accepted
    return result


class SelectorTests(unittest.TestCase):
    def setUp(self):
        self.ui = ui.UI(fake_atspi(), 123)

    def test_exact_pid_not_application_name(self):
        other = Node("gdrive-ui", pid=999)
        owned = Node("gdrive-ui", pid=123)
        client = ui.UI(fake_atspi([other, owned]), 123)
        self.assertIs(client.application(), owned)
        with self.assertRaises(ui.NotFound):
            ui.UI(fake_atspi([other]), 123).application()

    def test_duplicate_pid_refused(self):
        with self.assertRaisesRegex(ui.UIError, "Multiple"):
            ui.UI(fake_atspi([Node("a"), Node("b")]), 123).application()

    def test_exact_names_hidden_and_ancestry(self):
        hidden = Node("Cancel", showing=False)
        target = Node("Cancel")
        root = Node("app", children=[
            hidden, Node("Cancel everything"), Node("Drive", children=[target]),
        ])
        self.assertIs(self.ui.select(root, {"name": ["Cancel"]})[0], target)
        self.assertIs(self.ui.select(root, {
            "name": ["Cancel"], "ancestor": "Drive", "role": "push button",
        })[0], target)
        with self.assertRaisesRegex(ui.UIError, "Ambiguous"):
            self.ui.select(root, {"name": ["Cancel"], "include_hidden": True})
        with self.assertRaises(ui.NotFound):
            self.ui.select(root, {"name": ["cancel"]})

    def test_explicit_variants_must_still_be_unique(self):
        root = Node("app", children=[Node("Add folder"), Node("Add folder…")])
        with self.assertRaisesRegex(ui.UIError, "Ambiguous"):
            self.ui.select(root, {"name": ["Add folder", "Add folder…"]})

    def test_node_and_depth_limits_fail_closed(self):
        root = Node("app", children=[Node("x"), Node("y")])
        with self.assertRaisesRegex(ui.UIError, "max-nodes"):
            list(ui.UI(fake_atspi(), 123, max_nodes=2).walk(root))
        root = Node("app", children=[Node("child", children=[Node("grandchild")])])
        with self.assertRaisesRegex(ui.UIError, "max-depth"):
            list(ui.UI(fake_atspi(), 123, max_depth=1).walk(root))

    def test_changed_tree_refused(self):
        with self.assertRaisesRegex(ui.UIError, "changed"):
            list(self.ui.walk(Node("app", children=[None])))


class ActionTests(unittest.TestCase):
    def setUp(self):
        self.ui = ui.UI(fake_atspi(), 123)
        self.node = Node("Cancel")

    def perform(self, command="press", **args):
        info = next(self.ui.walk(self.node))[1]
        return self.ui.act(self.node, info, command, args)

    def test_press_uses_advertised_semantic_action(self):
        self.node.action = action("show menu", "Press")
        self.assertTrue(self.perform()["success"])
        self.node.action.do_action.assert_called_once_with(1)

    def test_press_never_chooses_setfocus(self):
        self.node.action = action("SetFocus", "Press")
        self.assertEqual(self.perform()["operation"], "Press")
        self.node.action.do_action.assert_called_once_with(1)

    def test_named_press_invocation_never_chooses_setfocus(self):
        self.node.action = action("SetFocus", "Press")
        self.assertEqual(self.perform("invoke", action="Press")["operation"], "Press")
        self.node.action.do_action.assert_called_once_with(1)

    def test_duplicate_press_names_choose_first_equivalent_index(self):
        self.node.action = action("Press", "SetFocus", "Press")
        self.assertEqual(self.perform()["operation"], "Press")
        self.node.action.do_action.assert_called_once_with(0)

    def test_case_variants_of_press_choose_first_equivalent_index(self):
        self.node.action = action("SetFocus", "pReSs", "Press")
        self.assertEqual(self.perform()["operation"], "pReSs")
        self.node.action.do_action.assert_called_once_with(1)

    def test_explicit_invoke_allows_identical_duplicate_names(self):
        self.node.action = action("SetFocus", "Press", "Press")
        self.assertEqual(self.perform("invoke", action="Press")["operation"], "Press")
        self.node.action.do_action.assert_called_once_with(1)

    def test_explicit_invoke_preserves_case_sensitive_matching(self):
        self.node.action = action("press", "SetFocus", "Press", "Press")
        self.assertEqual(self.perform("invoke", action="Press")["operation"], "Press")
        self.node.action.do_action.assert_called_once_with(2)

    def test_invoke_requires_exact_action(self):
        self.node.action = action("toggle")
        self.perform("invoke", action="toggle")
        self.node.action.do_action.assert_called_once_with(0)

    def test_no_coordinate_or_arbitrary_action_fallback(self):
        for names in ((), ("toggle",), ("press", "click")):
            self.node.action = action(*names)
            with self.assertRaises(ui.UIError):
                self.perform()
            self.node.action.do_action.assert_not_called()

    def test_disabled_and_hidden_controls_refused(self):
        for states in ({"enabled": False}, {"sensitive": False}, {"showing": False}):
            self.node.state_set = FakeStates(**states)
            self.node.action = action("press")
            with self.assertRaises(ui.UIError):
                self.perform()
            self.node.action.do_action.assert_not_called()

    def test_false_action_result_reported(self):
        self.node.action = action("press", accepted=False)
        with self.assertRaisesRegex(ui.UIError, "returned false"):
            self.perform()

    def test_edit_requires_editable_text_and_checks_result(self):
        with self.assertRaisesRegex(ui.UIError, "EditableText"):
            self.perform("edit", text="example")
        self.node.editable = Mock()
        self.node.editable.set_text_contents.return_value = True
        self.perform("edit", text="example")
        self.node.editable.set_text_contents.assert_called_once_with("example")
        self.node.editable.set_text_contents.return_value = False
        with self.assertRaisesRegex(ui.UIError, "rejected"):
            self.perform("edit", text="example")


class DeadlineTests(unittest.TestCase):
    def test_wait_invalidates_cached_hidden_state_between_polls(self):
        class CachedNode(Node):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, **kwargs)
                self.cached_states = None

            def get_state_set(self):
                if self.cached_states is None:
                    self.cached_states = FakeStates(**self.state_set.values)
                return self.cached_states

            def clear_cache(self):
                super().clear_cache()
                self.cached_states = None

        retry = CachedNode("Retry", showing=False)
        root = Node("app", children=[retry])
        atspi = fake_atspi([root])
        args = {
            "command": "wait", "pid": 123, "max_nodes": 100, "max_depth": 10,
            "name": ["Retry"], "wait_seconds": 2, "poll_seconds": 0.25,
        }

        def reveal(_seconds):
            retry.state_set.values["showing"] = True

        with patch.object(ui.time, "sleep", side_effect=reveal) as sleep:
            with patch.object(ui.time, "monotonic", side_effect=[0, 1, 3]):
                result = ui.worker(args, atspi=atspi)
        self.assertEqual(result["name"], "Retry")
        self.assertTrue(result["states"]["showing"])
        self.assertEqual(retry.cache_clears, 2)
        self.assertEqual(atspi.get_desktop.return_value.cache_clears, 2)
        sleep.assert_called_once()

    def test_poll_refreshes_new_children_not_just_states(self):
        class CachedChildren(Node):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, **kwargs)
                self.cached_children = None

            def clear_cache(self):
                super().clear_cache()
                self.cached_children = None

            def get_child_count(self):
                if self.cached_children is None:
                    self.cached_children = list(self.children)
                return len(self.cached_children)

            def get_child_at_index(self, index):
                return self.cached_children[index]

        root = CachedChildren("app")
        atspi = fake_atspi([root])
        args = {
            "command": "wait", "pid": 123, "max_nodes": 100, "max_depth": 10,
            "name": ["Retry"], "wait_seconds": 2, "poll_seconds": 0.25,
        }

        def insert_child(_seconds):
            root.children.append(Node("Retry"))

        with patch.object(ui.time, "sleep", side_effect=insert_child):
            with patch.object(ui.time, "monotonic", side_effect=[0, 1, 3]):
                self.assertEqual(ui.worker(args, atspi=atspi)["name"], "Retry")

    def test_gi_normal_and_startup_calls_have_timeout(self):
        atspi = Mock()
        gi = Mock()
        repository = types.ModuleType("gi.repository")
        repository.Atspi = atspi
        with patch.dict("sys.modules", {"gi": gi, "gi.repository": repository}):
            self.assertIs(ui.load_atspi(1500), atspi)
        gi.require_version.assert_called_once_with("Atspi", "2.0")
        atspi.set_timeout.assert_called_once_with(1500, 1500)

    def test_missing_gi_diagnostic_never_installs(self):
        with patch.dict("sys.modules", {"gi": None}):
            with self.assertRaisesRegex(ui.UIError, "never installs packages"):
                ui.load_atspi(1500)

    def test_hard_deadline_kills_worker_not_ui(self):
        args = {"timeout": 0.1, "pid": 123}
        with patch.object(ui.subprocess, "run", side_effect=subprocess.TimeoutExpired("worker", 0.1)) as run:
            with contextlib.redirect_stderr(io.StringIO()) as stderr:
                self.assertEqual(ui.run_bounded(args), 124)
        self.assertIn("already have completed", stderr.getvalue())
        self.assertEqual(run.call_args.kwargs["timeout"], 0.1)
        self.assertNotIn("kill", run.call_args.args[0])

    def test_wait_retries_only_missing_controls(self):
        args = {
            "command": "wait", "pid": 123, "max_nodes": 100, "max_depth": 10,
            "name": ["Cancel"], "wait_seconds": 3, "poll_seconds": 0.25,
        }
        client = Mock()
        client.select.side_effect = [ui.NotFound("not yet"), (Mock(), {"name": "Cancel"})]
        client.details.return_value = {"name": "Cancel"}
        with patch.object(ui, "UI", return_value=client), patch.object(ui.time, "sleep"):
            self.assertEqual(ui.worker(args, atspi=Mock()), {"name": "Cancel"})
        client.select.side_effect = ui.UIError("Ambiguous")
        with patch.object(ui, "UI", return_value=client):
            with self.assertRaisesRegex(ui.UIError, "Ambiguous"):
                ui.worker(args, atspi=Mock())

    def test_wait_expiry(self):
        args = {
            "command": "wait", "pid": 123, "max_nodes": 100, "max_depth": 10,
            "name": ["Cancel"], "wait_seconds": 1, "poll_seconds": 0.25,
        }
        client = Mock()
        client.select.side_effect = ui.NotFound("missing")
        with patch.object(ui, "UI", return_value=client):
            with patch.object(ui.time, "monotonic", side_effect=[10, 12]):
                with self.assertRaisesRegex(ui.NotFound, "Wait expired"):
                    ui.worker(args, atspi=Mock())

    def test_timeout_validation(self):
        for value in ("0", "-1", "nan", "inf"):
            with self.assertRaises(Exception):
                ui.positive_float(value)

    def test_smoke_refuses_existing_cancel(self):
        tree = {"nodes": [{"name": "Cancel", "states": {"showing": True}}]}
        outputs = [
            subprocess.CompletedProcess([], 0, '{"pid_verified": true}', ""),
            subprocess.CompletedProcess([], 0, json.dumps(tree), ""),
        ]
        with patch.object(smoke.subprocess, "run", side_effect=outputs) as run:
            with self.assertRaisesRegex(ui.UIError, "already showing"):
                smoke.smoke(123, 60, ["Add folder"])
        self.assertEqual(run.call_count, 2)

    def test_smoke_success_uses_direct_workers_and_no_save_action(self):
        closed = {"nodes": []}
        outputs = [
            {"pid_verified": True}, closed, {"name": "Add folder"},
            {"success": True}, {"name": "Cancel"}, {"success": True},
            closed, {"name": "Add folder"},
        ]
        completed = [
            subprocess.CompletedProcess([], 0, json.dumps(output), "")
            for output in outputs
        ]
        with patch.object(smoke.subprocess, "run", side_effect=completed) as run:
            result = smoke.smoke(123, 60, ["Add folder"])
        self.assertTrue(result["success"])
        self.assertFalse(result["settings_saved"])
        requests = []
        for call in run.call_args_list:
            self.assertEqual(call.args[0][-2], "--worker")
            request = json.loads(call.args[0][-1])
            self.assertEqual(request["pid"], 123)
            self.assertLessEqual(call.kwargs["timeout"], 15)
            requests.append(request)
        presses = [
            request["name"] for request in requests
            if request["command"] == "press"
        ]
        self.assertEqual(presses, [["Add folder"], ["Cancel"]])


if __name__ == "__main__":
    unittest.main()
