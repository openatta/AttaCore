# AttaCore 测试系统

## 概述

AttaCore 的集成测试系统支持两种执行模式，共用同一套 `.test` 用例脚本。
测试流程：读取 `.test` 用例 → 按轮执行 → LLM 比对输出 → 生成报告。

## 目录结构

```
tests/
├── README.md                  ← 本文件
├── ../.deepseek               ← 测试专用 API 配置（根目录，需填入真实 key）
│
├── run_api.sh                 ← API 模式一键测试脚本
├── run_cli.sh                 ← CLI 模式一键测试脚本
│
├── cases/                     ← 测试用例脚本（编号.test 文件，共用）
│   └── 000.c_project.test
│
├── runner/                    ← 测试运行器（独立 binary: attacore-test）
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs            ← CLI 入口 (--mode api|cli)
│       ├── script.rs          ← TestScript 解析器 (.test 文件)
│       ├── api_runner.rs      ← API 模式（直接构造 Agent）
│       ├── cli_runner.rs      ← CLI 模式（启动 daemon → JSON-RPC）
│       ├── comparator.rs      ← LLM 比对器
│       └── reporter.rs        ← 报告生成 (JSON + MD)
│
├── scripts/                   ← 工具脚本
│   └── convert.py             ← .jsonl → .md 转换
│
├── fixtures/vcr/              ← VCR 录制数据（旧结构，逐步迁移）
│   └── convert.py             ← (旧版，已迁移到 scripts/)
│
└── output/                    ← 测试输出（每个 case 一个子目录）
    └── {case}/
        ├── api/               ← API 模式输出
        │   ├── {case}.jsonl           ← VCR 原始录制
        │   ├── {case}.md              ← 输入输出日志 (convert.py 生成)
        │   └── {case}.telemetry.md    ← 遥测事件日志
        ├── cli/               ← CLI 模式输出
        │   ├── {case}.jsonl
        │   ├── {case}.md
        │   └── {case}.telemetry.md
        ├── {case}.report.json  ← 测试报告（机器读）
        └── {case}.report.md    ← 测试报告（人读）
```

## 测试用例格式 (`.test`)

```text
# 测试用例说明（第一个 >>>>>>>>>>>>>>>> 之前的内容）
# 前置条件、预期行为等

>>>>>>>>>>>>>>>>
[第 1 轮输入 — 发送给 Agent 的中文消息]
<<<<<<<<<<<<<<<<
[第 1 轮预期输出描述 — 给 LLM 比对的自然语言描述]

>>>>>>>>>>>>>>>>
[第 2 轮输入]
<<<<<<<<<<<<<<<<
[第 2 轮预期输出描述]
```

### 规则

1. **系统提示词不翻译** — CodingScene 自带的英文提示词保持原样
2. **用户提示词用中文** — `.test` 中的输入使用中文
3. **预期输出用自然语言描述** — 不是精确字符串匹配，LLM 会做语义比对
4. **每轮独立比对** — 不等全部跑完，每轮结束立即比对
5. **工具描述保持英文** — 工具 schema 中的 description 不翻译

## 运行方式

### 前置：配置 API Key

编辑根目录 `.deepseek` 文件，填入真实 key。

### 一键脚本（推荐）

```sh
# API 模式（开发时快速验证）
./tests/run_api.sh 000.c_project

# CLI 模式（端到端测试，自动启动 daemon）
./tests/run_cli.sh 000.c_project
```

### 手动运行

```sh
# API 录制
ATTA_VCR_RECORD=000.c_project cargo run -p test-runner -- \
  --mode api --case tests/cases/000.c_project.test

# API 回放
ATTA_VCR_REPLAY=000.c_project cargo run -p test-runner -- \
  --mode api --case tests/cases/000.c_project.test

# CLI 录制
source ../.deepseek
cargo build -p daemon
ATTA_VCR_RECORD=000.c_project cargo run -p test-runner -- \
  --mode cli --case tests/cases/000.c_project.test \
  --daemon-binary target/debug/attacored
```

### 生成可读日志

```sh
python3 tests/scripts/convert.py tests/output/000.c_project/api/000.c_project.jsonl
```

## 编写新测试用例

1. 按编号在 `tests/cases/` 下创建 `{xxx}.{name}.test` 文件（如 `001.my_feature.test`）
2. 按格式编写用例说明 + 多轮输入/预期输出
3. 录制: `./tests/run_api.sh 001.my_feature`
4. 生成日志: `python3 tests/scripts/convert.py tests/output/{name}/api/{name}.jsonl`
5. 提交 `.test` + `output/{name}/` 下的录制数据

## 当前用例

| 用例 | 轮次 | API | CLI | 说明 |
|------|------|-----|-----|------|
| `000.c_project.test` | 2 | ✅ | ⏳ | 创建 C 项目 + 构建运行 |

## 注意事项

- **.deepseek 已加入 .gitignore** — 提交的是模板，真实 key 不会被提交
- **测试后自动清理** — API 模式清理 `/tmp/atta_test_runner/`，CLI 模式清理 session + socket
- **遥测始终生成** — 无论录制还是回放，遥测日志都会写入
- **MOCK 回归测试** — 使用 `ATTA_VCR_REPLAY` 环境变量，VCR 回放录制数据，PanicModel 验证不穿透
