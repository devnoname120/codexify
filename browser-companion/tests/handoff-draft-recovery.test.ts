import {expect,test} from 'bun:test';
import {DraftRecoveryJournal} from '../src/draft-recovery';
import {renderActionPrompt} from '../src/protocol';
const draft=renderActionPrompt({kind:'turn_renewal',handoffId:`renew-${'a'.repeat(64)}`});
test('parallel new conversations retain independent draft ownership at the same project URL',()=>{
 const journal=new DraftRecoveryJournal(),url='https://chatgpt.com/g/g-p-fixture/project';
 const actions=['bionic','toolchain','devutils','fifa'];
 for(const action of actions)journal.prepare('account',url,action,`Initialize ${action}`);
 for(const action of actions){
  expect(journal.recoverable('account',url,`Initialize ${action}`)).toBe(true);
  journal.beforeSend(action);
  expect(journal.recoverable('account',url,`Initialize ${action}`)).toBe(false);
 }
});
test('a later uncertain send supersedes an older prepared copy of the same draft',()=>{
 const journal=new DraftRecoveryJournal();
 journal.prepare('account','url','interrupted','Owner instructions');
 journal.prepare('account','url','retry','Owner instructions');
 journal.beforeSend('retry');
 expect(journal.recoverable('account','url','Owner instructions')).toBe(false);
 journal.forget('retry');
 expect(journal.recoverable('account','url','Owner instructions')).toBe(false);
});
test('only the exact owned fixed handoff draft is eligible after final-turn proof',()=>{
 const journal=new DraftRecoveryJournal();
 expect(journal.recoverableHandoffDraft('account','url',draft)).toBe(false);
 journal.prepare('account','url','action',draft);
 journal.beforeSend('action');
 expect(journal.recoverable('account','url',draft)).toBe(false);
 expect(journal.recoverableHandoffDraft('account','url',draft)).toBe(true);
 expect(journal.recoverableHandoffDraft('account','url',draft.replace('\n\n','\n'))).toBe(true);
 expect(journal.recoverableHandoffDraft('account','url',draft.replace(/\n/g,'\r\n')+'\n')).toBe(true);
 expect(journal.recoverableHandoffDraft('other','url',draft)).toBe(false);
 expect(journal.recoverableHandoffDraft('account','other',draft)).toBe(false);
 expect(journal.recoverableHandoffDraft('account','url',draft+' user edit')).toBe(false);
});
test('uncertain project and user message batches cannot become handoff drafts',()=>{
 const journal=new DraftRecoveryJournal();
 const batch=renderActionPrompt({kind:'offline_chat',batchId:'batch',messages:[{id:'message',markdown:'Owner project instructions'}]});
 journal.prepare('account','url','action',batch);journal.beforeSend('action');
 expect(journal.recoverableHandoffDraft('account','url',batch)).toBe(false);
 expect(journal.recoverable('account','url',batch)).toBe(false);
});
