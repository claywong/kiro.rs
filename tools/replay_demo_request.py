#!/usr/bin/env python3
"""
根据 docs/demo.txt（原始 HTTP 请求 dump）重放 Claude Messages 请求，用于测试上游行为。

支持：
  - 解析形如：
      POST /v1/messages?beta=true HTTP/1.1
      header: value
      ...

      {json}
  - 覆盖 base_url / api_key / model / stream
  - 额外 header 覆盖、删除
  - stream=true 时逐行打印上游返回（SSE / chunked）

示例：
  uv run python3 tools/replay_demo_request.py --dry-run
  ANTHROPIC_API_KEY="<redacted>" uv run python3 tools/replay_demo_request.py \\
    --base-url "https://kizo.synthai.online" \\
    --model "claude-sonnet-4-5-20250929"
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import dataclass
from typing import Any, Dict, Optional

try:
    import requests
except ImportError:
    print("需要 requests 库，请先执行: uv sync")
    sys.exit(1)


DEFAULT_REQUEST_FILE = "docs/demo.txt"
DEFAULT_BASE_URL = "http://localhost:8080"
DEFAULT_TIMEOUT_SECS = 600
DEFAULT_ANTHROPIC_VERSION = "2023-06-01"

# hop-by-hop / 不应从 dump 里复用的 header
DROP_HEADERS = {
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
    "te",
    "trailer",
    "keep-alive",
}

API_KEY_ENV_CANDIDATES = ("ANTHROPIC_API_KEY", "KIRO_API_KEY", "API_KEY")


@dataclass(frozen=True)
class DocHttpRequest:
    method: str
    path: str
    headers: Dict[str, str]
    body_text: str
    body_json: Any


def _normalize_newlines(s: str) -> str:
    # 兼容 CRLF / CR
    return s.replace("\r\n", "\n").replace("\r", "\n")


def _find_header_key(headers: Dict[str, str], target_lower: str) -> Optional[str]:
    for k in headers.keys():
        if k.lower() == target_lower:
            return k
    return None


def _set_header(headers: Dict[str, str], key: str, value: str) -> None:
    existing = _find_header_key(headers, key.lower())
    if existing is not None and existing != key:
        del headers[existing]
    headers[key] = value


def _delete_header(headers: Dict[str, str], key: str) -> None:
    existing = _find_header_key(headers, key.lower())
    if existing is not None:
        del headers[existing]


def _redact_secret(s: str) -> str:
    s = (s or "").strip()
    if not s:
        return "<empty>"
    if len(s) <= 10:
        return "*" * len(s)
    return f"{s[:6]}...{s[-4:]}"


def _load_api_key(cli_api_key: Optional[str]) -> str:
    if cli_api_key and cli_api_key.strip():
        return cli_api_key.strip()
    for env in API_KEY_ENV_CANDIDATES:
        v = os.environ.get(env, "").strip()
        if v:
            return v
    raise SystemExit(
        "缺少 api key：请通过 --api-key 或环境变量 ANTHROPIC_API_KEY/KIRO_API_KEY/API_KEY 提供"
    )


def parse_doc_http_request(path: str) -> DocHttpRequest:
    raw = open(path, "rb").read()
    text = _normalize_newlines(raw.decode("utf-8", errors="replace"))

    if "\n\n" not in text:
        raise ValueError("请求文档缺少空行分隔（头/体），无法解析")

    head, body = text.split("\n\n", 1)
    head_lines = [ln.strip() for ln in head.split("\n") if ln.strip()]
    if not head_lines:
        raise ValueError("请求头为空，无法解析")

    req_line = head_lines[0]
    parts = req_line.split()
    if len(parts) < 2:
        raise ValueError(f"无法解析请求行: {req_line!r}")

    method = parts[0].upper()
    path_part = parts[1]

    headers: Dict[str, str] = {}
    for ln in head_lines[1:]:
        if ":" not in ln:
            continue
        k, v = ln.split(":", 1)
        headers[k.strip()] = v.strip()

    body_text = body.strip()
    body_json: Any
    if body_text:
        try:
            body_json = json.loads(body_text)
        except json.JSONDecodeError as e:
            snippet = body_text[:200]
            raise ValueError(f"请求体不是合法 JSON: {e}; snippet={snippet!r}") from e
    else:
        body_json = {}

    return DocHttpRequest(
        method=method,
        path=path_part,
        headers=headers,
        body_text=body_text,
        body_json=body_json,
    )


def build_url(base_url: str, path: str) -> str:
    base = (base_url or "").strip().rstrip("/")
    if not base:
        raise ValueError("base_url 不能为空")
    if not path.startswith("/"):
        path = "/" + path
    return base + path


def _apply_header_kv_overrides(headers: Dict[str, str], header_kv: list[str]) -> None:
    for kv in header_kv:
        if ":" not in kv:
            raise ValueError(f"--header 需要 'Key: Value' 形式，实际: {kv!r}")
        k, v = kv.split(":", 1)
        _set_header(headers, k.strip(), v.strip())


def _sanitize_headers_for_log(headers: Dict[str, str]) -> Dict[str, str]:
    safe = dict(headers)
    k = _find_header_key(safe, "x-api-key")
    if k is not None:
        safe[k] = _redact_secret(safe[k])
    k = _find_header_key(safe, "authorization")
    if k is not None:
        # 仅脱敏 token 部分
        val = safe[k]
        if " " in val:
            scheme, token = val.split(" ", 1)
            safe[k] = f"{scheme} {_redact_secret(token)}"
        else:
            safe[k] = _redact_secret(val)
    return safe


def main() -> int:
    p = argparse.ArgumentParser(
        description="Replay Claude Messages request from docs/demo.txt (raw HTTP dump)."
    )
    p.add_argument("--request-file", default=DEFAULT_REQUEST_FILE, help="请求 dump 文件路径")
    p.add_argument("--base-url", default=DEFAULT_BASE_URL, help="上游 base url（不含 path）")
    p.add_argument("--api-key", default=None, help="API Key（建议用环境变量提供，避免落入 shell history）")
    p.add_argument("--model", default=None, help="覆盖请求体 model")
    p.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT_SECS, help="请求超时秒数")

    stream_group = p.add_mutually_exclusive_group()
    stream_group.add_argument("--stream", action="store_true", help="强制 stream=true")
    stream_group.add_argument("--no-stream", action="store_true", help="强制 stream=false")

    p.add_argument(
        "--auth-header",
        choices=("auto", "x-api-key", "authorization"),
        default="auto",
        help="鉴权 header（默认 auto：优先复用 dump 中已有的 x-api-key/authorization）",
    )
    p.add_argument(
        "--header",
        action="append",
        default=[],
        help="额外覆盖 header（可重复），格式：'Key: Value'",
    )
    p.add_argument(
        "--remove-header",
        action="append",
        default=[],
        help="删除 header（可重复，大小写不敏感）",
    )
    p.add_argument("--dry-run", action="store_true", help="只打印最终请求摘要，不实际发送")

    args = p.parse_args()

    doc_req = parse_doc_http_request(args.request_file)
    url = build_url(args.base_url, doc_req.path)

    headers = dict(doc_req.headers)
    for k in list(headers.keys()):
        if k.lower() in DROP_HEADERS:
            del headers[k]

    api_key = _load_api_key(args.api_key)

    # 选择鉴权方式：尽量“按 dump 的风格”复用
    auth_mode = args.auth_header
    if auth_mode == "auto":
        if _find_header_key(headers, "x-api-key") is not None:
            auth_mode = "x-api-key"
        elif _find_header_key(headers, "authorization") is not None:
            auth_mode = "authorization"
        else:
            auth_mode = "x-api-key"

    if auth_mode == "x-api-key":
        _set_header(headers, "x-api-key", api_key)
        _delete_header(headers, "Authorization")
    else:
        _set_header(headers, "Authorization", f"Bearer {api_key}")
        _delete_header(headers, "x-api-key")

    # anthropic-version 是 Messages 协议的必需 header，dump 里没有时补一个默认值
    if _find_header_key(headers, "anthropic-version") is None:
        _set_header(headers, "anthropic-version", DEFAULT_ANTHROPIC_VERSION)

    # 用户额外覆盖/删除 header（覆盖优先级更高）
    _apply_header_kv_overrides(headers, args.header)
    for k in args.remove_header:
        _delete_header(headers, k)

    if not isinstance(doc_req.body_json, dict):
        raise SystemExit(f"请求体 JSON 顶层必须是 object，实际是 {type(doc_req.body_json).__name__}")

    payload: Dict[str, Any] = dict(doc_req.body_json)
    if args.model:
        payload["model"] = args.model
    if args.stream:
        payload["stream"] = True
    if args.no_stream:
        payload["stream"] = False

    stream = bool(payload.get("stream"))
    est_bytes = len(json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))

    print(f"request_file: {args.request_file}")
    print(f"request: {doc_req.method} {url}")
    print(f"timeout_secs: {args.timeout}")
    print(f"stream: {stream}")
    print(f"auth_mode: {auth_mode}")
    print(f"payload_bytes_est: {est_bytes}")
    print(f"headers: {json.dumps(_sanitize_headers_for_log(headers), indent=2, ensure_ascii=False)}")
    if args.dry_run:
        return 0

    resp = requests.request(
        method=doc_req.method,
        url=url,
        headers=headers,
        json=payload,
        timeout=args.timeout,
        stream=stream,
    )

    print(f"status_code: {resp.status_code}")

    if stream:
        # Anthropic/Claude 流式一般是 SSE 风格（逐行 data: {...}），这里按行透传即可
        for raw_line in resp.iter_lines(decode_unicode=False):
            if not raw_line:
                continue
            line = raw_line.decode("utf-8", errors="replace")
            if not line:
                continue
            print(line)
        return 0 if resp.status_code == 200 else 1

    try:
        data = resp.json()
        print(json.dumps(data, indent=2, ensure_ascii=False))
    except Exception:
        print(resp.text)

    return 0 if resp.status_code == 200 else 1


if __name__ == "__main__":
    raise SystemExit(main())
