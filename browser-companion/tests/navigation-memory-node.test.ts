import {expect,test} from "bun:test";
import {execFile} from "node:child_process";
import {promisify} from "node:util";
import {mkdir,writeFile} from "node:fs/promises";
import {dirname,join,resolve} from "node:path";

test.skipIf(process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME!=="1")(
  "private Chrome releases prior navigation documents instead of retaining BFCache heaps",
  async()=>{
    const project=resolve(import.meta.dirname,"..");
    const bundle=await Bun.build({entrypoints:[join(project,"scripts/navigation-memory-smoke.ts")],target:"node",packages:"external"});
    expect(bundle.success).toBe(true);
    const entry=join(project,"node_modules/.cache/codexify-navigation-memory-smoke.mjs");
    await mkdir(dirname(entry),{recursive:true});
    await writeFile(entry,await bundle.outputs[0]!.text());
    const {stdout}=await promisify(execFile)("node",[entry],{cwd:project,timeout:45000,windowsHide:true,maxBuffer:1024*1024});
    const payload=JSON.parse(stdout.trim().split(/\r?\n/).at(-1)!);
    expect(payload.navigationCache[1]).toMatchObject({disableCache:true,reusedPreviousDocument:false,persisted:"false"});
  },60000,
);
