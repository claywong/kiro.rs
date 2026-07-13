#!/usr/bin/env python3
"""
400 Improperly formed request 本地测试验证套件

基于 docs/troubleshooting/400-improperly-formed-request.md 中的实测案例矩阵（TC-01 ~ TC-08）。

前置条件：
  - 服务启动：cargo run -- -c config.json --credentials credentials.json
  - 打开 DEBUG/INFO 日志（建议 --features sensitive-logs）
  - BASE_URL / API_KEY 按实际环境修改

每个案例记录：HTTP 状态码、是否出现 Improperly formed request、
kiro_request_body_bytes、estimated_input_tokens、压缩统计。
"""

import json
import sys
import time
import base64
import struct
import io
import os
import argparse
from dataclasses import dataclass, field
from typing import Optional

try:
    import requests
except ImportError:
    print("需要 requests 库: pip install requests")
    sys.exit(1)

# ── 配置 ──────────────────────────────────────────────────────────────
DEFAULT_BASE_URL = "http://localhost:8080"
DEFAULT_API_KEY = "test-key"
DEFAULT_MODEL = "claude-sonnet-4"

# 运行时可变配置（避免 global 声明冲突）
_config = {"model": DEFAULT_MODEL}

# ── 结果记录 ──────────────────────────────────────────────────────────

@dataclass
class TestResult:
    name: str
    status_code: int
    is_improperly_formed: bool = False
    body_snippet: str = ""
    passed: Optional[bool] = None
    note: str = ""


results: list[TestResult] = []


# ── 工具函数 ──────────────────────────────────────────────────────────

def headers(api_key: str) -> dict:
    return {
        "x-api-key": api_key,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
    }


def send_request(base_url: str, api_key: str, payload: dict,
                 stream: bool = False, timeout: int = 60) -> requests.Response:
    """发送请求到 /v1/messages"""
    payload.setdefault("model", _config["model"])
    payload.setdefault("max_tokens", 1024)
    if stream:
        payload["stream"] = True
    return requests.post(
        f"{base_url}/v1/messages",
        headers=headers(api_key),
        json=payload,
        timeout=timeout,
        stream=stream,
    )


def is_improperly_formed(resp: requests.Response) -> bool:
    """检查响应是否为上游兜底 400"""
    if resp.status_code != 400:
        return False
    try:
        data = resp.json()
        msg = data.get("message", "") or data.get("error", {}).get("message", "")
        return "improperly formed" in msg.lower()
    except Exception:
        return "improperly formed" in resp.text.lower()


def safe_body(resp: requests.Response, max_len: int = 300) -> str:
    try:
        text = resp.text[:max_len]
        return text
    except Exception:
        return "<无法读取>"


def is_local_request_too_large(resp: requests.Response) -> bool:
    """检查是否为本地请求体超限拒绝"""
    if resp.status_code != 400:
        return False
    try:
        data = resp.json()
        msg = data.get("message", "") or data.get("error", {}).get("message", "")
        return "request too large" in msg.lower()
    except Exception:
        return "request too large" in resp.text.lower()


def record(name: str, resp: requests.Response,
           passed: Optional[bool] = None, note: str = "") -> TestResult:
    r = TestResult(
        name=name,
        status_code=resp.status_code,
        is_improperly_formed=is_improperly_formed(resp),
        body_snippet=safe_body(resp),
        passed=passed,
        note=note,
    )
    results.append(r)
    return r


def estimate_payload_bytes(payload: dict, stream: bool = False) -> int:
    """按 send_request 的默认补全规则估算请求 JSON 字节数"""
    body = json.loads(json.dumps(payload, ensure_ascii=False))
    body.setdefault("model", _config["model"])
    body.setdefault("max_tokens", 1024)
    if stream:
        body["stream"] = True
    return len(json.dumps(body, ensure_ascii=False, separators=(",", ":")))


def target_mb_values(start_mb: float, end_mb: float, step_mb: float) -> list[float]:
    """生成包含端点的 MB 阶梯值"""
    values = []
    current = start_mb
    while current <= end_mb + 1e-9:
        values.append(round(current, 1))
        current += step_mb
    return values


