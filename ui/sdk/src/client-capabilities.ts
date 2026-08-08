import type { KajiMcpHostCapabilities } from "./mcp-apps.js";

export interface KajiClientCapabilitiesMeta {
  kaji?: {
    mcpHostCapabilities?: KajiMcpHostCapabilities;
    customNotifications?: boolean;
  };
}
