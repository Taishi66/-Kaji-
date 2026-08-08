import { beforeEach, describe, expect, it, vi } from 'vitest';
import { createSession } from '../sessions';
import type { ExtensionConfig } from '../types/extensions';
import type { Session } from '../types/session';
import type { FixedExtensionEntry } from '../components/ConfigContext';
import type { KajiExtension, KajiExtensionEntry } from '@aaif/kaji-sdk';
import { getConfiguredKajiExtensions } from '../acp/extensions';
import { acpChatSessionController } from '../acp/chatSessionController';

vi.mock('../acp/extensions', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../acp/extensions')>();
  return {
    ...actual,
    getConfiguredKajiExtensions: vi.fn(),
  };
});

vi.mock('../acp/chatSessionController', () => ({
  acpChatSessionController: {
    createSession: vi.fn(),
  },
}));

const testSession: Session = {
  id: 'session-1',
  name: 'untitled',
  message_count: 0,
  created_at: '2026-06-19T00:00:00.000Z',
  updated_at: '2026-06-19T00:00:00.000Z',
  working_dir: '/tmp',
  extension_data: { active: [], installed: [] },
};

const extensionConfig = (name: string): ExtensionConfig => ({
  name,
  type: 'builtin',
  description: `${name} extension`,
});

const configuredExtension = (name: string, enabled: boolean): FixedExtensionEntry => ({
  ...extensionConfig(name),
  enabled,
});

const kajiExtension = (name: string): KajiExtension => ({
  type: 'builtin',
  name,
  description: `${name} extension`,
});

const kajiExtensionEntry = (name: string): KajiExtensionEntry => ({
  extension: kajiExtension(name),
  enabled: true,
});

const mockedGetConfiguredKajiExtensions = vi.mocked(getConfiguredKajiExtensions);
const mockedCreateAcpSession = vi.mocked(acpChatSessionController.createSession);

describe('createSession ACP session extensions', () => {
  beforeEach(() => {
    mockedGetConfiguredKajiExtensions.mockReset();
    mockedGetConfiguredKajiExtensions.mockResolvedValue([
      kajiExtensionEntry('developer'),
      kajiExtensionEntry('memory'),
    ]);
    mockedCreateAcpSession.mockReset();
    mockedCreateAcpSession.mockResolvedValue(testSession);
  });

  it('sends non-empty extension configs as ACP session extensions', async () => {
    await createSession('/tmp', {
      extensionConfigs: [extensionConfig('developer')],
    });

    expect(mockedGetConfiguredKajiExtensions).toHaveBeenCalledOnce();
    expect(mockedCreateAcpSession).toHaveBeenCalledWith('/tmp', [kajiExtension('developer')], {
      recipeDeeplink: undefined,
      recipeId: undefined,
    });
  });

  it('falls back to enabled configured extensions when extension configs are empty', async () => {
    await createSession('/tmp', {
      extensionConfigs: [],
      allExtensions: [configuredExtension('developer', true), configuredExtension('memory', false)],
    });

    expect(mockedGetConfiguredKajiExtensions).toHaveBeenCalledOnce();
    expect(mockedCreateAcpSession).toHaveBeenCalledWith('/tmp', [kajiExtension('developer')], {
      recipeDeeplink: undefined,
      recipeId: undefined,
    });
  });

  it('omits ACP session extensions when no configured extensions are enabled', async () => {
    await createSession('/tmp', {
      allExtensions: [configuredExtension('developer', false)],
    });

    expect(mockedGetConfiguredKajiExtensions).not.toHaveBeenCalled();
    expect(mockedCreateAcpSession).toHaveBeenCalledWith('/tmp', [], {
      recipeDeeplink: undefined,
      recipeId: undefined,
    });
  });
});
