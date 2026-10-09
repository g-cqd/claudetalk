/**
 * Per-namespace window-bucket rate limiter for the relay. Window count
 * resets at the slice boundary; simpler and slightly burstier than a
 * token bucket, which is fine for the "circuit breaker against a stuck
 * loop" goal.
 */

export interface RateConfig {
  framesPerWindow: number;
  windowMs: number;
}

interface RateBucket {
  windowStart: number;
  count: number;
}

export class NamespaceRateLimiter {
  private readonly buckets = new Map<string, RateBucket>();

  constructor(private readonly cfg: RateConfig) {}

  /** True if the frame should be admitted; false if rate-limited. */
  allow(namespace: string, now = Date.now()): boolean {
    const b = this.buckets.get(namespace);
    if (!b || now - b.windowStart >= this.cfg.windowMs) {
      this.buckets.set(namespace, { windowStart: now, count: 1 });
      return true;
    }
    if (b.count >= this.cfg.framesPerWindow) return false;
    b.count += 1;
    return true;
  }

  /** Test helper. */
  _resetForTests(): void {
    this.buckets.clear();
  }
}
