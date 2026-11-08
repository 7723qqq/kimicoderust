/** Where a steered message is, hop by hop, on the TUI side of v2 #4627. */
export type SteerStageName =
  | 'accepted'
  | 'pending'
  | 'submitted'
  | 'acknowledged'
  | 'released';

export interface SteerTraceEntry {
  /** Absent until the SDK's receipt names the prompt. */
  promptId?: string;
  stage: SteerStageName;
}

/**
 * An append-only log of the hops a steer takes inside the TUI, so a prompt's
 * position is answerable instead of inferred: `stagesOf(id)` is what happened,
 * `current(id)` is where it is now. The engine's side of the same prompt lives
 * in `kimi_ui_core::steer::SteerJournal`; the two are compared by prompt id.
 */
export class SteerTraceLog {
  private readonly records: SteerTraceEntry[] = [];

  append(stage: SteerStageName, promptId?: string): SteerTraceEntry {
    const entry: SteerTraceEntry = promptId === undefined ? { stage } : { promptId, stage };
    this.records.push(entry);
    return entry;
  }

  stagesOf(promptId: string): SteerStageName[] {
    return this.records
      .filter((entry) => entry.promptId === promptId)
      .map((entry) => entry.stage);
  }

  current(promptId: string): SteerStageName | undefined {
    return this.stagesOf(promptId).at(-1);
  }

  snapshot(): readonly SteerTraceEntry[] {
    return [...this.records];
  }
}
