#!/bin/bash
# AttaCore API 模式测试脚本
# 用法: ./tests/run_api.sh [case_name]
# 示例: ./tests/run_api.sh 000.c_project

set -euo pipefail
cd "$(dirname "$0")/.."

CASE="${1:-000.c_project}"
CASE_NUM="${CASE%%.*}"  # "000.c_project" → "000"
CONFIG=".deepseek"
CASE_FILE="tests/cases/${CASE}.test"

if [ ! -f "$CASE_FILE" ]; then
    echo "错误: 用例文件不存在: $CASE_FILE"
    echo "可用用例:"
    ls tests/cases/*.test | sed 's|tests/cases/||;s|\.test||'
    exit 1
fi

echo "=== API 模式: $CASE ==="
echo ""

# 录制（首次运行或更新 baseline）
echo ">>> 录制..."
ATTA_VCR_RECORD="$CASE_NUM" cargo run -p test-runner -- \
  --mode api --case "$CASE_FILE" --config "$CONFIG"

# 生成可读日志
python3 tests/scripts/convert.py "tests/output/${CASE_NUM}/api/${CASE_NUM}.jsonl"

# 回放（MOCK 回归验证）
echo ""
echo ">>> 回放验证..."
ATTA_VCR_REPLAY="$CASE_NUM" cargo run -p test-runner -- \
  --mode api --case "$CASE_FILE" --config "$CONFIG"

echo ""
echo "=== 完成 ==="
echo "输出目录: tests/output/${CASE_NUM}/"
