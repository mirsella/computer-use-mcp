#!/usr/bin/env python3
"""Exercise MCP directly or through the isolated-session runner."""

import base64
import binascii
import json
import os
import signal
import subprocess
import sys
import time


class Client:
    def __init__(
        self,
        binary=None,
        command=None,
        env=None,
        desktop=None,
        native_runner=None,
        compact=False,
    ):
        self.next_id = 1
        self.desktop = desktop
        self.compact = compact
        self.schemas = {}
        self.native_runner = (
            command is not None if native_runner is None else native_runner
        )
        if command is None:
            command = [binary, "mcp"]
            if compact:
                command.append("--compact-tools")
        self.process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=env,
        )

    def request(self, method, params):
        if (
            self.compact
            and method == "tools/call"
            and params["name"] not in {"help", "dispatch"}
        ):
            action = params["name"]
            if action not in self.schemas:
                help_response = self.request(
                    "tools/call", {"name": "help", "arguments": {"action": action}}
                )
                schema = json.loads(help_response["result"]["content"][0]["text"])
                assert schema["name"] == action
                self.schemas[action] = schema
                print(f"schema_loaded={action}")
            params = {
                "name": "dispatch",
                "arguments": {"action": action, "arguments": params["arguments"]},
            }
        request_id = self.next_id
        self.next_id += 1
        self.process.stdin.write(
            json.dumps(
                {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
            )
            + "\n"
        )
        self.process.stdin.flush()
        while True:
            line = self.process.stdout.readline()
            if not line:
                raise RuntimeError("MCP server closed stdout before its response")
            response = json.loads(line)
            if response.get("id") == request_id:
                return response

    def notify(self, method, params):
        self.process.stdin.write(
            json.dumps({"jsonrpc": "2.0", "method": method, "params": params}) + "\n"
        )
        self.process.stdin.flush()

    def close(self):
        if self.process.poll() is not None:
            return
        self.process.stdin.close()
        try:
            self.process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()


def call(client, name, arguments):
    desktop_allowed = name in {"list_desktop", "launch_application"}
    if name == "wait_for":
        condition = arguments.get("condition") or {}
        desktop_allowed = (
            condition.get("type") == "window_opened" and "target" not in arguments
        )
    if client.desktop is not None and desktop_allowed and "desktop" not in arguments:
        arguments = {**arguments, "desktop": client.desktop}
    response = client.request("tools/call", {"name": name, "arguments": arguments})
    if "error" in response:
        raise RuntimeError(f"{name}: JSON-RPC error: {response['error']}")
    result = response["result"]
    return result, result.get("structuredContent")


def windows(structured):
    return structured.get("windows", []) if isinstance(structured, dict) else []


def target_in(value):
    if isinstance(value, dict):
        if {"app_instance_id", "window_instance_id"} <= value.keys():
            return {
                "app_instance_id": value["app_instance_id"],
                "window_instance_id": value["window_instance_id"],
            }
        for child in value.values():
            target = target_in(child)
            if target:
                return target
    if isinstance(value, list):
        for child in value:
            target = target_in(child)
            if target:
                return target
    return None


def element(structured, predicate):
    for value in (structured or {}).get("elements", []):
        if predicate(value):
            return value
    return None


def dispatch_completed(result):
    progress = (result.get("structuredContent") or {}).get("action_progress") or {}
    return progress.get("dispatch_stage") == "completed"


def element_metadata(value):
    advertised = value.get("capabilities") or {}
    return {
        "element_id": value.get("element_id"),
        "role": value.get("role"),
        "states": value.get("states"),
        "capabilities": {
            key: advertised.get(key) for key in ("focus", "invoke", "set_value")
        },
        "has_text": value.get("text") is not None,
        "has_value": value.get("value") is not None,
    }


def print_text_candidates(observed, label):
    candidates = [
        element_metadata(value)
        for value in (observed or {}).get("elements", [])
        if value.get("capabilities", {}).get("set_value") == "text"
    ]
    print(label + "=" + json.dumps(candidates, sort_keys=True))


def print_element_candidates(observed, label):
    candidates = [
        {
            "element_id": value.get("element_id"),
            "role": value.get("role"),
            "name": value.get("name"),
            "capabilities": {
                key: (value.get("capabilities") or {}).get(key)
                for key in ("focus", "invoke", "set_value")
            },
        }
        for value in (observed or {}).get("elements", [])
    ]
    print(label + "=" + json.dumps(candidates, sort_keys=True))


def print_value_evidence(observed, label, needles):
    evidence = []
    for value in (observed or {}).get("elements", []):
        if value.get("capabilities", {}).get("set_value") != "text":
            continue
        text = value.get("text") or ""
        current = value.get("value") or ""
        evidence.append(
            {
                "element_id": value.get("element_id"),
                "text_length": len(text),
                "value_length": len(current),
                "contains": {
                    needle: needle in text or needle in current for needle in needles
                },
            }
        )
    print(label + "=" + json.dumps(evidence, sort_keys=True))


def response_error_text(result):
    if result.get("isError") is not True:
        return None
    messages = [
        item.get("text")
        for item in result.get("content", [])
        if isinstance(item, dict) and isinstance(item.get("text"), str)
    ]
    return " ".join(messages)[0:240] if messages else None


def png_bytes(result):
    for item in result.get("content", []):
        if isinstance(item, dict) and item.get("type") == "image":
            data = item.get("data")
            if isinstance(data, str):
                try:
                    return len(base64.b64decode(data, validate=True))
                except (ValueError, binascii.Error):
                    return None
    return None


def save_png(result):
    destination = os.environ.get("ISOLATED_MCP_SMOKE_PNG")
    if not destination:
        return
    for item in result.get("content", []):
        if isinstance(item, dict) and item.get("type") == "image":
            data = item.get("data")
            if isinstance(data, str):
                with open(destination, "wb") as output:
                    output.write(base64.b64decode(data, validate=True))
                return


def print_observation_response(result, observed, label):
    screenshot = (observed or {}).get("screenshot") or {}
    print(
        label
        + "="
        + json.dumps(
            {
                "content_types": [
                    item.get("type")
                    for item in result.get("content", [])
                    if isinstance(item, dict)
                ],
                "frame": screenshot.get("frame_id"),
                "ready": screenshot.get("ready"),
                "png_bytes": png_bytes(result),
                "structured_keys": sorted((observed or {}).keys()),
                "elements": len((observed or {}).get("elements", [])),
                "is_error": result.get("isError") is True,
                "error": response_error_text(result),
            },
            sort_keys=True,
        )
    )


def private_process(pid):
    if not isinstance(pid, int) or pid <= 0:
        return False
    try:
        with open(f"/proc/{pid}/environ", "rb") as environment:
            values = set(environment.read().split(b"\0"))
    except (FileNotFoundError, PermissionError, OSError):
        return False
    return {
        f"XDG_RUNTIME_DIR={os.environ['XDG_RUNTIME_DIR']}".encode(),
        f"WAYLAND_DISPLAY={os.environ['WAYLAND_DISPLAY']}".encode(),
    } <= values


def process_arguments(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as command_line:
            return [
                value.decode(errors="replace")
                for value in command_line.read().split(b"\0")
                if value
            ]
    except (FileNotFoundError, PermissionError, OSError):
        return []


def process_command(pid):
    return " ".join(process_arguments(pid))


def descendants(root_pid):
    parents = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat", "r", encoding="ascii") as stat_file:
                stat = stat_file.read()
            parent = int(stat[stat.rfind(")") + 2 :].split()[1])
            parents[int(entry)] = parent
        except (FileNotFoundError, PermissionError, OSError, ValueError, IndexError):
            continue
    result = []
    pending = [root_pid]
    while pending:
        parent = pending.pop()
        children = [pid for pid, ppid in parents.items() if ppid == parent]
        pending.extend(children)
        result.extend(children)
    return sorted(set(result))


def native_worker_evidence(client, expected=None):
    workers = [
        pid
        for pid in descendants(client.process.pid)
        if (arguments := process_arguments(pid))
        and arguments[0].endswith("/computer-use-mcp")
        and "__background_worker" in arguments[1:]
    ]
    foreground = [
        pid
        for pid in descendants(client.process.pid)
        if (arguments := process_arguments(pid))
        and arguments[0].endswith("/computer-use-mcp")
        and "__desktop_worker" in arguments[1:]
    ]
    print(f"foreground_worker_count={len(foreground)}")
    if foreground or not workers:
        raise RuntimeError(
            f"native worker routing invalid: background={workers} foreground={foreground}"
        )
    if expected is not None and workers != [expected]:
        raise RuntimeError(
            f"background worker was respawned: before={expected} after={workers}"
        )
    return workers[0]


def terminate_private_process(pid):
    if not private_process(pid):
        print("private_process_termination=refused_environment_not_private")
        return False
    os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if not os.path.exists(f"/proc/{pid}"):
            print("private_process_terminated=true")
            return True
        time.sleep(0.1)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        print("private_process_terminated=true signal=KILL")
        return True
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        if not os.path.exists(f"/proc/{pid}"):
            print("private_process_terminated=true signal=KILL")
            return True
        time.sleep(0.1)
    print("private_process_termination=timeout_after_private_kill")
    return False


def installed_applications(client):
    result = []
    cursor = None
    while True:
        args = {"scope": "applications", "limit": 100}
        if cursor:
            args["cursor"] = cursor
        _, page = call(client, "list_desktop", args)
        result.extend((page or {}).get("applications", []))
        cursor = (page or {}).get("next_cursor")
        if not cursor:
            return result


def main():
    if len(sys.argv) == 2:
        client = Client(binary=sys.argv[1])
        print("transport=direct_mcp_binary")
    elif len(sys.argv) == 3 and sys.argv[1] in {"--normal-mcp", "--compact-mcp"}:
        binary = sys.argv[2]
        client = Client(
            binary=binary,
            env=os.environ.copy(),
            desktop="background",
            native_runner=True,
            compact=sys.argv[1] == "--compact-mcp",
        )
        print(
            f"transport=normal_mcp_entrypoint desktop=background wrapper=none compact={client.compact}"
        )
    elif len(sys.argv) == 4 and sys.argv[1] == "--normal-runner":
        runner, binary = sys.argv[2:]
        binary_directory = os.path.dirname(os.path.abspath(binary))
        environment = os.environ.copy()
        environment.pop("COMPUTER_USE_MCP_BIN", None)
        environment["COMPUTER_USE_MCP_PRIVATE_PORTAL_AUTH"] = "require"
        environment["PATH"] = (
            binary_directory + os.pathsep + environment.get("PATH", "")
        )
        client = Client(
            command=[runner],
            env=environment,
            desktop="background",
        )
        print("transport=normal_mcp_entrypoint desktop=background")
    else:
        raise SystemExit(
            f"usage: {sys.argv[0]} MCP_BINARY | "
            f"{sys.argv[0]} --normal-mcp MCP_BINARY | --compact-mcp MCP_BINARY | "
            f"{sys.argv[0]} --normal-runner RUNNER MCP_BINARY"
        )
    target = None
    target_pid = None
    closed = False
    portal_blocked = False
    input_verified = False
    try:
        initialized = client.request(
            "initialize",
            {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "isolated-session-smoke", "version": "1"},
            },
        )
        if "error" in initialized:
            raise RuntimeError(f"initialize failed: {initialized['error']}")
        client.notify("notifications/initialized", {})

        _, initial = call(client, "list_desktop", {"scope": "windows", "limit": 100})
        background_worker_pid = None
        if client.native_runner:
            background_worker_pid = native_worker_evidence(client)
            print(f"background_worker_pid={background_worker_pid}")
        initial_ids = {
            window.get("target", {}).get("window_instance_id")
            for window in windows(initial)
        }
        apps = {app.get("desktop_id"): app for app in installed_applications(client)}
        desktop_id = next(
            (
                value
                for value in (
                    "org.kde.kwrite.desktop",
                    "org.kde.kate.desktop",
                    "org.gnome.TextEditor.desktop",
                )
                if value in apps and apps[value].get("shown", True)
            ),
            None,
        )
        if desktop_id is None:
            raise RuntimeError(
                "no disposable text-capable desktop entry was advertised"
            )
        print(f"application={desktop_id}")
        if client.native_runner:
            native_worker_evidence(client, background_worker_pid)
            print("background_worker_reused=true")

        launch, launch_data = call(
            client, "launch_application", {"desktop_id": desktop_id}
        )
        if launch.get("isError"):
            raise RuntimeError(f"launch_application failed: {launch.get('content')}")
        print(f"launch={launch_data}")
        app_name = (launch_data or {}).get("name", "").casefold()

        waited = None
        for _ in range(3):
            waited, wait_data = call(
                client,
                "wait_for",
                {
                    "condition": {"type": "window_opened", "desktop_id": desktop_id},
                    "timeout_ms": 5000,
                },
            )
            target = target_in(wait_data)
            if target:
                break
            time.sleep(1)
        if target is None:
            _, catalog = call(
                client, "list_desktop", {"scope": "windows", "limit": 100}
            )
            candidates = [
                window
                for window in windows(catalog)
                if window.get("target", {}).get("window_instance_id") not in initial_ids
                and not window.get("is_protected_surface")
                and (
                    window.get("app_id")
                    in {desktop_id, desktop_id.removesuffix(".desktop")}
                    or (app_name and app_name in window.get("title", "").casefold())
                )
            ]
            if len(candidates) == 1:
                target = candidates[0]["target"]
                target_pid = candidates[0].get("pid")
        if target is None:
            raise RuntimeError(
                f"window_opened did not return an exact target: {waited}"
            )
        if target_pid is None:
            _, catalog = call(
                client, "list_desktop", {"scope": "windows", "limit": 100}
            )
            target_pid = next(
                (
                    window.get("pid")
                    for window in windows(catalog)
                    if window.get("target") == target
                ),
                None,
            )
        else:
            _, catalog = call(
                client, "list_desktop", {"scope": "windows", "limit": 100}
            )
        target_window = next(
            (window for window in windows(catalog) if window.get("target") == target),
            {},
        )
        print(
            "window_metadata="
            + json.dumps(
                {
                    "app_id": target_window.get("app_id"),
                    "title": target_window.get("title"),
                    "pid": target_window.get("pid"),
                    "source": target_window.get("source"),
                    "capabilities": target_window.get("capabilities"),
                },
                sort_keys=True,
            )
        )
        print(
            f"target={target['app_instance_id']}/{target['window_instance_id']} pid={target_pid}"
        )
        activated, _ = call(
            client,
            "activate_window",
            {"target": target, "action": "activate"},
        )
        print(
            f"activate_dispatch_completed={dispatch_completed(activated)} "
            f"activate_result_error={activated.get('isError', False)}"
        )
        if dispatch_completed(activated):
            time.sleep(0.5)

        observe_result, observed = call(
            client,
            "observe",
            {
                "target": target,
                "view": "both",
                "accessibility": {
                    "scope": "full",
                    "limits": {"text_limit": 1000, "max_nodes": 500, "max_depth": 32},
                },
            },
        )
        save_png(observe_result)
        screenshot = (observed or {}).get("screenshot", {})
        accessibility = (observed or {}).get("accessibility", {})
        print(
            "observe="
            f"screenshot_ready:{screenshot.get('ready')} "
            f"screenshot_reason:{screenshot.get('reason')} "
            f"accessibility_ready:{accessibility.get('ready')} "
            f"elements:{len((observed or {}).get('elements', []))}"
        )
        print(
            "observe_response="
            + json.dumps(
                {
                    "result_keys": sorted(observe_result),
                    "content_types": [
                        item.get("type")
                        for item in observe_result.get("content", [])
                        if isinstance(item, dict)
                    ],
                    "frame": screenshot.get("frame_id"),
                    "crop": screenshot.get("crop"),
                    "png_bytes": png_bytes(observe_result),
                    "width": screenshot.get("width"),
                    "height": screenshot.get("height"),
                    "is_error": observe_result.get("isError") is True,
                    "error": response_error_text(observe_result),
                },
                sort_keys=True,
            )
        )
        print_text_candidates(observed, "text_candidates_initial")
        print_element_candidates(observed, "elements_initial")
        if observed and observed.get("observation_id"):
            waited, _ = call(
                client,
                "wait_for",
                {
                    "target": target,
                    "condition": {
                        "type": "accessibility_advanced",
                        "after_observation_id": observed["observation_id"],
                    },
                    "timeout_ms": 5000,
                },
            )
            print(
                "accessibility_wait_satisfied="
                + str((waited.get("structuredContent") or {}).get("satisfied"))
            )
            refreshed_result, observed = call(
                client,
                "observe",
                {
                    "target": target,
                    "view": "both",
                    "accessibility": {
                        "scope": "full",
                        "limits": {
                            "text_limit": 1000,
                            "max_nodes": 500,
                            "max_depth": 32,
                        },
                    },
                },
            )
            print_observation_response(
                refreshed_result, observed, "observe_after_accessibility_wait"
            )
            screenshot = (observed or {}).get("screenshot") or {}
            print_text_candidates(observed, "text_candidates_after_accessibility_wait")
            print_element_candidates(observed, "elements_after_accessibility_wait")
        if not screenshot.get("ready"):
            portal_blocked = True
            print("portal_eis_stage=private portal approval/capture was not ready")
            new_file = element(
                observed,
                lambda value: (
                    value.get("name") == "New File"
                    and value.get("capabilities", {}).get("invoke") is True
                ),
            )
            if new_file:
                opened, _ = call(
                    client,
                    "act",
                    {
                        "target": target,
                        "source_observation": {
                            "observation_id": observed["observation_id"]
                        },
                        "operation": {
                            "type": "semantic",
                            "element_id": new_file["element_id"],
                            "action": {"type": "invoke"},
                        },
                    },
                )
                print(f"new_file_dispatch_completed={dispatch_completed(opened)}")
                if dispatch_completed(opened):
                    time.sleep(0.5)
                    after_new_result, observed = call(
                        client,
                        "observe",
                        {
                            "target": target,
                            "view": "both",
                            "accessibility": {
                                "scope": "full",
                                "limits": {
                                    "text_limit": 1000,
                                    "max_nodes": 500,
                                    "max_depth": 32,
                                },
                            },
                        },
                    )
                    print_observation_response(
                        after_new_result, observed, "observe_after_new_file"
                    )
                    screenshot = (observed or {}).get("screenshot") or {}
                    print_text_candidates(observed, "text_candidates_after_new_file")
                    if not (observed or {}).get("screenshot", {}).get("ready"):
                        for attempt in range(2):
                            waited, _ = call(
                                client,
                                "wait_for",
                                {
                                    "target": target,
                                    "condition": {
                                        "type": "frame_stable",
                                        "for_ms": 100,
                                    },
                                    "timeout_ms": 5000,
                                },
                            )
                            print(
                                f"screenshot_wait_attempt={attempt + 1} "
                                f"satisfied={(waited.get('structuredContent') or {}).get('satisfied')}"
                            )
                            waited_result, observed = call(
                                client,
                                "observe",
                                {
                                    "target": target,
                                    "view": "both",
                                    "accessibility": {
                                        "scope": "full",
                                        "limits": {
                                            "text_limit": 1000,
                                            "max_nodes": 500,
                                            "max_depth": 32,
                                        },
                                    },
                                },
                            )
                            print_observation_response(
                                waited_result,
                                observed,
                                f"observe_after_wait_{attempt + 1}",
                            )
                            if (observed or {}).get("screenshot", {}).get("ready"):
                                screenshot = observed["screenshot"]
                                portal_blocked = False
                                break

        if not screenshot.get("ready"):
            print("portal_eis_stage=private portal capture remained unavailable")
            return 2
        portal_blocked = False
        text_element = element(
            observed,
            lambda value: value.get("capabilities", {}).get("set_value") == "text",
        )
        if text_element is None:
            new_file = element(
                observed,
                lambda value: (
                    value.get("name") == "New File"
                    and value.get("capabilities", {}).get("invoke") is True
                ),
            )
            if new_file is None:
                # AT-SPI exposes only KWrite's frame hierarchy on this host,
                # but the current PNG visibly contains the New File button.
                # Use that exact frame, never AT-SPI bounds, for one pointer
                # click and then require fresh accessibility evidence.
                opened, _ = call(
                    client,
                    "act",
                    {
                        "target": target,
                        "source_observation": {
                            "observation_id": observed["observation_id"],
                            "frame_id": screenshot["frame_id"],
                        },
                        "operation": {
                            "type": "pointer",
                            "action": {"type": "click", "x": 500, "y": 338},
                        },
                    },
                )
                print(
                    f"new_file_pointer_dispatch_completed={dispatch_completed(opened)} "
                    f"new_file_pointer_result_error={opened.get('isError', False)}"
                )
            else:
                opened, _ = call(
                    client,
                    "act",
                    {
                        "target": target,
                        "source_observation": {
                            "observation_id": observed["observation_id"]
                        },
                        "operation": {
                            "type": "semantic",
                            "element_id": new_file["element_id"],
                            "action": {"type": "invoke"},
                        },
                    },
                )
                print(
                    f"new_file_dispatch_completed={dispatch_completed(opened)} "
                    f"new_file_result_error={opened.get('isError', False)}"
                )
            if not dispatch_completed(opened):
                return 2
            time.sleep(0.5)
            after_new_result, observed = call(
                client,
                "observe",
                {
                    "target": target,
                    "view": "both",
                    "accessibility": {
                        "scope": "full",
                        "limits": {
                            "text_limit": 1000,
                            "max_nodes": 500,
                            "max_depth": 32,
                        },
                    },
                },
            )
            print_observation_response(
                after_new_result, observed, "observe_after_new_file"
            )
            screenshot = (observed or {}).get("screenshot") or {}
            text_element = element(
                observed,
                lambda value: value.get("capabilities", {}).get("set_value") == "text",
            )
            if not screenshot.get("ready"):
                print(
                    "portal_eis_stage=private frame became unavailable after New File"
                )
                return 2
        if text_element is None:
            print("no_advertised_text_set_value=true")
            return 2

        # This coordinate is taken from the exact current monitor frame above:
        # it is visibly inside KWrite's blank editor, not derived from AT-SPI
        # extents. Keep the frame and observation IDs paired with the click.
        click_x, click_y = 600, 300
        clicked, _ = call(
            client,
            "act",
            {
                "target": target,
                "source_observation": {
                    "observation_id": observed["observation_id"],
                    "frame_id": screenshot["frame_id"],
                },
                "operation": {
                    "type": "pointer",
                    "action": {"type": "click", "x": click_x, "y": click_y},
                },
            },
        )
        print(
            f"point_click_dispatch_completed={dispatch_completed(clicked)} "
            f"point_click_result_error={clicked.get('isError', False)}"
        )
        if clicked.get("isError") or not dispatch_completed(clicked):
            return 2

        _, after_click = call(
            client,
            "observe",
            {
                "target": target,
                "view": "both",
                "accessibility": {
                    "scope": "full",
                    "limits": {"text_limit": 1000, "max_nodes": 500, "max_depth": 32},
                },
            },
        )
        text_element = (
            element(
                after_click,
                lambda value: value.get("capabilities", {}).get("set_value") == "text",
            )
            or text_element
        )
        type_text = "isolated-session-type"
        typed, _ = call(
            client,
            "act",
            {
                "target": target,
                "source_observation": {"observation_id": after_click["observation_id"]},
                "operation": {
                    "type": "keyboard",
                    "focus": {
                        "type": "semantic",
                        "element_id": text_element["element_id"],
                    },
                    "events": [{"type": "type", "text": type_text}],
                },
            },
        )
        print(
            f"type_dispatch_completed={dispatch_completed(typed)} "
            f"type_result_error={typed.get('isError', False)}"
        )
        if typed.get("isError") or not dispatch_completed(typed):
            return 2

        _, after_type = call(
            client,
            "observe",
            {
                "target": target,
                "view": "accessibility",
                "accessibility": {
                    "scope": "full",
                    "limits": {"text_limit": 1000, "max_nodes": 500, "max_depth": 32},
                },
            },
        )
        typed_verified = any(
            type_text in (value.get("text") or "")
            or type_text in (value.get("value") or "")
            for value in (after_type or {}).get("elements", [])
        )
        print(f"type_verified={typed_verified}")
        if not typed_verified:
            return 2

        paste_element = (
            element(
                after_type,
                lambda value: value.get("capabilities", {}).get("set_value") == "text",
            )
            or text_element
        )
        paste_text = "isolated-session-paste"
        pasted, _ = call(
            client,
            "act",
            {
                "target": target,
                "source_observation": {"observation_id": after_type["observation_id"]},
                "operation": {
                    "type": "paste",
                    "focus": {
                        "type": "semantic",
                        "element_id": paste_element["element_id"],
                    },
                    "text": paste_text,
                },
            },
        )
        print(
            f"paste_dispatch_completed={dispatch_completed(pasted)} "
            f"paste_result_error={pasted.get('isError', False)}"
        )
        if pasted.get("isError") or not dispatch_completed(pasted):
            return 2

        verified_result, verified = call(
            client,
            "observe",
            {
                "target": target,
                "view": "both",
                "accessibility": {
                    "scope": "full",
                    "limits": {"text_limit": 1000, "max_nodes": 500, "max_depth": 32},
                },
            },
        )
        save_png(verified_result)
        print_observation_response(verified_result, verified, "observe_after_paste")
        values = [value.get("value") for value in (verified or {}).get("elements", [])]
        texts = [value.get("text") for value in (verified or {}).get("elements", [])]
        print_value_evidence(
            verified, "paste_readback_evidence", [type_text, paste_text]
        )
        paste_verified = any(paste_text in (value or "") for value in values + texts)
        input_verified = typed_verified and paste_verified
        print(f"paste_readback_verified={paste_verified}")
        print(f"input_verified={input_verified}")

        close_element = element(
            verified,
            lambda value: (
                value.get("name") == "Close"
                and value.get("capabilities", {}).get("invoke") is True
            ),
        )
        if close_element:
            closed_document, _ = call(
                client,
                "act",
                {
                    "target": target,
                    "source_observation": {
                        "observation_id": verified["observation_id"]
                    },
                    "operation": {
                        "type": "semantic",
                        "element_id": close_element["element_id"],
                        "action": {"type": "invoke"},
                    },
                },
            )
            print(
                f"document_close_dispatch_completed={dispatch_completed(closed_document)} "
                f"document_close_result_error={closed_document.get('isError', False)} "
                f"document_close_error={response_error_text(closed_document)}"
            )
        if portal_blocked or not input_verified:
            return 2
    finally:
        if target is not None and client.process.poll() is None:
            try:
                close_result, _ = call(
                    client, "activate_window", {"target": target, "action": "close"}
                )
                closed = not close_result.get("isError", False)
                if closed:
                    _, close_wait = call(
                        client,
                        "wait_for",
                        {
                            "target": target,
                            "condition": {
                                "type": "window_closed",
                                "window_instance_id": target["window_instance_id"],
                            },
                            "timeout_ms": 5000,
                        },
                    )
                    closed = (close_wait or {}).get("satisfied", False)
                print(f"window_closed={closed}")
            except Exception as error:
                print(f"window_close_api={error}")
        client.close()
        print(f"mcp_process_exit={client.process.returncode}")
        if client.native_runner:
            cleanup_deadline = time.monotonic() + 10
            while target_pid is not None and os.path.exists(f"/proc/{target_pid}"):
                if time.monotonic() >= cleanup_deadline:
                    break
                time.sleep(0.1)
            print(
                "native_target_gone="
                + str(target_pid is None or not os.path.exists(f"/proc/{target_pid}"))
            )
            print(
                "native_runner_descendants_after_stop="
                + str(
                    len(descendants(client.process.pid))
                    if client.process.poll() is None
                    else 0
                )
            )
        if target_pid is not None and not closed:
            if client.native_runner:
                print("native_runner_cleanup=delegated_to_private_broker")
            elif os.path.exists(f"/proc/{target_pid}"):
                terminate_private_process(target_pid)
            else:
                print("private_process_already_exited=true")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"smoke_error={error}", file=sys.stderr)
        raise
