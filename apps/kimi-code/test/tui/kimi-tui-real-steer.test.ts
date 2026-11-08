/**
 * The skill-activation steer path against the real engine and transport.
 *
 * `kimi-tui-message-flow.test.ts` proves the TUI's bookkeeping with a fake
 * session. This test runs one process end to end: the real `KimiTUI`, the real
 * native harness (Rust engine through the napi addon), the real
 * `NativeHttpLlm` behind a local OpenAI-shaped SSE stub, and the real skill
 * catalog read from the workspace.
 *
 * It pins v2 #4627 on the skill path: a Ctrl+S steered `/skill:` activation dims
 * its card until the engine reports the activation read, exactly like a steered
 * user message, and the TUI hop log answers where that prompt is.
 */
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { findKimiAgentAddon } from '@moonshot-ai/kimi-agent/session-handle';
import chalk from 'chalk';
import { createKimiHarnessNative, type KimiHostIdentity } from '@moonshot-ai/kimi-code-sdk';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { KimiTUI, type KimiTUIStartupInput, type TUIState } from '#/tui/kimi-tui';

const hasNativeAddon = findKimiAgentAddon() !== null;

const TEST_IDENTITY: KimiHostIdentity = {
  productName: 'kimi-code-cli',
  version: '0.0.0-test',
  platform: 'kimi_code_cli',
};

/** The TUI surface this test drives (the class itself is private-typed). */
interface Driver {
  state: TUIState;
  init(): Promise<boolean>;
  ensureSession(): Promise<unknown>;
  handleUserInput(text: string): void;
  persistInputHistory(text: string): Promise<void>;
  steerStagesOf(promptId: string): string[];
  steerCurrentStage(promptId: string): string | undefined;
  steerTraceSnapshot(): ReadonlyArray<{ promptId?: string; stage: string }>;
}

function startupInput(workDir: string): KimiTUIStartupInput {
  return {
    cliOptions: {
      session: undefined,
      continue: false,
      yolo: true,
      auto: false,
      plan: false,
      model: undefined,
      outputFormat: undefined,
      prompt: undefined,
      skillsDirs: [],
      agent: undefined,
      agentFiles: [],
    },
    tuiConfig: {
      theme: 'dark',
      locale: 'en',
      disablePasteBurst: false,
      editorCommand: null,
      notifications: { enabled: true, condition: 'unfocused' },
      upgrade: { autoInstall: true },
      astron: { stream: true, temperature: 1.0, maxTokens: 32768, searchDisable: true },
      statusLine: { items: null, command: null },
    },
    version: '0.0.0-test',
    workDir,
  };
}

function sse(chunks: readonly unknown[]): string {
  return chunks.map((chunk) => `data: ${JSON.stringify(chunk)}\n\n`).join('') + 'data: [DONE]\n\n';
}

/** Step one asks for a native tool so the turn reaches the step that drains. */
function toolCallSse(): string {
  return sse([
    {
      choices: [
        {
          delta: {
            tool_calls: [
              {
                index: 0,
                id: 'call_glob',
                type: 'function',
                function: { name: 'Glob', arguments: '{"pattern":"*.rs"}' },
              },
            ],
          },
          finish_reason: null,
        },
      ],
    },
    { choices: [{ delta: {}, finish_reason: 'tool_calls' }] },
    { choices: [], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } },
  ]);
}

function answerSse(): string {
  return sse([
    { choices: [{ delta: { content: 'answer after the skill' }, finish_reason: 'stop' }] },
    { choices: [], usage: { prompt_tokens: 20, completion_tokens: 3, total_tokens: 23 } },
  ]);
}

interface StubServer {
  readonly requests: readonly unknown[];
  readonly firstArrived: Promise<void>;
  readonly secondArrived: Promise<void>;
  releaseFirst(): void;
  close(): Promise<void>;
}

async function startStub(): Promise<StubServer> {
  const requests: unknown[] = [];
  let releaseFirst!: () => void;
  const firstGate = new Promise<void>((resolve) => {
    releaseFirst = resolve;
  });
  let firstArrived!: () => void;
  const firstArrivedPromise = new Promise<void>((resolve) => {
    firstArrived = resolve;
  });
  let secondArrived!: () => void;
  const secondArrivedPromise = new Promise<void>((resolve) => {
    secondArrived = resolve;
  });
  const server = createServer((request, response) => {
    let body = '';
    request.on('data', (chunk: Buffer) => {
      body += chunk.toString('utf8');
    });
    request.on('end', () => {
      void (async () => {
        requests.push(JSON.parse(body) as unknown);
        const index = requests.length - 1;
        response.writeHead(200, { 'content-type': 'text/event-stream' });
        if (index === 0) {
          firstArrived();
          await firstGate;
          response.end(toolCallSse());
          return;
        }
        secondArrived();
        response.end(answerSse());
      })();
    });
  });
  await new Promise<void>((resolve) => {
    server.listen(0, '127.0.0.1', resolve);
  });
  const port = (server.address() as AddressInfo).port;
  return {
    requests,
    firstArrived: firstArrivedPromise,
    secondArrived: secondArrivedPromise,
    releaseFirst,
    close: () =>
      new Promise<void>((resolve, reject) => {
        server.close((error) => (error === undefined ? resolve() : reject(error)));
      }),
    // `port` is read through the request bodies; no other consumer needs it.
    ...({ port } as object),
  } as StubServer;
}

const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

