"""Run a real ACP turn against an expanded Aion-managed Hermes runtime.

The test uses the ACP SDK shipped inside the runtime and a local
OpenAI-compatible streaming endpoint. It never sends prompts or credentials to
the public network. When --capture is provided, the emitted JSON is sanitized
and can be retained as protocol evidence.
"""

from __future__ import annotations

import sys

sys.dont_write_bytecode = True

import argparse
import asyncio
import contextlib
import json
import os
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


REPLY = "HERMES_E2E_OK"
PROMPT = "Reply with HERMES_E2E_OK and do not call tools."
WORKSPACE_FILE = "资料 文件.txt"
WORKSPACE_MARKER = "AION_WORKSPACE_RELATIVE_READ_OK"
WORKSPACE_PROMPT = f"读取 `{WORKSPACE_FILE}`，并逐字返回文件内容。"
MODEL = "aion-hermes-e2e-model"
FORBIDDEN_TOOL_PREFIXES = ("browser_", "web_")
REQUIRED_TOOLS = {"read_file", "terminal"}


class EndpointState:
    def __init__(self, secret: str) -> None:
        self.secret = secret
        self.requests: list[dict[str, Any]] = []
        self.workspace_root: Path | None = None


def _tool_names(body: dict[str, Any]) -> list[str]:
    names: list[str] = []
    for tool in body.get("tools") or []:
        if not isinstance(tool, dict):
            continue
        function = tool.get("function")
        if isinstance(function, dict) and isinstance(function.get("name"), str):
            names.append(function["name"])
    return sorted(names)


def _message_text(message: dict[str, Any]) -> str:
    content = message.get("content")
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    return "".join(
        str(block.get("text") or "")
        for block in content
        if isinstance(block, dict) and block.get("type") == "text"
    )


def _last_user_text(body: dict[str, Any]) -> str:
    for message in reversed(body.get("messages") or []):
        if isinstance(message, dict) and message.get("role") == "user":
            return _message_text(message)
    return ""


def _tool_description(body: dict[str, Any], name: str) -> str:
    for tool in body.get("tools") or []:
        function = tool.get("function") if isinstance(tool, dict) else None
        if isinstance(function, dict) and function.get("name") == name:
            return str(function.get("description") or "")
    return ""


def _handler_for(state: EndpointState) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
            length = int(self.headers.get("Content-Length", "0"))
            body = json.loads(self.rfile.read(length) or b"{}")
            authorization = self.headers.get("Authorization", "")
            messages = body.get("messages") or []
            last_message = messages[-1] if messages else {}
            is_workspace_turn = WORKSPACE_PROMPT in _last_user_text(body)
            has_tool_result = (
                is_workspace_turn
                and isinstance(last_message, dict)
                and last_message.get("role") == "tool"
            )
            encoded_root = (
                json.dumps(str(state.workspace_root), ensure_ascii=False)
                if state.workspace_root is not None
                else ""
            )
            system_text = (
                _message_text(messages[0])
                if messages and isinstance(messages[0], dict) and messages[0].get("role") == "system"
                else ""
            )
            state.requests.append(
                {
                    "path": self.path,
                    "model": body.get("model"),
                    "stream": body.get("stream"),
                    "messageCount": len(messages),
                    "toolNames": _tool_names(body),
                    "authorizationAccepted": authorization == f"Bearer {state.secret}",
                    "scenario": "workspace" if is_workspace_turn else "baseline",
                    "lastMessageRole": last_message.get("role") if isinstance(last_message, dict) else None,
                    "systemHasWorkspaceRoot": bool(encoded_root and encoded_root in system_text),
                    "readDescriptionHasWorkspaceRoot": bool(
                        encoded_root and encoded_root in _tool_description(body, "read_file")
                    ),
                    "searchDescriptionHasWorkspaceRoot": bool(
                        encoded_root and encoded_root in _tool_description(body, "search_files")
                    ),
                    "toolResultHasMarker": bool(
                        has_tool_result and WORKSPACE_MARKER in _message_text(last_message)
                    ),
                }
            )

            if self.path.rstrip("/") != "/v1/chat/completions":
                payload = json.dumps({"error": {"message": "unexpected path"}}).encode()
                self.send_response(404)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                return

            if body.get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Cache-Control", "no-cache")
                self.send_header("Connection", "close")
                self.end_headers()
                if is_workspace_turn and not has_tool_result:
                    delta = {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "call-aion-workspace-read",
                                "type": "function",
                                "function": {
                                    "name": "read_file",
                                    "arguments": json.dumps(
                                        {"path": WORKSPACE_FILE}, ensure_ascii=False
                                    ),
                                },
                            }
                        ],
                    }
                    finish_reason = "tool_calls"
                else:
                    delta = {
                        "role": "assistant",
                        "content": WORKSPACE_MARKER if is_workspace_turn else REPLY,
                    }
                    finish_reason = "stop"
                chunks = [
                    {
                        "id": "chatcmpl-aion-hermes-e2e",
                        "object": "chat.completion.chunk",
                        "created": 1,
                        "model": MODEL,
                        "choices": [
                            {"index": 0, "delta": delta, "finish_reason": None}
                        ],
                    },
                    {
                        "id": "chatcmpl-aion-hermes-e2e",
                        "object": "chat.completion.chunk",
                        "created": 1,
                        "model": MODEL,
                        "choices": [
                            {"index": 0, "delta": {}, "finish_reason": finish_reason}
                        ],
                        "usage": {
                            "prompt_tokens": 10,
                            "completion_tokens": 4,
                            "total_tokens": 14,
                        },
                    },
                ]
                for chunk in chunks:
                    self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
                    self.wfile.flush()
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                self.close_connection = True
                return

            payload = json.dumps(
                {
                    "id": "chatcmpl-aion-hermes-e2e",
                    "object": "chat.completion",
                    "created": 1,
                    "model": MODEL,
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": REPLY},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 4,
                        "total_tokens": 14,
                    },
                }
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    return Handler


