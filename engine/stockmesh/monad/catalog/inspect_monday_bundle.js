// Read-only inspection of a downloaded, public Monday web bundle. This does
// not make the bundle ABI an admitted XTXC execution interface.
const fs = require('fs');
const crypto = require('crypto');
const source = fs.readFileSync(process.argv[2], 'utf8');
const selected = new Set((process.argv[3] || '').split(',').filter(Boolean));
for (const variable of ['Ttn', 'Atn']) {
  const start = source.indexOf(`${variable}=[`);
  if (start < 0) throw new Error(`ABI variable absent: ${variable}`);
  let depth = 0;
  let quote = false;
  let escape = false;
  let end = -1;
  for (let index = start + variable.length + 1; index < source.length; index += 1) {
    const char = source[index];
    if (quote) {
      if (escape) escape = false;
      else if (char === '\\') escape = true;
      else if (char === '"') quote = false;
    } else if (char === '"') quote = true;
    else if (char === '[') depth += 1;
    else if (char === ']' && --depth === 0) { end = index; break; }
  }
  if (end < 0) throw new Error(`unterminated ABI: ${variable}`);
  const fragment = source.slice(start + variable.length + 1, end + 1);
  const abi = JSON.parse(fragment
    .replace(/([,{])([A-Za-z][A-Za-z0-9_]*):/g, '$1"$2":')
    .replace(/:!0/g, ':true').replace(/:!1/g, ':false'));
  const functions = abi.filter((entry) => entry.type === 'function').map((entry) => entry.name);
  const sha = crypto.createHash('sha256').update(JSON.stringify(abi)).digest('hex');
  process.stdout.write(`${variable} ${fragment.length} bytes sha256=${sha}: ${functions.join(', ')}\n`);
  if (selected.size) {
    for (const entry of abi.filter((entry) => selected.has(entry.name))) {
      process.stdout.write(`${variable} ${JSON.stringify(entry)}\n`);
    }
  }
}
