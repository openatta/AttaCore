#!/usr/bin/env node
/**
 * Entry point: load a DSH plugin, serve its tools over MCP on stdio.
 *
 * Usage: `atta-dsh-bridge <path-to-plugin.js>`
 *
 * stdout carries protocol messages and nothing else. Every diagnostic goes
 * to stderr, because one stray line on stdout corrupts the stream for the
 * client.
 */

import { createInterface } from 'node:readline';
import { basename } from 'node:path';
import { loadPlugin } from './load.js';
import { serveLine } from './server.js';
import { callTool } from './translate.js';

async function main(argv) {
  const entry = argv[2];
  if (!entry) {
    process.stderr.write('usage: atta-dsh-bridge <path-to-plugin.js>\n');
    process.exit(2);
  }

  let loaded;
  try {
    loaded = await loadPlugin(entry);
  } catch (e) {
    // Refusing to start is the right outcome for an unsupported plugin: an
    // MCP server that came up and then failed every call would be harder to
    // diagnose than one that never came up and said why.
    process.stderr.write(`atta-dsh-bridge: ${e.message}\n`);
    process.exit(1);
  }

  const { registry, ctx, name } = loaded;
  const serverName = name || basename(entry);
  process.stderr.write(
    `atta-dsh-bridge: serving ${registry.list().length} tool(s) from ${serverName}\n`,
  );

  const write = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);
  const deps = { registry, serverName, callTool };

  const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of rl) {
    await serveLine(line, deps, write);
  }

  // stdin closed: the client is gone, so run the plugin's cleanup.
  ctx.dispose();
}

main(process.argv).catch((e) => {
  process.stderr.write(`atta-dsh-bridge: ${e.stack ?? e}\n`);
  process.exit(1);
});
