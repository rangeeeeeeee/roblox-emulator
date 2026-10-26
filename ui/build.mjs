import {build} from 'esbuild';
import {readFile, writeFile} from 'node:fs/promises';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

const root = path.dirname(fileURLToPath(import.meta.url));
const result = await build({absWorkingDir: root, entryPoints: ['editor.js'], bundle: true,
  format: 'iife', globalName: 'EmulatorEditor', target: 'es2022', minify: true,
  legalComments: 'inline', metafile: true, write: false});
const packages = new Set(Object.keys(result.metafile.inputs).filter(name => name.startsWith('node_modules/'))
  .map(name => name.split('/').slice(0, name.startsWith('node_modules/@') ? 3 : 2).join('/')));
const licenses = [];
for (const folder of [...packages].sort()) {
  const manifest = JSON.parse(await readFile(path.join(root, folder, 'package.json'), 'utf8'));
  let license;
  for (const name of ['LICENSE', 'LICENSE.txt', 'LICENSE.md', 'license']) {
    try { license = await readFile(path.join(root, folder, name), 'utf8'); break; } catch {}
  }
  if (!license) throw new Error(`Missing license for ${manifest.name}`);
  licenses.push(`${manifest.name} ${manifest.version}\n${license}`);
}
await writeFile(path.join(root, 'editor.bundle.js'), '/*!\n' + licenses.join('\n\n').replaceAll('*/', '* /') + '\n*/\n' + result.outputFiles[0].text);
console.log('Built ui/editor.bundle.js');
