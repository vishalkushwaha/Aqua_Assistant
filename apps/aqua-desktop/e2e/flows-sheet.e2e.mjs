// Live test of the spreadsheet step: real flows write and read real .xlsx and .csv files in Aqua's workspace folder,
// through the real path guard, and a second, independent library (openpyxl, via python) checks that the files are
// valid workbooks with the right contents. Also the refusals: a path outside the workspace, a workbook Aqua did not
// make, a column that is not there.
//
//   1. start aqua-core            (target\debug\aqua-core.exe from the repo root)
//   2. node e2e/flows-sheet.e2e.mjs        (needs python with openpyxl: pip install openpyxl)
//
// It works in ~/AquaWorkspace/flow_e2e_sheet and removes it (and its flows) at the end.

import { readFileSync, existsSync, rmSync, mkdirSync, writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import os from 'node:os';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..', '..');
const CORE = 'http://127.0.0.1:8767';
const uiToken = readFileSync(path.join(root, 'data', '.aqua_ui_token'), 'utf8').trim();
const work = path.join(os.homedir(), 'AquaWorkspace', 'flow_e2e_sheet');

const results = [];
const check = (name, ok, detail = '') => {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : `  -> ${detail}`}`);
};
const api = async (method, url, body) => {
  const r = await fetch(`${CORE}${url}`, { method, headers: { Authorization: `Bearer ${uiToken}`, 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: r.status, json: await r.json().catch(() => null) };
};
const until = async (what, fn, ms = 30000) => {
  const t = Date.now();
  while (Date.now() - t < ms) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`timed out: ${what}`);
};
const run = (id) => api('GET', `/api/v1/flows/runs/${id}`).then((r) => r.json);
const runFlow = async (def, inputs = {}) => {
  const made = await api('POST', '/api/v1/flows', def);
  if (!made.json?.id) throw new Error('create failed: ' + JSON.stringify(made.json));
  const started = await api('POST', `/api/v1/flows/${made.json.id}/run`, { inputs });
  if (!started.json?.run) throw new Error('start failed: ' + JSON.stringify(started.json));
  const id = started.json.run.id;
  await until('the run to finish', async () => ['succeeded', 'failed'].includes((await run(id)).run.status));
  return run(id);
};
const py = (code) => execFileSync('python', ['-c', code], { encoding: 'utf8' }).trim();
const rel = (f) => `flow_e2e_sheet/${f}`;

const before = new Set((await api('GET', '/api/v1/flows')).json.flows.map((f) => f.id));
const created = async () => (await api('GET', '/api/v1/flows')).json.flows.filter((f) => !before.has(f.id));
rmSync(work, { recursive: true, force: true });

const node = (id, kind, name, config) => ({ id, kind, name, config });
const trig = node('start', 'trigger', 'Start', { trigger_type: 'manual' });
const chain = (nodes) => nodes.slice(1).map((n, i) => ({ from: nodes[i].id, to: n.id }));

try {
  // ---- a flow builds a workbook and adds, changes and reads rows ------------------------------------------------------------
  const nodes = [
    trig,
    node('a', 'excel', 'Add the first', { operation: 'append_row', path: rel('posts.xlsx'), sheet: 'Posts', columns: { Id: 'p1', Platform: 'LinkedIn', Likes: '12', Note: '=1+1' } }),
    node('b', 'excel', 'Add the second', { operation: 'append_row', path: rel('posts.xlsx'), sheet: 'Posts', columns: { id: 'p2', platform: '{{vars.platform}}', likes: '0' } }),
    node('c', 'excel', 'Update the first', { operation: 'update_row', path: rel('posts.xlsx'), sheet: 'Posts', key_column: 'Id', key: 'p1', columns: { Likes: '15' } }),
    node('d', 'excel', 'Read them', { operation: 'read_rows', path: rel('posts.xlsx'), sheet: 'Posts' }),
    node('say', 'notification', 'Tell me', { channel: 'in_app', message: 'rows: {{nodes.d.row_count}}' }),
  ];
  const good = await runFlow({ name: 'e2e-sheet-good', settings: { approval: 'never' }, variables: [{ name: 'platform', label: 'Platform', kind: 'text', required: true, default: 'X' }], nodes, edges: chain(nodes) }, { platform: 'Instagram' });
  check('the flow ran: build, add twice, change, read', good.run.status === 'succeeded', `${good.run.error} ${JSON.stringify(good.nodes.filter((n) => n.error).map((n) => [n.node_id, n.error]))}`);
  const read = good.nodes.find((n) => n.node_id === 'd');
  check('the read step sees two rows', read.outputs.row_count === 2 && read.outputs.rows[1].Platform === 'Instagram', JSON.stringify(read.outputs));
  check('and the change went to the right row', read.outputs.rows[0].Likes === 15 && read.outputs.rows[1].Likes === 0, JSON.stringify(read.outputs.rows));

  // ---- an independent library opens the file --------------------------------------------------------------------------------
  const file = path.join(work, 'posts.xlsx').replace(/\\/g, '/');
  const dump = JSON.parse(py(`import openpyxl,json;wb=openpyxl.load_workbook(r'${file}');ws=wb['Posts'];print(json.dumps({'rows':[[c.value for c in r] for r in ws.iter_rows()],'types':[c.data_type for c in ws[2]],'creator':wb.properties.creator}))`));
  check('openpyxl opens it as a real workbook with a Posts sheet', Array.isArray(dump.rows) && dump.rows.length === 3, JSON.stringify(dump));
  check('the header row and the values are right', JSON.stringify(dump.rows[0]) === JSON.stringify(['Id', 'Platform', 'Likes', 'Note']) && dump.rows[1][2] === 15 && dump.rows[2][1] === 'Instagram', JSON.stringify(dump.rows));
  check('numbers are real numbers, so they sum and sort', dump.types[2] === 'n', JSON.stringify(dump.types));
  check('text that starts with = is text, not a formula', dump.rows[1][3] === '=1+1' && dump.types[3] === 's', JSON.stringify([dump.rows[1][3], dump.types[3]]));
  check('the file says Aqua wrote it', /Aqua/.test(dump.creator ?? ''), dump.creator);

  // ---- refusals ----------------------------------------------------------------------------------------------------------------
  const refused = async (cfg) => (await runFlow({ name: 'e2e-sheet-bad', settings: { approval: 'never' }, nodes: [trig, node('x', 'excel', 'Bad step', cfg)], edges: chain([trig, node('x', 'excel', 'x', {})]) })).nodes.find((n) => n.node_id === 'x').error;
  const outside = await refused({ operation: 'append_row', path: path.join(os.tmpdir(), 'aqua_e2e_outside.xlsx'), columns: { A: 1 } });
  check('a file outside the folders Aqua may use is refused', /not allowed/.test(outside) && !existsSync(path.join(os.tmpdir(), 'aqua_e2e_outside.xlsx')), outside);
  const unknown = await refused({ operation: 'append_row', path: rel('posts.xlsx'), sheet: 'Posts', columns: { Nonsense: 1 } });
  check('a column the sheet does not have is an error that lists its columns', /no column called "Nonsense"/.test(unknown) && /Id, Platform, Likes, Note/.test(unknown), unknown);
  const ambiguous = await refused({ operation: 'update_row', path: rel('posts.xlsx'), sheet: 'Posts', key_column: 'Platform', key: 'nobody', columns: { Likes: 1 } });
  check('changing a row that does not exist changes nothing and says so', /no row has "nobody"/.test(ambiguous), ambiguous);
  check('the workbook is unchanged by all that', JSON.parse(py(`import openpyxl,json;print(json.dumps(openpyxl.load_workbook(r'${file}')['Posts'].max_row))`)) === 3);

  // ---- a workbook someone else made ----------------------------------------------------------------------------------------------
  mkdirSync(work, { recursive: true });
  const theirs = path.join(work, 'theirs.xlsx').replace(/\\/g, '/');
  py(`import openpyxl;wb=openpyxl.Workbook();ws=wb.active;ws.title='Posts';ws.append(['Title','Score']);ws.append(['Existing',5]);wb.save(r'${theirs}')`);
  const foreign = await refused({ operation: 'append_row', path: rel('theirs.xlsx'), sheet: 'Posts', columns: { Title: 'New' } });
  check('a workbook Aqua did not make is not rewritten by default', /not made by Aqua/.test(foreign) && /formatting/.test(foreign), foreign);
  const allowed = await runFlow({ name: 'e2e-sheet-allowed', settings: { approval: 'never' }, nodes: [trig, node('x', 'excel', 'Add to theirs', { operation: 'append_row', path: rel('theirs.xlsx'), sheet: 'Posts', columns: { Title: 'New' }, rewrite_existing: true })], edges: chain([trig, node('x', 'excel', 'x', {})]) });
  check('allowed, it adds the row and keeps a dated backup', allowed.run.status === 'succeeded' && py(`import glob;print(len(glob.glob(r'${work.replace(/\\/g, '/')}/theirs.aqua-backup-*.xlsx')))`) === '1', allowed.run.error);
  check('the person\'s own row is still there', py(`import openpyxl;ws=openpyxl.load_workbook(r'${theirs}')['Posts'];print([[c.value for c in r] for r in ws.iter_rows()])`).includes("'Existing', 5"));

  // ---- csv ------------------------------------------------------------------------------------------------------------------------------
  const csvNodes = [trig, node('w', 'excel', 'Write CSV', { operation: 'append_row', path: rel('log.csv'), columns: { Event: '=cmd|x', Count: '3' } })];
  const csvRun = await runFlow({ name: 'e2e-sheet-csv', settings: { approval: 'never' }, nodes: csvNodes, edges: chain(csvNodes) });
  const csvText = readFileSync(path.join(work, 'log.csv'), 'utf8');
  check('a CSV is written, and a leading = is defused', csvRun.run.status === 'succeeded' && /'=cmd\|x/.test(csvText), csvText);
  writeFileSync(path.join(work, 'done.txt'), 'x');
} catch (e) {
  check('the test ran to the end', false, String(e));
} finally {
  for (const f of await created().catch(() => [])) await api('POST', `/api/v1/flows/${f.id}/delete`);
  rmSync(work, { recursive: true, force: true });
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
