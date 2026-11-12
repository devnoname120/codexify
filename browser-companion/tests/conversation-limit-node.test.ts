import {expect,test} from 'bun:test';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {mkdir,writeFile} from 'node:fs/promises';
import {join,resolve,dirname} from 'node:path';

test.skipIf(process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME!=='1')('Node Chrome recognizes the conversation-length banner without discarding drafts or approvals',async()=>{
 const root=resolve(import.meta.dirname,'..');
 const bundle=await Bun.build({entrypoints:[join(root,'scripts/conversation-limit-smoke.ts')],target:'node',packages:'external'});
 expect(bundle.success).toBe(true);
 const entry=join(root,'node_modules/.cache/codexify-conversation-limit-smoke.mjs');
 await mkdir(dirname(entry),{recursive:true});await writeFile(entry,await bundle.outputs[0]!.text());
 const {stdout}=await promisify(execFile)('node',[entry],{cwd:root,timeout:60000,windowsHide:true});
 expect(JSON.parse(stdout.trim().split(/\r?\n/).at(-1)!)).toMatchObject({ok:true,maximumLengthDetected:true,draftsAndApprovalsPreserved:true});
},70000);
