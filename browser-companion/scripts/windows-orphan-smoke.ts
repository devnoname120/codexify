import assert from "node:assert/strict";
import {mkdtemp, rm, writeFile, readFile} from "node:fs/promises";
import {tmpdir} from "node:os";
import {join, resolve, sep} from "node:path";
import {execFile, spawn} from "node:child_process";
import {promisify} from "node:util";
import {chromium} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {recoverOrphanedWindowsChrome} from "../src/windows-chrome-recovery";

if (process.platform !== "win32") process.exit(0);
const profile = await mkdtemp(join(tmpdir(), "codexify-orphan-smoke-"));
const executablePath = await privateChromeExecutable(resolve(".browsers", "win32-x64"));
const marker = join(profile, "preserve-marker.txt");
await writeFile(marker, "preserve profile");
const args = [`--user-data-dir=${profile}`, "--headless=new", "--no-first-run", "--no-default-browser-check", "about:blank"];
const wait = (ms:number) => new Promise(resolve => setTimeout(resolve, ms));
let owned:ReturnType<typeof spawn> | undefined;
let orphanPid:number | undefined;
try {
  owned = spawn(executablePath, args, {windowsHide:true, stdio:"ignore"});
  assert.ok(owned.pid);
  await wait(500);
  assert.equal(await recoverOrphanedWindowsChrome({profilePath:profile, executablePath}), 0,
    "A Chrome with a live parent must never be stopped");
  process.kill(owned.pid!, 0);
  const exited = new Promise(resolve => owned!.once("exit", resolve));
  owned.kill(); await exited;
  await wait(500);
  const launcher = `const {spawn}=require('node:child_process');const p=spawn(process.env.CODEXIFY_SMOKE_CHROME,JSON.parse(process.env.CODEXIFY_SMOKE_ARGS),{detached:true,windowsHide:true,stdio:'ignore'});console.log(p.pid);p.unref();`;
  const {stdout} = await promisify(execFile)(process.execPath, ["-e", launcher], {
    windowsHide:true, timeout:10_000,
    env:{...process.env, CODEXIFY_SMOKE_CHROME:executablePath, CODEXIFY_SMOKE_ARGS:JSON.stringify(args)},
  });
  orphanPid = Number(stdout.trim());
  assert.ok(Number.isSafeInteger(orphanPid) && orphanPid > 0);
  await wait(500);
  process.kill(orphanPid, 0);
  const stopped = await recoverOrphanedWindowsChrome({profilePath:profile, executablePath});
  assert.ok(stopped >= 1, "The exact private orphan should be recovered");
  await wait(500);
  assert.throws(() => process.kill(orphanPid!, 0));
  assert.equal(await readFile(marker, "utf8"), "preserve profile");
  const browser = await chromium.launchPersistentContext(profile, {executablePath, headless:true, timeout:20_000});
  await browser.close();
  console.log(JSON.stringify({ok:true, protectedLiveParent:true, recoveredOrphan:true, profilePreserved:true}));
} finally {
  if (owned && owned.exitCode === null) owned.kill();
  if (orphanPid) { try { process.kill(orphanPid); } catch {} }
  if (!resolve(profile).startsWith(resolve(tmpdir()) + sep)) throw new Error("Unexpected test profile path");
  await rm(profile, {recursive:true, force:true});
}
