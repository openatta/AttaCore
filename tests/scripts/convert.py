#!/usr/bin/env python3
"""
录像转换脚本 — 把一份 recording 渲染成人类可读的 .md

一份 recording 是一个目录：`calls.jsonl` 是调用时间线，`blobs/` 存内容块。
请求里的 system 块 / 工具表 / 消息都是 blob 引用，这个脚本把它们解引用回来。

用法:
  python3 tests/scripts/convert.py tests/fixtures/cassettes/000/api/<round>/000
"""

import json, sys
from pathlib import Path

SEP_IN = ">>>>>>>>>>>>>>>>"
SEP_OUT = "<<<<<<<<<<<<<<<<"


def load_blob(blobs_dir, blob_id):
    path = blobs_dir / blob_id
    if not path.exists():
        return None
    with open(path) as f:
        return json.load(f)


def expand(record):
    """把一条存储行还原成它承载的 chunk 列表。"""
    t = record["type"]
    if t == "chunk":
        return [record["chunk"]]
    if t in ("text_chunks", "thinking_chunks"):
        kind = "text_delta" if t == "text_chunks" else "thinking_delta"
        return [{"kind": kind, "text": s} for s in record["texts"]]
    if t == "tool_args_chunks":
        return [
            {"kind": "tool_args_delta", "id": record["id"], "partial_json": s}
            for s in record["args"]
        ]
    return []


def render_response(chunks, out):
    for c in chunks:
        kind = c.get("kind")
        if kind == "text_delta":
            out.append(c["text"])
        elif kind == "thinking_delta":
            out.append(f"[thinking] {c['text']}")
        elif kind == "tool_args_delta":
            out.append(f"[ToolArgs {c['id']}] {c['partial_json']}")
        elif kind == "tool_use":
            inp = json.dumps(c.get("input", {}), ensure_ascii=False)
            out.append(f"\n[ToolUse: {c['name']} id={c['id']}]\n{inp}\n")
        elif kind == "end_turn":
            out.append(f"\n[EndTurn: {c.get('stop_reason', '')}]")


def main():
    if len(sys.argv) < 2:
        print("用法: python3 convert.py <path/to/recording-dir>")
        sys.exit(1)

    rec_dir = Path(sys.argv[1])
    jsonl_path = rec_dir / "calls.jsonl"
    if not jsonl_path.exists():
        print(f"错误: 不是一份 recording（缺 calls.jsonl）: {rec_dir}")
        sys.exit(1)
    blobs_dir = rec_dir / "blobs"

    records = []
    with open(jsonl_path) as f:
        for line in f:
            line = line.strip()
            if line:
                records.append(json.loads(line))

    # call seq → 该次调用收到的 chunk
    responses = {}
    ends = {}
    calls = []
    for r in records:
        t = r.get("type")
        if t == "call":
            calls.append(r)
            responses.setdefault(r["seq"], [])
        elif t == "end":
            ends[r["call"]] = r
        elif t in ("chunk", "text_chunks", "thinking_chunks", "tool_args_chunks"):
            responses.setdefault(r["call"], []).extend(expand(r))

    lines = []
    for call in calls:
        seq = call["seq"]
        params = call["params"]
        tools = load_blob(blobs_dir, call["tools"]) or []
        tool_names = [t.get("name", "?") for t in tools]

        lines.append(SEP_IN)
        lines.append(f"# Call seq={seq} turn={call['turn']} step={call['step']}")
        lines.append(f"# model={params['model']}  tools={tool_names}")
        lines.append("")
        for i, blob_id in enumerate(call["system"]):
            block = load_blob(blobs_dir, blob_id)
            content = block.get("content", "") if block else f"<missing blob {blob_id}>"
            lines.append(f"--- system block {i} ---")
            lines.extend(content.split("\n"))
        for i, blob_id in enumerate(call["messages"]):
            msg = load_blob(blobs_dir, blob_id)
            lines.append(f"--- message {i} ---")
            lines.append(json.dumps(msg, ensure_ascii=False))
        lines.append("")

        end = ends.get(seq)
        lines.append(SEP_OUT)
        lines.append(f"# Call seq={seq} — Response")
        if end:
            usage = end.get("usage") or {}
            lines.append(
                f"# outcome={end['outcome'].get('status')}  "
                f"stop_reason={end.get('stop_reason')}  "
                f"in={usage.get('input_tokens')}  out={usage.get('output_tokens')}  "
                f"{end['duration_ms']}ms"
            )
        else:
            lines.append("# (未闭合 — 录制在这次调用中途结束)")
        lines.append("")
        render_response(responses.get(seq, []), lines)
        lines.append("")
        lines.append("")

    md_path = rec_dir / "calls.md"
    with open(md_path, "w") as f:
        f.write("\n".join(lines))

    print(f"  → {md_path} ({len(calls)} calls)")


if __name__ == "__main__":
    main()
