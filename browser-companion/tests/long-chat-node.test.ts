import {expect,test} from "bun:test";
import {execFile} from "node:child_process";
import {promisify} from "node:util";
import {mkdir,writeFile} from "node:fs/promises";
import {dirname,join,resolve} from "node:path";

const execFileAsync=promisify(execFile);
test.skipIf(process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME !== "1")(
  "Node-backed Chrome validates 650-turn cache and private minimal UI on Windows",
  async()=>{
    const project=resolve(import.meta.dirname,"..");
    const bundle=await Bun.build({
      entrypoints:[join(project,"scripts","long-chat-smoke.ts")],
      target:"node",packages:"external",
    });
    expect(bundle.success).toBe(true);
    const entry=join(project,"node_modules",".cache","codexify-long-chat-smoke.mjs");
    await mkdir(dirname(entry),{recursive:true});
    await writeFile(entry,await bundle.outputs[0]!.text());
    const {stdout}=await execFileAsync("node",[entry],{
      cwd:project,timeout:45_000,windowsHide:true,
      maxBuffer:1024*1024,
    });
    const payload=JSON.parse(stdout.trim().split(/\r?\n/).at(-1)!);
    expect(payload).toMatchObject({ok:true,turns:650,fullScansBeforeBenchmark:3,fullScansAfterBenchmark:27});
    expect(payload.cached20PollsMs).toBeGreaterThan(0);
    expect(payload.cachedStyleReads).toBeLessThanOrEqual(210);
  },60_000,
);