def build_tier_payload(target_bytes: int, large_jpeg: bytes,
                       small_jpeg: bytes) -> tuple[dict, int]:
    """构造尽量贴近 target_bytes 的请求体（图片优先，文本补齐）"""
    content = [{"type": "text", "text": "请分析这些素材并给出结构化总结。"}]
    payload = {"messages": [{"role": "user", "content": content}]}

    large_block = image_content_block(large_jpeg, "image/jpeg")
    small_block = image_content_block(small_jpeg, "image/jpeg")

    def estimate() -> int:
        return estimate_payload_bytes(payload)

    # 先用大图快速接近目标
    while True:
        content.append(large_block)
        if estimate() > target_bytes:
            content.pop()
            break

    # 再用小图细化
    while True:
        content.append(small_block)
        if estimate() > target_bytes:
            content.pop()
            break

    # 最后文本补齐（ASCII，确保 1 字符≈1 字节）
    filler = {"type": "text", "text": "f:"}
    content.append(filler)

    lo, hi = 0, 1024
    filler["text"] = "f:" + ("x" * hi)
    while estimate() < target_bytes and hi < target_bytes:
        lo = hi
        hi *= 2
        filler["text"] = "f:" + ("x" * hi)

    best_len = lo
    best_est = estimate()
    left, right = lo, hi
    while left <= right:
        mid = (left + right) // 2
        filler["text"] = "f:" + ("x" * mid)
        est = estimate()
        if est <= target_bytes:
            best_len = mid
            best_est = est
            left = mid + 1
        else:
            right = mid - 1

    filler["text"] = "f:" + ("x" * best_len)
    final_est = estimate_payload_bytes(payload)
    # 防御性保障：若二分过程中估值漂移，用 best_est 回退
    if final_est > target_bytes and best_est <= target_bytes:
        filler["text"] = "f:" + ("x" * best_len)
        final_est = best_est

    return payload, final_est


