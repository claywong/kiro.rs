#!/usr/bin/env python3
"""向 config.json 的 vendors 数组追加 kiro.red（flavor=kirored）条目。

幂等：已存在同 id 则不重复追加。凭据留空，由人工填写。
"""
import json
import sys
from collections import OrderedDict

PATH = "config/config.json"
VENDOR_ID = "kirored"

entry = OrderedDict([
    ("id", VENDOR_ID),
    ("name", "kiro.red"),
    ("flavor", "kirored"),
    ("baseUrl", "https://kiro.red"),
    # 这家走 email + 密码登录，email 存在 apiKey 里
    ("apiKey", ""),
    ("vendorPassword", ""),
    # 无 webhook，发货靠下单后主动查订单详情
    ("webhookPathToken", ""),
    ("defaultGroups", []),
    ("defaultRpmLimit", 300),
    ("defaultApiRegion", ""),
    ("defaultAuthRegion", ""),
    ("autoPurchase", False),
    ("autoPurchaseMaxCount", 1),
    ("autoPurchaseSchedule", []),
    ("autoPurchasePerChannel", False),
])

with open(PATH, encoding="utf-8") as f:
    cfg = json.load(f, object_pairs_hook=OrderedDict)

vendors = cfg.setdefault("vendors", [])
if any(v.get("id") == VENDOR_ID for v in vendors):
    print(f"已存在 id={VENDOR_ID}，跳过")
    sys.exit(0)

vendors.append(entry)

with open(PATH, "w", encoding="utf-8") as f:
    json.dump(cfg, f, ensure_ascii=False, indent=2)
    f.write("\n")

print(f"已追加 id={VENDOR_ID}，现有 {len(vendors)} 家")
