import {createHash,randomUUID} from 'node:crypto';
import {lstatSync,readFileSync,writeFileSync,renameSync,unlinkSync} from 'node:fs';
import {z} from 'zod';
import {BrowserCompanionError} from './errors';
import {renderActionPrompt} from './protocol';
const record=z.strictObject({account:z.string().min(1).max(256),url:z.string().max(4096),actionId:z.string().min(1).max(256),hash:z.string().regex(/^[a-f0-9]{64}$/),phase:z.enum(['prepared','uncertain'])});
const records=z.array(record).max(32);
const hash=(text:string)=>createHash('sha256').update(text).digest('hex');

/** Hash-only pre-Send ownership survives companion crashes. Uncertain sends
 * never authorize recovering a draft containing user messages. */
export class DraftRecoveryJournal {
 private entries:z.infer<typeof records>=[];
 constructor(private readonly path?:string){
  if(!path)return;
  try {const info=lstatSync(path);if(info.isFile()&&!info.isSymbolicLink()&&info.size<=256*1024)this.entries=records.parse(JSON.parse(readFileSync(path,'utf8')));}catch{}
 }
 recoverable(account:string,url:string,text:string):boolean {
  return this.entries.some(e=>e.account===account&&e.url===url&&e.phase==='prepared'&&e.hash===hash(text));
 }
 /** Only after fresh proof that the handoff report is still the final turn:
  * no subsequent submitted user turn or response exists. Never user batches. */
 recoverableHandoffDraft(account:string,url:string,text:string):boolean {
  const fixed=renderActionPrompt({kind:'turn_renewal',handoffId:`renew-${'0'.repeat(64)}`});
  const normalize=(value:string)=>value.replace(/\s+/g,' ').trim();
  // ChatGPT can collapse blank paragraphs while restoring a saved draft.
  // Ownership must still match the original, exact fixed template hash.
  return normalize(text)===normalize(fixed)&&this.entries.some(e=>e.account===account&&e.url===url&&e.hash===hash(fixed));
 }
 prepare(account:string,url:string,actionId:string,text:string):void {
  this.entries=[...this.entries.filter(e=>e.account!==account||e.url!==url),record.parse({account,url,actionId,hash:hash(text),phase:'prepared'})].slice(-32);this.persist();
 }
 beforeSend(actionId:string):void {const entry=this.entries.find(e=>e.actionId===actionId);if(!entry)throw new BrowserCompanionError('Message draft ownership changed before Send','external_interference');entry.phase='uncertain';this.persist();}
 forget(actionId:string):void {this.entries=this.entries.filter(e=>e.actionId!==actionId);try{this.persist();}catch{}}
 private persist():void {
  if(!this.path)return;
  const temporary=`${this.path}.${randomUUID()}.tmp`;
  try {
   try{const info=lstatSync(this.path);if(!info.isFile()||info.isSymbolicLink())throw Error('Invalid draft journal');}catch(e){if((e as NodeJS.ErrnoException).code!=='ENOENT')throw e;}
   writeFileSync(temporary,JSON.stringify(this.entries),{flag:'wx',mode:0o600});renameSync(temporary,this.path);
  }catch(cause){throw new BrowserCompanionError('Could not save message draft ownership before Send','editor_unavailable',{cause});}
  finally{try{unlinkSync(temporary);}catch{}}
 }
}
