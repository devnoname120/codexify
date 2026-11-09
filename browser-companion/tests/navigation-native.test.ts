import {expect,test} from "bun:test";
import {join} from "node:path";
import {chromium} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {navigatePage} from "../src/navigation";

test.skipIf(process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME!=="1")("a real Chrome HTTP 429 stops another conversation before any document request",async()=>{
  const executablePath=await privateChromeExecutable(join(import.meta.dirname,'../.browsers',`${process.platform}-${process.arch}`));
  const browser=await chromium.launch({executablePath,headless:true});
  try{
    const context=await browser.newContext(),page=await context.newPage();const loads:string[]=[];
    await context.route('https://chatgpt.com/**',route=>{
      const path=new URL(route.request().url()).pathname;
      if(path.startsWith('/c/')){loads.push(path);return route.fulfill({status:429,headers:{'retry-after':'120'},contentType:'text/html',body:'Temporarily rate limited'});}
      return route.fulfill({contentType:'text/html',body:'Fixture home'});
    });
    await page.goto('https://chatgpt.com/');
    await expect(navigatePage(page,'https://chatgpt.com/c/limited')).rejects.toMatchObject({code:'conversation_load_rate_limited'});
    await expect(navigatePage(page,'https://chatgpt.com/c/another')).rejects.toMatchObject({code:'conversation_cooldown'});
    expect(loads).toEqual(['/c/limited']);
  }finally{await browser.close();}
},10_000);
