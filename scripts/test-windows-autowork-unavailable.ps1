param([string]$Engine=(Get-Process -Id $PID).Path)
$ErrorActionPreference='Stop'
$PSNativeCommandUseErrorActionPreference=$false
# A private loopback fixture keeps successful inventory cases independent of
# the actual owner API outage. It serves exactly one authenticated request.
if(-not ('CodexifyMonitorInventoryFixture' -as [type])){
  Add-Type -TypeDefinition @'
using System;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Threading.Tasks;
public sealed class CodexifyMonitorInventoryFixture : IDisposable {
  private readonly TcpListener listener;
  public int Port { get; private set; }
  public Task Completion { get; private set; }
  public CodexifyMonitorInventoryFixture(string body) : this(body, "/api/chats", "fixture-monitor-token") {}
  public CodexifyMonitorInventoryFixture(string body, string path, string token) {
    listener = new TcpListener(IPAddress.Loopback, 0);
    listener.Start();
    Port = ((IPEndPoint)listener.LocalEndpoint).Port;
    Completion = Task.Run(async () => {
      try {
        using (var client = await listener.AcceptTcpClientAsync()) {
          var stream = client.GetStream();
          var buffer = new byte[8192];
          var length = 0;
          while (length < buffer.Length) {
            var count = await stream.ReadAsync(buffer, length, buffer.Length - length);
            if (count == 0) break;
            length += count;
            if (Encoding.ASCII.GetString(buffer, 0, length).Contains("\r\n\r\n")) break;
          }
          var request = Encoding.ASCII.GetString(buffer, 0, length);
          if (!request.StartsWith("GET " + path + " HTTP/1.1\r\n") ||
              request.IndexOf("\r\nAuthorization: Bearer " + token + "\r\n", StringComparison.OrdinalIgnoreCase) < 0)
            throw new Exception("Unexpected request or missing fixture authorization");
          var payload = Encoding.UTF8.GetBytes(body);
          var header = Encoding.ASCII.GetBytes("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: " + payload.Length + "\r\n\r\n");
          await stream.WriteAsync(header, 0, header.Length);
          await stream.WriteAsync(payload, 0, payload.Length);
        }
      } finally { listener.Stop(); }
    });
  }
  public void Dispose() { listener.Stop(); }
}
'@
}
$base=[IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')+'\'
$root=Join-Path $base ('codexify-monitor-unavailable-'+[guid]::NewGuid().ToString('N'))
$null=New-Item -ItemType Directory -Path $root
$listener=[Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback,0)
try {
  $listener.Start()
  $unusedPort=([Net.IPEndPoint]$listener.LocalEndpoint).Port
  $listener.Stop()
  $registered=Join-Path $root ('projects\registered\chats\'+('a'*64))
  $fake=Join-Path $root ('projects\.tmp-history\chats\'+('b'*64))
  foreach($directory in @($registered,$fake)){
    $null=New-Item -ItemType Directory -Path $directory -Force
    [IO.File]::WriteAllText((Join-Path $directory 'automation.json'),'{"goal":{"status":"active"}}')
    [IO.File]::WriteAllText((Join-Path $directory 'CHAT.md'),'# monitor fixture')
  }
  $sha=[Security.Cryptography.SHA256]::Create()
  $fixtureNow=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
  $expired=$fixtureNow-3600000
  $future=$fixtureNow+3600000
  $rows=@(
    @{key='c';state=@{pairing=@{creatingChat=$true;pending=@{expiresAtMs=$expired}};action=@{status='unverified';sendAccepted=$true;updatedAtMs=$expired}}}
    @{key='d';state=@{goal=@{status='active'};action=@{status='cancelled';automaticRetryPaused=$true;updatedAtMs=$expired}}}
    @{key='e';state=@{goal=@{status='active'};action=@{updatedAtMs=$expired}};cursor=@{agentStopRequested=$true}}
    @{key='f';state=@{pairing=@{creatingChat=$true;pending=@{expiresAtMs=$future}};action=@{status='unverified';updatedAtMs=$expired}}}
    @{key='1';state=@{pairing=@{creatingChat=$true;pending=@{expiresAtMs=$expired}};action=@{status='failed';updatedAtMs=$fixtureNow}}}
    @{key='2';state=@{goal=@{status='active'};action=@{status='cancelled';automaticRetryPaused=$false;updatedAtMs=$expired}}}
    @{key='3';state=@{pairing=@{creatingChat=$true};action=@{status='queued';updatedAtMs=$expired}}}
  )
  try {
    $registeredId=-join($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes((Join-Path $registered 'CHAT.md')))|ForEach-Object {$_.ToString('x2')})
    $currentChats=@(@{id=$registeredId})
    foreach($row in $rows){
      $directory=Join-Path (Split-Path -Parent $registered) ($row.key*64)
      $null=New-Item -ItemType Directory -Path $directory
      $transcript=Join-Path $directory 'CHAT.md'
      [IO.File]::WriteAllText($transcript,'# monitor fixture')
      [IO.File]::WriteAllText((Join-Path $directory 'automation.json'),($row.state|ConvertTo-Json -Depth 8))
      if($row.cursor){[IO.File]::WriteAllText((Join-Path $directory 'cursor.json'),($row.cursor|ConvertTo-Json))}
      $id=-join($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($transcript))|ForEach-Object {$_.ToString('x2')})
      $currentChats+=@{id=$id}
    }
  }
  finally {$sha.Dispose()}
  $script=Join-Path $PSScriptRoot 'windows-autowork-status.ps1'
  $wrapper=Join-Path $root 'invoke-monitor.ps1'
  [IO.File]::WriteAllText($wrapper,@'
param([string]$Script,[string]$Root,[int]$Port,[string]$SnapshotPath,[switch]$Compact,[string]$FollowAgentsOfChatPrefix)
$PSNativeCommandUseErrorActionPreference=$true
$monitorArgs=@{Root=$Root;Port=$Port;SnapshotPath=$SnapshotPath;Compact=$Compact}
if($FollowAgentsOfChatPrefix){$monitorArgs.FollowAgentsOfChatPrefix=$FollowAgentsOfChatPrefix}
& $Script @monitorArgs
exit $LASTEXITCODE
'@)
  $browserDirectory=Join-Path $root 'browser'
  $null=New-Item -ItemType Directory -Path $browserDirectory
  $descriptor=Join-Path $browserDirectory 'descriptor.json'
  $browserFixtures=@{
    empty=@{status='busy';authenticated=$false;expected='pending'}
    single=@{status='idle';authenticated=$true;expected='verified'}
    current=@{status='authentication_required';authenticated=$false;expected='required'}
  }
  foreach($case in @('unavailable','empty','single','current','targeted-history','unavailable-team','unavailable-team-partial')){
    foreach($compact in @($false,$true)){
      $server=$null
      $browserServer=$null
      try {
        $port=$unusedPort
        if($case -notlike 'unavailable*'){
          $chats=@()
          if($case -eq 'single'){$chats+=@{id=$registeredId}}
          if($case -in @('current','targeted-history')){$chats=$currentChats}
          $server=[CodexifyMonitorInventoryFixture]::new((@{chats=$chats}|ConvertTo-Json -Compress))
          $port=$server.Port
        }
        [IO.File]::WriteAllText((Join-Path $root 'owner-chat.json'),
          (@{port=$port;token='fixture-monitor-token'}|ConvertTo-Json -Compress))
        $browserFixture=$browserFixtures[$case]
        $expectedAuthentication='unknown'
        if($browserFixture){
          $health=@{status=$browserFixture.status;authenticated=$browserFixture.authenticated}|ConvertTo-Json -Compress
          $browserServer=[CodexifyMonitorInventoryFixture]::new($health,'/v1/health','fixture-browser-token')
          [IO.File]::WriteAllText($descriptor,(@{endpoint=('http://127.0.0.1:'+$browserServer.Port);token='fixture-browser-token'}|ConvertTo-Json -Compress))
          $expectedAuthentication=$browserFixture.expected
        }
        if($case -like 'unavailable-team*'){
          $null=New-Item -ItemType Directory -Path (Join-Path $root 'agents') -Force
          $partial=if($case -eq 'unavailable-team-partial'){'true'}else{'false'}
          $helper=@'
export async function agent(request) {
 if(request.action==='list')return {teams:[{teamId:'good'},{teamId:'bad'}],hasMore:false};
 if(request.action!=='status')throw new Error('Monitor attempted a mutation');
 if(PARTIAL && request.teamId==='bad')throw new Error('Fixture unavailable status');
 return {team:{capacity:{active:1,limit:null},workers:[{taskKey:request.teamId,role:'fixture',goal:{status:'blocked',revision:2},stopped:true,toolCallCount:7,queuePosition:null}]}};
}
'@
          [IO.File]::WriteAllText((Join-Path $root 'agents\client.mjs'),$helper.Replace('PARTIAL',$partial))
        }
        $snapshot=Join-Path $root ('snapshot-'+$case+'-'+$compact+'.json')
        $arguments=@('-NoLogo','-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass',
          '-File',$wrapper,'-Script',$script,'-Root',$root,'-Port',$unusedPort,'-SnapshotPath',$snapshot)
        if($case -like 'unavailable*'){$arguments+=@('-FollowAgentsOfChatPrefix','a')}
        if($case -eq 'targeted-history'){$arguments+=@('-FollowAgentsOfChatPrefix','c')}
        if($compact){$arguments+=@('-Compact')}
        $output=& $Engine @arguments
        $code=$LASTEXITCODE
        if($server){
          if(-not $server.Completion.Wait(5000)){throw 'Inventory fixture did not finish its request'}
          $null=$server.Completion.GetAwaiter().GetResult()
        }
        if($browserServer){
          if(-not $browserServer.Completion.Wait(5000)){throw 'Browser fixture did not finish its request'}
          $null=$browserServer.Completion.GetAwaiter().GetResult()
        }
        $expectedCode=0;if($case -like 'unavailable*'){$expectedCode=1}
        if($code -ne $expectedCode){throw "$case inventory must return exit $expectedCode, got $code"}
        $shown=$output|ConvertFrom-Json
        $saved=Get-Content -LiteralPath $snapshot -Raw|ConvertFrom-Json
        foreach($result in @($shown,$saved)){
          if(-not $result.PSObject.Properties['server'] -or -not $result.PSObject.Properties['browser']){throw 'Independent health diagnostics were lost'}
          $authentication=if($result.browser -is [string]){$result.browserAuthentication}else{$result.browser.authenticationState}
          if($authentication -ne $expectedAuthentication){throw "Unexpected browser authentication observation for $case"}
          if($case -like 'unavailable*'){
            if($result.inventoryAvailable -ne $false -or -not $result.inventoryError){throw 'Inventory failure was not explicit'}
            if($null -ne $result.agents -or $null -ne $result.uninitializedAgents){throw 'Unknown inventory became an empty or guessed team'}
            if($case -like 'unavailable-team*'){
              if(-not $result.agentTeams.available -or -not $result.agentTeams.discoveryComplete -or $result.agentTeams.teams.Count -ne 2){throw 'Independent team inventory was lost'}
              $good=$result.agentTeams.teams[0]
              if(-not $good.statusAvailable -or -not $good.workers[0].stopped -or $good.workers[0].goalRevision -ne 2){throw 'Published stop/revision observation changed'}
              if($case -eq 'unavailable-team-partial'){
                $bad=$result.agentTeams.teams[1]
                if($result.agentTeams.allStatusesAvailable -or $bad.statusAvailable -or $null -ne $bad.workers -or -not $bad.error){throw 'Partial team status became empty success or was discarded'}
              }elseif(-not $result.agentTeams.allStatusesAvailable){throw 'Complete published team statuses were lost'}
            }elseif($result.agentTeams.available){throw 'Missing agent helper became available'}
          }else{
            if($result.inventoryAvailable -ne $true -or $result.inventoryError){throw 'Successful inventory was reported unknown'}
            $expectedKeys=switch($case){
              'single' {'a'}
              'current' {'1';'2';'3';'a';'f'}
              'targeted-history' {'c'}
            }
            $actualKeys=@($result.agents|ForEach-Object {$_.chat.Substring(0,1)}|Sort-Object)
            if($result.agents -isnot [array] -or ($actualKeys -join ',') -ne ($expectedKeys -join ',')){
              throw "$case inventory selected unexpected chats: $($actualKeys -join ',')"
            }
            if($case -eq 'targeted-history' -and ($result.agents[0].action -ne 'unverified' -or -not $result.agents[0].sendAccepted)){
              throw 'Targeted history lost the accepted pairing receipt'
            }
            if($result.uninitializedAgents -isnot [array] -or $result.uninitializedAgents.Count -ne 0){throw 'Known empty traversal lost its array shape'}
          }
        }
      } finally {
        if($server){$server.Dispose()}
        if($browserServer){$browserServer.Dispose()}
        if(Test-Path -LiteralPath $descriptor){Remove-Item -LiteralPath $descriptor}
      }
    }
  }
  'PASS: 14 cases; authenticated inventory, current and targeted history, published-team fallback, full/compact JSON, browser authentication and exit codes'
} finally {
  $listener.Stop()
  $resolved=[IO.Path]::GetFullPath($root)
  if(-not $resolved.StartsWith($base,[StringComparison]::OrdinalIgnoreCase) -or
      (Split-Path -Leaf $resolved) -notlike 'codexify-monitor-unavailable-*'){throw 'Unsafe test cleanup path'}
  Remove-Item -LiteralPath $resolved -Recurse -Force
}
