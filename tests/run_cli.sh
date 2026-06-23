#!/bin/bash
# AttaCore CLI (Daemon) 模式测试脚本
# 用法: ./tests/run_cli.sh [case_name]
# 示例: ./tests/run_cli.sh 000.c_project

set -euo pipefail
cd "$(dirname "$0")/.."

CASE="${1:-000.c_project}"
CASE_NUM="${CASE%%.*}"  # "000.c_project" → "000"
CONFIG=".deepseek"
CASE_FILE="tests/cases/${CASE}.test"
DAEMON_BIN="target/debug/attacored"

if [ ! -f "$CASE_FILE" ]; then
    echo "错误: 用例文件不存在: $CASE_FILE"
    echo "可用用例:"
    ls tests/cases/*.test | sed 's|tests/cases/||;s|\.test||'
    exit 1
fi

echo "=== CLI 模式: $CASE ==="
echo ""

# 确保 daemon 已构建
if [ ! -f "$DAEMON_BIN" ]; then
    echo ">>> 构建 daemon..."
    cargo build -p daemon
fi

# 加载配置
source "$CONFIG"

# 清理残留
rm -f /tmp/attacore-test.sock ~/.atta/code/daemon.lock 2>/dev/null
killall attacored 2>/dev/null || true

echo ">>> 录制..."
ATTA_VCR_RECORD="$CASE_NUM" cargo run -p test-runner -- \
  --mode cli --case "$CASE_FILE" --config "$CONFIG" \
  --daemon-binary "$DAEMON_BIN"

# 生成可读日志
python3 tests/scripts/convert.py "tests/output/${CASE_NUM}/cli/${CASE_NUM}.jsonl"

# 清理
rm -f /tmp/attacore-test.sock ~/.atta/code/daemon.lock 2>/dev/null
killall attacored 2>/dev/null || true

echo ""
echo "=== 完成 ==="
echo "输出目录: tests/output/${CASE_NUM}/"
