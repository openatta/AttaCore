#!/usr/bin/env python3
"""
VCR fixture 转换脚本

从 .jsonl 原始录制生成两个可读文件：
  {scenario}-pretty.jsonl  — 格式化 JSON（多轮完整）
  {scenario}.md            — >>>>/<<<< 分隔的输入输出日志

用法:
  python3 tests/fixtures/vcr/convert.py vcr_agent
"""

import json, sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
SEP_IN  = ">>>>>>>>>>>>>>>>"   # 16 >
SEP_OUT = "<<<<<<<<<<<<<<<<"   # 16 <


def load_entries(scenario: str) -> list[dict]:
    path = SCRIPT_DIR / f"{scenario}.jsonl"
    entries = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                entries.append(json.loads(line))
    return entries


# ═══════════════════════════════════════════════════════════════════
# -pretty.jsonl
# ═══════════════════════════════════════════════════════════════════

def write_pretty(scenario: str, entries: list[dict]):
    path = SCRIPT_DIR / f"{scenario}-pretty.jsonl"
    with open(path, "w") as f:
        for i, entry in enumerate(entries):
            if len(entries) > 1:
                f.write(f"# === Turn {i} ===\n")
            json.dump(entry, f, ensure_ascii=False, indent=2)
            f.write("\n")
    print(f"  → {path.name} ({len(entries)} turns)")


# ═══════════════════════════════════════════════════════════════════
# .md — simplified input/output log
# ═══════════════════════════════════════════════════════════════════

def write_markdown(scenario: str, entries: list[dict]):
    path = SCRIPT_DIR / f"{scenario}.md"
    lines = []

    for turn_no, entry in enumerate(entries):
        req = entry["request"]
        chunks = entry["chunks"]

        # ── INPUT ──
        lines.append(SEP_IN)
        lines.append(f"# Turn {turn_no} — Request")
        lines.append(f"# model={req['model']}  tools={req['tools']}")
        lines.append("")
        lines.append("## System Prompt")
        lines.append("")
        for line in req["system_text"].split("\n"):
            lines.append(line)
        lines.append("")

        # ── OUTPUT ──
        lines.append(SEP_OUT)
        lines.append(f"# Turn {turn_no} — Response")
        lines.append(f"# stop_reason={entry['response']['stop_reason']}  "
                     f"in={entry['response']['input_tokens']}  out={entry['response']['output_tokens']}")
        lines.append("")

        # Reconstruct text and tool calls from chunks
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

    with open(path, "w") as f:
        f.write("\n".join(lines))
    print(f"  → {path.name} ({len(entries)} turns)")


# ═══════════════════════════════════════════════════════════════════

def main():
    if len(sys.argv) < 2:
        print("用法: python3 convert.py <scenario>")
        sys.exit(1)

    scenario = sys.argv[1]
    print(f"读取 {scenario}.jsonl ...")
    entries = load_entries(scenario)
    print(f"  {len(entries)} 条录制记录")

    write_pretty(scenario, entries)
    write_markdown(scenario, entries)
    print("完成。")


if __name__ == "__main__":
    main()
