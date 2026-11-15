import {test,expect} from "bun:test";
import {mkdtempSync,readFileSync,lstatSync,writeFileSync,symlinkSync,rmSync} from "node:fs";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {ConnectorAppCache} from "../src/connector-cache";

test("connector identities persist across restarts and stay bound to account and name",()=>{
  const root=mkdtempSync(join(tmpdir(),"connector-cache-")),path=join(root,"apps.json"),account="a".repeat(64);
  try {
    const cache=new ConnectorAppCache(path);cache.remember(account,"My connector","app://asdk_app_fixture");
    const restarted=new ConnectorAppCache(path);
    expect(restarted.get(account,"My connector")).toBe("asdk_app_fixture");
    expect(restarted.get("b".repeat(64),"My connector")).toBeUndefined();expect(restarted.get(account,"Other")).toBeUndefined();
    if(process.platform!=="win32")expect(lstatSync(path).mode&0o777).toBe(0o600);
    cache.rememberAllowAllSummary(account,"My connector","Permiso Permitir todo");
    expect(new ConnectorAppCache(path).getAllowAllSummary(account,"My connector")).toBe("Permiso Permitir todo");
    expect(cache.getAllowAllSummary("b".repeat(64),"My connector")).toBeUndefined();
    cache.remember(account,"My connector","https://untrusted.example");expect(cache.get(account,"My connector")).toBe("asdk_app_fixture");
    writeFileSync(path,"invalid JSON");expect(new ConnectorAppCache(path).get(account,"My connector")).toBeUndefined();
  }finally{rmSync(root,{recursive:true,force:true});}
});
test.skipIf(process.platform==="win32")("connector cache never follows or replaces a symlink",()=>{
  const root=mkdtempSync(join(tmpdir(),"connector-cache-")),target=join(root,"target"),path=join(root,"apps.json");
  try {
    writeFileSync(target,"keep");symlinkSync(target,path);
    const cache=new ConnectorAppCache(path);cache.remember("a".repeat(64),"My connector","asdk_app_fixture");
    expect(readFileSync(target,"utf8")).toBe("keep");expect(lstatSync(path).isSymbolicLink()).toBeTrue();
  }finally{rmSync(root,{recursive:true,force:true});}
});
