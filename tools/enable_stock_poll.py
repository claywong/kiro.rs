#!/usr/bin/env python3
"""给指定卖家开启库存轮询（config/config.json）。

默认只开轮询、**不动 autoPurchase** —— 那是「先观察、不扣费」的模式：
轮询发现的新车照样合成事件落库、面板上看得到，但不会自动下单。
确认轮询节奏对了再单独开 autoPurchase。

用法:
    python3 tools/enable_stock_poll.py kirored            # 60 秒，遵循总闸
    python3 tools/enable_stock_poll.py kirored --secs 120
    python3 tools/enable_stock_poll.py kirored --no-gate   # 不遵循全局总闸
"""

import argparse
import json
import shutil
import sys
from datetime import datetime
from pathlib import Path

CFG = Path(__file__).resolve().parent.parent / "config" / "config.json"
# 与 src/model/config.rs 的 MIN_STOCK_POLL_INTERVAL_SECS 保持一致
MIN_SECS = 60


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("vendor_id", help="要开启轮询的卖家 id，如 kirored")
    ap.add_argument("--secs", type=int, default=MIN_SECS, help=f"轮询间隔秒数（下限 {MIN_SECS}）")
    ap.add_argument(
        "--no-gate",
        action="store_true",
        help="不遵循全局总闸：总闸关着也继续发现新车（仍不会自动下单）",
    )
    args = ap.parse_args()

    if args.secs < MIN_SECS:
        print(f"间隔 {args.secs} 小于下限，后端会抬到 {MIN_SECS}，这里直接按下限写入")
        args.secs = MIN_SECS

    cfg = json.loads(CFG.read_text(encoding="utf-8"))
    vendors = cfg.get("vendors") or []
    target = next((v for v in vendors if v.get("id") == args.vendor_id), None)
    if target is None:
        known = ", ".join(v.get("id", "?") for v in vendors)
        print(f"找不到卖家 {args.vendor_id}；已配置的有: {known}", file=sys.stderr)
        return 1

    target["stockPollIntervalSecs"] = args.secs
    target["stockPollRespectGlobalGate"] = not args.no_gate

    backup = CFG.with_suffix(f".json.bak.{datetime.now():%Y%m%d%H%M%S}")
    shutil.copy2(CFG, backup)
    CFG.write_text(json.dumps(cfg, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    print(f"{args.vendor_id}: 轮询 {args.secs}s，遵循总闸={not args.no_gate}")
    print(f"autoPurchase 保持 {target.get('autoPurchase')}（未改动）")
    print(f"备份: {backup.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
