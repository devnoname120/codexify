import {expect,test} from "bun:test";
import {execFile} from "node:child_process";
import {promisify} from "node:util";
import {mkdir,writeFile} from "node:fs/promises";
import {dirname,join,resolve} from "node:path";

test.skipIf(process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME!=="1")("model control remains stable when opening changes its styling",async()=>{
  const project=resolve(import.meta.dirname,"..");
  const bundle=await Bun.build({entrypoints:[join(project,"scripts","effort-control-smoke.ts")],target:"node",packages:"external"});
  expect(bundle.success).toBe(true);
  const entry=join(project,"node_modules",".cache","codexify-effort-control-smoke.mjs");
  await mkdir(dirname(entry),{recursive:true});await writeFile(entry,await bundle.outputs[0]!.text());
  const {stdout}=await promisify(execFile)("node",[entry],{cwd:project,timeout:30_000,windowsHide:true});
  expect(JSON.parse(stdout.trim().split(/\r?\n/).at(-1)!)).toMatchObject({ok:true,styleChangePreservesOwnedControl:true,abortedActivationDoesNotClick:true});
},40_000);
