# AttaCore v2.0.0 Changelog

> 从固定 Prompt 到任务驱动引擎的全面架构升级

---

## 概览

v2.0.0 对 CodingScene 进行了全面重构，新增 **12 种编程任务类型**的自动分类与路由、**结构化上下文工程**、**验证闭环**、**策略钩子**、**Skill 驱动的规则系统**、**多 Provider 模型路由**和**三级模型强度自动升级**引擎。

```
代码量: +8,861 行 (158 files changed)
新模块: 10 个 Rust 源文件
测试:   全覆盖, 0 failures
```

---

## 一、TaskProfile Routing — 任务级路由

### 12 种编程任务类型

| # | 类型 | 默认模型 | 工具策略 | 验证 | 典型触发词 |
|---|---|---|---|---|---|
| 1 | Explain | lite | read_only | none | `explain`, `what does this` |
| 2 | Search | lite | read_only | none | `find`, `search for`, `where is` |
| 3 | Generate | normal | read_write | none | `create`, `generate`, `write a` |
| 4 | **Modify** | normal | read_write | suggested | **默认回退** |
| 5 | Debug | strong | read_write_test | required | `debug`, `panic`, `not working` |
| 6 | Review | normal | read_only | review_only | `review`, `audit`, `is this safe` |
| 7 | Refactor | strong | read_write_test | required | `refactor`, `restructure` |
| 8 | Document | lite | read_only | none | `document`, `add comment` |
| 9 | Plan | normal | read_only | none | `plan`, `design`, `architecture` |
| 10 | Test | normal | read_write_test | required | `write tests`, `test coverage` |
| 11 | Perf | strong | read_write_test | required | `optimize`, `too slow`, `profile` |
| 12 | Deps | normal | read_write | suggested | `update deps`, `cargo update` |

### 分类器

- **RuleBasedTaskRouter**: 双层启发式（强信号词 + 模式匹配），<1ms，中英双语
- **TaskClassifier trait**: 预留 LLM 分类器扩展点
- 默认回退为 Modify

### 不同任务使用不同 System Prompt

每种任务有独立的 `PromptProfile`，包含：
- **system_rules**: 任务特定的行为指令（如 Debug 要求"先定位根因、最小修复、必须验证"）
- **output_format**: 任务特定的输出格式要求

---

## 二、Provider / Model 两级配置

### Level 1: Provider（服务商入口）

```toml
[providers.anthropic]
base_url = "https://api.anthropic.com"
auth_token = "sk-xxx"
models = ["claude-opus-4-8", "claude-sonnet-4-6", "claude-haiku-4-5"]
```

### Level 2: ModelProfile（任务模型配置）

```toml
[model_profiles.strong]
provider = "anthropic"
model = "claude-opus-4-8"
max_tokens = 8192

[model_profiles.debug]
model = "$strong"              # $ 引用解析
thinking_mode = "extended"
```

- **$strong / $normal / $lite** 字段级引用解析
- 内置三个默认 tier，用户可覆盖或新增功能级 profile
- `ModelProfileRegistry::resolve()` 自动展开所有 `$` 引用

---

## 三、Context Engineering — 上下文工程

### ContextPack

结构化的上下文收集，在 model 调用前构建：

```markdown
# Context Pack
## Task
- Kind: Debug
## User Request
修复 cargo test 失败
## Git Status
On branch main
## Error Summary
- Type: TestFailure
- Command: cargo test test_parse_config
- Message: assertion failed
## Suggested Commands
- cargo test test_parse_config
```

### ContextPackBuilder

- 从 `ScenePromptContext` 中提取 git status、分支等信息
- 自动解析用户消息中的错误信息（`ErrorSummary`）
- 提取失败命令和文件位置
- 预留在 prompt 构建前执行只读工具收集上下文的扩展点

---

## 四、Verification Loop — 验证闭环

### 5 级验证

| Level | 说明 |
|---|---|
| None | 不验证（Explain/Search/Document/Plan） |
| DiffSelfCheck | Agent 自查 diff |
| StaticCheck | 运行 linter/formatter |
| TargetedTest | 运行相关测试 |
| FullTest | 全量测试 |
| CiEquivalent | CI 等价检查 |

### 验证状态机

