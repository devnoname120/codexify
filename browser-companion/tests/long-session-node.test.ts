import {expect,test} from 'bun:test';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {mkdir,writeFile} from 'node:fs/promises';
import {dirname,join,resolve} from 'node:path';

test.skipIf(process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME!=='1')(
  '80 long conversation switches keep retained documents and heap bounded',async()=>{
    const root=resolve(import.meta.dirname,'..');
    const bundle=await Bun.build({entrypoints:[join(root,'scripts/long-session-smoke.ts')],target:'node',packages:'external'});
    expect(bundle.success).toBe(true);
    const entry=join(root,'node_modules/.cache/long-session-smoke.mjs');
    await mkdir(dirname(entry),{recursive:true});await writeFile(entry,await bundle.outputs[0]!.text());
    const {stdout}=await promisify(execFile)('node',[entry],{cwd:root,timeout:300_000,windowsHide:true,maxBuffer:1024*1024});
    const result=JSON.parse(stdout.trim().split(/\r?\n/).at(-1)!);
    expect(result).toMatchObject({ok:true,switches:80,turnsPerPage:500});
    expect(result.finalDom.documents).toBeLessThan(20);
    expect(result.finalHeapBytes).toBeLessThan(128*1024*1024);
  },310_000,
);
