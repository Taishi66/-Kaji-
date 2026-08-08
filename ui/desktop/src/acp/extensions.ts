import type { ExtensionConfig, ExtensionEntry } from '../types/extensions';
import type { KajiExtension, KajiExtensionEntry } from '@aaif/kaji-sdk';
import { getAcpClient } from './acpConnection';

export type ConfiguredExtensionEntry = ExtensionEntry & { configKey?: string };

export interface ConfiguredExtensionsResponse {
  extensions: ConfiguredExtensionEntry[];
  warnings: string[];
}

export function kajiExtensionName(extension: KajiExtension): string {
  return extension.type === 'mcp' ? extension.server.name : extension.name;
}

function headersToRecord(headers: { name: string; value: string }[] = []) {
  return Object.fromEntries(headers.map(({ name, value }) => [name, value]));
}

function availableToolsOrUndefined(availableTools?: string[] | null): string[] | undefined {
  return availableTools?.length ? availableTools : undefined;
}

export function kajiExtensionToExtensionConfig(extension: KajiExtension): ExtensionConfig | null {
  switch (extension.type) {
    case 'builtin':
    case 'platform':
      return {
        ...extension,
        description: extension.description ?? '',
        available_tools: availableToolsOrUndefined(extension.available_tools),
      };
    case 'mcp': {
      const server = extension.server;
      if ('command' in server) {
        return {
          type: 'stdio',
          name: server.name,
          description: extension.description ?? '',
          cmd: server.command,
          args: server.args,
          env_keys: extension.envKeys ?? [],
          timeout: extension.timeout,
          bundled: extension.bundled,
          available_tools: availableToolsOrUndefined(extension.available_tools),
        };
      }
      if ('url' in server) {
        return {
          type: 'streamable_http',
          name: server.name,
          description: extension.description ?? '',
          uri: server.url,
          headers: headersToRecord(server.headers),
          env_keys: extension.envKeys ?? [],
          timeout: extension.timeout,
          socket: extension.socket,
          bundled: extension.bundled,
          available_tools: availableToolsOrUndefined(extension.available_tools),
        };
      }
      return null;
    }
  }
}

function kajiExtensionEntryToExtensionEntry(
  entry: KajiExtensionEntry
): ConfiguredExtensionEntry | null {
  const config = kajiExtensionToExtensionConfig(entry.extension);
  if (!config) {
    return null;
  }
  return { ...config, enabled: entry.enabled, configKey: entry.configKey ?? undefined };
}

export async function getConfiguredKajiExtensions(): Promise<KajiExtensionEntry[]> {
  const client = await getAcpClient();
  const response = await client.kaji.configExtensionsList_unstable({});
  return response.extensions;
}

export async function getConfiguredExtensions(): Promise<ConfiguredExtensionsResponse> {
  const client = await getAcpClient();
  const response = await client.kaji.configExtensionsList_unstable({});
  return {
    extensions: response.extensions
      .map(kajiExtensionEntryToExtensionEntry)
      .filter((entry): entry is ConfiguredExtensionEntry => entry !== null),
    warnings: response.warnings ?? [],
  };
}

export function extensionConfigToKajiExtension(config: ExtensionConfig): KajiExtension | null {
  switch (config.type) {
    case 'builtin':
      return {
        type: 'builtin',
        name: config.name,
        description: config.description,
        display_name: config.display_name,
        timeout: config.timeout,
        bundled: config.bundled,
        available_tools: availableToolsOrUndefined(config.available_tools),
      };
    case 'platform':
      return {
        type: 'platform',
        name: config.name,
        description: config.description,
        display_name: config.display_name,
        bundled: config.bundled,
        available_tools: availableToolsOrUndefined(config.available_tools),
      };
    case 'stdio':
      return {
        type: 'mcp',
        server: { name: config.name, command: config.cmd, args: config.args ?? [], env: [] },
        envKeys: config.env_keys ?? [],
        description: config.description,
        timeout: config.timeout,
        bundled: config.bundled,
        available_tools: availableToolsOrUndefined(config.available_tools),
      };
    case 'streamable_http':
      return {
        type: 'mcp',
        server: {
          type: 'http',
          name: config.name,
          url: config.uri,
          headers: Object.entries(config.headers ?? {}).map(([name, value]) => ({ name, value })),
        },
        envKeys: config.env_keys ?? [],
        description: config.description,
        timeout: config.timeout,
        socket: config.socket,
        bundled: config.bundled,
        available_tools: availableToolsOrUndefined(config.available_tools),
      };
    case 'sse':
    case 'frontend':
    case 'inline_python':
      return null;
  }
}

export async function addConfigExtension(config: ExtensionConfig, enabled: boolean): Promise<void> {
  const extension = extensionConfigToKajiExtension(config);
  if (!extension) {
    throw new Error(`Unsupported extension type for ACP: ${config.type}`);
  }
  const client = await getAcpClient();
  await client.kaji.configExtensionsAdd_unstable({ extension, enabled });
}

export async function removeConfigExtension(configKey: string): Promise<void> {
  const client = await getAcpClient();
  await client.kaji.configExtensionsRemove_unstable({ configKey });
}

export async function setConfigExtensionEnabled(
  configKey: string,
  enabled: boolean
): Promise<void> {
  const client = await getAcpClient();
  await client.kaji.configExtensionsSetEnabled_unstable({ configKey, enabled });
}
