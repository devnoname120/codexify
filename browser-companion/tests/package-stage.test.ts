import {expect,test} from "bun:test";
import {createRequire} from "node:module";
import {mkdtempSync,mkdirSync,writeFileSync,readFileSync,existsSync,rmSync,symlinkSync} from "node:fs";
import {join} from "node:path";
import {tmpdir} from "node:os";

const {stageWindowsApplication}=createRequire(import.meta.url)("../scripts/package-stage.cjs");

test("Windows package staging moves its completed application intact instead of retaining a second runtime",()=>{
  const scratch=mkdtempSync(join(tmpdir(),"codexify-package-stage-"));
  try {
    const source=join(scratch,"builder","win-unpacked"),target=join(scratch,"package","release","app");
    mkdirSync(join(source,"resources","chrome"),{recursive:true});
    writeFileSync(join(source,"companion.exe"),"fixture executable");
    writeFileSync(join(source,"resources","chrome","chrome.exe"),"fixture browser");
    stageWindowsApplication(scratch,source,target);
    expect(existsSync(source)).toBe(false);
    expect(readFileSync(join(target,"companion.exe"),"utf8")).toBe("fixture executable");
    expect(readFileSync(join(target,"resources","chrome","chrome.exe"),"utf8")).toBe("fixture browser");
  } finally {rmSync(scratch,{recursive:true,force:true});}
});

test("Windows staging refuses existing outputs and paths outside its private scratch directory",()=>{
  const parent=mkdtempSync(join(tmpdir(),"codexify-package-stage-")),scratch=join(parent,"owned");
  try {
    const source=join(scratch,"builder"),target=join(scratch,"app"),outside=join(parent,"outside");
    mkdirSync(source,{recursive:true});mkdirSync(target);mkdirSync(outside);
    writeFileSync(join(source,"keep.txt"),"preserved");
    expect(()=>stageWindowsApplication(scratch,source,target)).toThrow("Unsafe");
    expect(()=>stageWindowsApplication(scratch,source,join(outside,"app"))).toThrow("Unsafe");
    expect(()=>stageWindowsApplication(scratch,outside,join(scratch,"new-app"))).toThrow("Unsafe");
    expect(readFileSync(join(source,"keep.txt"),"utf8")).toBe("preserved");
  } finally {rmSync(parent,{recursive:true,force:true});}
});

test("staging cannot follow a linked application or create directories through an escaped parent",()=>{
  const parent=mkdtempSync(join(tmpdir(),"codexify-package-stage-")),scratch=join(parent,"owned");
  try {
    const source=join(scratch,"builder"),outside=join(parent,"outside"),sourceLink=join(scratch,"linked-app");
    mkdirSync(source,{recursive:true});mkdirSync(outside);
    const kind=process.platform==="win32"?"junction":"dir";
    symlinkSync(source,sourceLink,kind);
    symlinkSync(outside,join(scratch,"linked-parent"),kind);
    expect(()=>stageWindowsApplication(scratch,sourceLink,join(scratch,"app"))).toThrow("Unsafe");
    expect(()=>stageWindowsApplication(scratch,source,join(scratch,"linked-parent","new-folder","app"))).toThrow("escaped");
    expect(existsSync(join(outside,"new-folder"))).toBe(false);
    expect(existsSync(source)).toBe(true);
  } finally {rmSync(parent,{recursive:true,force:true});}
});
