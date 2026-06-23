Launch a new agent to handle complex, multi-step tasks autonomously.

The Agent tool launches specialized agents (subprocesses) that autonomously handle complex tasks. Each agent type has specific capabilities and tools available to it.

Available agent types and the tools they have access to:
- claude: Catch-all for any task that doesn't fit a more specific agent. FleetView's default when no agent name is typed. (Tools: All tools)
- general-purpose: General-purpose agent for researching complex questions, searching for code, and executing multi-step tasks. (Tools: All tools)
- explore: Fast read-only search agent for locating code. Uses Read/Grep/Glob/WebSearch/WebFetch/LSP. (Tools: Read, Grep, Glob, WebSearch, WebFetch, LSP)
- plan: Software architect agent for designing implementation plans. Uses Read/Grep/Glob/WebSearch/WebFetch/Write. (Tools: Read, Grep, Glob, WebSearch, WebFetch, Write)
- code-reviewer: Code review specialist. Uses Read/Grep/Glob/LSP/Bash for read-only inspection. (Tools: Read, Grep, Glob, LSP, Bash)

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