class RecordingClient:
    def __init__(self) -> None:
        self.updates: list[dict[str, Any]] = []

    async def session_update(self, session_id: str, update: Any, **_kwargs: Any) -> None:
        self.updates.append(
            {
                "sessionId": session_id,
                "update": update.model_dump(mode="json", by_alias=True, exclude_none=True),
            }
        )

    async def request_permission(self, **_kwargs: Any) -> Any:
        raise AssertionError("Hermes unexpectedly requested permission in the no-tool smoke turn")

    async def write_text_file(self, **_kwargs: Any) -> Any:
        raise AssertionError("Hermes unexpectedly requested a file write in the no-tool smoke turn")

    async def read_text_file(self, **_kwargs: Any) -> Any:
        raise AssertionError("Hermes unexpectedly requested a file read in the no-tool smoke turn")


def _extract_agent_text(updates: list[dict[str, Any]]) -> str:
    parts: list[str] = []
    for notification in updates:
        update = notification.get("update") or {}
        if update.get("sessionUpdate") != "agent_message_chunk":
            continue
        content = update.get("content") or {}
        if content.get("type") == "text" and isinstance(content.get("text"), str):
            parts.append(content["text"])
    return "".join(parts)


def _first_turn_action(updates: list[dict[str, Any]]) -> tuple[str, str | None]:
    for notification in updates:
        update = notification.get("update") or {}
        update_type = update.get("sessionUpdate")
        if update_type == "tool_call":
            return "tool", update.get("kind")
        if update_type == "agent_message_chunk":
            content = update.get("content") or {}
            if content.get("type") == "text" and content.get("text"):
                return "message", None
    return "none", None


async def _wait_for_first_turn_action(
    client: RecordingClient, start: int, timeout: float = 30
) -> tuple[str, str | None]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        action = _first_turn_action(client.updates[start:])
        if action[0] != "none":
            return action
        await asyncio.sleep(0.05)
    return "none", None


def _sanitize(value: Any, replacements: dict[str, str]) -> Any:
    if isinstance(value, dict):
        return {key: _sanitize(item, replacements) for key, item in value.items()}
    if isinstance(value, list):
        return [_sanitize(item, replacements) for item in value]
    if isinstance(value, str):
        sanitized = value
        for original, replacement in replacements.items():
            sanitized = sanitized.replace(original, replacement)
        return sanitized
    return value


async def _read_stderr(
    process: asyncio.subprocess.Process, lines: list[str] | None = None
) -> list[str]:
    if process.stderr is None:
        return []
    if lines is None:
        lines = []
    while line := await process.stderr.readline():
        lines.append(line.decode("utf-8", errors="replace").rstrip())
    return lines


