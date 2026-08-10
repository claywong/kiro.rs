#!/usr/bin/env python3
"""给 config.json 里 id=kirored 的条目填登录凭据。

从环境变量读取，避免密码进入命令行历史：
  KIRORED_EMAIL / KIRORED_PASSWORD
"""
import json
import os
import sys
from collections import OrderedDict

PATH = "config/config.json"
VENDOR_ID = "kirored"

email = os.environ.get("KIRORED_EMAIL", "").strip()
password = os.environ.get("KIRORED_PASSWORD", "")

if not email or not password:
    sys.exit("需要设置 KIRORED_EMAIL 与 KIRORED_PASSWORD")

with open(PATH, encoding="utf-8") as f:
    cfg = json.load(f, object_pairs_hook=OrderedDict)

target = None
for v in cfg.get("vendors", []):
    if v.get("id") == VENDOR_ID:
        target = v
        break

if target is None:
    sys.exit(f"未找到 id={VENDOR_ID} 的条目")

# 这家用 email（存在 apiKey）+ 密码登录
target["apiKey"] = email
target["vendorPassword"] = password

with open(PATH, "w", encoding="utf-8") as f:
    json.dump(cfg, f, ensure_ascii=False, indent=2)
    f.write("\n")

print(f"已写入 id={VENDOR_ID}: apiKey={email}, vendorPassword=<{len(password)} 字符>")
