#!/usr/bin/env node
//
// Hand-rolled npm publishing for sigil — esbuild-style optionalDependencies.
//
// Inputs (env):
//   VERSION         Required. The release version, e.g. "0.4.1".
//   ARTIFACTS_DIR   Required. Directory containing cargo-dist's per-target
//                   archives (e.g. sigil-aarch64-apple-darwin.tar.gz).
//   STAGE_DIR       Optional. Working dir for staging packages.
//                   Default: ./npm-stage
//   DRY_RUN         Optional. If "1", prepares packages but skips publish.
//
// What this does:
//   1. For each target in TARGETS, extract the binary from the archive
//      and stage a thin npm package: @knova-run/sigil-<platform>.
//      package.json carries os/cpu (and libc on linux) so npm only
//      installs the matching one for each user.
//   2. Stage and publish the wrapper @knova-run/sigil. It carries no
//      binary, only:
//        - bin/sigil.js — a small shim that resolves the matching
//          platform package and exec's the binary inside it.
//        - optionalDependencies pinning each platform package at VERSION.
//
// Auth: relies on whatever the calling environment provides (GitHub
// Actions OIDC trusted publishing, or NODE_AUTH_TOKEN in .npmrc).
//

import { execFileSync } from 'node:child_process';
import {
    chmodSync,
    copyFileSync,
    cpSync,
    existsSync,
    mkdirSync,
    readFileSync,
    rmSync,
    writeFileSync,
} from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, '..');

const SCOPE = '@knova-run';
const WRAPPER = `${SCOPE}/sigil`;

const VERSION = required('VERSION');
const ARTIFACTS_DIR = resolve(required('ARTIFACTS_DIR'));
const STAGE_DIR = resolve(process.env.STAGE_DIR || join(REPO_ROOT, 'npm-stage'));
const DRY_RUN = process.env.DRY_RUN === '1';

// Each target: which cargo-dist archive, which platform metadata,
// and the npm package suffix. node `process.platform` / `process.arch`
// is what the shim matches at runtime.
//
// `libc` is only set on linux; npm's optional-dependency resolver
// honours it (npm >= 10), so musl users won't accidentally install
// a glibc binary — they'll just get an explicit "no platform" error
// from the shim.
const TARGETS = [
    {
        triple: 'aarch64-apple-darwin',
        suffix: 'darwin-arm64',
        os: 'darwin',
        cpu: 'arm64',
        binName: 'sigil',
        archiveExt: '.tar.gz',
    },
    // `x86_64-apple-darwin` (darwin-x64) was dropped in 0.6.2 — `dist`
    // no longer builds the archive. Intel-only Mac users without Rosetta
    // 2 need `cargo install sigil` or `brew install` instead of `npx`.
    {
        triple: 'aarch64-unknown-linux-gnu',
        suffix: 'linux-arm64-gnu',
        os: 'linux',
        cpu: 'arm64',
        libc: 'glibc',
        binName: 'sigil',
        archiveExt: '.tar.gz',
    },
    {
        triple: 'x86_64-unknown-linux-gnu',
        suffix: 'linux-x64-gnu',
        os: 'linux',
        cpu: 'x64',
        libc: 'glibc',
        binName: 'sigil',
        archiveExt: '.tar.gz',
    },
    {
        triple: 'x86_64-pc-windows-msvc',
        suffix: 'win32-x64-msvc',
        os: 'win32',
        cpu: 'x64',
        binName: 'sigil.exe',
        archiveExt: '.zip',
    },
];

function required(name) {
    const v = process.env[name];
    if (!v) {
        throw new Error(`Missing required env var: ${name}`);
    }
    return v;
}

function run(cmd, args, opts = {}) {
    process.stdout.write(`$ ${cmd} ${args.join(' ')}\n`);
    execFileSync(cmd, args, { stdio: 'inherit', ...opts });
}

function extractArchive(archive, into) {
    mkdirSync(into, { recursive: true });
    if (archive.endsWith('.tar.gz')) {
        run('tar', ['xzf', archive, '-C', into]);
    } else if (archive.endsWith('.zip')) {
        run('unzip', ['-q', archive, '-d', into]);
    } else {
        throw new Error(`Unknown archive format: ${archive}`);
    }
}

