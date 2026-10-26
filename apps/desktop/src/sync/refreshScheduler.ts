export interface RefreshSchedulerOptions {
  delayMs: number;
  concurrency: number;
  run: (key: string) => Promise<void>;
  onError?: (key: string, error: unknown) => void;
}

/**
 * 事件刷新调度器：按资源键合并、去重、并发受限。
 * 调用方在 resync 时 clear() 并用代际号丢弃在途结果。
 */
export class RefreshScheduler {
  private pending = new Set<string>();
  private timer: ReturnType<typeof setTimeout> | null = null;
  private running: Promise<void> | null = null;
  private disposed = false;

  constructor(private readonly options: RefreshSchedulerOptions) {}

  mark(key: string): void {
    if (this.disposed) return;
    this.pending.add(key);
    if (!this.timer && !this.running) {
      this.timer = setTimeout(() => {
        this.timer = null;
        void this.flushNow();
      }, this.options.delayMs);
    }
  }

  get pendingCount(): number {
    return this.pending.size;
  }

  clear(): void {
    this.pending.clear();
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  dispose(): void {
    this.disposed = true;
    this.clear();
  }

  /** stop 后重新 start 时恢复调度。 */
  reset(): void {
    this.disposed = false;
    this.clear();
  }

  async flushNow(): Promise<void> {
    if (this.running) return this.running;
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    this.running = this.drain();
    try {
      await this.running;
    } finally {
      this.running = null;
      if (!this.disposed && this.pending.size > 0 && !this.timer) {
        this.timer = setTimeout(() => {
          this.timer = null;
          void this.flushNow();
        }, this.options.delayMs);
      }
    }
  }

  private async drain(): Promise<void> {
    for (let round = 0; round < 20; round += 1) {
      if (this.pending.size === 0) return;
      const batch = [...this.pending].slice(0, this.options.concurrency);
      for (const key of batch) this.pending.delete(key);
      await Promise.all(
        batch.map(async (key) => {
          try {
            await this.options.run(key);
          } catch (error) {
            this.options.onError?.(key, error);
          }
        }),
      );
    }
  }
}