describe.skipIf(!hasNativeAddon)('KimiTUI skill steer (real engine)', () => {
  it('dims a Ctrl+S steered skill activation until the engine reads it', async () => {
    const stub = await startStub();
    const port = (stub as unknown as { port: number }).port;
    const home = await mkdtemp(join(tmpdir(), 'kimi-tui-real-skill-'));
    tempDirs.push(home);
    const workDir = join(home, 'project');
    await mkdir(join(workDir, '.agents', 'skills', 'demo'), { recursive: true });
    await writeFile(
      join(workDir, '.agents', 'skills', 'demo', 'SKILL.md'),
      ['---', 'name: demo', 'description: demo skill', '---', 'Say hello from the skill.', ''].join(
        '\n',
      ),
      'utf8',
    );
    await writeFile(
      join(home, 'config.toml'),
      [
        'default_model = "local/a"',
        '',
        '[providers.local]',
        'type = "openai"',
        `base_url = "http://127.0.0.1:${port}/v1"`,
        'api_key = "sk-test"',
        '',
        '[models."local/a"]',
        'provider = "local"',
        'model = "stub-model"',
        'max_context_size = 200000',
        '',
      ].join('\n'),
      'utf8',
    );

    const harness = createKimiHarnessNative({ homeDir: home, identity: TEST_IDENTITY });
    const driver = new KimiTUI(harness as never, startupInput(workDir)) as unknown as Driver;
    vi.spyOn(driver.state.ui, 'requestRender').mockImplementation(() => {});
    vi.spyOn(driver.state.terminal, 'setProgress').mockImplementation(() => {});
    driver.persistInputHistory = vi.fn(async () => {});
    const previousChalkLevel = chalk.level;
    try {
      await driver.init();
      driver.state.appState.model = 'local/a';
      await driver.ensureSession();
      expect(driver.state.appState.sessionId).toBeDefined();
      chalk.level = 3;
      // A turn in flight, parked in the provider call.
      driver.handleUserInput('first question');
      await stub.firstArrived;
      expect(driver.state.appState.streamingPhase).not.toBe('idle');

      // Queue a standalone skill activation: a running turn queues it.
      driver.handleUserInput('/skill:demo go');
      await vi.waitFor(() => {
        expect(driver.state.queuedMessages.some((item) => item.mode === 'skill')).toBe(true);
      });

      // Ctrl+S steers the queued activation into the running turn.
      driver.state.editor.handleInput('\u0013');

      const card = () => {
        const entry = driver.state.transcriptEntries.find(
          (candidate) => candidate.kind === 'skill_activation' && candidate.skillName === 'demo',
        );
        return entry;
      };
      await vi.waitFor(() => {
        expect(card()).toBeDefined();
      });
      await vi.waitFor(() => {
        expect(card()?.steerPending).toBe(true);
      });
      // Every hop is on record under one prompt id: the TUI's own log answers
      // where the activation is, not just whether it is pending.
      const promptId = driver
        .steerTraceSnapshot()
        .find((record) => record.promptId !== undefined)?.promptId;
      if (promptId === undefined) throw new Error('the TUI never logged a prompt id');
      expect(driver.steerStagesOf(promptId)).toEqual(['accepted', 'pending', 'submitted']);
      expect(driver.steerCurrentStage(promptId)).toBe('submitted');
      const tracedIds = new Set(
        driver
          .steerTraceSnapshot()
          .flatMap((record) => (record.promptId === undefined ? [] : [record.promptId])),
      );
      expect([...tracedIds]).toEqual([promptId]);

      // The user-visible hop: the rendered card is dimmed while pending.
      const rawCardLine = () =>
        driver.state.transcriptContainer
          .render(120)
          .find((line) => line.includes('demo'));
      expect(rawCardLine()).toContain('[2m');

      // Release step one; the tool runs, step two drains the activation.
      stub.releaseFirst();
      await stub.secondArrived;
      await vi.waitFor(() => {
        expect(card()?.steerPending).toBeUndefined();
      });
      expect(rawCardLine()).not.toContain('[2m');

      // The engine's acknowledgement is the last hop, on the same prompt id.
      expect(driver.steerStagesOf(promptId)).toEqual([
        'accepted',
        'pending',
        'submitted',
        'acknowledged',
      ]);
      expect(driver.steerCurrentStage(promptId)).toBe('acknowledged');

      // The streamed text became exactly one assistant entry, and the steer
      // neither duplicated the skill card nor added a second message row. The
      // acknowledgement arrives before step two streams, so wait for the text.
      await vi.waitFor(() => {
        expect(
          driver.state.transcriptEntries.filter(
            (entry) =>
              entry.kind === 'assistant' && entry.content.includes('answer after the skill'),
          ),
        ).toHaveLength(1);
      });
      expect(
        driver.state.transcriptEntries.filter(
          (entry) => entry.kind === 'skill_activation' && entry.skillName === 'demo',
        ),
      ).toHaveLength(1);

      // The model read the rendered skill body in the second request.
      const second = stub.requests[1] as {
        messages: Array<{ role: string; content: unknown }>;
      };
      expect(JSON.stringify(second.messages)).toContain('Say hello from the skill.');
      // The activation sits after the tool result of step one.
      const serialized = JSON.stringify(second.messages);
      expect(serialized.indexOf('call_glob')).toBeLessThan(
        serialized.indexOf('Say hello from the skill.'),
      );
    } finally {
      chalk.level = previousChalkLevel;
      await harness.close();
      await stub.close();
    }
  }, 60_000);
});