def make_minimal_gif(width: int = 100, height: int = 100,
                     num_frames: int = 20, frame_size_kb: int = 10) -> bytes:
    """生成一个最小化的多帧 GIF（纯二进制构造，不依赖 PIL）。

    每帧用随机像素数据填充以抵抗压缩，确保 base64 后体积可控。
    frame_size_kb 控制每帧的近似大小。
    """
    try:
        from PIL import Image
        frames = []
        import random
        for i in range(num_frames):
            # 用渐变+噪声生成不可压缩的帧
            img = Image.new("RGB", (width, height))
            pixels = []
            for y in range(height):
                for x in range(width):
                    r = (x * 255 // width + i * 17) % 256
                    g = (y * 255 // height + i * 31) % 256
                    b = (x * y + i * 53) % 256
                    pixels.append((r, g, b))
            img.putdata(pixels)
            frames.append(img)

        buf = io.BytesIO()
        frames[0].save(
            buf, format="GIF", save_all=True,
            append_images=frames[1:],
            duration=100, loop=0,
        )
        return buf.getvalue()
    except ImportError:
        # 无 PIL 时生成最简 GIF（单帧，体积较小）
        print("  [警告] 未安装 Pillow，使用最简 GIF 替代（帧数/体积受限）")
        return _make_minimal_gif_no_pil(width, height)


def _make_minimal_gif_no_pil(w: int, h: int) -> bytes:
    """无依赖的最简单帧 GIF89a"""
    # GIF89a header + logical screen descriptor + GCE + image + trailer
    header = b"GIF89a"
    lsd = struct.pack("<HH", w, h) + b"\x80\x00\x00"  # 2-color palette
    palette = b"\x00\x00\x00\xff\xff\xff"  # black + white
    gce = b"\x21\xf9\x04\x00\x0a\x00\x00\x00"  # graphic control ext
    img_desc = b"\x2c" + struct.pack("<HH HH", 0, 0, w, h) + b"\x00"
    # LZW minimum code size + dummy data block + terminator
    lzw = b"\x02\x02\x4c\x01\x00"
    trailer = b"\x3b"
    return header + lsd + palette + gce + img_desc + lzw + trailer


def make_jpeg_bytes(width: int = 800, height: int = 600,
                    quality: int = 85) -> bytes:
    """生成一张 JPEG 图片"""
    try:
        from PIL import Image
        img = Image.new("RGB", (width, height))
        pixels = []
        for y in range(height):
            for x in range(width):
                pixels.append(((x * 7 + y * 3) % 256,
                               (x * 3 + y * 7) % 256,
                               (x + y) % 256))
        img.putdata(pixels)
        buf = io.BytesIO()
        img.save(buf, format="JPEG", quality=quality)
        return buf.getvalue()
    except ImportError:
        print("  [警告] 未安装 Pillow，跳过 JPEG 生成")
        return b""


def image_content_block(data: bytes, media_type: str = "image/gif") -> dict:
    """构造 Anthropic 格式的 image content block"""
    return {
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media_type,
            "data": base64.b64encode(data).decode(),
        },
    }


def long_text(chars: int = 100_000) -> str:
    """生成指定长度的长文本"""
    unit = "这是一段用于测试的长文本内容，模拟 quoted message 或 memory 片段。"
    repeat = (chars // len(unit)) + 1
    return (unit * repeat)[:chars]


# ── TC-01: 空消息内容（基线防线）────────────────────────────────────

def tc01_empty_content(base_url: str, api_key: str):
    """验证本地空 content 拦截仍有效"""
    print("\n[TC-01] 空消息内容（基线防线）")

    # 1a: 空字符串
    print("  1a: content = ''")
    resp = send_request(base_url, api_key, {
        "messages": [{"role": "user", "content": ""}],
    })
    print(f"  状态码: {resp.status_code}")
    r = record("TC-01a 空字符串", resp,
               passed=(resp.status_code == 400 and not is_improperly_formed(resp)),
               note="预期本地 400，非上游兜底")
    if r.is_improperly_formed:
        print("  ⚠ 空 content 泄漏到上游！")

    # 1b: 仅空白文本块
    print("  1b: content = [空白 text blocks]")
    resp = send_request(base_url, api_key, {
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "   "},
            {"type": "text", "text": "\n\t"},
        ]}],
    })
    print(f"  状态码: {resp.status_code}")
    record("TC-01b 空白文本块", resp,
           passed=(resp.status_code == 400 and not is_improperly_formed(resp)),
           note="预期本地 400")

    # 1c: null content
    print("  1c: content = null")
    resp = send_request(base_url, api_key, {
        "messages": [{"role": "user", "content": None}],
    })
    print(f"  状态码: {resp.status_code}")
    record("TC-01c null content", resp,
           passed=(resp.status_code == 400 and not is_improperly_formed(resp)),
           note="预期本地 400")


# ── TC-02: tool_use/tool_result 配对异常 ─────────────────────────────

def tc02_tool_pairing(base_url: str, api_key: str):
    """验证孤立 tool 事件不会漏到上游"""
    print("\n[TC-02] tool_use/tool_result 配对异常（结构完整性）")

    # 2a: 孤立 tool_result（无对应 tool_use）
    print("  2a: 孤立 tool_result")
    resp = send_request(base_url, api_key, {
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "thinking..."},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "nonexistent_id_123",
                 "content": "some result"},
                {"type": "text", "text": "继续"},
            ]},
        ],
        "tools": [{"name": "test_tool", "description": "test",
                    "input_schema": {"type": "object", "properties": {}}}],
    })
    print(f"  状态码: {resp.status_code}")
    r = record("TC-02a 孤立 tool_result", resp,
               passed=(not is_improperly_formed(resp)),
               note="禁止泄漏到上游兜底 400")
    if r.is_improperly_formed:
        print("  ⚠ 孤立 tool_result 泄漏到上游！")

    # 2b: 孤立 tool_use（assistant 有 tool_use 但 user 无 tool_result）
    print("  2b: 孤立 tool_use")
    resp = send_request(base_url, api_key, {
        "messages": [
            {"role": "user", "content": "请帮我读取文件"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "好的，我来读取"},
                {"type": "tool_use", "id": "orphan_tool_001",
                 "name": "read_file", "input": {"path": "/tmp/test.txt"}},
            ]},
            {"role": "user", "content": "算了不用了，直接回答"},
        ],
        "tools": [{"name": "read_file", "description": "Read a file",
                    "input_schema": {"type": "object",
                                     "properties": {"path": {"type": "string"}}}}],
    })
    print(f"  状态码: {resp.status_code}")
    r = record("TC-02b 孤立 tool_use", resp,
               passed=(not is_improperly_formed(resp)),
               note="禁止泄漏到上游兜底 400")
    if r.is_improperly_formed:
        print("  ⚠ 孤立 tool_use 泄漏到上游！")

    # 2c: 重复 tool_result（同一 tool_use_id 出现两次）
    print("  2c: 重复 tool_result")
    resp = send_request(base_url, api_key, {
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "let me check"},
                {"type": "tool_use", "id": "dup_tool_001",
                 "name": "test_tool", "input": {}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "dup_tool_001",
                 "content": "result A"},
            ]},
            {"role": "assistant", "content": "got it"},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "dup_tool_001",
                 "content": "result B (duplicate)"},
                {"type": "text", "text": "继续"},
            ]},
        ],
        "tools": [{"name": "test_tool", "description": "test",
                    "input_schema": {"type": "object", "properties": {}}}],
    })
    print(f"  状态码: {resp.status_code}")
    record("TC-02c 重复 tool_result", resp,
           passed=(not is_improperly_formed(resp)),
           note="重复 tool_result 应被过滤，不泄漏上游")


