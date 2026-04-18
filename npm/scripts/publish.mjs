#!/usr/bin/env node
// Build and publish the @endevco/aube npm packages for a given tag.
//
// For each of the 5 release targets this:
//   1. downloads `aube-<tag>-<target>.{tar.gz,zip}` directly from the
//      public GitHub release asset URL (no API, no auth token),
//   2. extracts the three binaries (aube, aubr, aubx) into a staging
//      dir,
//   3. generates a platform-scoped package.json and publishes it as
//      `@endevco/aube-<os>-<arch>`.
// Then rewrites the root `npm/package.json` version and publishes
// `@endevco/aube` last, so the preinstall script can resolve every
// sub-package it might want to install.
//
// Env:
//   TAG           — release tag, with leading `v` (e.g. v1.0.0-beta.1)
//   REPO          — owner/repo for the release assets (optional;
//                   defaults to $GITHUB_REPOSITORY or `endevco/aube`)
//   NPM_TAG       — npm dist-tag (optional; defaults to `next` for
//                   pre-releases, `latest` otherwise)
//   DRY_RUN=1     — stage + `npm pack` but don't publish
//   SKIP_ROOT=1   — publish only the platform packages
//   SKIP_PLATFORMS=1 — publish only the root package

import { spawnSync } from 'node:child_process';
import { createWriteStream } from 'node:fs';
import { mkdirSync, readFileSync, writeFileSync, cpSync, rmSync, existsSync, chmodSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const npmDir = resolve(__dirname, '..');
const stageRoot = resolve(npmDir, '.stage');

const TARGETS = [
    { triple: 'aarch64-apple-darwin',       os: 'darwin', cpu: 'arm64', ext: '.tar.gz', exe: '' },
    { triple: 'x86_64-unknown-linux-gnu',   os: 'linux',  cpu: 'x64',   ext: '.tar.gz', exe: '' },
    { triple: 'aarch64-unknown-linux-gnu',  os: 'linux',  cpu: 'arm64', ext: '.tar.gz', exe: '' },
    { triple: 'x86_64-pc-windows-msvc',     os: 'win32',  cpu: 'x64',   ext: '.zip',    exe: '.exe' },
    { triple: 'aarch64-pc-windows-msvc',    os: 'win32',  cpu: 'arm64', ext: '.zip',    exe: '.exe' },
];

const BINS = ['aube', 'aubr', 'aubx'];

function run(cmd, args, opts = {}) {
    const result = spawnSync(cmd, args, { stdio: 'inherit', ...opts });
    if (result.status !== 0) {
        throw new Error(`${cmd} ${args.join(' ')} exited ${result.status}`);
    }
}

function assertEnv(name) {
    const v = process.env[name];
    if (!v) throw new Error(`${name} env var is required`);
    return v;
}

function versionFromTag(tag) {
    if (!tag.startsWith('v')) throw new Error(`TAG must start with 'v': ${tag}`);
    return tag.slice(1);
}

function defaultNpmTag(version) {
    return version.includes('-') ? 'next' : 'latest';
}

async function downloadArchive(repo, tag, target, outDir) {
    const archiveName = `aube-${tag}-${target.triple}${target.ext}`;
    const archivePath = resolve(outDir, archiveName);
    mkdirSync(outDir, { recursive: true });
    // Public release assets redirect from /releases/download/<tag>/<asset>
    // to a signed CDN URL — no auth, no API, fetch follows the redirect.
    const url = `https://github.com/${repo}/releases/download/${tag}/${archiveName}`;
    console.log(`[publish] downloading ${url}`);
    const res = await fetch(url, { redirect: 'follow' });
    if (!res.ok) throw new Error(`download ${url} failed: ${res.status} ${res.statusText}`);
    await pipeline(Readable.fromWeb(res.body), createWriteStream(archivePath));
    return archivePath;
}

function extractArchive(archivePath, target, destDir) {
    mkdirSync(destDir, { recursive: true });
    if (target.ext === '.tar.gz') {
        run('tar', ['-xzf', archivePath, '-C', destDir, '--strip-components=0']);
    } else {
        run('unzip', ['-o', archivePath, '-d', destDir]);
    }
    // taiki-e/upload-rust-binary-action packs each bin at the archive
    // root (no containing directory). Verify each expected binary lands
    // where we think, so a silent rename doesn't ship an empty package.
    for (const bin of BINS) {
        const binPath = resolve(destDir, bin + target.exe);
        if (!existsSync(binPath)) {
            throw new Error(`missing ${binPath} in extracted archive`);
        }
        if (target.os !== 'win32') chmodSync(binPath, 0o755);
    }
}

async function buildPlatformPackage(repo, tag, version, target) {
    const pkgName = `@endevco/aube-${target.os}-${target.cpu}`;
    const stageDir = resolve(stageRoot, `${target.os}-${target.cpu}`);
    rmSync(stageDir, { recursive: true, force: true });
    const binDir = resolve(stageDir, 'bin');
    mkdirSync(binDir, { recursive: true });

    const dlDir = resolve(stageRoot, '_downloads');
    const archivePath = await downloadArchive(repo, tag, target, dlDir);
    const extractDir = resolve(stageRoot, `_extract-${target.os}-${target.cpu}`);
    rmSync(extractDir, { recursive: true, force: true });
    extractArchive(archivePath, target, extractDir);

    const bins = {};
    for (const bin of BINS) {
        const src = resolve(extractDir, bin + target.exe);
        const destName = bin + target.exe;
        const dest = resolve(binDir, destName);
        cpSync(src, dest);
        if (target.os !== 'win32') chmodSync(dest, 0o755);
        bins[bin] = `bin/${destName}`;
    }

    const pkgJson = {
        name: pkgName,
        version,
        description: 'Platform binaries for aube — do not install directly, see @endevco/aube.',
        homepage: 'https://aube.en.dev',
        repository: { type: 'git', url: 'https://github.com/endevco/aube' },
        license: 'MIT',
        bin: bins,
        files: ['bin', 'README.md'],
        os: [target.os],
        cpu: [target.cpu],
    };
    writeFileSync(resolve(stageDir, 'package.json'), JSON.stringify(pkgJson, null, 2) + '\n');

    const rootReadme = resolve(npmDir, '..', 'README.md');
    cpSync(rootReadme, resolve(stageDir, 'README.md'));

    return { pkgName, stageDir };
}

function npmPublish(stageDir, npmTag, dryRun) {
    // `--provenance` is separate from Trusted Publishing: OIDC covers
    // the auth handshake, but the provenance attestation is only
    // attached when this flag is set explicitly.
    const args = ['publish', '--access', 'public', '--tag', npmTag, '--provenance'];
    if (dryRun) args.push('--dry-run');
    run('npm', args, { cwd: stageDir });
}

async function main() {
    const tag = assertEnv('TAG');
    const version = versionFromTag(tag);
    const repo = process.env.REPO || process.env.GITHUB_REPOSITORY || 'endevco/aube';
    const npmTag = process.env.NPM_TAG || defaultNpmTag(version);
    const dryRun = process.env.DRY_RUN === '1';
    const skipPlatforms = process.env.SKIP_PLATFORMS === '1';
    const skipRoot = process.env.SKIP_ROOT === '1';

    console.log(`[publish] repo=${repo} tag=${tag} version=${version} npmTag=${npmTag} dryRun=${dryRun}`);

    if (!skipPlatforms) {
        for (const target of TARGETS) {
            console.log(`\n[publish] --- ${target.os}-${target.cpu} (${target.triple}) ---`);
            const { pkgName, stageDir } = await buildPlatformPackage(repo, tag, version, target);
            console.log(`[publish] staged ${pkgName} at ${stageDir}`);
            npmPublish(stageDir, npmTag, dryRun);
        }
    }

    if (!skipRoot) {
        console.log(`\n[publish] --- @endevco/aube (root) ---`);
        const rootPkgPath = resolve(npmDir, 'package.json');
        const rootPkg = JSON.parse(readFileSync(rootPkgPath, 'utf8'));
        rootPkg.version = version;
        writeFileSync(rootPkgPath, JSON.stringify(rootPkg, null, 2) + '\n');
        // `npm pack` doesn't follow symlinks, so stage a real README
        // copy next to package.json rather than linking ../README.md.
        cpSync(resolve(npmDir, '..', 'README.md'), resolve(npmDir, 'README.md'));
        npmPublish(npmDir, npmTag, dryRun);
    }
}

main().catch((e) => {
    console.error(e);
    process.exit(1);
});