const COMMON_PKG = {
    version: VERSION,
    homepage: 'https://github.com/knova-run/sigil',
    repository: { type: 'git', url: 'git+https://github.com/knova-run/sigil.git' },
    bugs: { url: 'https://github.com/knova-run/sigil/issues' },
    license: 'Apache-2.0',
    author: 'knova-run',
};

function stagePlatformPackage(target) {
    const archiveName = `sigil-${target.triple}${target.archiveExt}`;
    const archivePath = join(ARTIFACTS_DIR, archiveName);
    if (!existsSync(archivePath)) {
        throw new Error(`Missing archive for ${target.triple}: ${archivePath}`);
    }

    const extractDir = join(STAGE_DIR, `extracted-${target.triple}`);
    rmSync(extractDir, { recursive: true, force: true });
    extractArchive(archivePath, extractDir);

    // cargo-dist's Linux/macOS tarballs unpack into a top-level
    // `sigil-<triple>/` directory, but the Windows .zip is flat —
    // files (including sigil.exe) sit at the archive root.
    const innerDir = target.archiveExt === '.tar.gz'
        ? join(extractDir, `sigil-${target.triple}`)
        : extractDir;
    const binSrc = join(innerDir, target.binName);
    if (!existsSync(binSrc)) {
        throw new Error(`Binary not found inside archive: ${binSrc}`);
    }

    const pkgDir = join(STAGE_DIR, `pkg-${target.triple}`);
    rmSync(pkgDir, { recursive: true, force: true });
    mkdirSync(pkgDir, { recursive: true });

    copyFileSync(binSrc, join(pkgDir, target.binName));
    chmodSync(join(pkgDir, target.binName), 0o755);

    const pkgJson = {
        name: `${SCOPE}/sigil-${target.suffix}`,
        ...COMMON_PKG,
        description: `sigil prebuilt binary for ${target.triple}. Installed automatically as an optional dependency of ${WRAPPER}.`,
        os: [target.os],
        cpu: [target.cpu],
        ...(target.libc ? { libc: [target.libc] } : {}),
        files: [target.binName, 'README.md'],
    };
    writeFileSync(
        join(pkgDir, 'package.json'),
        JSON.stringify(pkgJson, null, 2) + '\n',
    );
    writeFileSync(
        join(pkgDir, 'README.md'),
        `# ${pkgJson.name}\n\nPrebuilt \`sigil\` binary for \`${target.triple}\`.\n\nThis package is an internal optional dependency of [\`${WRAPPER}\`](https://www.npmjs.com/package/${WRAPPER}).\nDo not install it directly — install the wrapper instead.\n`,
    );

    return { pkgDir, pkgJson };
}

