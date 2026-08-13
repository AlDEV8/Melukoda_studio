#!/usr/bin/env node
/**
 * Fetches a pinned, statically packaged FFmpeg encoder for a release build.
 * It is deliberately separate from normal development builds: a release must
 * not silently pick up a random FFmpeg from PATH.
 */
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { cp, mkdtemp, mkdir, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const appRoot = resolve(here, '..');
const resourceDir = join(appRoot, 'src-tauri', 'resources');
const releases = {
  'linux-x64': {
    url: 'https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-08-02-13-17/ffmpeg-n8.1.2-34-g9b6c8969e0-linux64-lgpl-8.1.tar.xz',
    sha256: '74425e4f2cc73341777abae35d761470f30ba95878fee729fde67ffcf122da35',
    archive: 'tar.xz',
    binary: 'ffmpeg',
    notice: 'BtbN FFmpeg Builds, FFmpeg n8.1.2-34-g9b6c8969e0 (Linux x64 LGPL variant).\nSource and licence information: https://github.com/BtbN/FFmpeg-Builds\n',
  },
  'windows-x64': {
    url: 'https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-08-02-13-17/ffmpeg-n8.1.2-34-g9b6c8969e0-win64-lgpl-8.1.zip',
    sha256: '1c17a2af80ca4f85e3e72a1137eb4645f8a88c2e2d754339e270b1f234f8d49c',
    archive: 'zip',
    binary: 'ffmpeg.exe',
    notice: 'BtbN FFmpeg Builds, FFmpeg n8.1.2-34-g9b6c8969e0 (Windows x64 LGPL variant).\nSource and licence information: https://github.com/BtbN/FFmpeg-Builds\n',
  },
};

function currentTarget() {
  if (process.platform === 'linux' && process.arch === 'x64') return 'linux-x64';
  if (process.platform === 'win32' && process.arch === 'x64') return 'windows-x64';
  return '';
}

async function findFile(directory, filename) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const candidate = join(directory, entry.name);
    if (entry.isFile() && entry.name === filename) return candidate;
    if (entry.isDirectory()) {
      const found = await findFile(candidate, filename);
      if (found) return found;
    }
  }
  return null;
}

const target = process.env.MELUKODA_RELEASE_TARGET || currentTarget();
const release = releases[target];
if (!release) {
  throw new Error('No pinned prebuilt encoder is defined for this platform. macOS releases use scripts/build-macos-ffmpeg.sh before bundling.');
}

const temp = await mkdtemp(join(tmpdir(), 'melukoda-ffmpeg-'));
try {
  const archive = join(temp, basename(new URL(release.url).pathname));
  const response = await fetch(release.url);
  if (!response.ok) throw new Error(`Encoder download failed with HTTP ${response.status}.`);
  const bytes = Buffer.from(await response.arrayBuffer());
  const digest = createHash('sha256').update(bytes).digest('hex');
  if (digest !== release.sha256) throw new Error(`Encoder checksum mismatch: expected ${release.sha256}, received ${digest}.`);
  await writeFile(archive, bytes);
  const unpacked = join(temp, 'unpacked');
  await mkdir(unpacked);
  if (release.archive === 'zip') {
    execFileSync('powershell', ['-NoProfile', '-NonInteractive', '-Command', `Expand-Archive -LiteralPath '${archive}' -DestinationPath '${unpacked}'`], { stdio: 'inherit' });
  } else {
    execFileSync('tar', ['-xJf', archive, '-C', unpacked], { stdio: 'inherit' });
  }
  const source = await findFile(unpacked, release.binary);
  if (!source) throw new Error(`Downloaded archive did not contain ${release.binary}.`);
  await mkdir(resourceDir, { recursive: true });
  await cp(source, join(resourceDir, 'ffmpeg'));
  if (process.platform !== 'win32') execFileSync('chmod', ['755', join(resourceDir, 'ffmpeg')]);
  await writeFile(join(resourceDir, 'FFMPEG-NOTICE.txt'), release.notice);
  console.log(`Prepared verified ${target} encoder at ${join(resourceDir, 'ffmpeg')}`);
} finally {
  await rm(temp, { recursive: true, force: true });
}
