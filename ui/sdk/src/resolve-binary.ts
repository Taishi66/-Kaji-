import { createRequire } from "node:module";
import { dirname, join } from "node:path";

const PLATFORMS: Record<string, string> = {
  "darwin-arm64": "@aaif/kaji-binary-darwin-arm64",
  "darwin-x64": "@aaif/kaji-binary-darwin-x64",
  "linux-arm64": "@aaif/kaji-binary-linux-arm64",
  "linux-x64": "@aaif/kaji-binary-linux-x64",
  "win32-x64": "@aaif/kaji-binary-win32-x64",
};

/**
 * Resolves the path to the kaji binary.
 *
 * Resolution order:
 *   1. `KAJI_BINARY` environment variable (explicit override)
 *   2. Platform-specific `@aaif/kaji-binary-*` optional dependency
 *
 * @throws if no binary can be found
 */
export function resolveKajiBinary(): string {
  const envBinary = process.env.KAJI_BINARY;
  if (envBinary) return envBinary;

  const key = `${process.platform}-${process.arch}`;
  const pkg = PLATFORMS[key];
  if (!pkg) {
    throw new Error(
      `No kaji binary available for ${key}. Set KAJI_BINARY to the path of a kaji binary.`,
    );
  }

  try {
    const require = createRequire(import.meta.url);
    const pkgDir = dirname(require.resolve(`${pkg}/package.json`));
    const binName = process.platform === "win32" ? "kaji.exe" : "kaji";
    return join(pkgDir, "bin", binName);
  } catch {
    throw new Error(
      `kaji binary package ${pkg} is not installed. Set KAJI_BINARY or install the native package.`,
    );
  }
}
