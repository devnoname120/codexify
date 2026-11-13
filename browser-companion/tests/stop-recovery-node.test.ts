import {expect,test} from 'bun:test';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {mkdir,writeFile} from 'node:fs/promises';
import {dirname,join,resolve} from 'node:path';

test.skipIf(process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME!=='1')('stalled answers stop before sending a fresh prompt, with bounded retries and draft preservation',async()=>{
 const project=resolve(import.meta.dirname,'..');
 const bundle=await Bun.build({entrypoints:[join(project,'scripts','stop-recovery-smoke.ts')],target:'node',packages:'external'});
 expect(bundle.success).toBe(true);
 const entry=join(project,'node_modules','.cache','codexify-stop-recovery-smoke.mjs');
 await mkdir(dirname(entry),{recursive:true});await writeFile(entry,await bundle.outputs[0]!.text());
 // The scenarios include deliberate Stop waits; Windows Chrome can then take
 // its own bounded 30-second close grace. Internal operation bounds stay asserted.
 const {stdout}=await promisify(execFile)('node',[entry],{cwd:project,timeout:150_000,windowsHide:true});
 expect(JSON.parse(stdout.trim().split(/\r?\n/).at(-1)!)).toMatchObject({ok:true,lateStopBeforeSend:true,disabledStopWaited:true,ignoredStopRetried:true,draftPreserved:true,cancellationRespected:true});
},160_000);