# ── TC-03: 单 GIF 大包（03-03 复现）──────────────────────────────────

def tc03_gif_large_payload(base_url: str, api_key: str):
    """复现 3.7MB+ 主簇失败"""
    print("\n[TC-03] 单 GIF 大包（03-03 复现）")
    print("  生成大 GIF（~20 帧，高分辨率）...")

    gif_data = make_minimal_gif(width=640, height=480, num_frames=20)
    b64_size = len(base64.b64encode(gif_data))
    print(f"  GIF 原始: {len(gif_data)} bytes, base64: {b64_size} bytes")

    resp = send_request(base_url, api_key, {
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "请描述这个动图的内容"},
            image_content_block(gif_data, "image/gif"),
        ]}],
    }, timeout=120)
    print(f"  状态码: {resp.status_code}")
    r = record("TC-03 单 GIF 大包", resp,
               passed=(resp.status_code == 200 and not is_improperly_formed(resp)),
               note=f"GIF base64={b64_size}B; 预期 200 且不泄漏上游 400")
    if r.is_improperly_formed:
        print("  → 复现成功：上游 400 Improperly formed request")
    elif is_local_request_too_large(resp):
        print("  → 本地拒绝：Request too large（未泄漏上游）")
    elif resp.status_code == 400:
        print(f"  → 本地 400: {safe_body(resp, 200)}")
    else:
        print(f"  → 未触发 400（状态码 {resp.status_code}）")


# ── TC-04: 多图中包（03-05 非极大包失败复现）─────────────────────────

def tc04_multi_image_medium(base_url: str, api_key: str):
    """验证非 3MB+ 也可能失败"""
    print("\n[TC-04] 多图中包（03-05 非极大包失败复现）")

    jpeg_data = make_jpeg_bytes(width=400, height=300, quality=70)
    if not jpeg_data:
        print("  跳过：无法生成 JPEG（需要 Pillow）")
        results.append(TestResult("TC-04 多图中包", 0, note="跳过: 无 Pillow"))
        return

    # 2~4 张中等图片 + 长文本 + 较长历史
    history_msgs = []
    for i in range(10):
        history_msgs.append({"role": "user", "content": f"历史消息 {i}: {long_text(2000)}"})
        history_msgs.append({"role": "assistant", "content": f"回复 {i}: 收到。"})

    current_content = [
        {"type": "text", "text": f"请分析这些图片。\n\n背景信息：{long_text(5000)}"},
    ]
    for _ in range(3):
        current_content.append(image_content_block(jpeg_data, "image/jpeg"))

    messages = history_msgs + [{"role": "user", "content": current_content}]

    payload_est = sum(len(json.dumps(m)) for m in messages)
    print(f"  预估 payload: ~{payload_est // 1024}KB ({len(messages)} 条消息, 3 张 JPEG)")

    resp = send_request(base_url, api_key, {"messages": messages}, timeout=120)
    print(f"  状态码: {resp.status_code}")
    r = record("TC-04 多图中包", resp,
               passed=(resp.status_code == 200 and not is_improperly_formed(resp)),
               note=f"payload_est={payload_est}B; 预期 200 且不泄漏上游 400")
    if r.is_improperly_formed:
        print("  → 复现成功：非极大包也触发上游 400")
    else:
        print(f"  → 状态码 {resp.status_code}")


