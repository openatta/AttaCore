#!/usr/bin/env python3
"""
VCR fixture 转换脚本 — 从 .jsonl 生成人类可读的 .md

用法:
  python3 tests/scripts/convert.py tests/output/c_project/api/vcr_agent.jsonl
"""

import json, sys, os
from pathlib import Path

SEP_IN  = ">>>>>>>>>>>>>>>>"
SEP_OUT = "<<<<<<<<<<<<<<<<"


def main():
    if len(sys.argv) < 2:
        print("用法: python3 convert.py <path/to/scenario.jsonl>")
        print("示例: python3 convert.py tests/output/c_project/api/vcr_agent.jsonl")
        sys.exit(1)

    jsonl_path = Path(sys.argv[1])
    if not jsonl_path.exists():
        print(f"错误: 文件不存在: {jsonl_path}")
        sys.exit(1)

    entries = []
    with open(jsonl_path) as f:
        for line in f:
            line = line.strip()
            if line:
                entries.append(json.loads(line))

    stem = jsonl_path.stem  # e.g. "vcr_agent"
    md_path = jsonl_path.parent / f"{stem}.md"

    lines = []
    for turn_no, entry in enumerate(entries):
        req = entry["request"]
        chunks = entry["chunks"]

        lines.append(SEP_IN)
        lines.append(f"# Turn {turn_no}")
        lines.append(f"# model={req['model']}  tools={req['tools']}")
        lines.append("")
        for line in req["system_text"].split("\n"):
            lines.append(line)
        lines.append("")

        lines.append(SEP_OUT)
        lines.append(f"# Turn {turn_no} — Response")
        lines.append(f"# stop_reason={entry['response']['stop_reason']}  "
                     f"in={entry['response']['input_tokens']}  out={entry['response']['output_tokens']}")
        lines.append("")

        for c in chunks:
            t = c["type"]
            if t == "text_delta":
                lines.append(c["text"])
            elif t == "tool_use":
                inp = json.dumps(c.get("input", {}), ensure_ascii=False)
                lines.append(f"\n[ToolUse: {c['name']} id={c['id']}]\n{inp}\n")
            elif t == "end_turn":
                lines.append(f"\n[EndTurn: {c.get('stop_reason','')}]")
        lines.append("")
        lines.append("")

    with open(md_path, "w") as f:
        f.write("\n".join(lines))

    print(f"  → {md_path} ({len(entries)} turns)")


if __name__ == "__main__":
    main()
