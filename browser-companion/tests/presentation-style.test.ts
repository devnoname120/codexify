import {test,expect} from "bun:test";
import {mkdtemp,readFile,writeFile} from "node:fs/promises";
import {join} from "node:path";
import {tmpdir} from "node:os";
import {readPresentationCss,savePresentationCss,validatePresentationCss} from "../src/presentation-style";

test("live presentation accepts bounded local CSS and rejects resource requests",()=>{
  expect(validatePresentationCss('#sidebar { display:none !important; }')).toContain('display:none');
  for(const css of ['@import "https://example.com/style";','body {background:URL(https://example.com)}','body{background:u\\72l(https://example.com)}','body{width:expression(alert(1))}','x'.repeat(32769)]){
    expect(()=>validatePresentationCss(css)).toThrow();
  }
});
test("live presentation persists separately from account and automation state",async()=>{
  const root=await mkdtemp(join(tmpdir(),'fast-presentation-'));
  const path=join(root,'fast-ui.css'),account=join(root,'account-sentinel');
  await writeFile(account,'untouched');
  expect(await readPresentationCss(path)).toBe('');
  await savePresentationCss(path,'#sidebar {display:none!important;}');
  expect(await readPresentationCss(path)).toBe('#sidebar {display:none!important;}');
  await expect(savePresentationCss(path,'@import "https://example.com";')).rejects.toThrow();
  expect(await readFile(account,'utf8')).toBe('untouched');
  expect(await readPresentationCss(path)).toBe('#sidebar {display:none!important;}');
});
