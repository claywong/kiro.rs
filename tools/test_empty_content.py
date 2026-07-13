#!/usr/bin/env python3
"""测试空消息内容和 prefill 处理的改进"""

import json
import requests

BASE_URL = "http://localhost:8080"
API_KEY = "test-key"

def safe_print_response(response):
    """安全打印响应，处理非 JSON 情况"""
    try:
        data = response.json()
        print(f"响应: {json.dumps(data, indent=2, ensure_ascii=False)}")
        return data
    except (json.JSONDecodeError, ValueError):
        print(f"响应 (非 JSON): {response.text}")
        return None

def test_empty_content():
    """测试空消息内容应返回 400 错误"""
    print("测试 1: 空消息内容")
    response = requests.post(
        f"{BASE_URL}/v1/messages",
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
        },
        json={
            "model": "claude-sonnet-4",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": ""}
            ]
        }
    )
    print(f"状态码: {response.status_code}")
    data = safe_print_response(response)
    assert response.status_code == 400, "应返回 400 错误"
    if data:
        assert "消息内容为空" in data.get("error", {}).get("message", ""), "错误消息应包含'消息内容为空'"
    print("✓ 测试通过\n")

def test_empty_text_blocks():
    """测试仅包含空白文本块的消息"""
    print("测试 2: 仅包含空白文本块")
    response = requests.post(
        f"{BASE_URL}/v1/messages",
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
        },
        json={
            "model": "claude-sonnet-4",
            "max_tokens": 1024,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "   "},
                        {"type": "text", "text": "\n\t"}
                    ]
                }
            ]
        }
    )
    print(f"状态码: {response.status_code}")
    data = safe_print_response(response)
    assert response.status_code == 400, "应返回 400 错误"
    if data:
        assert "消息内容为空" in data.get("error", {}).get("message", ""), "错误消息应包含'消息内容为空'"
    print("✓ 测试通过\n")

def test_prefill_with_empty_user():
    """测试 prefill 场景下空 user 消息"""
    print("测试 3: Prefill 场景下空 user 消息")
    response = requests.post(
        f"{BASE_URL}/v1/messages",
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
        },
        json={
            "model": "claude-sonnet-4",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": ""},
                {"role": "assistant", "content": "Hi there"}
            ]
        }
    )
    print(f"状态码: {response.status_code}")
    data = safe_print_response(response)
    assert response.status_code == 400, "应返回 400 错误"
    if data:
        assert "消息内容为空" in data.get("error", {}).get("message", ""), "错误消息应包含'消息内容为空'"
    print("✓ 测试通过\n")

def test_valid_message():
    """测试正常消息应该成功"""
    print("测试 4: 正常消息（对照组）")
    response = requests.post(
        f"{BASE_URL}/v1/messages",
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
        },
        json={
            "model": "claude-sonnet-4",
            "max_tokens": 50,
            "messages": [
                {"role": "user", "content": "Say 'test' only"}
            ]
        }
    )
    print(f"状态码: {response.status_code}")
    if response.status_code == 200:
        print("✓ 测试通过：正常消息处理成功\n")
    else:
        safe_print_response(response)
        print()

if __name__ == "__main__":
    print("=" * 60)
    print("空消息内容验证测试")
    print("=" * 60 + "\n")

    try:
        test_empty_content()
        test_empty_text_blocks()
        test_prefill_with_empty_user()
        test_valid_message()
        print("=" * 60)
        print("所有测试通过！")
        print("=" * 60)
    except AssertionError as e:
        print(f"\n✗ 测试失败: {e}")
    except requests.exceptions.ConnectionError:
        print("\n✗ 无法连接到服务器，请确保服务正在运行")
    except Exception as e:
        print(f"\n✗ 发生错误: {e}")
