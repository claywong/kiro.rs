#!/usr/bin/env python3
"""探针：直连 kiro.red，打印 /api/user/user/info 的原始 JSON。

用途是核对 UserData 的字段名与卖家实际返回是否一致（余额没显示的排查）。
签名与解密逻辑对齐 src/vendor/flavor_kirored.rs。
"""
import base64
import hashlib
import json
import os
import sys
import time
import urllib.request

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

BASE = "https://kiro.red"
PATH_LOGIN = "/api/user/auth/login"
PATH_USER_INFO = "/api/user/user/info"
PATH_PRODUCTS = "/api/common/products"


def md5_hex(data: bytes) -> str:
    return hashlib.md5(data).hexdigest()


def sign_request(full_path: str, method: str, ts: int) -> str:
    payload = (
        '{"url":"%s","method":"%s","timestamp":%d,"localTimestamp":%d}'
        % (full_path, method.upper(), ts, ts)
    )
    b64 = base64.b64encode(payload.encode()).decode()
    return md5_hex(md5_hex(b64.encode()).encode())


def decrypt_response(cipher_b64: str, signature: str) -> str:
    iv_hex = md5_hex(signature.encode())
    iv = iv_hex[:16].encode()
    key = md5_hex(iv)[:16].encode()
    raw = base64.b64decode(cipher_b64.strip())
    dec = Cipher(algorithms.AES(key), modes.CBC(iv)).decryptor()
    plain = dec.update(raw) + dec.finalize()
    return plain[: -plain[-1]].decode()  # 去 PKCS7 padding


def signed_post(full_path, body, token=None):
    ts = int(time.time())
    sig = sign_request(full_path, "POST", ts)
    headers = {
        "X-Signature": sig,
        "X-Timestamp": str(ts),
        "X-localTimestamp": str(ts),
        "Content-Type": "application/json",
        "Accept": "application/json",
    }
    if token:
        headers["X-Token"] = token
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(
        BASE + full_path, data=json.dumps(body).encode(), headers=headers, method="POST"
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        raw = resp.read().decode()
        encrypted = resp.headers.get("X-Signature-Status", "").strip() == "1"
    if encrypted:
        return json.loads(decrypt_response(raw.strip().strip('"'), sig))
    return json.loads(raw)


def main() -> None:
    email = os.environ.get("KIRORED_EMAIL", "").strip()
    password = os.environ.get("KIRORED_PASSWORD", "")
    if not email or not password:
        sys.exit("需要 KIRORED_EMAIL / KIRORED_PASSWORD")

    login = signed_post(PATH_LOGIN, {"email": email, "password": password})
    print("=== 登录响应结构 ===")
    print("顶层键:", list(login.keys()))
    data = login.get("data") or {}
    print("data 键:", list(data.keys()) if isinstance(data, dict) else type(data))
    token = data.get("token", "")
    if not token:
        sys.exit(f"登录未拿到 token: {json.dumps(login, ensure_ascii=False)[:300]}")

    info = signed_post(PATH_USER_INFO, {}, token)
    print()
    print("=== /api/user/user/info 原始返回 ===")
    # 打码敏感值，只关心键名与余额类字段
    print(json.dumps(info, ensure_ascii=False, indent=2)[:2000])

    products = signed_post(PATH_PRODUCTS, {}, token)
    print()
    print("=== /api/common/products 原始返回 ===")
    for p in products.get("data", {}).get("list", []):
        b = p.get("latest_batch") or {}
        print(
            f"id={p.get('id'):>3} sku={p.get('sku_id'):>3} "
            f"health={b.get('health'):>5} "
            f"import_time={b.get('import_time')} "
            f"max_alive={b.get('max_alive_seconds')}s({b.get('max_alive_text')}) "
            f"dead_time={b.get('dead_time')} "
            f"purchasable={p.get('purchasable')} | {p.get('name')}"
        )


if __name__ == "__main__":
    main()