function stageWrapper() {
    const pkgDir = join(STAGE_DIR, 'pkg-wrapper');
    rmSync(pkgDir, { recursive: true, force: true });
    mkdirSync(join(pkgDir, 'bin'), { recursive: true });

    // Pin each platform package to the exact same version as the wrapper.
    const optionalDependencies = Object.fromEntries(
        TARGETS.map(t => [`${SCOPE}/sigil-${t.suffix}`, VERSION]),
    );

    const pkgJson = {
        name: WRAPPER,
        ...COMMON_PKG,
        description: 'Structural code intelligence for AI coding agents — sigil CLI.',
        bin: { sigil: 'bin/sigil.js' },
        files: ['bin/sigil.js', 'README.md', 'LICENSE'],
        optionalDependencies,
        engines: { node: '>=18' },
    };
    writeFileSync(
        join(pkgDir, 'package.json'),
        JSON.stringify(pkgJson, null, 2) + '\n',
    );

    // Map of `<process.platform>-<process.arch>` (and -<libc> on linux) → platform package.
    const PLATFORM_MAP = TARGETS.reduce((acc, t) => {
        const pkgName = `${SCOPE}/sigil-${t.suffix}`;
        const baseKey = `${t.os}-${t.cpu}`;
        if (t.libc) {
            acc[`${baseKey}-${t.libc}`] = { pkg: pkgName, bin: t.binName };
        } else {
            acc[baseKey] = { pkg: pkgName, bin: t.binName };
        }
        return acc;
    }, {});

    const shim = `#!/usr/bin/env node
'use strict';

// Hand-rolled platform-binary shim for ${WRAPPER}. Resolves the
// matching @knova-run/sigil-<platform> optional dependency and exec's
// the binary inside it. No network access, no postinstall.

const { execFileSync } = require('node:child_process');
const path = require('node:path');

const PLATFORMS = ${JSON.stringify(PLATFORM_MAP, null, 4)};

function detectLibc() {
    if (process.platform !== 'linux') return null;
    try {
        const report = process.report && process.report.getReport && process.report.getReport();
        if (report && report.header && report.header.glibcVersionRuntime) return 'glibc';
        return 'musl';
    } catch (_e) {
        return 'glibc';
    }
}

const libc = detectLibc();
const arch = process.arch;
const platform = process.platform;
const key = libc ? \`\${platform}-\${arch}-\${libc}\` : \`\${platform}-\${arch}\`;
const target = PLATFORMS[key];

if (!target) {
    console.error(\`sigil: no prebuilt binary for \${platform}-\${arch}\${libc ? '-' + libc : ''}.\`);
    console.error('Supported: ' + Object.keys(PLATFORMS).join(', '));
    process.exit(1);
}

let pkgRoot;
try {
    pkgRoot = path.dirname(require.resolve(\`\${target.pkg}/package.json\`));
} catch (_e) {
    console.error(\`sigil: \${target.pkg} is not installed.\`);
    console.error('This usually means npm skipped optional dependencies.');
    console.error('Reinstall with:  npm install --include=optional ${WRAPPER}');
    process.exit(1);
}

const binPath = path.join(pkgRoot, target.bin);

try {
    execFileSync(binPath, process.argv.slice(2), { stdio: 'inherit' });
} catch (err) {
    if (err && typeof err.status === 'number') process.exit(err.status);
    if (err && err.code === 'ENOENT') {
        console.error(\`sigil: binary missing at \${binPath}\`);
        process.exit(1);
    }
    throw err;
}
`;

    writeFileSync(join(pkgDir, 'bin', 'sigil.js'), shim);
    chmodSync(join(pkgDir, 'bin', 'sigil.js'), 0o755);

    // Bring in the top-level README and LICENSE so the npm page is useful.
    const wrapperReadme = `# sigil

Structural code intelligence for AI coding agents.

\`\`\`bash
npx ${WRAPPER} --help
npm install -g ${WRAPPER}    # then run \`sigil\` from anywhere
\`\`\`

This package is a thin wrapper. The actual binary is delivered as one
of these platform-specific optional dependencies (npm picks the
matching one for your machine):

${TARGETS.map(t => `- \`${SCOPE}/sigil-${t.suffix}\` — ${t.triple}`).join('\n')}

Full docs: https://github.com/knova-run/sigil
`;
    writeFileSync(join(pkgDir, 'README.md'), wrapperReadme);
    if (existsSync(join(REPO_ROOT, 'LICENSE'))) {
        copyFileSync(join(REPO_ROOT, 'LICENSE'), join(pkgDir, 'LICENSE'));
    }

    return { pkgDir, pkgJson };
}

function publish(pkgDir) {
    const args = ['publish', '--access', 'public', '--provenance'];
    if (DRY_RUN) args.push('--dry-run');
    run('npm', args, { cwd: pkgDir });
}

function main() {
    rmSync(STAGE_DIR, { recursive: true, force: true });
    mkdirSync(STAGE_DIR, { recursive: true });

    const platformPkgs = TARGETS.map(t => {
        const { pkgDir, pkgJson } = stagePlatformPackage(t);
        return { target: t, pkgDir, pkgJson };
    });

    // Publish platform packages first so the wrapper's optionalDependencies
    // resolve cleanly (npm >= 10 doesn't strictly require this, but it
    // avoids an install-time race where users grab the wrapper before
    // the platform tarballs are visible in the registry).
    for (const p of platformPkgs) {
        publish(p.pkgDir);
    }

    const wrapper = stageWrapper();
    publish(wrapper.pkgDir);

    process.stdout.write(`\n✓ Published ${WRAPPER}@${VERSION} + ${platformPkgs.length} platform packages.\n`);
}

main();
