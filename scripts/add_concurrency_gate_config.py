#!/usr/bin/env python3
"""向 config/config.json 插入 concurrencyGate 配置块。

只补缺失字段，已存在则原样保留（幂等，可重复跑）。token 复用 healthGate 的
——同一个站点（4code.us），没必要配两份。
"""

import json
import shutil
import sys
from collections import OrderedDict
from pathlib import Path

CONFIG = Path("config/config.json")


def main() -> int:
    if not CONFIG.exists():
        print(f"找不到 {CONFIG}", file=sys.stderr)
        return 1

    raw = CONFIG.read_text(encoding="utf-8")
    config = json.loads(raw, object_pairs_hook=OrderedDict)

    if "concurrencyGate" in config:
        print("concurrencyGate 已存在，未改动")
        return 0

    health = config.get("healthGate", {})
    token = health.get("token", "")
    if not token:
        print("healthGate.token 为空，无法复用；请手动填 concurrencyGate.token", file=sys.stderr)

    block = OrderedDict([
        ("enabled", False),
        ("baseUrl", "https://4code.us"),
        ("token", token),
        ("authHeader", "X-API-Key"),
        # 158 = 4code_rs，其上游正是本机，容量口径才对得上。
        ("accountIds", [158]),
        ("divisor", 6),
        ("unlimitedRpm", 300),
        ("minConcurrency", 1),
        ("maxConcurrency", 200),
        ("checkIntervalSecs", 60),
        ("reaffirmIntervalSecs", 300),
        ("maxAttempts", 3),
    ])

    # 插到 trafficIngress 之后，三个联动配置在文件里挨着，便于对照。
    rebuilt = OrderedDict()
    inserted = False
    for key, value in config.items():
        rebuilt[key] = value
        if key == "trafficIngress":
            rebuilt["concurrencyGate"] = block
            inserted = True
    if not inserted:
        rebuilt["concurrencyGate"] = block

    backup = CONFIG.with_suffix(".json.bak-concurrency-gate")
    shutil.copy2(CONFIG, backup)

    CONFIG.write_text(
        json.dumps(rebuilt, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    print(f"已写入 concurrencyGate（enabled=false），备份: {backup}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
