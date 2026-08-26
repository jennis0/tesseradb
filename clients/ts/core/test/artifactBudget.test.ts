import {describe, expect, it} from 'vitest';
import {BASE_ARTIFACT_BUDGET, MAX_ARTIFACT_BUDGET, artifactBudgetFor, levelForBudget} from '../src/artifactBudget.js';

describe('the artifact budget follows the zoom (design §6)', () => {
  it('is the base at the overview, doubles per zoom level, and is capped', () => {
    expect(artifactBudgetFor(0)).toBe(BASE_ARTIFACT_BUDGET);
    expect(artifactBudgetFor(-2)).toBe(BASE_ARTIFACT_BUDGET);
    expect(artifactBudgetFor(1)).toBe(BASE_ARTIFACT_BUDGET * 2);
    expect(artifactBudgetFor(3)).toBe(BASE_ARTIFACT_BUDGET * 8);
    expect(artifactBudgetFor(2.5)).toBe(Math.round(BASE_ARTIFACT_BUDGET * 2 ** 2.5));
    expect(artifactBudgetFor(16)).toBe(MAX_ARTIFACT_BUDGET);
    expect(artifactBudgetFor(Number.NaN)).toBe(BASE_ARTIFACT_BUDGET);
  });
});

describe('levelForBudget follows the cut a budget would have served', () => {
  it('draws the deepest level that fits with every level above it, and level 0 always', () => {
    const tiers = [16, 46, 161, 574];
    expect(levelForBudget(tiers, 24)).toBe(0);
    expect(levelForBudget(tiers, 62)).toBe(1);
    expect(levelForBudget(tiers, 100)).toBe(1);
    expect(levelForBudget(tiers, 223)).toBe(2);
    expect(levelForBudget(tiers, 2048)).toBe(3);
    expect(levelForBudget([500], 24)).toBe(0);
    expect(levelForBudget([], 24)).toBe(0);
  });
});