# ── TC-05: 高文本低图片（排除法）─────────────────────────────────────

def tc05_high_text_no_image(base_url: str, api_key: str):
    """区分纯文本过长与多模态结构问题"""
    print("\n[TC-05] 高文本低图片（排除法）")

    history_msgs = []
    for i in range(20):
        history_msgs.append({
            "role": "user",
            "content": f"[memory block {i}] {long_text(8000)}",
        })
        history_msgs.append({
            "role": "assistant",
            "content": f"已记录。摘要：段落 {i} 包含关于项目架构的讨论。",
        })

    messages = history_msgs + [{
        "role": "user",
        "content": f"请总结以上所有内容。\n\n补充：{long_text(10000)}",
    }]

    payload_est = sum(len(json.dumps(m)) for m in messages)
    print(f"  预估 payload: ~{payload_est // 1024}KB"
          f" ({len(messages)} 条消息, 0 图片)")

    resp = send_request(base_url, api_key,
                        {"messages": messages}, timeout=120)
    print(f"  状态码: {resp.status_code}")
    r = record("TC-05 高文本无图片", resp,
               note=f"payload_est={payload_est}B; 纯文本排除法")
    if r.is_improperly_formed:
        print("  → 纯文本也触发上游 400，需检查结构字段")
    elif resp.status_code == 400:
        print(f"  → 本地 400: {safe_body(resp, 200)}")
    else:
        print(f"  → 成功（{resp.status_code}），图片链路是更强触发因子")


# ── TC-06: 流式模式回归 ──────────────────────────────────────────────

def tc06_stream_regression(base_url: str, api_key: str):
    """验证流式路径和非流式路径的差异"""
    print("\n[TC-06] 流式模式回归")

    jpeg_data = make_jpeg_bytes(width=400, height=300, quality=70)
    if not jpeg_data:
        print("  跳过：无法生成 JPEG（需要 Pillow）")
        results.append(TestResult("TC-06 流式回归", 0,
                                  note="跳过: 无 Pillow"))
        return

    history = []
    for i in range(5):
        history.append({"role": "user",
                        "content": f"消息 {i}: {long_text(2000)}"})
        history.append({"role": "assistant", "content": f"回复 {i}"})

    current = [
        {"type": "text", "text": "请分析这张图片"},
        image_content_block(jpeg_data, "image/jpeg"),
    ]
    messages = history + [{"role": "user", "content": current}]

    # 6a: 非流式
    print("  6a: stream=false")
    resp_sync = send_request(base_url, api_key,
                             {"messages": messages},
                             stream=False, timeout=120)
    print(f"  状态码: {resp_sync.status_code}")
    record("TC-06a 非流式", resp_sync,
           passed=(resp_sync.status_code == 200 and
                   not is_improperly_formed(resp_sync)),
           note="预期 200 且不泄漏上游 400")

    # 6b: 流式
    print("  6b: stream=true")
    resp_s = send_request(base_url, api_key,
                          {"messages": messages},
                          stream=True, timeout=120)
    status = resp_s.status_code
    body_start = ""
    if status == 200:
        try:
            for chunk in resp_s.iter_content(chunk_size=512):
                body_start = chunk.decode("utf-8", errors="replace")[:300]
                break
        except Exception:
            pass
    else:
        body_start = resp_s.text[:300]
    resp_s.close()
    print(f"  状态码: {status}")

    r = TestResult(
        name="TC-06b 流式",
        status_code=status,
        is_improperly_formed="improperly formed" in body_start.lower(),
        body_snippet=body_start,
        passed=(status == 200 and
                "improperly formed" not in body_start.lower() and
                status == resp_sync.status_code),
        note="预期与非流式同状态码，且不出现上游兜底 400",
    )
    results.append(r)

    if resp_sync.status_code != status:
        print(f"  ⚠ 流/非流状态码不一致: "
              f"{resp_sync.status_code} vs {status}")
    else:
        print(f"  两种模式状态码一致: {status}")


