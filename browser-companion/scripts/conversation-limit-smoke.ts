import assert from 'node:assert/strict';
import {mkdtemp,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
import {chromium} from 'patchright';
import {privateChromeExecutable} from '../src/chrome-executable';
import {conversationUnavailableVisible,throwIfSessionAttention} from '../src/browser-session';
import {CONVERSATION_UNAVAILABLE_MESSAGE} from '../src/errors';
import {renderActionPrompt} from '../src/protocol';

const profile=await mkdtemp(join(tmpdir(),'codexify-length-limit-'));
const executablePath=await privateChromeExecutable(resolve('.browsers',`${process.platform}-${process.arch}`));
const context=await chromium.launchPersistentContext(profile,{executablePath,headless:true});
const page=context.pages()[0]!;
const banner=`<div style="border:1px solid rgb(255,204,204);border-radius:8px;padding:20px;color:red"><span>You've reached the maximum length for this conversation, but you can keep talking by starting a new chat.</span><button type="button">Start new chat</button></div>`;
let body='';let newChatClicks=0;
try {
 await page.exposeFunction('newChatClicked',()=>newChatClicks++);
 await context.route('https://chatgpt.com/**',r=>r.fulfill({contentType:'text/html',body:`<main>${body}</main><script>document.querySelectorAll('button').forEach(b=>b.onclick=()=>window.newChatClicked());</script>`}));
 const load=async(html:string,url='https://chatgpt.com/c/length-limit-fixture')=>{body=html;await page.goto(url);};
 const composer='<form><div id="prompt-textarea" contenteditable="true" style="min-height:30px"></div></form>';
 await load(`<article>Previous activity</article>${banner}${composer}`);
 assert.equal(await conversationUnavailableVisible(page),true);
 await assert.rejects(throwIfSessionAttention(page),{code:'conversation_unavailable',message:CONVERSATION_UNAVAILABLE_MESSAGE});
 assert.equal(newChatClicks,0,'Detection must use guarded native task replacement, not click an unverified new-chat button');
 await page.locator('#prompt-textarea').fill('Keep my edited draft');
 assert.equal(await conversationUnavailableVisible(page),false);
 assert.equal(await page.locator('#prompt-textarea').innerText(),'Keep my edited draft');
 await page.locator('#prompt-textarea').fill(renderActionPrompt({kind:'offline_goal',goalRevision:1}));
 assert.equal(await conversationUnavailableVisible(page),true);
 await load(`<div hidden>${banner}</div>${composer}`);assert.equal(await conversationUnavailableVisible(page),false);
 await load(`<article><div class="markdown">A quoted example: ${banner}</div></article>${composer}`);assert.equal(await conversationUnavailableVisible(page),false);
 await load(`<div data-user-message-bubble>${banner}</div>${composer}`);assert.equal(await conversationUnavailableVisible(page),false);
 await load(`${banner}<div role="dialog">A tool approval requires your decision</div>${composer}`);assert.equal(await conversationUnavailableVisible(page),false);
 await load(banner,'https://chatgpt.com/');assert.equal(await conversationUnavailableVisible(page),false);
 console.log(JSON.stringify({ok:true,maximumLengthDetected:true,usesExistingGuardedReplacement:true,draftsAndApprovalsPreserved:true,quotedAndHiddenBannersIgnored:true}));
}finally{await context.close();const absolute=resolve(profile);if(!absolute.startsWith(resolve(tmpdir())+requireSeparator()+'codexify-length-limit-'))throw new Error('Unexpected test profile');await rm(absolute,{recursive:true,force:true,maxRetries:8,retryDelay:200});}
function requireSeparator(){return process.platform==='win32'?'\\':'/';}
