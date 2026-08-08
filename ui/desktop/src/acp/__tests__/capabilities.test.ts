import type { InitializeResponse } from '@agentclientprotocol/sdk';
import { describe, expect, it } from 'vitest';
import { hasLocalInferenceCapability } from '../capabilities';

function initializeResponseWithMeta(meta?: unknown): Pick<InitializeResponse, 'agentCapabilities'> {
  return {
    agentCapabilities: {
      _meta: meta,
    },
  } as Pick<InitializeResponse, 'agentCapabilities'>;
}

describe('ACP capabilities', () => {
  it('detects local inference support from Kaji metadata', () => {
    expect(
      hasLocalInferenceCapability(
        initializeResponseWithMeta({
          kaji: {
            localInference: {},
          },
        })
      )
    ).toBe(true);
  });

  it('treats missing local inference metadata as unsupported', () => {
    expect(hasLocalInferenceCapability(initializeResponseWithMeta())).toBe(false);
    expect(hasLocalInferenceCapability(initializeResponseWithMeta({}))).toBe(false);
    expect(hasLocalInferenceCapability(initializeResponseWithMeta({ kaji: {} }))).toBe(false);
  });

  it('ignores malformed Kaji metadata', () => {
    expect(hasLocalInferenceCapability(initializeResponseWithMeta({ kaji: true }))).toBe(false);
    expect(hasLocalInferenceCapability(initializeResponseWithMeta({ kaji: null }))).toBe(false);
  });
});