# ── TC-07: JSON Schema 畸形工具定义 ──────────────────────────────────

def tc07_malformed_tool_schema(base_url: str, api_key: str):
    """验证 MCP 工具常见的 schema 畸形被修复"""
    print("\n[TC-07] JSON Schema 畸形工具定义")

    # 7a: required: null
    print("  7a: required: null")
    resp = send_request(base_url, api_key, {
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{
            "name": "mcp_tool_a",
            "description": "A tool with null required",
            "input_schema": {
                "type": "object",
                "properties": {"x": {"type": "string"}},
                "required": None,
            },
        }],
    })
    print(f"  状态码: {resp.status_code}")
    record("TC-07a required:null", resp,
           passed=(not is_improperly_formed(resp)),
           note="required:null 应被修复为 []")

    # 7b: properties: null
    print("  7b: properties: null")
    resp = send_request(base_url, api_key, {
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{
            "name": "mcp_tool_b",
            "description": "A tool with null properties",
            "input_schema": {
                "type": "object",
                "properties": None,
            },
        }],
    })
    print(f"  状态码: {resp.status_code}")
    record("TC-07b properties:null", resp,
           passed=(not is_improperly_formed(resp)),
           note="properties:null 应被修复为 {}")

    # 7c: 嵌套 schema 畸形
    print("  7c: 嵌套 schema 畸形")
    resp = send_request(base_url, api_key, {
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{
            "name": "mcp_tool_c",
            "description": "Nested null fields",
            "input_schema": {
                "type": "object",
                "properties": {
                    "config": {
                        "type": "object",
                        "properties": None,
                        "required": None,
                    },
                },
            },
        }],
    })
    print(f"  状态码: {resp.status_code}")
    record("TC-07c 嵌套畸形", resp,
           passed=(not is_improperly_formed(resp)),
           note="嵌套 null 也应被递归修复")


# ── TC-08: 边界值与混合场景 ──────────────────────────────────────────

def tc08_boundary_mixed(base_url: str, api_key: str):
    """边界值和混合触发因子"""
    print("\n[TC-08] 边界值与混合场景")

    # 8a: 空 tools 数组
    print("  8a: tools=[]")
    resp = send_request(base_url, api_key, {
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [],
    })
    print(f"  状态码: {resp.status_code}")
    record("TC-08a 空 tools 数组", resp,
           passed=(not is_improperly_formed(resp)),
           note="空 tools 数组不应导致上游 400")

    # 8b: 超长 system prompt
    print("  8b: 超长 system prompt")
    resp = send_request(base_url, api_key, {
        "system": long_text(50000),
        "messages": [{"role": "user", "content": "hello"}],
    })
    print(f"  状态码: {resp.status_code}")
    record("TC-08b 超长 system", resp,
           note="观测是否触发压缩或上游 400")

    # 8c: assistant prefill（Claude 4.x 已弃用）
    print("  8c: assistant prefill（末尾 assistant 消息）")
    resp = send_request(base_url, api_key, {
        "messages": [
            {"role": "user", "content": "写一首诗"},
            {"role": "assistant", "content": "好的，"},
        ],
    })
    print(f"  状态码: {resp.status_code}")
    record("TC-08c assistant prefill", resp,
           note="Claude 4.x 弃用 prefill，末尾 assistant 应被丢弃")

    # 8d: 混合 — 畸形 tool + 图片 + 长历史
    print("  8d: 混合场景（畸形 tool + 图片 + 长历史）")
    gif_data = make_minimal_gif(width=200, height=200, num_frames=5)
    history = []
    for i in range(8):
        history.append({"role": "user",
                        "content": f"msg {i}: {long_text(3000)}"})
        history.append({"role": "assistant",
                        "content": f"reply {i}"})

    messages = history + [{"role": "user", "content": [
        {"type": "text", "text": "分析这个动图"},
        image_content_block(gif_data, "image/gif"),
    ]}]

    resp = send_request(base_url, api_key, {
        "messages": messages,
        "tools": [{
            "name": "bad_tool",
            "description": "tool with null schema fields",
            "input_schema": {
                "type": "object",
                "properties": None,
                "required": None,
            },
        }],
    }, timeout=120)
    print(f"  状态码: {resp.status_code}")
    record("TC-08d 混合场景", resp,
           passed=(resp.status_code == 200 and not is_improperly_formed(resp)),
           note="预期 200 且不泄漏上游 400")


