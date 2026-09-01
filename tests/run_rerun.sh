#!/bin/bash
# 把录像的输入原样重发给真模型，判断录像是否仍然成立。
#
# 用法: ./tests/run_rerun.sh [用例...] [--round <轮次>]
#   ./tests/run_rerun.sh                      # 所有有录像的用例
#   ./tests/run_rerun.sh 000.c_project        # 单个
#   ./tests/run_rerun.sh --round 2026-08-19   # 指定轮次
#
# 工具调用逐参数精确比对（不交判官），文本交语义判官。
# 花钱：每条录到的调用一次真实请求，文本有差异时再加一次判官请求。
# 实现见 crates/telemetry/src/recorder/rerun.rs。

set -uo pipefail
cd "$(dirname "$0")/.."

ROUND="${ATTA_RECORD_ROUND:-$(date -u +%Y-%m-%d)}"
CASES=()
while [ $# -gt 0 ]; do
    case "$1" in
        --round) ROUND="$2"; shift 2 ;;
        *) CASES+=("$1"); shift ;;
    esac
done

# 没点名就跑所有当前轮次有录像的用例。用例路径由录像目录反推，
# 而不是列 tests/cases/ —— 没录过的用例 rerun 无从谈起。
if [ ${#CASES[@]} -eq 0 ]; then
    while IFS= read -r jsonl; do
        scenario="${jsonl#tests/fixtures/cassettes/}"
        scenario="${scenario%%/api/*}"
        # "000" → tests/cases/000.*.test；"skills/001_x" → tests/cases/skills/001_x.test
        match=$(ls "tests/cases/${scenario}".*.test "tests/cases/${scenario}.test" 2>/dev/null | head -1)
        [ -n "$match" ] && CASES+=("$match")
    done < <(find tests/fixtures/cassettes -path "*/api/${ROUND}/*" -name calls.jsonl | sort)
fi

if [ ${#CASES[@]} -eq 0 ]; then
    echo "轮次 ${ROUND} 下没有录像。先录: ./tests/run_api.sh <用例>"
    exit 1
fi

echo "=== Rerun ${#CASES[@]} 个用例 (round: $ROUND) ==="
echo ""

FAILED=()
for case_file in "${CASES[@]}"; do
    # 用例文件可能是路径也可能是用例名
    [ -f "$case_file" ] || case_file="tests/cases/${case_file}.test"
    name=$(basename "$case_file" .test)
    echo "──── $name"
    if cargo run -q -p test-runner -- \
        --rerun --case "$case_file" --config .env --round "$ROUND"; then
        :
    else
        FAILED+=("$name")
    fi
    echo ""
done

echo "════ 汇总"
if [ ${#FAILED[@]} -eq 0 ]; then
    echo "全部录像仍然成立。"
else
    echo "${#FAILED[@]} 个用例出现分歧: ${FAILED[*]}"
    echo "逐条明细见各自的 tests/output/<用例>/*/rerun.md"
    # 分歧是发现，不是崩溃——但退出码得说出来，否则包装脚本会把
    # 一份坏掉的录像报成成功。
    exit 1
fi
