#!/bin/bash
# AttaCore API 模式测试脚本
# 用法: ./tests/run_api.sh [case_name] [round]
# 示例: ./tests/run_api.sh 000.c_project
#      ./tests/run_api.sh 000.c_project 002   # 显式指定轮次，不用默认的今天日期

set -euo pipefail
cd "$(dirname "$0")/.."

CASE="${1:-000.c_project}"
CASE_NUM="${CASE%%.*}"  # "000.c_project" → "000"
CONFIG=".env"
CASE_FILE="tests/cases/${CASE}.test"
# 和 tests/runner/src/config.rs::resolve_record_round 同一套优先级：
# 命令行参数 > $ATTA_RECORD_ROUND > 今天的 UTC 日期
ROUND="${2:-${ATTA_RECORD_ROUND:-$(date -u +%Y-%m-%d)}}"

if [ ! -f "$CASE_FILE" ]; then
    echo "错误: 用例文件不存在: $CASE_FILE"
    echo "可用用例:"
    ls tests/cases/*.test | sed 's|tests/cases/||;s|\.test||'
    exit 1
fi

echo "=== API 模式: $CASE (round: $ROUND) ==="
echo ""

# 录制（首次运行，或者要开新一轮更新 baseline 时传第二个参数指定新轮次）
echo ">>> 录制..."
ATTA_RECORD="$CASE_NUM" cargo run -p test-runner -- \
  --mode api --case "$CASE_FILE" --config "$CONFIG" --round "$ROUND"

RECORDING_DIR="tests/fixtures/cassettes/${CASE_NUM}/api/${ROUND}/${CASE_NUM}"

# 生成可读日志
python3 tests/scripts/convert.py "$RECORDING_DIR"

# 回放（MOCK 回归验证，同一轮）
echo ""
echo ">>> 回放验证..."
ATTA_REPLAY="$CASE_NUM" cargo run -p test-runner -- \
  --mode api --case "$CASE_FILE" --config "$CONFIG" --round "$ROUND"

echo ""
echo "=== 完成 ==="
echo "recording: $RECORDING_DIR"
echo "输出目录: tests/output/${CASE_NUM}/"
