#!/usr/bin/env python3
"""把 kiro.ooo（第七家）追加进 config/config.json 的 vendors 数组。

幂等：已存在同 id 的条目就只补缺失字段，不覆盖已有值（尤其不覆盖 apiKey
与 webhookPathToken —— 后者若换了，卖家侧配好的推送地址会立刻 404）。

字段形状照抄现有条目（见 VendorConfig），避免漏字段走进 serde 默认值：
- autoPurchase 给 False：本家 webhook 载荷形态未实测，事件名可能落成 unknown，
  先手动提几次确认推送能认再开。
- defaultApiRegion 留空：区域由提取时按成交区逐单写进凭据，写死会盖掉它。
"""

import json
import os
import secrets
import shutil
import sys
from datetime import datetime
from pathlib import Path

CFG = Path(__file__).resolve().parent.parent / "config" / "config.json"
VENDOR_ID = "kiro-ooo"

ENTRY = {
    "id": VENDOR_ID,
    "name": "kiro.ooo",
    "flavor": "kiro-ooo",
    "baseUrl": "https://kiro.ooo",
    # 凭据走环境变量，不硬编码进仓库（同 tools/probe_kirored_user.py 的做法）：
    #   KIROOOO_API_KEY=usr-xxx python3 tools/add_kiroooo.py
    "apiKey": os.environ.get("KIROOOO_API_KEY", ""),
    # 入站推送凭证。卖家推送端不带签名，靠这个不可猜测的路径段做唯一凭据。
    "webhookPathToken": "",
    "defaultGroups": [],
    "defaultRpmLimit": 300,
    "defaultPriority": None,
    "defaultApiRegion": "",
    "defaultAuthRegion": "",
    "autoPurchase": False,
    "autoPurchaseMaxCount": 1,
    "autoPurchaseSchedule": [],
    "autoPurchasePerChannel": False,
    "vendorPassword": "",
}


def main() -> int:
    if not ENTRY["apiKey"]:
        print(
            "缺少 KIROOOO_API_KEY 环境变量。用法:\n"
            "  KIROOOO_API_KEY=usr-xxx python3 tools/add_kiroooo.py",
            file=sys.stderr,
        )
        return 1

    cfg = json.loads(CFG.read_text(encoding="utf-8"))
    vendors = cfg.setdefault("vendors", [])

    existing = next((v for v in vendors if v.get("id") == VENDOR_ID), None)
    entry = dict(ENTRY)
    entry["webhookPathToken"] = "whk_" + secrets.token_hex(24)

    if existing is not None:
        added = [k for k, v in entry.items() if k not in existing]
        for k in added:
            existing[k] = entry[k]
        if not existing.get("webhookPathToken"):
            existing["webhookPathToken"] = entry["webhookPathToken"]
            added.append("webhookPathToken")
        if not added:
            print(f"{VENDOR_ID} 已存在且字段齐全，未改动")
            return 0
        print(f"{VENDOR_ID} 已存在，补齐字段: {', '.join(added)}")
    else:
        vendors.append(entry)
        print(f"已追加 {VENDOR_ID}，当前 {len(vendors)} 家")

    backup = CFG.with_suffix(f".json.bak.{datetime.now():%Y%m%d%H%M%S}")
    shutil.copy2(CFG, backup)
    # 保留缩进与非 ASCII 原样（配置里有中文分组名），并补尾随换行
    CFG.write_text(
        json.dumps(cfg, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"备份: {backup.name}")

    token = (existing or entry)["webhookPathToken"]
    print(f"入站 webhook 路径: /webhook/vendor/{token}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
