#!/bin/bash
# AttaCore CLI (Daemon) 模式测试脚本
# 用法: ./tests/run_cli.sh [case_name] [round]
# 示例: ./tests/run_cli.sh 000.c_project
#      ./tests/run_cli.sh 000.c_project 002   # 显式指定轮次

set -euo pipefail
cd "$(dirname "$0")/.."

CASE="${1:-000.c_project}"
CASE_NUM="${CASE%%.*}"  # "000.c_project" → "000"
CONFIG=".env"
CASE_FILE="tests/cases/${CASE}.test"
DAEMON_BIN="target/debug/attacored"
# 和 tests/runner/src/config.rs::resolve_record_round 同一套优先级：
# 命令行参数 > $ATTA_RECORD_ROUND > 今天的 UTC 日期
ROUND="${2:-${ATTA_RECORD_ROUND:-$(date -u +%Y-%m-%d)}}"

if [ ! -f "$CASE_FILE" ]; then
    echo "错误: 用例文件不存在: $CASE_FILE"
    echo "可用用例:"
    ls tests/cases/*.test | sed 's|tests/cases/||;s|\.test||'
    exit 1
fi

echo "=== CLI 模式: $CASE (round: $ROUND) ==="
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
ATTA_RECORD="$CASE_NUM" cargo run -p test-runner -- \
  --mode cli --case "$CASE_FILE" --config "$CONFIG" \
  --daemon-binary "$DAEMON_BIN" --round "$ROUND"

RECORDING_DIR="tests/fixtures/cassettes/${CASE_NUM}/cli/${ROUND}/${CASE_NUM##*/}"

# 生成可读日志
python3 tests/scripts/convert.py "$RECORDING_DIR"

# 回放：和 api 模式同一套。这一步以前没有，于是 cli 模式只录不放——
# 录像录完就没有任何东西再读它，daemon 那条路的回归价值是零。
echo ""
echo ">>> 回放验证..."
rm -f /tmp/attacore-test.sock ~/.atta/code/daemon.lock 2>/dev/null
killall attacored 2>/dev/null || true
ATTA_REPLAY="$CASE_NUM" ATTA_REPLAY_STRICT=1 cargo run -p test-runner -- \
  --mode cli --case "$CASE_FILE" --config "$CONFIG" \
  --daemon-binary "$DAEMON_BIN" --round "$ROUND" --compare

# 清理
rm -f /tmp/attacore-test.sock ~/.atta/code/daemon.lock 2>/dev/null
killall attacored 2>/dev/null || true

echo ""
echo "=== 完成 ==="
echo "recording: $RECORDING_DIR"
echo "输出目录: tests/output/${CASE_NUM}/"
