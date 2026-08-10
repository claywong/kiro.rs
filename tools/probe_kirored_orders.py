#!/usr/bin/env python3
"""探针：打印 kiro.red `/api/user/order/index` 的原始返回。

用途是核对历史订单 DTO 的字段名（数量 / 金额 / 状态 / 时间）与卖家实际返回一致。
签名与解密逻辑对齐 src/vendor/flavor_kirored.rs。凭据从 config/config.json 的
id=kirored 条目读取，不打印密码。
"""
import json
import sys

sys.path.insert(0, "tools")
from probe_kirored_user import PATH_LOGIN, signed_post  # noqa: E402

PATH_ORDER_INDEX = "/api/user/order/index"
PATH_ORDER_DETAIL = "/api/user/order/detail"
CONFIG = "config/config.json"


def load_creds():
    with open(CONFIG, encoding="utf-8") as f:
        cfg = json.load(f)
    for v in cfg.get("vendors", []):
        if v.get("id") == "kirored":
            return v.get("apiKey", "").strip(), v.get("vendorPassword", "")
    sys.exit("config.json 里没有 id=kirored 的条目")


def main() -> None:
    email, password = load_creds()
    if not email or not password:
        sys.exit("kirored 条目缺 apiKey(email) 或 vendorPassword")

    login = signed_post(PATH_LOGIN, {"email": email, "password": password})
    token = (login.get("data") or {}).get("token", "")
    if not token:
        sys.exit(f"登录未拿到 token: code={login.get('code')} msg={login.get('message')}")

    resp = signed_post(PATH_ORDER_INDEX, {"page": 1, "page_size": 5}, token)
    print("=== /api/user/order/index 顶层 ===")
    print("键:", list(resp.keys()))
    data = resp.get("data") or {}
    print("data 键:", list(data.keys()) if isinstance(data, dict) else type(data))
    print()
    print("=== data 原始（前 3 条）===")
    if isinstance(data, dict):
        shallow = {k: v for k, v in data.items() if k != "list"}
        print("分页字段:", json.dumps(shallow, ensure_ascii=False))
        for i, row in enumerate((data.get("list") or [])[:3]):
            print(f"--- list[{i}] ---")
            print(json.dumps(row, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