# ── TC-09: 阶梯压测（0.5MB 步进到 4MB，每档 2 次）───────────────────

def tc09_size_ladder(base_url: str, api_key: str):
    """分档压测请求体大小，输出失败拐点"""
    print("\n[TC-09] 阶梯压测（0.5MB 步进到 4MB，每档 2 次）")

    large_jpeg = make_jpeg_bytes(width=1200, height=900, quality=90)
    small_jpeg = make_jpeg_bytes(width=800, height=600, quality=85)
    if not large_jpeg or not small_jpeg:
        print("  跳过：无法生成 JPEG（需要 Pillow）")
        results.append(TestResult("TC-09 阶梯压测", 0, note="跳过: 无 Pillow"))
        return

    tiers_mb = target_mb_values(0.5, 4.0, 0.5)
    repeats = 2
    tier_stats: dict[float, list[dict]] = {mb: [] for mb in tiers_mb}

    for mb in tiers_mb:
        target_bytes = int(mb * 1024 * 1024)
        for run_idx in range(1, repeats + 1):
            payload, est_bytes = build_tier_payload(
                target_bytes=target_bytes,
                large_jpeg=large_jpeg,
                small_jpeg=small_jpeg,
            )
            resp = send_request(base_url, api_key, payload, timeout=180)
            improper = is_improperly_formed(resp)
            too_large = is_local_request_too_large(resp)

            if resp.status_code == 200:
                outcome = "200"
            elif improper:
                outcome = "上游400"
            elif too_large:
                outcome = "本地超限400"
            else:
                outcome = f"{resp.status_code}"

            print(
                f"  {mb:.1f}MB 第{run_idx}次: "
                f"payload_est={est_bytes}B, 状态码={resp.status_code}, 结果={outcome}"
            )

            tier_stats[mb].append({
                "status": resp.status_code,
                "improper": improper,
                "too_large": too_large,
                "payload_est": est_bytes,
            })

            note = f"target={mb:.1f}MB, payload_est={est_bytes}B, outcome={outcome}"
            record(
                f"TC-09 {mb:.1f}MB x{run_idx}",
                resp,
                passed=(resp.status_code == 200 and not improper),
                note=note,
            )

    # 失败拐点统计
    first_any_fail = None
    first_upstream_400 = None
    first_stable_fail = None  # 同档 2 次都非 200

    for mb in tiers_mb:
        rows = tier_stats[mb]
        any_fail = any(r["status"] != 200 for r in rows)
        any_upstream_400 = any(r["improper"] for r in rows)
        stable_fail = all(r["status"] != 200 for r in rows)
        if first_any_fail is None and any_fail:
            first_any_fail = mb
        if first_upstream_400 is None and any_upstream_400:
            first_upstream_400 = mb
        if first_stable_fail is None and stable_fail:
            first_stable_fail = mb

    print("  阶梯结果汇总：")
    for mb in tiers_mb:
        rows = tier_stats[mb]
        ok = sum(1 for r in rows if r["status"] == 200)
        upstream = sum(1 for r in rows if r["improper"])
        local_reject = sum(1 for r in rows if r["too_large"])
        payload_min = min(r["payload_est"] for r in rows)
        payload_max = max(r["payload_est"] for r in rows)
        print(
            f"    - {mb:.1f}MB: 200={ok}/{repeats}, "
            f"上游400={upstream}, 本地超限400={local_reject}, "
            f"payload_est=[{payload_min}, {payload_max}]B"
        )

    if first_any_fail is None:
        print("  拐点: 未发现失败拐点（0.5MB~4.0MB 全部返回 200）")
    else:
        print(f"  拐点: 首次出现非 200 的档位 = {first_any_fail:.1f}MB")
        if first_upstream_400 is not None:
            print(f"  拐点: 首次出现上游 Improperly formed request = {first_upstream_400:.1f}MB")
        else:
            print("  拐点: 未出现上游 Improperly formed request")
        if first_stable_fail is not None:
            print(f"  拐点: 首次稳定失败（同档 2/2 失败） = {first_stable_fail:.1f}MB")
        else:
            print("  拐点: 未出现稳定失败档位（2/2 失败）")


