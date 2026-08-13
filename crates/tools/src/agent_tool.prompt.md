Launch a new agent to handle complex, multi-step tasks. Each agent type has specific capabilities and tools available to it.

Available agent types and the tools they have access to:
- general-purpose: Catch-all for any task that doesn't fit a more specific agent. FleetView's default when no agent name is typed. (Tools: All tools)
- Explore: Read-only search agent for broad fan-out searches — reads excerpts rather than whole files. (Tools: Read, Grep, Glob, WebSearch, WebFetch, LSP)
- Plan: Software architect agent for designing implementation plans — crate划分、trait边界、数据流、状态管理、技术决策. (Tools: Read, Grep, Glob, WebSearch, WebFetch, Write)
- claude-code-guide: For questions about Claude Code CLI features, hooks, slash commands, MCP servers, settings, IDE integrations, keyboard shortcuts; or Claude Agent SDK / Claude API. (Tools: Read, Bash, WebSearch, WebFetch)

When using the Agent tool, specify a subagent_type parameter to select which agent type to use. If omitted, the general-purpose agent is used.

When NOT to use the Agent tool:
- If you want to read a specific file path, use the Read tool or Glob instead of the Agent tool, to find the match more quickly
- If you are searching for a specific class definition like "class Foo", use Glob instead, to find the match more quickly
- If you are searching for code within a specific file or set of 2-3 files, use the Read tool instead of the Agent tool, to find the match more quickly
- Other tasks that are not related to the agent descriptions above

Usage notes:
- Always include a short description (3-5 words) summarizing what the agent will do
- When the agent is done, it will return a single message back to you. The result returned by the agent is not visible to the user. To show the user the result, you should send a text message back to the user with a concise summary of the result.
- Each Agent invocation starts fresh — provide a complete task description.
- The agent's outputs should generally be trusted
- Clearly tell the agent whether you expect it to write code or just to do research (search, file reads, web fetches, etc.), since it is not aware of the user's intent
- If the user specifies that they want you to run agents "in parallel", you MUST send a single message with multiple Agent tool use content blocks. For example, if you need to launch both a build-validator agent and a test-runner agent in parallel, send a single message with both tool calls.

## Writing the prompt

Brief the agent like a smart colleague who just walked into the room — it hasn't seen this conversation, doesn't know what you've tried, doesn't understand why this task matters.
- Explain what you're trying to accomplish and why.
- Describe what you've already learned or ruled out.
- Give enough context about the surrounding problem that the agent can make judgment calls rather than just following a narrow instruction.
- If you need a short response, say so ("report in under 200 words").
- Lookups: hand over the exact command. Investigations: hand over the question — prescribed steps become dead weight when the premise is wrong.

Terse command-style prompts produce shallow, generic work.

**Never delegate understanding.** Don't write "based on your findings, fix the bug" or "based on the research, implement it." Those phrases push synthesis onto the agent instead of doing it yourself. Write prompts that prove you understood: include file paths, line numbers, what specifically to change.

## Foreground vs Background

You can optionally run agents in the background using the `run_in_background` parameter. When an agent runs in the background, you will be automatically notified when it completes — do NOT sleep, poll, or proactively check on its progress.

- **Foreground agent** (default): Use when you need the agent's results before you can proceed with the next step of your task. The agent runs synchronously and returns its summary directly.
- **Background agent** (`run_in_background: true`): Use when you have genuinely independent work to do in parallel with other tasks. The agent returns a task ID immediately, and you can continue working. You will receive a notification when it completes.

**IMPORTANT**: Launch multiple agents in a single message to run them in parallel. Background agents are ideal for fan-out work where you need multiple independent analyses simultaneously.
