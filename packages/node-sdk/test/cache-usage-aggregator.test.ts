import { describe, expect, it } from 'vitest';

import { CacheUsageAggregator } from '../src/native/sdk-client/helpers';
import type { TokenUsage } from '../src/types';

const usage = (over: Partial<TokenUsage> = {}): TokenUsage => ({
  inputOther: 100,
  output: 20,
  inputCacheRead: 0,
  inputCacheCreation: 0,
  ...over,
});

describe('CacheUsageAggregator (the host half of the prompt-cache probe)', () => {
  it('folds a turn\u2019s steps and reports one record with upstream\u2019s denominator', () => {
    const agg = new CacheUsageAggregator();
    agg.add(usage({ inputOther: 100, inputCacheRead: 300, inputCacheCreation: 50, output: 20 }));
    agg.add(usage({ inputOther: 20, inputCacheRead: 500, inputCacheCreation: 30, output: 10 }));

    const record = agg.settle();
    expect(record).toEqual({
      inputTokens: 100 + 300 + 50 + 20 + 500 + 30,
      cacheRead: 800,
      cacheCreation: 80,
      output: 30,
      // Denominator is TOTAL input, not cacheable input \u2014 upstream
      // cacheProbe.ts:25 divides inputCacheRead by inputTotal.
      cacheHitRate: 800 / (100 + 300 + 50 + 20 + 500 + 30),
    });
  });

  it('resets between turns: a second turn reports only its own steps', () => {
    const agg = new CacheUsageAggregator();
    agg.add(usage({ inputCacheRead: 999 }));
    agg.reset();
    agg.add(usage({ inputOther: 10, output: 1 }));

    expect(agg.settle()).toMatchObject({ inputTokens: 10, output: 1, cacheRead: 0 });
  });

  it('settle clears the aggregate: one record per turn even if settle runs twice', () => {
    const agg = new CacheUsageAggregator();
    agg.add(usage({ inputCacheRead: 5 }));
    expect(agg.settle()).toBeDefined();
    expect(agg.settle()).toBeUndefined();
  });

  it('a turn whose steps carry no usage produces no record', () => {
    const agg = new CacheUsageAggregator();
    agg.add(undefined);
    expect(agg.settle()).toBeUndefined();
  });

  it('a zero total input reports no record rather than a divide-by-zero rate', () => {
    const agg = new CacheUsageAggregator();
    agg.add({ inputOther: 0, output: 0, inputCacheRead: 0, inputCacheCreation: 0 });
    expect(agg.settle()).toBeUndefined();
  });

  it('steps without usage do not clear an aggregate the turn already started', () => {
    const agg = new CacheUsageAggregator();
    agg.add(usage({ inputCacheRead: 5 }));
    agg.add(undefined);
    agg.add(undefined);
    expect(agg.settle()).toMatchObject({ cacheRead: 5 });
  });
});