```
Plan → Edit → Review → Verify
  → Pass → Summarize → Complete
  → Fail → Diagnose → Repair → Verify again
  → 超限 → Blocked
```

### Agent 集成

- `record_verification()`: 记录验证命令执行结果
- `build_verification_reminder()`: 注入 `<system-reminder>` 提醒模型执行验证
- `check_verification_block()`: turn 完成前检查验证状态
- Debug/Refactor/Perf 默认 `require_verification = true`

---

## 五、Policy Hook — 策略钩子

### 9 种 Hook Point

`BeforeModelCall`, `BeforeToolCall`, `AfterToolCall`, `BeforeFileWrite`, `AfterFileWrite`, `BeforeCommandExec`, `AfterCommandExec`, `AfterVerification`, `BeforeTaskComplete`

### 4 种 Hook Decision

| Decision | 效果 |
|---|---|
| `Allow` | 放行 |
| `Deny { reason }` | 拒绝 |
| `RequireUserApproval { reason }` | 需用户确认 |
| `RequireRemediation { message, required_actions }` | 要求补充动作 |

### 4 个内置 Hook

| Hook | Hook Point | 功能 |
|---|---|---|
| `CompletionVerificationHook` | BeforeTaskComplete | tier 要求验证但未执行 → Deny |
| `DiffSummaryHook` | BeforeTaskComplete | 有文件变更但无摘要 → RequireRemediation |
| `DangerousCommandHook` | BeforeCommandExec | `rm -rf /` → Deny; `git push --force` → RequireUserApproval |
| `SkillRequiredHook` | BeforeTaskComplete | 按 skill 声明的 hook_rules 检查合规 |

### PolicyHookRunner

- 注册 → 按 HookPoint 执行 → 首个 Block 短路
- 优先级高于外部 Hook 系统（command/prompt/HTTP/agent hooks）

---

## 六、Skill-driven Hook Rules

Skill 可在 YAML frontmatter 中声明规则，由 PolicyHook 强制执行：

```yaml
name: rust-policy
hook_rules:
  - id: rust-fmt-required
    condition:
      changed_file_ext: ".rs"
    require:
      command_executed: "cargo fmt"
  - id: rust-test-required
    condition:
      changed_file_ext: ".rs"
    require:
      command_executed_matches: "cargo test"
```

- `SkillHookRule` / `SkillHookCondition` / `SkillHookRequirement` 类型系统
- YAML frontmatter `hook_rules` 解析器
- Agent build 时自动从 SkillManager 收集所有 skill 的 hook_rules 并注册到 SkillRequiredHook

---

## 七、ModelRouter — 多 Provider 模型路由

- **`RwLock<HashMap<String, Arc<dyn Model>>>`** — 内部可变性，`&self` 懒创建
- **双检锁模式**: 读锁命中 → 直接返回; 未命中 → 写锁创建 → 复查 → 插入
- `warm_from_registry()`: 从 ProviderRegistry 预初始化所有 provider
- Agent 通过 `resolve_active_model()` 在 turn 循环中按 provider 选择 Model 实例

---

## 八、Model Tier Escalation — 模型强度自动升级

### 三级模型

```text
lite:    Explain  Search  Document
normal:  Generate Modify  Review  Plan  Test  Deps
strong:  Debug    Refactor Perf
```

### 三层升级引擎

```
EscalationInput { base_tier, task_kind, signals, runtime_feedback }
  ↓
Layer 1: Force Rules (7 rules)
  F1: Debug/Refactor/Perf → strong
  F2: stack trace → strong
  F3: security-sensitive → strong
  F4: concurrency/unsafe → strong
  F5: previous normal failure → strong
  F6: last verification failed → strong
  F7: hook rejected >= 2 → strong
  ↓
Layer 2: Risk Scoring (11 signals, configurable weights)
  has_error_log +3    has_stack_trace +3    previous_attempt_failed +3
  last_verification_failed +3    touches_security +3
  touches_public_api +2    touches_concurrency +2    multi_file_change +2
  touches_deps +2    hook_rejected +2    large_context +1    production_grade +1

  Thresholds:
    lite: score>=2→normal, >=5→strong
    normal: score>=4→strong (or >=3 with multi-file)
  ↓
Layer 3: Runtime Feedback
  previous_attempt_failed / last_verification_failed / hook_rejected_count
  carried via RuntimeFeedback struct between turns
  strong failure → blocked (not infinite upgrade)
  ↓
TierDecision { final_tier, score, reasons, force_reason, runtime_policy }
```