async def run(runtime_root: Path, capture: Path | None) -> dict[str, Any]:
    python = runtime_root / "python" / "python.exe"
    bash = runtime_root / "tools" / "git" / "bin" / "bash.exe"
    git = runtime_root / "tools" / "git" / "cmd"
    rg = runtime_root / "tools" / "rg"
    for required in (python, bash, git / "git.exe", rg / "rg.exe"):
        if not required.is_file():
            raise AssertionError(f"managed runtime is missing {required}")

    from acp import PROTOCOL_VERSION
    from acp.connection import StreamEvent
    from acp.schema import Implementation, TextContentBlock
    from acp.stdio import spawn_agent_process

    secret = "aion-hermes-e2e-secret"
    endpoint_state = EndpointState(secret)
    server = ThreadingHTTPServer(("127.0.0.1", 0), _handler_for(endpoint_state))
    server_thread = threading.Thread(target=server.serve_forever, daemon=True, name="hermes-e2e-endpoint")
    server_thread.start()

    wire: list[dict[str, Any]] = []

    def observe(event: StreamEvent) -> None:
        wire.append({"direction": event.direction.value, "message": event.message})

    try:
        with tempfile.TemporaryDirectory(
            prefix="Aion Hermes ACP 中文 ", ignore_cleanup_errors=True
        ) as temporary:
            temp_root = Path(temporary)
            hermes_home = temp_root / "Hermes 会话"
            workspace = temp_root / "ACP 工作区"
            hermes_home.mkdir()
            workspace.mkdir()
            workspace_file = workspace / WORKSPACE_FILE
            workspace_file.write_text(WORKSPACE_MARKER, encoding="utf-8")
            if (workspace / ".git").exists():
                raise AssertionError("workspace fixture must remain a non-Git directory")
            endpoint_state.workspace_root = workspace
            (hermes_home / "config.yaml").write_text(
                "security:\n"
                "  allow_lazy_installs: false\n"
                "auxiliary:\n"
                "  title_generation:\n"
                "    enabled: false\n",
                encoding="utf-8",
            )

            env = {
                **os.environ,
                "OPENAI_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
                "OPENAI_API_KEY": secret,
                "HERMES_INFERENCE_MODEL": MODEL,
                "HERMES_HOME": str(hermes_home),
                "HERMES_GIT_BASH_PATH": str(bash),
                "HERMES_DISABLE_LAZY_INSTALLS": "1",
                "HERMES_ACP_SKIP_CONFIGURED_MCP": "1",
                "HERMES_ACP_TOOLSET": "hermes-acp-lite",
                "PYTHONDONTWRITEBYTECODE": "1",
                "PYTHONNOUSERSITE": "1",
                "PYTHONSAFEPATH": "1",
                "PYTHONIOENCODING": "utf-8",
                "PYTHONUTF8": "1",
                "PATH": os.pathsep.join([str(rg), str(git), os.environ.get("PATH", "")]),
            }
            probe_code = (
                "import sys; "
                "from tools.terminal_tool import register_task_env_overrides; "
                "from model_tools import handle_function_call; "
                "task_id='aion-workspace-probe'; "
                "register_task_env_overrides(task_id, {'cwd': sys.argv[1]}); "
                "result=handle_function_call('read_file', {'path': sys.argv[2]}, "
                "task_id, tool_call_id='probe-call', session_id='probe-session', "
                "enabled_tools=['read_file'], skip_pre_tool_call_hook=True, "
                "skip_tool_request_middleware=True, "
                "enabled_toolsets=['hermes-acp-lite']); "
                "assert sys.argv[3] in result, result; print(sys.argv[3])"
            )
            probe = await asyncio.create_subprocess_exec(
                str(python),
                "-P",
                "-c",
                probe_code,
                str(workspace),
                WORKSPACE_FILE,
                WORKSPACE_MARKER,
                cwd=workspace,
                env=env,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            probe_stdout, probe_stderr = await asyncio.wait_for(probe.communicate(), timeout=30)
            if probe.returncode != 0 or WORKSPACE_MARKER not in probe_stdout.decode(
                "utf-8", errors="replace"
            ):
                raise AssertionError(
                    "relative workspace file-tool probe failed: "
                    f"exit={probe.returncode}, stderr_lines={len(probe_stderr.splitlines())}"
                )
            client = RecordingClient()
            started = time.monotonic()
            stderr_lines: list[str] = []
            async with spawn_agent_process(
                client,
                str(python),
                "-P",
                "-m",
                "acp_adapter",
                env=env,
                cwd=workspace,
                observers=[observe],
            ) as (connection, process):
                stderr_task = asyncio.create_task(_read_stderr(process, stderr_lines))
                initialized = await asyncio.wait_for(
                    connection.initialize(
                        protocol_version=PROTOCOL_VERSION,
                        client_info=Implementation(name="aion-hermes-smoke", version="1.0.0"),
                    ),
                    timeout=30,
                )
                session = await asyncio.wait_for(connection.new_session(cwd=str(workspace), mcp_servers=[]), timeout=60)
                response = await asyncio.wait_for(
                    connection.prompt(
                        prompt=[TextContentBlock(type="text", text=PROMPT)],
                        session_id=session.session_id,
                    ),
                    timeout=120,
                )
                await asyncio.sleep(0.2)
                workspace_updates_start = len(client.updates)
                workspace_prompt_task = asyncio.create_task(
                    connection.prompt(
                        prompt=[TextContentBlock(type="text", text=WORKSPACE_PROMPT)],
                        session_id=session.session_id,
                    )
                )
                first_action, first_action_kind = await _wait_for_first_turn_action(
                    client, workspace_updates_start
                )
                if first_action != "tool" or first_action_kind not in {"read", "search"}:
                    raise AssertionError(
                        "workspace read did not begin with a read/search tool action: "
                        f"{first_action}/{first_action_kind}"
                    )
                await connection.cancel(session_id=session.session_id)
                workspace_prompt_task.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await workspace_prompt_task
            stderr_lines = await asyncio.wait_for(stderr_task, timeout=5)

            if secret in "\n".join(stderr_lines):
                raise AssertionError("Hermes stderr exposed the provider credential")
            if response.stop_reason != "end_turn":
                raise AssertionError(f"unexpected ACP stop reason: {response.stop_reason}")
            if REPLY not in _extract_agent_text(client.updates):
                raise AssertionError("ACP session updates did not contain the model response")
            model_requests = [
                request
                for request in endpoint_state.requests
                if request["path"].rstrip("/") == "/v1/chat/completions"
            ]
            if len(model_requests) != 2:
                raise AssertionError(
                    "expected baseline plus workspace-action model requests, got "
                    f"{json.dumps(endpoint_state.requests, ensure_ascii=False)}"
                )

            request = model_requests[0]
            if request["model"] != MODEL:
                raise AssertionError(f"model propagation failed: {request['model']}")
            if not request["authorizationAccepted"]:
                raise AssertionError("provider credential was not propagated through the child environment")
            tools = set(request["toolNames"])
            forbidden = sorted(name for name in tools if name.startswith(FORBIDDEN_TOOL_PREFIXES))
            if forbidden:
                raise AssertionError(f"forbidden browser/web tools reached the model request: {forbidden}")
            missing = sorted(REQUIRED_TOOLS - tools)
            if missing:
                raise AssertionError(f"required lite tools were absent from the model request: {missing}")

            workspace_requests = [
                request for request in model_requests if request["scenario"] == "workspace"
            ]
            if len(workspace_requests) != 1:
                raise AssertionError(f"expected one workspace request, got {workspace_requests}")
            first_workspace_request = workspace_requests[0]
            for field in (
                "systemHasWorkspaceRoot",
                "readDescriptionHasWorkspaceRoot",
                "searchDescriptionHasWorkspaceRoot",
            ):
                if not first_workspace_request[field]:
                    raise AssertionError(f"workspace contract missing from model request: {field}")
            if first_workspace_request["lastMessageRole"] != "user":
                raise AssertionError("workspace tool decision did not follow the user message")

            replacements = {
                str(runtime_root): "<runtime-root>",
                str(temp_root): "<temporary-root>",
                str(workspace): "<workspace>",
                str(hermes_home): "<hermes-home>",
                session.session_id: "<session-id>",
                secret: "<redacted>",
                f"127.0.0.1:{server.server_port}": "127.0.0.1:<port>",
            }
            result = {
                "fixture": "hermes-agent/0.19.0+aion.1/win32-x64",
                "runtimeRoot": str(runtime_root),
                "elapsedSeconds": round(time.monotonic() - started, 3),
                "initialize": initialized.model_dump(mode="json", by_alias=True, exclude_none=True),
                "sessionId": session.session_id,
                "promptStopReason": response.stop_reason,
                "workspacePromptStopReason": "cancelled_after_first_action",
                "workspaceFirstAction": {
                    "type": first_action,
                    "kind": first_action_kind,
                },
                "workspaceMarkerReturned": True,
                "agentText": _extract_agent_text(client.updates),
                "modelEndpointRequests": endpoint_state.requests,
                "stderr": {
                    "lineCount": len(stderr_lines),
                    "credentialPresent": False,
                },
                "acpWire": wire,
            }
            result = _sanitize(result, replacements)
            if capture is not None:
                capture.parent.mkdir(parents=True, exist_ok=True)
                capture.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
            return result
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=5)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("runtime_root", type=Path)
    parser.add_argument("--capture", type=Path)
    args = parser.parse_args()
    result = asyncio.run(run(args.runtime_root.resolve(), args.capture))
    print(
        json.dumps(
            {
                "ok": True,
                "fixture": result["fixture"],
                "promptStopReason": result["promptStopReason"],
                "workspacePromptStopReason": result["workspacePromptStopReason"],
                "workspaceFirstAction": result["workspaceFirstAction"],
                "workspaceMarkerReturned": result["workspaceMarkerReturned"],
                "agentText": result["agentText"],
                "requestCount": len(result["modelEndpointRequests"]),
                "modelRequestCount": sum(
                    request["path"].rstrip("/") == "/v1/chat/completions"
                    for request in result["modelEndpointRequests"]
                ),
                "toolCount": max(
                    len(request["toolNames"]) for request in result["modelEndpointRequests"]
                ),
            },
            ensure_ascii=False,
        )
    )


if __name__ == "__main__":
    main()
