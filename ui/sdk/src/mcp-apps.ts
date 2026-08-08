import { RESOURCE_MIME_TYPE } from "@modelcontextprotocol/ext-apps/app-bridge";
import type {
  McpUiAppResourceConfig,
  McpUiAppToolConfig,
} from "@modelcontextprotocol/ext-apps/server";
import type {
  BlobResourceContents,
  ReadResourceResult,
  TextResourceContents,
  Tool,
} from "@modelcontextprotocol/sdk/types.js";

export const KAJI_MCP_UI_EXTENSION_ID = "io.modelcontextprotocol/ui" as const;

export interface KajiMcpUiExtensionSettings {
  mimeTypes: string[];
}

export interface KajiMcpHostCapabilities {
  extensions: Record<string, KajiMcpUiExtensionSettings>;
}

export type KajiToolUiMetadata = Extract<
  McpUiAppToolConfig["_meta"],
  { ui: unknown }
>["ui"];

export type KajiToolMetadata = NonNullable<Tool["_meta"]> & {
  ui?: KajiToolUiMetadata;
  kaji_extension?: string;
};

export type KajiSessionTool = Tool & {
  meta?: KajiToolMetadata;
  _meta?: KajiToolMetadata;
};

export type KajiTextResourceContents = TextResourceContents;

export type KajiBlobResourceContents = BlobResourceContents;

export type KajiResourceContents = TextResourceContents | BlobResourceContents;

export type KajiReadResourceResult = ReadResourceResult;

export type KajiResourceMetadata = NonNullable<
  Extract<NonNullable<McpUiAppResourceConfig["_meta"]>, { ui?: unknown }>["ui"]
>;

export interface KajiMcpAppToolPayload {
  toolName: string;
  extensionName: string;
  resourceUri: string;
  toolMeta?: KajiToolMetadata;
  resourceResult?: KajiReadResourceResult | null;
  readError?: string;
}

export interface KajiToolCallUpdateMeta {
  kaji?: {
    mcpApp?: KajiMcpAppToolPayload;
    [key: string]: unknown;
  };
  [key: string]: unknown;
}

export const DEFAULT_KAJI_MCP_HOST_CAPABILITIES: KajiMcpHostCapabilities = {
  extensions: {
    [KAJI_MCP_UI_EXTENSION_ID]: {
      mimeTypes: [RESOURCE_MIME_TYPE],
    },
  },
};
