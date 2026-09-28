/**
 * Copy the Lattice Grid files the wall loads from `node_modules` into
 * `src/lattice/`, so the app ships them in its bundle and fetches nothing at
 * runtime. `src/lattice/` is generated: it is gitignored and rebuilt by
 * `npm install` (the `prepare` script), `npm run dev` and `npm run build`.
 *
 * The list is exactly what `src/index.html` loads, plus the grid's LICENSE.
 */
import { copyFileSync, mkdirSync, rmSync, existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const from = join(root, 'node_modules', '@toclocoinc', 'lattice-grid');
const to = join(root, 'src', 'lattice');

const FILES = [
  'lattice-grid.min.js',
  'lattice-grid.min.css',
  'LICENSE',
  'modules/layout.min.js',
  'modules/data-router.min.js',
  'modules/charts.min.js',
  'modules/chart-markermap.min.js',
  'modules/geo-world-110m.min.js',
  'modules/kpi.min.js',
  'modules/alarms.min.js',
];

if (!existsSync(join(from, 'package.json'))) {
  console.error('[vendor-grid] @toclocoinc/lattice-grid is not installed; run `npm install` first.');
  process.exit(1);
}
const { version } = JSON.parse(readFileSync(join(from, 'package.json'), 'utf8'));
rmSync(to, { recursive: true, force: true });
for (const file of FILES) {
  mkdirSync(dirname(join(to, file)), { recursive: true });
  copyFileSync(join(from, file), join(to, file));
}
console.log(`[vendor-grid] Lattice Grid ${version}: ${FILES.length} files copied into src/lattice/.`);
