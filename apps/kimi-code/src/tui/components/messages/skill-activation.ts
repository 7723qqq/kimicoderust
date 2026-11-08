/**
 * Skill activation card.
 *
 * When the user runs `/skill:foo bar`, the TUI renders a compact card instead
 * of expanding the SKILL.md body into the user bubble:
 *
 *   ▶ Activated skill: foo
 *     bar
 *
 * The args line is optional. Core expands the skill body into the LLM context;
 * the TUI only consumes the `skill.activated` event and user_message origin
 * metadata.
 */

import { Container, Text, Spacer } from '@moonshot-ai/pi-tui';

import { t } from '#/i18n';
import { currentTheme } from '#/tui/theme';
import type { SkillActivationTrigger } from '#/tui/types';

const ARGS_PREVIEW_MAX = 200;

export class SkillActivationComponent extends Container {
  private headText: Text;
  private previewText?: Text;
  private name: string;
  private args?: string;
  /** v2 #4627: a steered activation stays dimmed until the engine reads it. */
  private steerPending: boolean;

  constructor(
    name: string,
    args: string | undefined,
    readonly trigger?: SkillActivationTrigger,
    steerPending = false,
  ) {
    super();
    this.name = name;
    this.args = args;
    this.steerPending = steerPending;
    this.addChild(new Spacer(1));
    this.headText = new Text(this.headLine(), 0, 0);
    this.addChild(this.headText);
    const trimmed = args?.trim() ?? '';
    if (trimmed.length > 0) {
      this.previewText = new Text(this.previewLine(trimmed), 0, 0);
      this.addChild(this.previewText);
    }
  }

  /** The pending flag is presentation: the card is dimmed, not rewritten. */
  setSteerPending(pending: boolean): void {
    if (this.steerPending === pending) return;
    this.steerPending = pending;
    this.invalidate();
  }

  override invalidate(): void {
    this.headText.setText(this.headLine());
    if (this.previewText !== undefined && this.args !== undefined) {
      this.previewText.setText(this.previewLine(this.args.trim()));
    }
    super.invalidate();
  }

  private headLine(): string {
    const head =
      currentTheme.boldFg('primary', t('tui.messages.skillActivation.activated')) +
      currentTheme.boldFg('roleUser', this.name);
    return this.steerPending ? currentTheme.dim(head) : head;
  }

  private previewLine(trimmed: string): string {
    const preview =
      trimmed.length > ARGS_PREVIEW_MAX ? trimmed.slice(0, ARGS_PREVIEW_MAX) + '…' : trimmed;
    const line = '  ' + currentTheme.fg('textDim', preview);
    return this.steerPending ? currentTheme.dim(line) : line;
  }
}
