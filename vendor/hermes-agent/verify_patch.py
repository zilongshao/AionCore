"""Build-time contract checks for the pinned Aion-managed Hermes patch."""

from __future__ import annotations

import argparse
import importlib
import json
import os
import subprocess
import sys
import types
from pathlib import Path


FORBIDDEN_LITE_TOOLS = {
    "web_search",
    "web_extract",
    "browser_navigate",
    "browser_snapshot",
    "browser_click",
    "browser_type",
    "browser_scroll",
    "browser_back",
    "browser_press",
    "browser_get_images",
    "browser_vision",
    "browser_console",
    "browser_cdp",
    "browser_dialog",
}

REQUIRED_LITE_TOOLS = {
    "terminal",
    "process",
    "read_file",
    "write_file",
    "patch",
    "search_files",
    "todo",
    "memory",
    "session_search",
    "delegate_task",
}


def verify(source_root: Path) -> None:
    sys.path.insert(0, str(source_root))

    toolsets = importlib.import_module("toolsets")
    lite = set(toolsets.TOOLSETS["hermes-acp-lite"]["tools"])
    assert REQUIRED_LITE_TOOLS <= lite, sorted(REQUIRED_LITE_TOOLS - lite)
    assert not (FORBIDDEN_LITE_TOOLS & lite), sorted(FORBIDDEN_LITE_TOOLS & lite)

    session = importlib.import_module("acp_adapter.session")
    coding_context = importlib.import_module("agent.coding_context")
    model_metadata = importlib.import_module("agent.model_metadata")
    assert session._expand_acp_enabled_toolsets(["hermes-acp-lite"], ["docs"]) == [
        "hermes-acp-lite",
        "mcp-docs",
    ]

    base_tools = [
        {
            "type": "function",
            "function": {
                "name": name,
                "description": f"base {name}",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "file path"}
                    },
                },
            },
        }
        for name in ("read_file", "search_files", "write_file", "patch")
    ]
    first_agent = types.SimpleNamespace(
        tools=base_tools.copy(), ephemeral_system_prompt="existing instruction"
    )
    second_agent = types.SimpleNamespace(tools=base_tools.copy(), ephemeral_system_prompt=None)
    first_root = "C:\\ACP 工作区\\项目 一"
    second_root = "D:\\other workspace"
    encoded_first_root = json.dumps(first_root, ensure_ascii=False)
    encoded_second_root = json.dumps(second_root, ensure_ascii=False)
    session._bind_acp_workspace(first_agent, first_root)
    session._bind_acp_workspace(second_agent, second_root)

    assert "# ACP workspace" in first_agent.ephemeral_system_prompt
    assert encoded_first_root in first_agent.ephemeral_system_prompt
    assert "Do not ask for the workspace root" in first_agent.ephemeral_system_prompt
    for tool in first_agent.tools:
        function = tool["function"]
        assert encoded_first_root in function["description"]
        assert "Relative paths are supported" in function["description"]
        assert encoded_first_root in function["parameters"]["properties"]["path"]["description"]
    for tool in second_agent.tools:
        assert encoded_second_root in tool["function"]["description"]
        assert encoded_first_root not in tool["function"]["description"]
    for tool in base_tools:
        assert encoded_first_root not in tool["function"]["description"]
        assert encoded_second_root not in tool["function"]["description"]

    updated_root = "E:\\更新 工作区"
    encoded_updated_root = json.dumps(updated_root, ensure_ascii=False)
    session._bind_acp_workspace(first_agent, updated_root)
    assert encoded_updated_root in first_agent.ephemeral_system_prompt
    assert encoded_first_root not in first_agent.ephemeral_system_prompt
    for tool in first_agent.tools:
        assert encoded_updated_root in tool["function"]["description"]
        assert encoded_first_root not in tool["function"]["description"]

    calls: list[dict[str, object]] = []
    runtime_provider = types.ModuleType("hermes_cli.runtime_provider")

    def fake_resolve_runtime_provider(**kwargs):
        calls.append(kwargs)
        return {"provider": "custom"}

    runtime_provider.resolve_runtime_provider = fake_resolve_runtime_provider
    hermes_cli = sys.modules.setdefault("hermes_cli", types.ModuleType("hermes_cli"))
    hermes_cli.runtime_provider = runtime_provider
    sys.modules["hermes_cli.runtime_provider"] = runtime_provider

    previous = {
        name: os.environ.get(name)
        for name in ("OPENAI_BASE_URL", "OPENAI_API_KEY", "HERMES_PROVIDER_TYPE")
    }
    try:
        os.environ["OPENAI_BASE_URL"] = "http://127.0.0.1:43123/v1"
        os.environ["OPENAI_API_KEY"] = "aion-build-test-key"
        os.environ["HERMES_PROVIDER_TYPE"] = "openai"
        result = session._managed_acp_runtime(
            requested_provider="ignored",
            config_provider="ignored",
            selected_model="aion-build-test-model",
        )
    finally:
        for name, value in previous.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value

    assert result["provider"] == "custom"
    assert calls == [
        {
            "requested": "openai",
            "explicit_api_key": "aion-build-test-key",
            "explicit_base_url": "http://127.0.0.1:43123/v1",
            "target_model": "aion-build-test-model",
        }
    ]

    entry_source = (source_root / "acp_adapter" / "entry.py").read_text(encoding="utf-8")
    assert 'os.environ.get("HERMES_ACP_SKIP_CONFIGURED_MCP") != "1"' in entry_source

    events_source = (source_root / "acp_adapter" / "events.py").read_text(encoding="utf-8")
    assert "future.add_done_callback(_log_delivery_failure)" in events_source
    assert "future.result(timeout=5)" not in events_source

    agent_init_source = (source_root / "agent" / "agent_init.py").read_text(encoding="utf-8")
    assert 'os.environ.get("HERMES_CONTEXT_LENGTH", "").strip()' in agent_init_source

    server_source = (source_root / "acp_adapter" / "server.py").read_text(encoding="utf-8")
    assert "stage=first_visible_token" in server_source
    assert "user_text[:100]" not in server_source

    conversation_source = (source_root / "agent" / "conversation_loop.py").read_text(encoding="utf-8")
    assert "stage=system_prompt_generation" in conversation_source
    assert "stage=first_model_request_dispatch" in conversation_source

    assert model_metadata._MODEL_PROBE_TIMEOUT == (1.5, 3.0)
    assert model_metadata._OLLAMA_SHOW_TIMEOUT == 2.0
    assert model_metadata._is_explicit_ollama_provider("ollama")
    assert model_metadata._is_explicit_ollama_provider("ollama-cloud")
    assert not model_metadata._is_explicit_ollama_provider("custom")
    model_metadata_source = (source_root / "agent" / "model_metadata.py").read_text(encoding="utf-8")
    assert '_log_aion_probe_timing("ollama_show_probe", probe_started, "cache_hit")' in model_metadata_source
    assert '_log_aion_probe_timing("ollama_show_probe", now, "cache_hit")' not in model_metadata_source

    original_requests_get = model_metadata.requests.get
    models_probe_calls: list[object] = []

    def fake_failed_models_get(url, **kwargs):
        models_probe_calls.append(kwargs.get("timeout"))
        raise TimeoutError("build-time probe failure")

    try:
        model_metadata._endpoint_model_metadata_cache.clear()
        model_metadata._endpoint_model_metadata_cache_time.clear()
        model_metadata.requests.get = fake_failed_models_get
        assert model_metadata.fetch_endpoint_model_metadata("https://company.invalid/v1") == {}
        assert model_metadata.fetch_endpoint_model_metadata("https://company.invalid/v1") == {}
        assert models_probe_calls == [
            model_metadata._MODEL_PROBE_TIMEOUT,
            model_metadata._MODEL_PROBE_TIMEOUT,
        ]
    finally:
        model_metadata.requests.get = original_requests_get
        model_metadata._endpoint_model_metadata_cache.clear()
        model_metadata._endpoint_model_metadata_cache_time.clear()

    original_ollama_probe = model_metadata._query_ollama_api_show_uncached
    ollama_probe_calls = 0

    def fake_failed_ollama_probe(model, base_url, api_key=""):
        nonlocal ollama_probe_calls
        ollama_probe_calls += 1
        return None

    try:
        model_metadata._LOCAL_CTX_PROBE_CACHE.clear()
        model_metadata._query_ollama_api_show_uncached = fake_failed_ollama_probe
        assert model_metadata._query_ollama_api_show("model", "http://127.0.0.1:11434") is None
        assert model_metadata._query_ollama_api_show("model", "http://127.0.0.1:11434") is None
        assert ollama_probe_calls == 1
    finally:
        model_metadata._query_ollama_api_show_uncached = original_ollama_probe
        model_metadata._LOCAL_CTX_PROBE_CACHE.clear()

    original_endpoint_context = model_metadata._resolve_endpoint_context_length
    original_known_provider = model_metadata._is_known_provider_base_url
    original_custom_endpoint = model_metadata._is_custom_endpoint
    original_ollama_query = model_metadata._query_ollama_api_show
    gated_ollama_calls = 0

    def fake_ollama_query(model, base_url, api_key=""):
        nonlocal gated_ollama_calls
        gated_ollama_calls += 1
        return None

    try:
        model_metadata._resolve_endpoint_context_length = lambda *args, **kwargs: None
        model_metadata._is_known_provider_base_url = lambda *args, **kwargs: False
        model_metadata._is_custom_endpoint = lambda *args, **kwargs: True
        model_metadata._query_ollama_api_show = fake_ollama_query
        model_metadata.get_model_context_length(
            "private-model",
            base_url="https://company.invalid/v1",
            provider="custom",
        )
        assert gated_ollama_calls == 0
        model_metadata.get_model_context_length(
            "private-model",
            base_url="https://company.invalid/v1",
            provider="ollama",
        )
        assert gated_ollama_calls == 1
    finally:
        model_metadata._resolve_endpoint_context_length = original_endpoint_context
        model_metadata._is_known_provider_base_url = original_known_provider
        model_metadata._is_custom_endpoint = original_custom_endpoint
        model_metadata._query_ollama_api_show = original_ollama_query

    original_is_windows = coding_context.IS_WINDOWS
    original_windows_hide_flags = coding_context.windows_hide_flags
    original_run = coding_context.subprocess.run
    calls: list[dict[str, object]] = []

    class Completed:
        returncode = 0

    def fake_windows_run(command, **kwargs):
        calls.append({"command": command, **kwargs})
        stdout_file = kwargs["stdout"]
        stdout_file.write("clean\n")
        stdout_file.flush()
        return Completed()

    try:
        coding_context.IS_WINDOWS = True
        coding_context.windows_hide_flags = lambda: 0x08000000
        coding_context.subprocess.run = fake_windows_run
        coding_context._GIT_TIMEOUT_ROOTS.clear()
        assert coding_context._git(Path("C:/repo"), "status", "--short") == "clean"
        assert calls[0]["stderr"] is subprocess.DEVNULL
        assert "capture_output" not in calls[0]
        assert calls[0]["creationflags"] == 0x08000000

        timeout_calls = 0

        def fake_timeout_run(command, **kwargs):
            nonlocal timeout_calls
            timeout_calls += 1
            raise subprocess.TimeoutExpired(command, coding_context._GIT_TIMEOUT)

        coding_context.subprocess.run = fake_timeout_run
        coding_context._GIT_TIMEOUT_ROOTS.clear()
        assert coding_context._git(Path("C:/slow-repo"), "status") == ""
        assert coding_context._git(Path("C:/slow-repo"), "log") == ""
        assert timeout_calls == 1
    finally:
        coding_context.IS_WINDOWS = original_is_windows
        coding_context.windows_hide_flags = original_windows_hide_flags
        coding_context.subprocess.run = original_run
        coding_context._GIT_TIMEOUT_ROOTS.clear()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("source_root", type=Path)
    args = parser.parse_args()
    verify(args.source_root.resolve())


if __name__ == "__main__":
    main()
