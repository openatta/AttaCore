Launch a new agent to handle complex, multi-step tasks autonomously.

The Agent tool launches specialized agents (subprocesses) that autonomously handle complex tasks. Each agent type has specific capabilities and tools available to it.

Available agent types and the tools they have access to:
- general: Catch-all for any task that doesn't fit a more specific agent (Tools: All tools)
- explore: Fast read-only search agent for locating code (Tools: All tools except Edit, Write, NotebookEdit)
- plan: Software architect agent for designing implementation plans (Tools: All tools except Edit, Write, NotebookEdit)
- claude-code-guide: Use this agent when the user asks questions about: (1) Claude Code features, hooks, slash commands, MCP servers, settings; (2) Claude Agent SDK; (3) Claude API usage. IMPORTANT: Before spawning a new agent, check if there is already a running or recently completed claude-code-guide agent. (Tools: Bash, Read, WebFetch, WebSearch)
- Explore: Read-only search agent for broad fan-out searches. Specify search breadth: "medium" for moderate exploration, "very thorough" for multiple locations. (Tools: All tools except Agent, ExitPlanMode, Edit, Write, NotebookEdit)
- general-purpose: General-purpose agent for researching complex questions and executing multi-step tasks. (Tools: *)
- Plan: Software architect agent for designing implementation plans. Returns step-by-step plans. (Tools: All tools except Agent, ExitPlanMode, Edit, Write, NotebookEdit)
- statusline-setup: Use this agent to configure the user's status line setting. (Tools: Read, Edit)

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
- If the agent description mentions that it should be used proactively, then you should try your best to use it without the user having to ask for it first. Use your judgement.
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

You can optionally run agents in the background using the `background` parameter. When an agent runs in the background, you will be automatically notified when it completes — do NOT sleep, poll, or proactively check on its progress.

- **Foreground agent** (default): Use when you need the agent's results before you can proceed with the next step of your task. The agent runs synchronously and returns its summary directly.
- **Background agent** (`background: true`): Use when you have genuinely independent work to do in parallel with other tasks. The agent returns a task ID immediately, and you can continue working. You will receive a notification when it completes.

**IMPORTANT**: Launch multiple agents in a single message to run them in parallel. Background agents are ideal for fan-out work where you need multiple independent analyses simultaneously.

## When to fork

You can omit `subagent_type` to fork yourself — the forked agent inherits your context and shares your prompt cache. Use this when the intermediate tool output isn't worth keeping in your context.

- **Research**: fork open-ended questions. If research can be broken into independent questions, launch parallel forks in one message.
- **Implementation**: fork work that requires more than a couple of edits. Do research before jumping to implementation.
- **Don't peek**: the tool result includes an output — don't Read or tail it unless asked. Trust the completion notification.
- **Don't race**: after launching a fork, never fabricate or predict its results. Wait for the notification.

## Worktree isolation

Use `isolation: "worktree"` to give the agent its own git worktree — an isolated copy of the repo where it can safely make changes without affecting your working directory. The worktree is auto-cleaned if unchanged.
