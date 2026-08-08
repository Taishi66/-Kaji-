import os from 'node:os';
import path from 'node:path';
import { describe, expect, it } from 'vitest';
import { isAbsoluteKajiPath, resolveKajiPathRoot, sanitizeKajiPathRoot } from './pathUtils';

describe('resolveKajiPathRoot', () => {
  it('rejects empty and relative values', () => {
    expect(resolveKajiPathRoot(undefined)).toBeUndefined();
    expect(resolveKajiPathRoot('   ')).toBeUndefined();
    expect(resolveKajiPathRoot('relative/root')).toBeUndefined();
  });

  it('retains absolute paths without requiring them to exist', () => {
    const absolute = path.resolve('nonexistent-kaji-root');
    expect(resolveKajiPathRoot(`  ${absolute}  `)).toBe(absolute);
  });

  it('expands a home-relative root before validation', () => {
    expect(resolveKajiPathRoot('~')).toBe(os.homedir());
  });

  it('removes a rejected value from the child-process environment', () => {
    const env = { KAJI_PATH_ROOT: 'relative/root' };
    expect(sanitizeKajiPathRoot(env)).toBeUndefined();
    expect(env).not.toHaveProperty('KAJI_PATH_ROOT');
  });

  it('matches Rust absolute-path handling on Windows', () => {
    expect(isAbsoluteKajiPath('C:\\kaji\\root', 'win32')).toBe(true);
    expect(isAbsoluteKajiPath('\\\\server\\share\\kaji', 'win32')).toBe(true);
    expect(isAbsoluteKajiPath('C:kaji\\root', 'win32')).toBe(false);
    expect(isAbsoluteKajiPath('\\kaji\\root', 'win32')).toBe(false);
    expect(isAbsoluteKajiPath('/kaji/root', 'win32')).toBe(false);
  });
});
