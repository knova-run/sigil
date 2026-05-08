#!/usr/bin/env node
//
// One-time bootstrap: claim the @knova-run/sigil-<platform> names on npm
// by publishing 0.0.0 stub packages, so trusted publishers can be
// configured per package before the first real release.
//
// Run locally (npm CLI must be logged in):
//
//   node scripts/bootstrap-npm-stubs.mjs        # stage + publish all 5
//   STAGE_ONLY=1 node scripts/bootstrap-npm-stubs.mjs   # just stage
//
// After this lands, go to
//   https://www.npmjs.com/package/@knova-run/sigil-<platform>/access
// for each, and add the GitHub Actions trusted publisher (repo
// knova-run/sigil, workflow release.yml). Then the next tagged release
// can publish without an NPM_TOKEN.
//

import { execFileSync } from 'node:child_process';
import { mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, '..');
const STAGE_DIR = resolve(REPO_ROOT, 'npm-stub-stage');
const STAGE_ONLY = process.env.STAGE_ONLY === '1';

const SCOPE = '@knova-run';
const STUBS = [
    { suffix: 'darwin-arm64',     os: 'darwin', cpu: 'arm64' },
    { suffix: 'darwin-x64',       os: 'darwin', cpu: 'x64' },
    { suffix: 'linux-arm64-gnu',  os: 'linux',  cpu: 'arm64', libc: 'glibc' },
    { suffix: 'linux-x64-gnu',    os: 'linux',  cpu: 'x64',   libc: 'glibc' },
    { suffix: 'win32-x64-msvc',   os: 'win32',  cpu: 'x64' },
];

rmSync(STAGE_DIR, { recursive: true, force: true });

for (const s of STUBS) {
    const name = `${SCOPE}/sigil-${s.suffix}`;
    const dir = join(STAGE_DIR, s.suffix);
    mkdirSync(dir, { recursive: true });

    const pkg = {
        name,
        version: '0.0.0',
        description: `Placeholder reserving the npm name. The real prebuilt sigil binary for ${s.suffix} ships from the next tagged release of knova-run/sigil.`,
        homepage: 'https://github.com/knova-run/sigil',
        repository: { type: 'git', url: 'git+https://github.com/knova-run/sigil.git' },
        bugs: { url: 'https://github.com/knova-run/sigil/issues' },
        license: 'Apache-2.0',
        author: 'knova-run',
        os: [s.os],
        cpu: [s.cpu],
        ...(s.libc ? { libc: [s.libc] } : {}),
        files: ['README.md'],
    };
    writeFileSync(join(dir, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
    writeFileSync(
        join(dir, 'README.md'),
        `# ${name}\n\nPlaceholder reserving the npm name. The real prebuilt sigil binary for \`${s.suffix}\` ships from the next tagged release of [knova-run/sigil](https://github.com/knova-run/sigil).\n`,
    );

    process.stdout.write(`staged ${name} -> ${dir}\n`);

    if (!STAGE_ONLY) {
        execFileSync('npm', ['publish', '--access', 'public'], {
            cwd: dir,
            stdio: 'inherit',
        });
    }
}

if (STAGE_ONLY) {
    process.stdout.write(`\nStaged 5 packages under ${STAGE_DIR}.\n`);
    process.stdout.write(`Publish each with:  for d in ${STAGE_DIR}/*/; do (cd "$d" && npm publish --access public); done\n`);
} else {
    process.stdout.write(`\n✓ Published 5 platform stub packages under ${SCOPE}/sigil-*.\n`);
    process.stdout.write(`Next: configure trusted publishers for each at\n`);
    for (const s of STUBS) {
        process.stdout.write(`  https://www.npmjs.com/package/${SCOPE}/sigil-${s.suffix}/access\n`);
    }
}