### 升级联动

升级到 strong 不只是换模型，同时切换执行策略：

| 策略维度 | lite | normal | strong |
|---|---|---|---|
| max_context_tokens | 16,000 | 64,000 | 160,000 |
| require_plan | false | false | **true** |
| require_review | false | false | **true** |
| require_verification | false | false | **true** |
| max_repair_iterations | 0 | 1 | 3 |

### 信号提取

- `SignalExtractor` trait（关键词 → AST/LSP/LLM 可演进）
- `KeywordSignalExtractor`: 中英双语关键词检测，<1ms
- `CodingSignals`: 9 个布尔信号，RiskEvaluator 不直接扫描原始字符串

### 配置

```toml
[escalation]
enabled = true
allow_force_override = true

[escalation.thresholds]
lite_to_normal = 2
lite_to_strong = 5
normal_to_strong = 4

[escalation.weights]
has_error_log = 3
touches_security = 3
# ... all 11 weights configurable
```

---

## 九、提示词注入格式

Routing ON 时，每次 turn 注入以下结构（仅当前任务类型）：

```markdown
# Context Pack
## Task
- Kind: Debug
## User Request
...
## Git Status
...
## Error Summary
...

# Execution Tier (Upgraded)
- Base tier: normal
- Final tier: strong
- Score: 5
- Upgrade reason (force): security-sensitive change detected
## Runtime Requirements
- require_plan: true
- require_verification: true
- max_repair_iterations: 3

# Current Task: Debug
# Task: Debug
You are finding and fixing a bug...
## Task Policy
- Model tier: strong
- Tool policy: read_write_test
- Verification: required
## Output Format
- Root cause (1-2 sentences)
- Fix description
- Verification result (MANDATORY)
```

Routing OFF 时：与 v2.0.0 之前的 prompt 完全一致。

---

## 十、模块结构

```
crates/scene/src/coding/
  mod.rs           — 模块索引
  task.rs          — CodingTaskKind (12 种) + TaskRouter + 分类器
  prompt.rs        — TaskProfile (12 种) + PromptProfile (12 种)
  config.rs        — ModelProfile + $引用解析 + CodingSceneConfig
  context.rs       — ContextPack + ContextPackBuilder
  verify.rs        — VerificationPolicy + VerificationRecord + CodingLoopState
  policy.rs        — PolicyHook trait + 4 内置 Hook + PolicyHookRunner
  tier.rs          — ModelTier + TierRuntimePolicy + CompletionRequirements
  escalation.rs    — CodingSignals + SignalExtractor + RiskEvaluator + TierDecision

crates/model/src/
  router.rs        — ModelRouter (RwLock + 双检锁 + 多 provider 缓存)

crates/core/src/
  provider.rs      — ProviderDef 扩展 (from_env, models, register, find)
  interface/scene.rs — SceneVerificationPolicy + user_message + verification_policy()
  frozen/skill.rs  — SkillHookRule + SkillHookCondition + SkillHookRequirement
```

---

## 配置开关

| 开关 | 默认值 | 作用 |
|---|---|---|
| `enable_task_routing` | `true` | 控制 12 种任务分类 + 独立 PromptProfile |
| `enable_context_pack` | `true` | 控制 ContextPack 构建与注入 |
| `enable_verification_loop` | `false` | 控制验证闭环（Phase 3） |
| `enable_policy_hooks` | `false` | 控制策略钩子（Phase 4） |
| `enable_model_escalation` | `true` | 控制模型强度自动升级 |
| `escalation.enabled` | `true` | 控制整个升级引擎 |
| `escalation.allow_force_override` | `true` | force rules 是否可覆盖用户显式模型配置 |

关闭全部开关 → 行为与 v2.0.0 之前完全一致。

---

## 向后兼容

- 所有新字段均为 `Option` 或提供 `Default`
- `AgentScene` trait 新方法均有默认实现
- `CodingScene::default_scene()` 保持原有构造方式
- Routing OFF 时 prompt 逐字节与原始版本相同
- 用户显式配置的 `task_profiles.x.model_profile` 优先于自动升级（force rules 除外）