# ── 结果汇总 ──────────────────────────────────────────────────────────

def print_summary():
    """打印测试结果汇总表"""
    print("\n" + "=" * 72)
    print("测试结果汇总")
    print("=" * 72)
    print(f"{'案例':<30} {'状态码':>6} {'上游400':>7} {'通过':>6}")
    print("-" * 72)

    passed_count = 0
    failed_count = 0
    skipped_count = 0

    for r in results:
        if r.status_code == 0:
            status = "跳过"
            skipped_count += 1
        elif r.passed is True:
            status = "✓"
            passed_count += 1
        elif r.passed is False:
            status = "✗"
            failed_count += 1
        else:
            status = "—"  # 观测型，无明确 pass/fail

        improperly = "是" if r.is_improperly_formed else "否"
        if r.status_code == 0:
            improperly = "—"

        print(f"{r.name:<30} {r.status_code:>6} {improperly:>7} "
              f"{status:>6}")
        if r.note:
            print(f"  └ {r.note}")

    print("-" * 72)
    total = len(results)
    print(f"总计: {total} | 通过: {passed_count} | "
          f"失败: {failed_count} | 跳过: {skipped_count} | "
          f"观测: {total - passed_count - failed_count - skipped_count}")

    # 关键发现
    leaked = [r for r in results if r.is_improperly_formed]
    if leaked:
        print(f"\n⚠ 发现 {len(leaked)} 个案例泄漏到上游 400:")
        for r in leaked:
            print(f"  - {r.name}")
    else:
        print("\n✓ 无案例泄漏到上游 Improperly formed request")

    print("=" * 72)
    return failed_count == 0 and not leaked


# ── 主入口 ────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(
        description="400 Improperly formed request 本地测试套件")
    parser.add_argument("--base-url", default=DEFAULT_BASE_URL,
                        help=f"服务地址 (默认: {DEFAULT_BASE_URL})")
    parser.add_argument("--api-key", default=DEFAULT_API_KEY,
                        help="API Key")
    parser.add_argument("--model", default=DEFAULT_MODEL,
                        help=f"模型 (默认: {DEFAULT_MODEL})")
    parser.add_argument("--tc", nargs="*", type=int,
                        help="指定运行的案例编号 (如 --tc 1 2 7)")
    args = parser.parse_args()

    _config["model"] = args.model

    all_cases = {
        1: ("TC-01 空消息内容", tc01_empty_content),
        2: ("TC-02 tool 配对异常", tc02_tool_pairing),
        3: ("TC-03 单 GIF 大包", tc03_gif_large_payload),
        4: ("TC-04 多图中包", tc04_multi_image_medium),
        5: ("TC-05 高文本无图片", tc05_high_text_no_image),
        6: ("TC-06 流式回归", tc06_stream_regression),
        7: ("TC-07 畸形 tool schema", tc07_malformed_tool_schema),
        8: ("TC-08 边界值混合", tc08_boundary_mixed),
        9: ("TC-09 阶梯压测", tc09_size_ladder),
    }

    selected = args.tc if args.tc else sorted(all_cases.keys())

    print(f"目标: {args.base_url}")
    print(f"模型: {args.model}")
    print(f"案例: {', '.join(str(i) for i in selected)}")

    # 连通性检查
    try:
        r = requests.get(f"{args.base_url}/v1/models",
                         headers=headers(args.api_key), timeout=5)
        print(f"连通性检查: {r.status_code}")
    except Exception as e:
        print(f"连通性检查失败: {e}")
        print("请确认服务已启动。")
        sys.exit(1)

    for tc_num in selected:
        if tc_num not in all_cases:
            print(f"\n[警告] TC-{tc_num:02d} 不存在，跳过")
            continue
        name, func = all_cases[tc_num]
        try:
            func(args.base_url, args.api_key)
        except Exception as e:
            print(f"\n[错误] {name} 异常: {e}")
            results.append(TestResult(name, 0, note=f"异常: {e}"))

    all_passed = print_summary()
    sys.exit(0 if all_passed else 1)


if __name__ == "__main__":
    main()
