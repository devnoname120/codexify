# Entry-point coverage: every updater's own browser idle predicate must refuse
# a parallel pool whose free page reports idle while another page owns a Send.
$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'windows-agent-update-guards.ps1')

$script:fixtureRoot=Join-Path ([IO.Path]::GetTempPath()) ('codexify-pool-idle-'+[guid]::NewGuid().ToString('N'))
$null=New-Item -ItemType Directory -Path (Join-Path $script:fixtureRoot 'browser') -Force
@{endpoint='http://127.0.0.1:1';token='test-only';pid=$PID}|ConvertTo-Json|
  Set-Content -LiteralPath (Join-Path $script:fixtureRoot 'browser\descriptor.json')
$script:fixtureHealth=$null
function Invoke-RestMethod {
  param([Parameter(Position=0)]$Uri,$Headers,$TimeoutSec)
  if("$Uri" -like '*/v1/liveness'){return [pscustomobject]@{available=$true}}
  if("$Uri" -notlike '*/v1/health'){throw "Unexpected fixture request $Uri"}
  return $script:fixtureHealth
}

function Import-UpdaterFunction([string]$Script,[string]$Name) {
  $ast=[Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot $Script),[ref]$null,[ref]$null)
  $definition=$ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $Name},$true)
  if(-not $definition){throw "$Script does not define $Name"}
  return [scriptblock]::Create($definition.Body.GetScriptBlock().ToString())
}
function Read-SharedJson([string]$Path){Get-Content -LiteralPath $Path -Raw|ConvertFrom-Json}

$base=@{kind='codexify-browser-companion';status='idle';authenticated=$true;accountFingerprint='fixture';activeActionId=$null}
$pooledBusy=$base.Clone();$pooledBusy.actionCapacity=2;$pooledBusy.availableActionSlots=1;$pooledBusy.activeActionIds=@('accepted-action')
$pooledIdle=$base.Clone();$pooledIdle.actionCapacity=2;$pooledIdle.availableActionSlots=2;$pooledIdle.activeActionIds=@()
$listOnly=$base.Clone();$listOnly.activeActionIds=@('accepted-action')

try {
  $root=$script:fixtureRoot
  foreach($script in @('windows-install-local-update.ps1','windows-install-browser-patch.ps1')){
    $idle=Import-UpdaterFunction $script 'Browser-Idle'
    $script:fixtureHealth=[pscustomobject]$pooledBusy
    if(& $idle){throw "$script accepted a pool with an active action on another page"}
    $script:fixtureHealth=[pscustomobject]$listOnly
    if(& $idle){throw "$script ignored activeActionIds"}
    $script:fixtureHealth=[pscustomobject]$pooledIdle
    if(-not(& $idle)){throw "$script rejected a fully idle pool"}
    $script:fixtureHealth=[pscustomobject]$base
    if(-not(& $idle)){throw "$script rejected an idle single window"}
  }
  if(Test-BrowserPoolUnowned ([pscustomobject]$pooledBusy)){throw 'Shared guard accepted a partially owned pool'}
  $missingSlots=$base.Clone();$missingSlots.actionCapacity=2
  if(Test-BrowserPoolUnowned ([pscustomobject]$missingSlots)){throw 'Shared guard accepted a pool without slot accounting'}
  # The graceful shutdown helper must load the same guard it uses for its
  # initial and final rechecks.
  . (Join-Path $PSScriptRoot 'windows-browser-graceful-update.ps1')
  if(-not(Get-Command Stop-ManagedBrowserGracefully -ErrorAction SilentlyContinue)){throw 'Graceful helper did not load'}
  $graceful=Get-Content -LiteralPath (Join-Path $PSScriptRoot 'windows-browser-graceful-update.ps1') -Raw
  if(([regex]::Matches($graceful,'Test-BrowserPoolUnowned')).Count -lt 4){throw 'Graceful helper must check pool ownership before and after the stop intent'}
  'Updater pool idle guards: ok'
} finally {
  Remove-Item -LiteralPath $script:fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
}
