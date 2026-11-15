import {expect,test} from "bun:test";
import {execFile} from "node:child_process";
import {promisify} from "node:util";
import {recoveryScript} from "../src/windows-chrome-recovery";

const cases=[
 {name:'preserves its actual live parent',parentReused:false,handleReused:false,exitTimeout:false,stopped:0},
 {name:'recognizes a reused parent PID and waits for exit',parentReused:true,handleReused:false,exitTimeout:false,stopped:1},
 {name:'preserves a reused process handle',parentReused:true,handleReused:true,exitTimeout:false,stopped:0},
 {name:'rejects an orphan that has not exited',parentReused:true,handleReused:false,exitTimeout:true,stopped:0},
];
for(const fixture of cases)test.skipIf(process.platform!=="win32")(`private orphan recovery ${fixture.name}`,async()=>{
 const script=String.raw`
$script:child=[pscustomobject]@{Name='chrome.exe';ExecutablePath='C:\CodexifyTest\runtime\chrome.exe';CommandLine='chrome.exe --user-data-dir=C:\CodexifyTest\profile';ProcessId=123;ParentProcessId=456;CreationDate=[datetime]'2026-10-25T01:00:00Z'}
$script:parent=[pscustomobject]@{ProcessId=456;CreationDate=[datetime]'${fixture.parentReused?'2026-10-25T01:01:00Z':'2026-10-25T00:59:00Z'}'}
$script:killed=$false
function Get-CimInstance { [CmdletBinding()]param([string]$ClassName,[string]$Filter)
 if(-not $Filter){return @($script:child,$script:parent)}
 if($Filter -eq 'ProcessId=123'){return $script:child}
 if($Filter -eq 'ProcessId=456'){return $script:parent}
}
function Get-Process { [CmdletBinding()]param([int]$Id)
 if($Id -ne 123){throw 'Unexpected process selected'}
 $process=[pscustomobject]@{StartTime=[datetime]'${fixture.handleReused?'2026-10-25T01:02:00Z':'2026-10-25T01:00:00Z'}'}
 $process|Add-Member -MemberType ScriptMethod -Name Kill -Value {$script:killed=$true}
 $process|Add-Member -MemberType ScriptMethod -Name WaitForExit -Value {
  param($milliseconds)
  if(-not $script:killed -or $milliseconds -ne 5000){throw 'Exit must be observed after terminating the verified handle'}
  return $${fixture.exitTimeout?'false':'true'}
 }
 $process|Add-Member -MemberType ScriptMethod -Name Dispose -Value {}
 return $process
}
`+recoveryScript;
 const pending=promisify(execFile)('powershell.exe',['-NoProfile','-NonInteractive','-OutputFormat','Text','-EncodedCommand',Buffer.from(script,'utf16le').toString('base64')],{windowsHide:true,timeout:15000,env:{...process.env,CODEXIFY_RECOVERY_PROFILE:String.raw`C:\CodexifyTest\profile`,CODEXIFY_RECOVERY_EXECUTABLE:String.raw`C:\CodexifyTest\runtime\chrome.exe`,CODEXIFY_RECOVERY_RUNTIME:String.raw`C:\CodexifyTest\runtime`}});
 if(fixture.exitTimeout){await expect(pending).rejects.toThrow('Private Chrome did not exit');return;}
 const {stdout}=await pending;
 expect(JSON.parse(stdout.trim()).stopped).toBe(fixture.stopped);
},20000);
