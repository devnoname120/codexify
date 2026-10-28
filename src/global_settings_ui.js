function startGlobalSettings({api,onPlan,bindBackdrop=()=>{},getChats=()=>[],onOpenChat=()=>{},onReadiness=()=>{}}) {
  const byId=id=>document.getElementById(id);
  const icons={
    general:'<path d="M12 3v3m0 12v3M3 12h3m12 0h3M5.6 5.6l2.1 2.1m8.6 8.6 2.1 2.1M5.6 18.4l2.1-2.1m8.6-8.6 2.1-2.1"/><circle cx="12" cy="12" r="4"/>',
    chats:'<path d="M20 15a3 3 0 0 1-3 3H8l-5 3V6a3 3 0 0 1 3-3h11a3 3 0 0 1 3 3z"/><path d="M7 8h9M7 12h6"/>',
    notifications:'<path d="M18 8a6 6 0 0 0-12 0c0 7-3 7-3 9h18c0-2-3-2-3-9M10 21h4"/>',
    browser:'<rect x="3" y="3" width="18" height="18" rx="3"/><path d="M3 8h18M7 5.5h.01M10 5.5h.01M10 12l-3 3 3 3m4-6 3 3-3 3"/>',
    workspaces:'<path d="M3 7V5a2 2 0 0 1 2-2h5l2 3h7a2 2 0 0 1 2 2v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7z"/>',
    connections:'<path d="M8 3v5m8-5v5M6 8h12v3a6 6 0 0 1-12 0V8zm6 9v4"/>',
    tools:'<path d="m8 7-5 5 5 5m8-10 5 5-5 5m-3-13-2 20"/>',
    advanced:'<path d="M4 6h16M4 12h16M4 18h16"/><circle cx="8" cy="6" r="2"/><circle cx="16" cy="12" r="2"/><circle cx="10" cy="18" r="2"/>',
    search:'<circle cx="10.5" cy="10.5" r="6.5"/><path d="m16 16 5 5"/>',
    close:'<path d="m6 6 12 12M6 18 18 6"/>',
    code:'<path d="m8 6-6 6 6 6m8-12 6 6-6 6m-3-14-2 16"/>',
    arrow:'<path d="m9 5 7 7-7 7"/>',
    check:'<path d="m5 12 4 4L19 6"/>',
    extras:'<path d="m12 3 2.7 5.5 6.1.9-4.4 4.3 1 6.1L12 17l-5.4 2.8 1-6.1-4.4-4.3 6.1-.9Z"/>',
    analytics:'<path d="M4 20V10m8 10V4m8 16v-7M2 20h20"/>',
    restart:'<path d="M3 10a9 9 0 1 1 1 8M3 3v7h7"/>',
  };
  const icon=name=>`<svg viewBox="0 0 24 24" aria-hidden="true">${icons[name]||icons.advanced}</svg>`;
  const style=document.createElement('style');
  style.textContent=`
#global-footer{border-top:1px solid var(--line);display:flex;align-items:center;gap:12px;min-height:52px;padding:10px 16px;font-size:15px;line-height:20px;color:var(--muted);background:var(--panel)}
#account-plan{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-weight:500;color:var(--ink)}
#global-footer-actions{display:flex;align-items:center;gap:6px;flex:none;margin-left:auto}
#global-version{white-space:nowrap;font-variant-numeric:tabular-nums}
:is(#global-settings,#global-statistics,#global-install){display:grid;place-items:center;flex:none;width:32px;height:32px;padding:0;border:0;border-radius:8px;background:transparent;color:var(--muted);cursor:pointer}
:is(#global-settings,#global-statistics,#global-install):hover{background:var(--bg);color:var(--ink)}
:is(#global-settings,#global-statistics,#global-install) svg{display:block;width:18px;height:18px;fill:none;stroke:currentColor;stroke-width:1.6;stroke-linecap:round;stroke-linejoin:round}
#global-update{display:inline-flex;align-items:center;justify-content:center;min-height:32px;margin-left:4px;border:0;background:var(--selected);color:var(--accent);padding:0 10px;border-radius:8px;font:inherit;font-size:13px;line-height:20px;cursor:pointer}
#global-update[aria-busy=true]::before,.global-progress-spinner{content:"";display:inline-block;flex:none;width:14px;height:14px;margin-right:6px;border:2px solid currentColor;border-right-color:transparent;border-radius:50%;animation:global-progress-spin .8s linear infinite}
@keyframes global-progress-spin{to{transform:rotate(360deg)}}
body.system-progress-visible{height:100dvh;display:flex;flex-direction:column}body.system-progress-visible #layout{flex:1;min-height:0;height:auto}
#global-progress{display:flex;align-items:center;gap:10px;flex:none;padding:10px max(16px,env(safe-area-inset-right)) 10px max(16px,env(safe-area-inset-left));border-bottom:1px solid var(--line);background:var(--selected);color:var(--ink);font-size:13px;line-height:1.4}
#global-progress[hidden]{display:none}#global-progress-copy{flex:1;min-width:0}#global-progress-title{font-weight:600}#global-progress-detail{color:var(--muted);font-size:12px}#global-progress[data-error=true]{background:var(--panel);color:#b42328}#global-progress button{flex:none;border:1px solid var(--line);border-radius:7px;background:var(--panel);color:var(--ink);padding:6px 10px;font-size:12px}#global-progress-close{border:0!important;background:transparent!important;font-size:18px!important}
#global-footer[data-maintenance=true]{flex-wrap:wrap}#global-footer[data-maintenance=true] #global-footer-actions{min-width:0;flex-wrap:wrap}
@media(max-width:760px),(hover:none) and (pointer:coarse){body.system-progress-visible #layout{top:calc(var(--owner-viewport-top,0px) + var(--system-progress-height,60px));height:calc(var(--owner-viewport-height,100dvh) - var(--system-progress-height,60px))}#global-progress{position:fixed;z-index:10;top:var(--owner-viewport-top,0px);left:0;width:100%;gap:7px;padding:8px max(12px,env(safe-area-inset-right)) 8px max(12px,env(safe-area-inset-left));padding-top:max(8px,env(safe-area-inset-top));font-size:12px}#global-progress-detail{font-size:11px}#global-progress button{padding:5px 7px;font-size:11px}}
@media(prefers-reduced-motion:reduce){.global-progress-spinner,#global-update[aria-busy=true]::before{animation:none}}
@media(prefers-color-scheme:dark){#global-progress[data-error=true]{color:#ff989b}}
#global-footer [hidden]{display:none!important}
#global-footer button:focus-visible{outline:2px solid var(--accent);outline-offset:2px}
@media(max-width:760px),(hover:none) and (pointer:coarse){#global-footer{flex:none;gap:8px;min-height:49px;padding:2px 14px;font-size:13px}#global-footer-actions{gap:4px}:is(#global-settings,#global-statistics,#global-install){width:44px;height:44px}#global-version{font-size:12px}#global-update{font-size:12px}}
#settings-dialog{--settings-error:#b42328;--settings-shadow:0 24px 90px #0003;width:min(1040px,calc(100vw - 48px));height:min(790px,calc(100dvh - 64px));max-width:none;max-height:none;margin:auto;padding:0;border:1px solid var(--line);border-radius:20px;background:var(--panel);color:var(--ink);box-shadow:var(--settings-shadow);font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;overflow:hidden}
#settings-dialog::backdrop{background:#10191673;backdrop-filter:blur(5px)}
#settings-dialog [hidden]{display:none!important}
#settings-dialog .settings-shell{height:100%;display:flex;flex-direction:column;min-height:0}
#settings-dialog svg{width:18px;height:18px;flex:none;fill:none;stroke:currentColor;stroke-width:1.6;stroke-linecap:round;stroke-linejoin:round}
#settings-dialog button{display:inline-flex;align-items:center;justify-content:center;gap:7px;border:1px solid var(--line);border-radius:9px;padding:8px 12px;background:var(--panel);color:var(--ink);font:inherit;line-height:1.3;cursor:pointer;transition:background .12s,border-color .12s}
#settings-dialog button:hover{background:var(--bg)}
#settings-dialog button:disabled{opacity:.5;cursor:default}
#settings-dialog :is(button,input,select,textarea):focus-visible{outline:2px solid var(--accent);outline-offset:2px}
#settings-dialog .settings-icon-button{width:34px;height:34px;padding:7px;background:transparent;border-color:transparent}
#settings-toolbar{display:flex;align-items:center;gap:12px;flex:none;padding:22px 26px;border-bottom:1px solid var(--line);background:var(--panel)}
#settings-toolbar .settings-heading-copy{margin-right:auto;min-width:0}
#settings-heading{margin:0;font-size:21px;line-height:1.3;letter-spacing:-.45px;font-weight:650}
#settings-toolbar p{margin:4px 0 0;color:var(--muted);font-size:12px}
#settings-dialog .settings-body{display:grid;grid-template-columns:216px minmax(0,1fr);flex:1;min-height:0}
#settings-sidebar{min-width:0;padding:20px 12px;background:var(--bg);border-right:1px solid var(--line);overflow:auto}
#settings-dialog .settings-search{display:flex;align-items:center;gap:7px;border:1px solid var(--line);border-radius:9px;padding:0 9px;background:var(--panel);margin:0 2px 18px;color:var(--muted)}
#settings-search{width:100%;min-width:0;padding:10px 0;border:0;outline:0;background:transparent;color:var(--ink);font:inherit;font-size:12px}
#settings-dialog .settings-search:focus-within{outline:2px solid var(--accent);outline-offset:2px}
#settings-dialog #settings-search:focus-visible{outline:0}
#settings-nav{min-width:0;display:flex;flex-direction:column;gap:3px}
#settings-nav button{justify-content:flex-start;width:100%;border:0;background:transparent;color:var(--muted);padding:10px 12px;white-space:nowrap;font-size:13px}
#settings-nav button:hover{background:var(--panel);color:var(--ink)}
#settings-nav button[aria-current=true]{background:var(--selected);color:var(--accent);font-weight:600}
#settings-main{display:block;overflow:auto;min-width:0;overscroll-behavior:contain;scrollbar-gutter:stable;padding:26px 30px 32px}
#settings-content-heading{margin:0;font-size:23px;letter-spacing:-.5px;font-weight:650;line-height:1.25}
#settings-content-description{margin:7px 0 24px;color:var(--muted);font-size:13px;max-width:520px}
#settings-dialog .settings-card{border:1px solid var(--line);border-radius:12px;background:var(--panel);margin:0 0 18px;overflow:hidden}
#settings-dialog .settings-card-heading{padding:14px 18px;background:var(--bg);border-bottom:1px solid var(--line);margin:0;font-size:13px;font-weight:600}
#settings-dialog .settings-browser-heading{display:flex;align-items:center;justify-content:space-between;gap:10px;flex-wrap:wrap}#settings-dialog .settings-browser-heading h4{font:inherit;margin:0}#settings-browser-state{margin:0;font-size:12px;font-weight:400;color:var(--muted)}#settings-browser-state[data-state=connected]{color:var(--accent)}#settings-browser-state[data-state=unavailable]{color:var(--settings-error)}#settings-browser-state:before{content:'';display:inline-block;width:6px;height:6px;margin-right:6px;border-radius:50%;background:currentColor;vertical-align:middle}
#settings-dialog .settings-browser-body{padding:16px 18px}#settings-dialog .settings-browser-copy{margin:0 0 12px;color:var(--muted);font-size:12px;line-height:1.5}#settings-dialog .settings-browser-actions{display:flex;flex-wrap:wrap;gap:8px}#settings-dialog .settings-browser-actions button{font-size:13px}#settings-browser-feedback{margin:12px 0 0;font-size:12px;line-height:1.5;overflow-wrap:anywhere;color:var(--muted)}#settings-browser-feedback[data-error=true]{color:var(--settings-error)}
@media(max-width:760px){#settings-dialog .settings-browser-body{padding:10px 12px}#settings-dialog .settings-browser-actions{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:6px}#settings-dialog .settings-browser-actions button{font-size:12px;padding:8px}#settings-browser-state{font-size:11px}}
#settings-dialog .setting-row{padding:16px 18px;display:grid;grid-template-columns:minmax(0,1fr) minmax(155px,230px);gap:20px;align-items:center}
#settings-dialog .setting-row+.setting-row{border-top:1px solid var(--line)}
#settings-dialog .setting-row.setting-wide{grid-template-columns:1fr;gap:10px}
#settings-dialog .setting-label{display:block;font-weight:500;font-size:13px;line-height:1.4;cursor:default}
#settings-dialog .setting-help{margin:4px 0 0;color:var(--muted);font-size:12px;line-height:1.5;overflow-wrap:anywhere}
#settings-dialog .setting-input{width:100%;min-width:0;border:1px solid var(--line);border-radius:8px;padding:9px 10px;background:var(--bg);color:var(--ink);font:inherit;font-size:13px;line-height:1.4}
#settings-dialog .setting-input::placeholder{color:var(--muted);opacity:1}
#settings-dialog .setting-input[data-default=true],#settings-dialog .setting-boolean button[data-default=true][aria-pressed=true]{color:var(--muted)}
#settings-dialog select.setting-input{cursor:pointer;padding-right:24px}
#settings-dialog textarea.setting-input{resize:vertical;min-height:96px;max-height:340px;font:12px/1.6 ui-monospace,SFMono-Regular,Menlo,monospace}
#settings-dialog [aria-invalid=true]{border-color:var(--settings-error)}
#settings-dialog .setting-boolean{display:flex;border:1px solid var(--line);border-radius:8px;padding:3px;background:var(--bg)}
#settings-dialog .setting-boolean button{border:0;border-radius:5px;flex:1;font-size:12px;padding:7px 9px;color:var(--muted);background:transparent}
#settings-dialog .setting-boolean button[aria-pressed=true]{color:var(--accent);background:var(--panel);box-shadow:0 1px 4px #0001;font-weight:600}
#settings-dialog .setting-hint-override button:first-child{flex:1.6}
#settings-dialog .setting-editor-tools{display:flex;align-items:center;gap:8px;margin-top:8px;font-size:12px;color:var(--muted)}
#settings-dialog .setting-editor-tools button,#settings-dialog .setting-reset{border:0;background:transparent;color:var(--muted);font-size:11px;padding:2px 0;border-radius:3px}
#settings-dialog .setting-editor-tools button:hover,#settings-dialog .setting-reset:hover{color:var(--accent)}
.setting-turn-limits{display:grid;gap:10px;width:100%}.setting-turn-limit{display:grid;grid-template-columns:minmax(0,1fr) 80px 28px;align-items:center;gap:8px;font-size:13px}.setting-turn-limit input{min-width:0;width:100%;box-sizing:border-box}.setting-turn-limit-label{display:flex;align-items:center;gap:9px;cursor:pointer;min-width:0}.setting-turn-limit-label span{overflow-wrap:anywhere}.setting-turn-limit input[type=checkbox]{width:16px;height:16px;margin:0;flex:none;accent-color:var(--accent);cursor:pointer}.setting-turn-limit[data-enabled=false] .setting-turn-limit-label{color:var(--muted)}#settings-dialog .setting-control .setting-reset{margin-top:6px}
#settings-dialog .setting-object-summary{display:flex;align-items:center;justify-content:space-between;gap:12px;color:var(--muted);font-size:12px}
#settings-dialog .setting-object-summary button{font-size:12px;white-space:nowrap}
#settings-dialog .cutoff-body{padding:14px 18px}#settings-dialog .cutoff-body p{margin:0 0 12px}#settings-dialog .cutoff-body p:last-child{margin-bottom:0}#settings-dialog .cutoff-scroll{overflow-x:auto;margin:14px 0}#settings-dialog .cutoff-scroll table{width:100%;border-collapse:collapse;font-size:12px;font-variant-numeric:tabular-nums}#settings-dialog .cutoff-scroll :is(th,td){text-align:right;white-space:nowrap;padding:9px 8px;border-bottom:1px solid var(--line)}#settings-dialog .cutoff-scroll :is(th,td):first-child{text-align:left;white-space:normal;overflow-wrap:anywhere;min-width:120px}#settings-dialog .cutoff-scroll th{color:var(--muted);font-weight:500}
#settings-dialog.analytics-view{width:min(1280px,calc(100vw - 48px))}#settings-nav button[data-category=analytics]{margin-top:18px;border-top:1px solid var(--line);border-radius:0;padding-top:18px}#settings-dialog.analytics-view #settings-content-description{margin-bottom:18px}
@media(max-width:680px){#settings-nav button[data-category=analytics]{margin-top:0;border-top:0;padding-top:10px}}
#settings-dialog .settings-empty{padding:40px 20px;text-align:center;color:var(--muted);border:1px dashed var(--line);border-radius:12px}
#settings-dialog .settings-empty p{margin:8px 0 0;font-size:13px}
#settings-raw-panel p{color:var(--muted);font-size:13px;margin:0 0 16px}
#settings-raw{width:100%;height:max(240px,calc(100dvh - 380px));max-height:460px;resize:vertical;border:1px solid var(--line);border-radius:10px;padding:16px;background:var(--bg);color:var(--ink);font:12px/1.7 ui-monospace,SFMono-Regular,Menlo,monospace;tab-size:2}
#settings-dialog .settings-raw-actions{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-top:14px}
#settings-file{color:var(--muted);font-size:11px;overflow-wrap:anywhere}
#settings-dialog .settings-footer{display:flex;align-items:center;justify-content:space-between;gap:16px;padding:16px 26px;border-top:1px solid var(--line);background:var(--panel);flex:none;min-height:70px}
#settings-save-state{margin:0;display:flex;align-items:center;gap:7px;color:var(--muted);font-size:12px;line-height:1.45}
#settings-save-state[data-error=true]{color:var(--settings-error)}
#settings-dialog .settings-maintenance{display:flex;flex-wrap:wrap;align-items:center;justify-content:flex-end;gap:8px;flex:none}
#settings-dialog .settings-primary{background:var(--accent);border-color:var(--accent);color:var(--panel)}
#settings-dialog .settings-primary:hover{filter:brightness(.95);background:var(--accent)}
@media(prefers-color-scheme:dark){#settings-dialog{--settings-error:#ff989b;--settings-shadow:0 24px 90px #0008}}
@media(max-width:760px){#settings-dialog{width:calc(100vw - 24px);height:calc(100dvh - 32px);border-radius:15px}#settings-toolbar{padding:18px;gap:8px}#settings-toolbar p{font-size:11px}#settings-raw-toggle{font-size:12px!important;padding:8px!important}#settings-raw-toggle svg{display:none}#settings-dialog .settings-body{grid-template-columns:minmax(0,1fr);grid-template-rows:auto minmax(0,1fr)}#settings-sidebar{min-width:0;padding:12px 14px 8px;border-right:0;border-bottom:1px solid var(--line);overflow:visible}#settings-dialog .settings-search{margin:0 0 8px}#settings-nav{flex-direction:row;overflow:auto;gap:4px;padding-bottom:4px}#settings-nav button{width:auto;padding:8px 10px;font-size:12px}#settings-nav svg{width:15px;height:15px}#settings-main{padding:22px 18px}#settings-content-heading{font-size:20px}#settings-content-description{margin-bottom:18px}#settings-dialog .setting-row{grid-template-columns:1fr;gap:10px;padding:15px}#settings-dialog .setting-boolean{max-width:250px}#settings-dialog .settings-footer{padding:12px 18px;align-items:flex-start;flex-direction:column;gap:10px}#settings-dialog .settings-maintenance{justify-content:flex-start;width:100%;gap:6px}#settings-dialog .settings-maintenance button{font-size:12px;padding:8px 9px}#settings-dialog .settings-raw-actions{align-items:flex-start;flex-direction:column}}
#settings-dialog.analytics-view .settings-body{grid-template-columns:minmax(0,1fr);grid-template-rows:minmax(0,1fr)}
#settings-dialog.analytics-view #settings-sidebar,#settings-dialog.analytics-view .settings-footer,#settings-dialog.analytics-view #settings-content-heading,#settings-dialog.analytics-view #settings-content-description{display:none}
#settings-dialog.analytics-view #settings-toolbar{padding:14px 22px}
#settings-dialog.analytics-view #settings-toolbar p{display:none}
#settings-dialog.analytics-view #settings-heading{font-size:16px}
#settings-analytics-back svg{transform:rotate(180deg)}
#settings-dialog.analytics-view #settings-main{padding:20px 24px 28px}
@media(max-width:680px){#settings-dialog.analytics-view #settings-main{padding:12px}#settings-dialog.analytics-view #settings-toolbar{padding:12px}}

@media(max-width:760px){
 #settings-toolbar{padding:12px 14px;gap:6px}#settings-heading{font-size:18px}#settings-toolbar p{display:none}#settings-raw-toggle{width:32px;height:32px;padding:6px!important}#settings-raw-toggle svg{display:block;width:17px;height:17px}#settings-raw-toggle span{display:none}#settings-dialog .settings-icon-button{width:32px;height:32px;padding:6px}
 #settings-sidebar{padding:8px 12px 5px}#settings-dialog .settings-search{margin-bottom:5px}#settings-search{padding:7px 0;font-size:13px}#settings-nav{gap:2px;padding-bottom:2px}#settings-nav button{padding:6px 8px;font-size:12px;min-height:32px}
 #settings-main{padding:14px 12px 18px}#settings-content-heading{font-size:19px}#settings-content-description{font-size:12px;margin:5px 0 14px}#settings-dialog .settings-card{margin-bottom:12px}#settings-dialog .settings-card-heading{padding:10px 12px;font-size:12px}
 #settings-dialog .setting-row{padding:10px 12px;gap:6px 10px;grid-template-columns:minmax(0,1fr) minmax(128px,46%)}#settings-dialog .setting-row.setting-wide{grid-template-columns:1fr}#settings-dialog .setting-label{font-size:12px}#settings-dialog .setting-help{font-size:11px;line-height:1.35;margin-top:3px}#settings-dialog .setting-input{padding:7px 9px;line-height:1.4;min-height:38px}#settings-dialog textarea.setting-input{min-height:42px;height:54px;max-height:220px;font-size:13px;line-height:1.4}#settings-dialog .setting-boolean button{padding:6px 4px;font-size:11px;min-height:32px}#settings-dialog :is(input,select,textarea){font-size:13px!important}#settings-search{font-size:13px!important}#settings-dialog textarea.setting-input{font-family:inherit}#settings-dialog .setting-editor-tools{margin-top:4px}#settings-dialog .setting-reset{font-size:10px}
 #settings-dialog .settings-footer{padding:8px 12px;min-height:0;gap:5px}#settings-save-state{font-size:11px}#settings-dialog .settings-maintenance{gap:5px}#settings-dialog .settings-maintenance button{padding:6px 8px;font-size:11px;min-height:32px}
}

@media(prefers-reduced-motion:reduce){#settings-dialog button{transition:none}}
`;
  document.head.append(style);
  const dialog=document.createElement('dialog');dialog.id='settings-dialog';dialog.setAttribute('aria-labelledby','settings-heading');
  dialog.innerHTML=`<div class="settings-shell"><header id="settings-toolbar"><button type="button" id="settings-analytics-back" class="settings-icon-button" aria-label="Back to settings" hidden>${icon('arrow')}</button><div class="settings-heading-copy"><h2 id="settings-heading">Codexify settings</h2><p>Changes save automatically. Some settings require a restart.</p></div><button type="button" id="settings-raw-toggle" aria-label="View raw config">${icon('code')}<span>View raw config</span></button><button type="button" id="settings-close" class="settings-icon-button" aria-label="Close settings">${icon('close')}</button></header><div class="settings-body"><nav id="settings-sidebar" aria-label="Settings categories"><label class="settings-search">${icon('search')}<input id="settings-search" type="search" placeholder="Search settings" aria-label="Search settings"></label><div id="settings-nav"></div></nav><main id="settings-main"><h3 id="settings-content-heading"></h3><p id="settings-content-description"></p><div id="settings-form"></div><div id="settings-raw-panel" hidden><p>Edit the complete JSON configuration. Changes are validated before saving.</p><textarea id="settings-raw" aria-label="Raw configuration JSON" spellcheck="false"></textarea><div class="settings-raw-actions"><span id="settings-file"></span><button type="button" id="settings-save-raw" class="settings-primary">Save raw config</button></div></div></main></div><footer class="settings-footer"><p id="settings-save-state" role="status"></p><div class="settings-maintenance"><button type="button" id="settings-reload" hidden>Reload settings</button><button type="button" id="settings-refresh-connector">Refresh connector</button><button type="button" id="settings-update" hidden>Update Codexify</button><button type="button" id="settings-restart">${icon('restart')}Restart Codexify</button></div></footer></div>`;
  document.body.append(dialog);
  const progress=document.createElement('section');progress.id='global-progress';progress.hidden=true;progress.setAttribute('role','status');progress.setAttribute('aria-live','polite');
  progress.innerHTML='<span class="global-progress-spinner" aria-hidden="true"></span><div id="global-progress-copy"><div id="global-progress-title"></div><div id="global-progress-detail"></div></div><button type="button" id="global-progress-start" hidden>Start browser</button><button type="button" id="global-progress-action" hidden></button><button type="button" id="global-progress-close" aria-label="Dismiss update status" hidden>×</button>';
  document.body.prepend(progress);
  const progressSize=typeof ResizeObserver==='function'?new ResizeObserver(()=>{document.documentElement.style.setProperty('--system-progress-height',`${progress.hidden?0:progress.getBoundingClientRect().height}px`);}):null;
  progressSize?.observe(progress);
  const categories=[
    {id:'general',name:'General',description:'Project access, server preferences, and the chat interface.'},
    {id:'chats',name:'Chats & goals',description:'Configure chat delivery and the automation queue.'},
    {id:'notifications',name:'Notifications',description:'Choose which events reach you and where notifications are delivered.'},
    {id:'browser',name:'Browser',description:'Connect ChatGPT and control the browser companion.'},
    {id:'workspaces',name:'Workspaces',description:'Manage worktrees, project discovery, instructions, and skills.'},
    {id:'connections',name:'Connections',description:'Manage authentication, MCP servers, and OpenAI tunnels.'},
    {id:'tools',name:'Tools & files',description:'Set command limits and control file access, imports, and exports.'},
    {id:'advanced',name:'Advanced',description:'Logging, diagnostics, memory, and experimental features.'},
    {id:'extras',name:'Extras',description:'Make room for a little celebration. Statistics and sharing always stay available.'},
    {id:'analytics',name:'Analytics',description:'Turns, tools, goals, and the momentum behind your conversations.'},
  ];
  const groups={general:'Preferences',agentChat:'Chat server',worktrees:'Worktrees',projectCatalog:'Project catalogue',projectDoc:'Project instructions',skills:'Skills',codexMcp:'Imported MCP servers',tree:'Directory tree',ignore:'Ignored files',command:'Command limits',exec:'Command sessions',output:'Output limits',filesystem:'Filesystem limits',diff:'Diff limits',artifactIngress:'File imports',artifactEgress:'File exports',toolLogging:'Tool logging',audit:'Audit log',memory:'Memory',experimental:'Experimental features'};
  const names={workDir:'Project access directory',projectCloneDir:'Git clone directory',uiWidgets:'Embedded chat widgets',apiKey:'API key',conversationAuthToken:'Conversation setup reference',openaiTunnel:'Primary OpenAI tunnel',openaiTunnels:'Additional OpenAI tunnels',mcpServers:'MCP servers',allowedHosts:'Allowed host names',debug:'Debug mode'};
  const annotationHintOverrides=new Set(['experimental.forceDestructiveToolAnnotations','experimental.forceIdempotentToolAnnotations','experimental.forceOpenWorldToolAnnotations']);
  const metadata={
    instanceName:['Optional installation label shown by connected clients. Leave blank to keep the existing appearance. This does not rename your ChatGPT connector.','general','Instance name'],
    'artifactEgress.enabled':['Let ChatGPT materialize and offer downloads of project files and full attachments. ChatGPT may ask permission for each file. Off by default; inline attachment text and image previews remain available. Restart Codexify to apply.','tools','File materialization in ChatGPT'],
    'audit.enabled':['Record bounded, redacted tool activity in a private local audit log.','advanced','Audit logging'],
    'audit.logFile':['Optional JSONL destination. Leave blank to use the per-project default shown in the field. Ignored when Audit logging is off.','advanced','Log file'],
    'agentChat.automation.requestBlockingEnabled':['Block matching requests before they leave the browser. Off by default. Use Restart Codexify to apply.','browser','Block matching requests'],
    'agentChat.extras.achievements':['Celebrate newly earned badges. Your badge collection stays in Statistics. Applies immediately.','extras','Achievement celebrations'],
    'agentChat.extras.animations':['Colorful bursts and motion, lasting a few seconds. Respects reduced motion. Applies immediately.','extras','Animations'],
    'agentChat.extras.sounds':['Brief fireworks and a musical flourish for celebrations. Applies immediately.','extras','Celebration sounds'],
    'agentChat.extras.weeklyRecap':['Show your completed Monday–Sunday recap once a week. You can always open it in Statistics. Applies immediately.','extras','Weekly recap'],
    'agentChat.extras.dailyStreaks':['A small, non-blocking celebration for each active day. Streak statistics stay available. Applies immediately.','extras','Daily streak celebrations'],
    'agentChat.extras.personalRecords':['Celebrate each conversation or goal only the first time it breaks a record. Applies immediately.','extras','Personal record celebrations'],
    'experimental.forceReadOnlyToolAnnotations':['When enabled, set only readOnlyHint to true for every advertised tool. Other hints are unchanged.','advanced'],
    'experimental.forceDestructiveToolAnnotations':['Set destructiveHint on every advertised tool. The default sets it to false.','advanced','Destructive hint override'],
    'experimental.forceIdempotentToolAnnotations':['Set idempotentHint on every advertised tool. The default preserves existing hints.','advanced','Idempotent hint override'],
    'experimental.forceOpenWorldToolAnnotations':['Set openWorldHint on every advertised tool. The default sets it to false.','advanced','Open-world hint override'],
    workDir:['Project directory Codexify can access.','general'],projectCloneDir:['Where new Git repositories are cloned.','general'],uiWidgets:['Show embedded chat interfaces in tool responses.','general'],port:['HTTP port used by the server.','general'],debug:['Include diagnostic timing information.','advanced'],
    'agentChat.offlineTimeoutMinutes':['Minutes without an agent tool call before a chat is considered offline and automatic follow-ups may be sent. Whole minutes, from 1 to 1440. Restart to apply.','chats','Offline timeout (minutes)'],
    'agentChat.rolloutEnabled':['Keep chronological per-chat progress reports and checkpoint reminders. Independent of turn renewal. Applies immediately.','chats','Progress journal'],
    'agentChat.automation.automaticRetries':['Retry failed browser actions automatically. Turn off to keep manual Retry available without scheduling retries. Applies immediately.','chats','Automatic retries'],
    'agentChat.automation.retryTimings.actionDelaysSeconds':['Delays between retries when a browser action failed before sending. One delay per retry, in order; stop after the last entry. Use s, m, or h, for example 30s, 1m, 2m. Restart to apply.','chats','Failed send retry delays'],
    'agentChat.automation.retryTimings.chatgptAccessDelaysSeconds':['Retry delays for sign-in, inaccessible conversations, rate limits, and shared loading cooldowns. One delay per retry, then stop. Use s, m, or h. Restart to apply.','chats','ChatGPT access retry delays'],
    'agentChat.automation.retryTimings.goalRecoveryDelaysSeconds':['Delays before trying to recover a goal when the agent stays offline after a continuation. One delay per retry, then stop. Use s, m, or h. Restart to apply.','chats','Goal recovery retry delays'],
    'agentChat.automation.retryTimings.jitterPercent':['Add a random extra wait from 0 to this percentage of each retry delay and approval resume wait. Stable for each attempt; 0 disables jitter. Restart to apply.','chats','Retry jitter (%)'],
    'agentChat.automation.retryTimings.approvalResumeGraceSeconds':['Wait for agent activity after accepting a tool or file approval before considering another continuation. Seconds, plus retry jitter. Restart to apply.','chats','Wait after accepting approval (seconds)'],
    'agentChat.automation.retryTimings.conversationLoadIntervalSeconds':['Minimum time between conversation loads or reloads across the shared browser. Reusing the open conversation needs no load. Seconds. Restart to apply.','chats','Conversation loading interval (seconds)'],
    'agentChat.automation.retryTimings.conversationUnavailableCooldownSeconds':['Shared delay between conversation loads while a conversation is inaccessible. The normal loading interval also applies. Seconds. Restart to apply.','chats','Conversation error cooldown (seconds)'],
    'agentChat.automation.retryTimings.rateLimitCooldownSeconds':['Shared cooldown after an HTTP 429 without Retry-After. A server-provided Retry-After and the normal loading interval still apply. Seconds. Restart to apply.','chats','Rate limit fallback cooldown (seconds)'],

    'agentChat.turnRenewal.enabled':['Ask for a final handoff at the model limit, then continue automatically after the report finishes. Applies immediately.','chats','Turn renewal'],
    'agentChat.turnRenewal.limits':['Renew only checked model and effort pairs, measured from the last accepted ChatGPT send using the actual model. Unchecked and unlisted selections are left alone. Limits stay editable while off. Applies immediately.','chats','Turn limits by model'],
    'agentChat.enabled':['Enable conversations and agent messaging.','chats'], 'agentChat.rolloutReminderInterval':['Agent work tool calls between progress checkpoint reminders. UI polling and idle wait loops do not count. Applies immediately.','chats','Progress report interval'], 'agentChat.defaultMessageMode':['Default composer action. Cmd+Enter on Mac or Ctrl+Enter on Windows/Linux uses the other mode for one message.','chats','Default message mode'], 'agentChat.port':['Port for the owner chat interface.','chats'], 'agentChat.maxWaitMs':['Maximum duration of a chat_await or turn_completed call, in milliseconds.','chats'],
    'agentChat.automation.enabled':['Use the browser companion to send messages and continue goals.','browser'], 'agentChat.automation.connectorName':['The connector to mention when sending messages in ChatGPT.','browser'], 'agentChat.automation.chatgptProjectUrl':['ChatGPT project used for new conversations.','browser'], 'agentChat.automation.descriptorPath':['Location of the browser companion connection descriptor.','browser'], 'agentChat.automation.requestBlockList':['One case-sensitive Chrome regex per line. Matching requests never reach the server. Use Restart Codexify to apply browser policy changes.','browser','Blocked requests'], 'agentChat.automation.maxQueuedActions':['Maximum number of actions waiting to be sent.','chats'], 'agentChat.automation.autoApproveToolCalls':['Automatically approve tool calls in ChatGPT.','browser'],
    'agentChat.notifications.events.completed':['A chat or goal finishes.','notifications','Completion'], 'agentChat.notifications.events.inputRequired':['A chat needs your reply. Repeated alerts are deduplicated.','notifications','Input required'], 'agentChat.notifications.events.goalBlocked':['A goal becomes blocked.','notifications','Goal blocked'], 'agentChat.notifications.events.errors':['An error prevents a chat from continuing.','notifications','Chat errors'], 'agentChat.notifications.events.messages':['Notify for new agent messages.','notifications','New messages'], 'agentChat.notifications.events.offline':['Notify when an agent goes offline.','notifications','Agent offline'], 'agentChat.notifications.urls':['Apprise destination URLs, one per line.','notifications','Delivery destinations'], 'agentChat.notifications.pythonPath':['Python executable used to deliver notifications.','notifications','Python executable'], 'agentChat.notifications.timeoutMs':['Notification delivery timeout, in milliseconds.','notifications','Delivery timeout'],
    'worktrees.mode':['Automatically create worktrees, always require them, or disable them.','workspaces','Creation mode'], 'worktrees.root':['Directory containing managed worktrees.','workspaces','Worktree directory'], 'worktrees.autoCleanupEnabled':['Remove old worktrees automatically.','workspaces','Automatic cleanup'], 'worktrees.keepCount':['Number of worktrees to keep during automatic cleanup.','workspaces','Worktrees to keep'], 'worktrees.allowSetupScript':['Run configured setup scripts when creating a worktree.','workspaces','Run setup scripts'], 'worktrees.upstreamRefreshMode':['Choose whether to refresh upstream references.','workspaces','Upstream refresh'],
    apiKey:['Optional API authentication key.','connections'],conversationAuthToken:['Reference used when setting up conversations.','connections'],mcpServers:['Configure server commands, connections, and tool policies.','connections'],openaiTunnels:['Configure additional outbound tunnel connections.','connections'],openaiTunnel:['Configure the primary outbound tunnel connection.','connections'],allowedHosts:['HTTP Host names accepted by the server, one per line.','connections'],
  };
  const enums={
    'agentChat.defaultMessageMode':[['steer','Steer'],['queue','Queue']],'worktrees.mode':[['auto','Automatic'],['always','Always'],['never','Never']],'worktrees.upstreamRefreshMode':[['never','Never'],['best-effort','Best effort']],'toolLogging.mode':[['off','Off'],['requests','Requests'],['responses','Responses'],['all','All']],'toolLogging.level':[['trace','Trace'],['debug','Debug'],['info','Info'],['warn','Warning'],['error','Error']]};
  const label=key=>names[key]||key.replace(/([a-z])([A-Z])/g,'$1 $2').replace(/Ms$/,' (ms)').replace(/^./,letter=>letter.toUpperCase());
  const RESET=Symbol('default'),invalidFields=new Map(),invalidText=new Map();
  let snapshot=null,global=null,dirty={},saving=false,savePromise=null,saveTimer,maintenance=false,statusTimer,previousInstance=null,rawEdited=false,shownRefreshState=null,category='general',rawMode=false,fields=[],saveError=null,inFlightChanges={},refreshSubmitting=false,refreshTimer=null,refreshEpoch=0;
  let notice=null,noticeTimer=null,maintenanceKind=null,statusReading=false,lastStatusAt=0,connectionLost=false,dismissedUpdate=null;
  const updatePending=value=>['preparing','scheduled','installing','validating','restarting'].includes(value?.state);
  const updateFailure=value=>['failed','rolled_back'].includes(value?.state);
  const updateMessage=value=>({
    preparing:['Downloading and verifying update…','The update is being prepared. You can keep writing a draft.'],
    scheduled:['Update verified','Waiting for the updater to install the new version…'],
    installing:['Installing update…','Replacing the installed application with the verified release.'],
    validating:['Validating update…','Checking the installed application before restarting.'],
    restarting:['Restarting Codexify…','Waiting for Codexify and its MCP tools to start.'],
  }[value?.state]);
  function refreshNeedsRestart(refresh=global?.connectorRefresh) {
    return refresh?.phase==='waiting_for_restart'&&!refresh.updateId&&String(refresh.originPid??global?.instanceId)===String(global?.instanceId);
  }
  function renderRuntime() {
    const update=global?.update,readiness=global?.readiness,refresh=global?.connectorRefresh;
    const recoveringUpdate=Boolean(update?.state==='succeeded'&&update.targetVersion===global?.version&&['waiting_for_restart','starting_browser','queued','running'].includes(refresh?.phase)&&readiness?.loading);
    const updating=maintenanceKind==='update'||updatePending(update)||recoveringUpdate,busy=maintenance||updatePending(update)||recoveringUpdate;
    const message=updateMessage(update);
    let view=notice;
    if(notice?.guardSettings||notice?.error&&!updateFailure(update))view=notice;
    else if(connectionLost)view={title:'Starting Codexify…',detail:'Waiting for the service and MCP tools to finish loading.',loading:true};
    else if(message)view={title:message[0],detail:message[1],loading:true};
    else if(readiness?.loading)view={title:readiness.detail||'Loading Codexify…',detail:readiness.phase==='starting_browser'?'The server is back. Messages that need the browser will be available when it is ready.':'',loading:true};
    else if(['queued','starting_browser','running'].includes(refresh?.phase))view={title:refresh.phase==='starting_browser'?'Starting the browser companion…':refresh.phase==='running'?'Refreshing ChatGPT connector tools…':'Connector refresh queued…',detail:'Waiting for the browser to finish refreshing the connector tools.',loading:true};
    else if(!maintenance&&updateFailure(update)&&dismissedUpdate!==(update.updateId||update.failureDetail))view={title:update.state==='rolled_back'?'Update rolled back':'Update failed',detail:update.failureDetail||'The update could not complete. Try again or check the service logs.',error:true,dismiss:true,action:'update',label:'Retry update'};
    else if(!maintenance&&readiness&&['browser_stopped','sign_in_required','manual_refresh_required'].includes(readiness.phase))view={title:readiness.detail,action:'browser',label:'Browser settings'};
    else if(!maintenance&&refreshNeedsRestart(refresh))view={title:'Restart required to refresh tools',detail:'Your settings are saved. Restart Codexify to apply them; the connector will refresh automatically.',action:'restart',label:'Restart Codexify'};
    else if(!maintenance&&refresh?.phase==='failed')view={title:'Connector refresh failed',detail:refresh.result?.error||'Open settings to retry the connector refresh.',action:'settings',label:'Open settings',error:true};
    const blocked=busy||connectionLost?{loading:true,browserOnly:false,detail:view?.title}:readiness||{loading:false};
    onReadiness(blocked);
    progress.hidden=!view;document.body.classList.toggle('system-progress-visible',Boolean(view));
    if(view){byId('global-progress-title').textContent=view.title;byId('global-progress-detail').textContent=view.detail||'';progress.dataset.error=String(Boolean(view.error));progress.querySelector('.global-progress-spinner').hidden=!view.loading;byId('global-progress-close').hidden=!view.dismiss;const action=byId('global-progress-action');action.hidden=!view.action;action.textContent=view.label||'';action.dataset.action=view.action||'';action.disabled=maintenance;}
    const start=byId('global-progress-start');start.hidden=readiness?.phase!=='browser_stopped'||view?.action!=='browser';start.disabled=maintenance||Boolean(browserOperation);start.textContent=browserOperation==='start'?'Starting…':'Start browser';
    const button=byId('global-update');button.hidden=!global?.latest?.updateAvailable&&!updating&&!updateFailure(update);button.disabled=maintenance||updatePending(update);button.setAttribute('aria-busy',String(updating));button.textContent=updating?'Updating…':updateFailure(update)?'Retry update':'Update';button.title=view&&updating?`${view.title} ${view.detail||''}`:'Update Codexify';
    byId('settings-update').disabled=button.disabled;byId('global-footer').dataset.maintenance=String(busy);
    document.documentElement.style.setProperty('--system-progress-height',`${progress.hidden?0:progress.getBoundingClientRect().height}px`);
  }
  function runtimeNotice(title,detail='',error=false) {
    clearTimeout(noticeTimer);notice={title,detail,error,loading:maintenance,dismiss:!maintenance};renderRuntime();
    if(!maintenance&&!error)noticeTimer=setTimeout(()=>{notice=null;renderRuntime();},5000);
  }
  byId('global-progress-close').addEventListener('click',()=>{notice=null;dismissedUpdate=global?.update?.updateId||global?.update?.failureDetail;renderRuntime();});
  byId('global-progress-action').addEventListener('click',()=>{
    if(byId('global-progress-action').dataset.action==='update'){void maintain('update');return;}
    if(byId('global-progress-action').dataset.action==='restart'){void maintain('restart');return;}
    category=byId('global-progress-action').dataset.action==='browser'?'browser':'general';byId('global-settings').click();
  });
  byId('global-progress-start').addEventListener('click',async()=>{
    await controlBrowser('start');
    if(browserFeedbackError&&browserFeedbackOperation==='start')runtimeNotice('Browser startup failed',browserFeedback,true);
    else await pollStatus();
  });
  let browser=null,browserOperation=null,browserRevision=0,browserReading=false,browserFeedback='',browserFeedbackError=false,browserFeedbackOperation=null;
  const browserCard=document.createElement('section');browserCard.id='settings-browser';browserCard.className='settings-card';browserCard.setAttribute('aria-labelledby','settings-browser-heading');
  browserCard.innerHTML='<div class="settings-card-heading settings-browser-heading"><h4 id="settings-browser-heading">Browser session</h4><p id="settings-browser-state" role="status">Checking browser…</p></div><div class="settings-browser-body"><p class="settings-browser-copy">The ChatGPT browser session is shared by all conversations.</p><div class="settings-browser-actions"><button type="button" id="settings-browser-start">Start browser</button><button type="button" id="settings-browser-stop">Stop browser</button><button type="button" id="settings-browser-login">Sign in</button><button type="button" id="settings-browser-models">Refresh models</button></div><p id="settings-browser-feedback" role="status" hidden></p></div>';
  const browserNodes=Object.fromEntries(['state','start','stop','login','models','feedback'].map(key=>[key,browserCard.querySelector(`#settings-browser-${key}`)]));
  function renderBrowser(){
    const c=browser||{},stopping=browserOperation==='stop'||c.stopping,starting=browserOperation==='start'||c.recovering;
    browserNodes.state.textContent=stopping?'Stopping browser…':starting?'Starting browser…':!browser?'Checking browser…':c.manuallyStopped?'Browser auto-start paused':c.waitingForServerRestart?'Waiting for Codexify to restart':c.responsive&&c.status==='unavailable'?'Browser is running; check the ChatGPT page':c.authenticated?`Connected · ${(c.status||'idle').replace(/^./,v=>v.toUpperCase())}`:c.status==='unavailable'?(c.detail||'Browser companion is unavailable'):'ChatGPT sign-in required';
    browserNodes.state.dataset.state=c.manuallyStopped||stopping||starting||c.waitingForServerRestart?'disconnected':c.authenticated?'connected':c.status==='unavailable'?'unavailable':'disconnected';
    browserNodes.start.disabled=!browser||Boolean(browserOperation)||(!c.manuallyStopped&&c.status!=='unavailable'&&!c.recovering);
    browserNodes.stop.disabled=!browser||browserOperation==='stop'||(c.manuallyStopped===true&&browserOperation!=='start');
    browserNodes.start.textContent=browserOperation==='start'?'Starting…':'Start browser';browserNodes.stop.textContent=stopping?'Stopping…':'Stop browser';
    browserNodes.login.disabled=!browser||c.authenticated===true||Boolean(browserOperation)||c.manuallyStopped===true;
    browserNodes.models.disabled=!browser||c.authenticated!==true||c.status!=='idle'||Boolean(c.activeActionId)||Boolean(browserOperation)||c.manuallyStopped===true||c.recovering===true||c.stopping===true;
    browserNodes.models.textContent=browserOperation==='capabilities'?'Refreshing…':'Refresh models';
    browserNodes.feedback.hidden=!browserFeedback;browserNodes.feedback.textContent=browserFeedback;browserNodes.feedback.dataset.error=String(browserFeedbackError);
  }
  async function refreshBrowser(){
    if(browserReading||browserOperation)return;browserReading=true;const revision=browserRevision;
    try{const result=await(await api('/api/browser')).json();if(revision!==browserRevision)return;browser=result.companion||{};if(browserFeedbackOperation==='login'&&!browserFeedbackError&&browser.authenticated)browserFeedback='Signed in.';if(browserFeedbackError&&browserFeedbackOperation==='start'&&(browser.authenticated||browser.responsive||browser.status==='authentication_required')){browserFeedback='';browserFeedbackError=false;}}
    catch(error){if(revision===browserRevision)browser={status:'unavailable',authenticated:false,detail:error.message||'Browser status is unavailable'};}
    finally{browserReading=false;renderBrowser();}
  }
  async function controlBrowser(operation){
    if(browserOperation==='stop'||browserOperation&&operation!=='stop')return;
    const revision=++browserRevision;browserOperation=operation;browserFeedback='';browserFeedbackError=false;browserFeedbackOperation=operation;renderBrowser();renderRuntime();
    try{
      const random=globalThis.crypto?.randomUUID?.()||`${Date.now()}-${Math.random().toString(16).slice(2)}`;
      const result=await(await api(`/api/browser/${operation}`,{method:'POST',body:JSON.stringify({requestId:`browser-${operation}-${random}`,...(operation==='capabilities'?{refresh:true}:{})})})).json();
      if(revision!==browserRevision)return;browser={...result.companion,...result.lifecycle};
      browserFeedback=operation==='capabilities'?'Models refreshed.':operation==='login'?(browser.authenticated?'Signed in.':'Complete sign-in in the browser.') : '';
      window.dispatchEvent(new Event('codexify-browser-change'));
    }catch(error){if(revision===browserRevision){browserFeedback=error.message||'Browser control failed';browserFeedbackError=true;}}
    finally{if(revision===browserRevision){browserOperation=null;renderBrowser();renderRuntime();void refreshBrowser();}}
  }
  for(const [key,operation]of [['start','start'],['stop','stop'],['login','login'],['models','capabilities']])browserNodes[key].addEventListener('click',()=>void controlBrowser(operation));
  const browserTimer=setInterval(()=>{if(dialog.open&&!rawMode&&browserCard.isConnected&&!document.hidden)void refreshBrowser();},1500);
  const message=(text,error=false)=>{byId('settings-save-state').textContent=text;byId('settings-save-state').dataset.error=String(error);byId('settings-reload').hidden=!error;};
  const storedAt=(object,path)=>path.split('.').reduce((value,key)=>value&&Object.hasOwn(value,key)?value[key]:undefined,object);
  const draftAt=path=>{const pending=Object.hasOwn(dirty,path)?dirty:inFlightChanges;return Object.hasOwn(pending,path)?pending[path]===RESET?undefined:pending[path]:storedAt(snapshot.document,path);};
  function describe(path,sample) {
    const root=path.split('.')[0],meta=metadata[path],key=path.split('.').at(-1);
    let section=groups[root]||'Other settings',group=meta?.[1];
    if(!group){if(path.startsWith('agentChat.notifications'))group='notifications';else if(path.startsWith('agentChat.automation'))group='browser';else if(root==='agentChat')group='chats';else if(['worktrees','projectCatalog','projectDoc','skills','codexMcp'].includes(root))group='workspaces';else if(['tree','ignore','command','exec','output','filesystem','diff','artifactIngress','artifactEgress'].includes(root))group='tools';else if(['workDir','projectCloneDir','multiProject','uiWidgets','port'].includes(root))group='general';else group='advanced';}
    if(group==='general')section='Preferences';
    if(group==='extras')section='Celebrations';
    if(group==='notifications')section=path.includes('.events.')?'Events':'Delivery';
    if(group==='browser')section='Browser companion';
    if(path.startsWith('agentChat.turnRenewal'))section='Turn renewal';
    if(path.startsWith('agentChat.automation.retryTimings')||path==='agentChat.automation.automaticRetries')section='Automatic retries';
    if(path==='agentChat.rolloutEnabled'||path==='agentChat.rolloutReminderInterval')section='Progress journal';
    if(group==='connections')section=['apiKey','conversationAuthToken','allowedHosts'].includes(root)?'Authentication & access':'Integrations';
    return {path,sample,category:group,section,name:meta?.[2]||label(key),help:meta?.[0]||'',secret:/(?:token|key|password|secret)/i.test(key)&&(sample==null||typeof sample!=='object')};
  }
  function collect(schema,stored,prefix='',defaults=snapshot.defaults) {
    for(const key of new Set([...Object.keys(schema||{}),...Object.keys(defaults||{}),...Object.keys(stored||{})])) {
      if(key==='schemaVersion'||key==='multiProject'&&!prefix||prefix==='agentChat'&&key==='analyticsEnabled')continue;
      const path=prefix?`${prefix}.${key}`:key,fallback=schema?.[key]??defaults?.[key],value=stored?.[key],sample=value??fallback??(annotationHintOverrides.has(path)?false:undefined);
      const atomic=['mcpServers','openaiTunnel','env','httpHeaders','envHttpHeaders'].includes(key);
      if(sample&&typeof sample==='object'&&!Array.isArray(sample)&&!atomic)collect(fallback||{},value||{},path,defaults?.[key]);
      else fields.push(describe(path,sample));
    }
  }
  const retrySchedule=path=>path.startsWith('agentChat.automation.retryTimings.')&&path.endsWith('DelaysSeconds');
  const formatRetryDelays=values=>values.map(seconds=>seconds%3600===0?`${seconds/3600}h`:seconds%60===0?`${seconds/60}m`:`${seconds}s`).join(', ');
  function defaultText(path) {
    const value=storedAt(snapshot.defaults,path),label=snapshot.defaultLabels?.[path];
    const text=retrySchedule(path)?formatRetryDelays(value):label??(enums[path]?.find(([choice])=>choice===value)?.[1]
      ??(typeof value==='boolean'?(annotationHintOverrides.has(path)?String(value):(value?'On':'Off')):value==null?'None':typeof value==='object'?JSON.stringify(value):String(value)));
    return label==='Required'?label:`${text} (default)`;
  }
  function queue(path,value,control) {
    control?.setAttribute('data-default',String(value===RESET));
    if(control?.tagName!=='SELECT'&&control?.matches('input,textarea'))control.placeholder=value===RESET?defaultText(path):'';
    const previous=draftAt(path),wasInvalid=invalidFields.has(path);
    invalidFields.delete(path);invalidText.delete(path);control?.removeAttribute('aria-invalid');
    if(value===RESET?previous==null:JSON.stringify(previous)===JSON.stringify(value)){
      if(wasInvalid)message(invalidFields.size?invalidFields.values().next().value:snapshot.restartRequired?'Saved. Restart Codexify to apply changes.':'All changes saved.',invalidFields.size>0);
      return;
    }
    dirty[path]=value;saveError=null;message('Saving…');clearTimeout(saveTimer);saveTimer=setTimeout(()=>void saveChanges(),600);
  }
  function drawField(field) {
    const {path,name,help,sample}=field,storedValue=draftAt(path),value=path==='audit.logFile'&&typeof storedValue==='string'&&!storedValue.trim()?undefined:storedValue,row=document.createElement('div');row.className='setting-row';row.dataset.settingRow=path;
    const copy=document.createElement('div'),caption=document.createElement('label');caption.className='setting-label';caption.textContent=name;caption.title=path;copy.append(caption);
    if(help){const description=document.createElement('p');description.className='setting-help';description.textContent=help;copy.append(description);}
    const controls=document.createElement('div');controls.className='setting-control';row.append(copy,controls);
    let control,read;
    if(path==='agentChat.turnRenewal.limits') {
      control=document.createElement('div');control.className='setting-turn-limits';
      const defaults=storedAt(snapshot.defaults,path)??sample??[],saved=value??defaults;
      const key=limit=>`${limit.model}\u0000${limit.effort}`;
      const supplied=new Map(saved.map(limit=>[key(limit),limit]));
      const limits=defaults.map(limit=>({...structuredClone(supplied.get(key(limit))??limit),enabled:supplied.has(key(limit))&&supplied.get(key(limit)).enabled!==false}));
      const known=new Set(limits.map(key));
      for(const limit of saved)if(!known.has(key(limit)))limits.push({...structuredClone(limit),enabled:limit.enabled!==false});
      const checks=[];
      for(const limit of limits) {
        const line=document.createElement('div');line.className='setting-turn-limit';line.dataset.enabled=String(limit.enabled);
        const model={'gpt-6-astra':'GPT-6 Astra','gpt-6-sol':'GPT-6 Sol','gpt-5.6-sol':'GPT-5.6 Sol','gpt-5.5-sol':'GPT-5.5 Sol','gpt-5.6-luna':'GPT-5.6 Luna'}[limit.model]??limit.model;
        const effort={max:'Pro',xhigh:'Extra High',high:'High',medium:'Medium',low:'Instant'}[limit.effort]??limit.effort;
        const name=document.createElement('span');name.textContent=`${model} ${effort}`;
        const label=document.createElement('label');label.className='setting-turn-limit-label';
        const enabled=document.createElement('input');enabled.type='checkbox';enabled.checked=limit.enabled;enabled.setAttribute('aria-label',`Renew ${model} ${effort}`);
        enabled.addEventListener('change',()=>{limit.enabled=enabled.checked;line.dataset.enabled=String(enabled.checked);queue(path,structuredClone(limits),control);});
        label.append(enabled,name);checks.push({limit,line,enabled});
        const input=document.createElement('input');input.type='number';input.min='1';input.max='1440';input.step='1';input.value=limit.minutes;input.className='setting-input';input.setAttribute('aria-label',`${limit.model} ${limit.effort} turn limit in minutes`);
        input.addEventListener('input',()=>{const n=Number(input.value);if(!Number.isInteger(n)||n<1||n>1440){input.setAttribute('aria-invalid','true');invalidFields.set(path,'Turn limits must be whole minutes between 1 and 1440.');message(invalidFields.get(path),true);return;}limit.minutes=n;input.removeAttribute('aria-invalid');queue(path,structuredClone(limits),control);});
        const unit=document.createElement('span');unit.textContent='min';line.append(label,input,unit);control.append(line);
      }
      const tools=document.createElement('div');tools.className='setting-editor-tools';tools.style.margin='0 0 12px';
      for(const [text,checked] of [['Enable all',true],['Disable all',false]]) {
        const button=document.createElement('button');button.type='button';button.textContent=text;
        button.addEventListener('click',()=>{for(const row of checks){row.limit.enabled=checked;row.enabled.checked=checked;row.line.dataset.enabled=String(checked);}queue(path,structuredClone(limits),control);});tools.append(button);
      }
      const reset=document.createElement('button');reset.type='button';reset.textContent='Use defaults';reset.addEventListener('click',()=>{queue(path,RESET,control);renderForm();});tools.append(reset);controls.append(tools,control);
    } else if(retrySchedule(path)) {
      control=document.createElement('input');control.type='text';control.className='setting-input';control.value=value==null?'':formatRetryDelays(value);control.placeholder=defaultText(path);control.id=`setting-${path.replaceAll('.','-')}`;control.setAttribute('aria-label',path);caption.htmlFor=control.id;
      if(invalidText.has(path))control.value=invalidText.get(path);
      control.addEventListener('input',()=>{
        const fail=()=>{delete dirty[path];invalidText.set(path,control.value);invalidFields.set(path,`${name}: enter 1 to 32 delays, separated by commas, each between 1s and 24h (for example 30s, 1m, 2m).`);control.setAttribute('aria-invalid','true');message(invalidFields.get(path),true);};
        if(!control.value.trim()){queue(path,RESET,control);return;}
        const entries=control.value.split(',');
        if(entries.length>32){fail();return;}
        const delays=[];
        for(const entry of entries){const match=entry.trim().match(/^(\d+)\s*([smh]?)$/i);if(!match){fail();return;}const seconds=Number(match[1])*({s:1,m:60,h:3600}[match[2].toLowerCase()||'s']);if(!Number.isSafeInteger(seconds)||seconds<1||seconds>86400){fail();return;}delays.push(seconds);}
        queue(path,delays,control);
      });
      const reset=document.createElement('button');reset.type='button';reset.className='setting-reset';reset.textContent='Use default';reset.setAttribute('aria-label',`Use default for ${name}`);reset.addEventListener('click',()=>{queue(path,RESET,control);renderForm();});controls.append(control,reset);
    } else if(typeof sample==='boolean') {
      control=document.createElement('div');control.className='setting-boolean';control.setAttribute('role','group');control.setAttribute('aria-label',path);
      const hintOverride=annotationHintOverrides.has(path);
      if(hintOverride)control.classList.add('setting-hint-override');
      for(const [text,choice] of [[defaultText(path),RESET],[hintOverride?'True':'On',true],[hintOverride?'False':'Off',false]]) {
        const button=document.createElement('button');button.type='button';button.textContent=text;if(choice===RESET){button.title='Use the application default';button.dataset.default='true';}button.setAttribute('aria-pressed',String(choice===RESET?value==null:value===choice));
        button.addEventListener('click',()=>{for(const other of control.children)other.setAttribute('aria-pressed',String(other===button));queue(path,choice,control);});control.append(button);
      }
      controls.append(control);
    }else {
      const stringList=Array.isArray(sample)&&sample.every(item=>typeof item==='string')&&!['openaiTunnels','projectCatalog.entries'].includes(path)||path==='skills.dirs';
      const structured=sample!==null&&typeof sample==='object'&&!stringList;
      if(enums[path]){control=document.createElement('select');control.add(new Option(defaultText(path),''));for(const [choice,text] of enums[path])control.add(new Option(text,choice));control.value=value??'';read=()=>control.value||RESET;}
      else if(stringList){control=document.createElement('textarea');control.value=(value||[]).join('\n');control.placeholder=value==null?defaultText(path):'';read=()=>control.value.split('\n').map(item=>item.trim()).filter(Boolean);row.classList.add('setting-wide');}
      else if(structured){
        row.classList.add('setting-wide');const summary=document.createElement('div');summary.className='setting-object-summary';const info=document.createElement('span');info.textContent=value==null?defaultText(path):`${Array.isArray(value)?value.length:Object.keys(value).length} configured ${Array.isArray(value)?'entries':path==='mcpServers'?'servers':'fields'}`;
        const edit=document.createElement('button');edit.type='button';edit.innerHTML=`Edit configuration ${icon('arrow')}`;edit.setAttribute('aria-expanded','false');summary.append(info,edit);controls.append(summary);
        control=document.createElement('textarea');control.hidden=true;control.value=value==null?'':JSON.stringify(value,null,2);control.placeholder=value==null?defaultText(path):'';read=()=>control.value.trim()?JSON.parse(control.value):RESET;
        edit.addEventListener('click',()=>{control.hidden=!control.hidden;tools.hidden=control.hidden;edit.setAttribute('aria-expanded',String(!control.hidden));edit.firstChild.textContent=control.hidden?'Edit configuration ':'Hide editor ';if(!control.hidden)control.focus();});
        const tools=document.createElement('div');tools.className='setting-editor-tools';tools.hidden=true;const format=document.createElement('button');format.type='button';format.textContent='Format JSON';format.addEventListener('click',()=>{try{control.value=JSON.stringify(JSON.parse(control.value),null,2);control.dispatchEvent(new Event('input'));}catch{control.dispatchEvent(new Event('input'));}});tools.append(format,document.createTextNode('JSON · validated before saving'));controls.append(control,tools);
      }else{control=document.createElement('input');control.type=typeof sample==='number'||path.endsWith('port')?'number':field.secret?'password':'text';control.value=value??'';control.placeholder=value==null?defaultText(path):'';if(path.startsWith('agentChat.automation.retryTimings.')){control.min=path.endsWith('jitterPercent')?'0':'1';control.max=path.endsWith('jitterPercent')?'100':'86400';control.step='1';}if(control.type==='password')control.autocomplete='new-password';read=()=>control.value.trim()===''?RESET:control.type==='number'?Number(control.value):control.value;}
      if(path==='agentChat.offlineTimeoutMinutes'){control.min='1';control.max='1440';control.step='1';}
      if(path==='agentChat.offlineTimeoutMinutes'||path.startsWith('agentChat.automation.retryTimings.'))read=()=>{if(!control.value.trim())return RESET;const number=Number(control.value);if(!Number.isInteger(number)||number<Number(control.min)||number>Number(control.max))throw new Error(`${name} must be a whole number between ${control.min} and ${control.max}.`);return number;};
      if(invalidText.has(path))control.value=invalidText.get(path);
      control.dataset.default=String(value==null);control.className='setting-input';control.id=`setting-${path.replaceAll('.','-')}`;control.setAttribute('aria-label',path);caption.htmlFor=control.id;
      control.addEventListener(control.tagName==='SELECT'?'change':'input',()=>{try{queue(path,read(),control);}catch(error){delete dirty[path];invalidText.set(path,control.value);invalidFields.set(path,(path==='agentChat.offlineTimeoutMinutes'||path.startsWith('agentChat.automation.retryTimings.'))?error.message:`${name} must be valid JSON.`);control.setAttribute('aria-invalid','true');message(invalidFields.get(path),true);}});
      if(!structured)controls.append(control);
      if(stringList||structured){const reset=document.createElement('button');reset.type='button';reset.className='setting-reset';reset.textContent='Use default';reset.setAttribute('aria-label',`Use default for ${name}`);reset.addEventListener('click',()=>{queue(path,RESET,control);renderForm();});controls.append(reset);}
    }
    control.dataset.setting=path;
    if(invalidFields.has(path))control.setAttribute('aria-invalid','true');
    return row;
  }
  let cutoffEstimates=null,cutoffRequest=0;
  function cutoffCard() {
    const card=document.createElement('section');card.className='settings-card';card.id='cutoff-estimates';
    const heading=document.createElement('h4');heading.className='settings-card-heading';heading.textContent='Model cutoff estimates';
    const body=document.createElement('div');body.className='cutoff-body';
    const help=document.createElement('p');help.className='setting-help';help.textContent='Inferred from unfinished turns while Turn renewal is off. Duration runs from the confirmed send to the last tool call; the offline detection delay is excluded. These are estimates, not proof of a ChatGPT timeout. Intentional endings and durations over five hours are excluded.';
    body.append(help);
    const rows=Array.isArray(cutoffEstimates?.models)?cutoffEstimates.models.filter(row=>row&&typeof row.model==='string'&&typeof row.effort==='string'&&['samples','medianMs','p90Ms','minMs','maxMs'].every(key=>Number.isFinite(row[key])&&row[key]>0)):[];
    if(!rows.length){const empty=document.createElement('p');empty.className='setting-help';empty.textContent='No cutoff estimates yet.';body.append(empty);}
    else {
      const scroll=document.createElement('div');scroll.className='cutoff-scroll';
      const table=document.createElement('table');table.setAttribute('aria-label','Model cutoff estimates');
      const head=table.createTHead().insertRow();for(const label of ['Model / effort','Samples','Median','P90','Range']){const cell=document.createElement('th');cell.scope='col';cell.textContent=label;head.append(cell);}
      const duration=ms=>{const minutes=Math.max(1,Math.round(ms/60000));return minutes<60?`${minutes}m`:`${Math.floor(minutes/60)}h${minutes%60?` ${minutes%60}m`:''}`;};
      const tbody=table.createTBody();for(const row of rows){const tr=tbody.insertRow();for(const value of [`${row.model} / ${row.effort}`,row.samples,duration(row.medianMs),duration(row.p90Ms),`${duration(row.minMs)}–${duration(row.maxMs)}`]){const cell=tr.insertCell();cell.textContent=String(value);}}
      scroll.append(table);body.append(scroll);
      const note=document.createElement('p');note.className='setting-help';note.textContent='P90 is the duration below which 90% of these observations fall. Small sample counts are uncertain. Collection never changes turn limits or continuation behavior.';body.append(note);
    }
    card.append(heading,body);return card;
  }
  async function loadCutoffEstimates() {
    const request=++cutoffRequest;
    try {const response=await api('/api/analytics/turn-cutoffs');if(!response.ok)return;const data=await response.json();if(request!==cutoffRequest)return;cutoffEstimates=data;const card=byId('cutoff-estimates');if(card)card.replaceWith(cutoffCard());}catch{/* Optional analytics must never affect settings or display errors. */}
  }
  const extras=startStatisticsExtras({api,getChats,onOpenChat,bindBackdrop});
  const usageDashboard=createUsageAnalytics({api,getChats,bindBackdrop,getHighlights:section=>extras.highlights(section),onOpenChat:id=>{dialog.close();onOpenChat(id);},isVisible:()=>dialog.open&&category==='analytics'&&!rawMode,cutoffCard});
  function renderForm() {
    for(const button of byId('settings-nav').children)button.setAttribute('aria-current',String(!rawMode&&button.dataset.category===category));
    byId('settings-sidebar').hidden=rawMode;byId('settings-form').hidden=rawMode;byId('settings-raw-panel').hidden=!rawMode;byId('settings-dialog').querySelector('.settings-body').style.gridTemplateColumns=rawMode?'minmax(0,1fr)':'';
    byId('settings-raw-toggle').setAttribute('aria-label',rawMode?'Back to settings':'View raw config');byId('settings-raw-toggle').innerHTML=`${icon('code')}<span>${rawMode?'Back to settings':'View raw config'}</span>`;
    const query=byId('settings-search').value.trim().toLowerCase(),current=categories.find(item=>item.id===category);
    const matching=fields.filter(field=>query?`${field.name} ${field.help} ${field.path} ${field.section} ${categories.find(item=>item.id===field.category).name}`.toLowerCase().includes(query):field.category===category);
    byId('settings-content-heading').textContent=rawMode?'Raw configuration':query?'Search results':current.name;
    byId('settings-content-description').textContent=rawMode?'Edit the configuration file directly.':query?`${matching.length} ${matching.length===1?'setting':'settings'} matching “${byId('settings-search').value.trim()}”`:current.description;
    const analytics=category==='analytics'&&!query&&!rawMode;
    dialog.classList.toggle('analytics-view',analytics);
    byId('settings-sidebar').hidden=rawMode||analytics;
    byId('settings-analytics-back').hidden=!analytics;
    byId('settings-raw-toggle').hidden=analytics;
    if(rawMode)return;
    const form=byId('settings-form');form.replaceChildren();
    if(category==='analytics'&&!query){usageDashboard.mount(form);void extras.refresh();return;}
    const browserVisible=!query&&category==='browser'||query&&'browser session start browser stop browser sign in refresh models'.includes(query);
    if(browserVisible){form.append(browserCard);renderBrowser();void refreshBrowser();}
    const eventOrder=['completed','inputRequired','goalBlocked','errors','messages','offline'];
    matching.sort((a,b)=>{const aEvent=a.category==='notifications'&&a.section==='Events',bEvent=b.category==='notifications'&&b.section==='Events';return Number(bEvent)-Number(aEvent)||(aEvent&&bEvent?eventOrder.indexOf(a.path.split('.').at(-1))-eventOrder.indexOf(b.path.split('.').at(-1)):0);});
    const sections=new Map();for(const field of matching){const title=query?`${categories.find(item=>item.id===field.category).name} · ${field.section}`:field.section;if(!sections.has(title))sections.set(title,[]);sections.get(title).push(field);}
    for(const [title,items] of sections){const card=document.createElement('section');card.className='settings-card';const heading=document.createElement('h4');heading.className='settings-card-heading';heading.textContent=title;card.append(heading,...items.map(drawField));form.append(card);}
    if(category==='chats'&&!query)form.append(cutoffCard());
    if(!matching.length&&!browserVisible){const empty=document.createElement('div');empty.className='settings-empty';empty.innerHTML='<strong>No settings found</strong><p>Try another search or choose a category.</p>';form.append(empty);}
  }
  for(const item of categories){const button=document.createElement('button');button.type='button';button.dataset.category=item.id;button.innerHTML=`${icon(item.id)}<span>${item.name}</span>`;button.addEventListener('click',()=>{category=item.id;byId('settings-search').value='';renderForm();byId('settings-main').scrollTop=0;});byId('settings-nav').append(button);}
  byId('settings-analytics-back').addEventListener('click',()=>{category='general';byId('settings-search').value='';renderForm();byId('settings-main').scrollTop=0;});
  byId('settings-search').addEventListener('input',()=>{renderForm();byId('settings-main').scrollTop=0;});
  function renderEditor() {
    invalidFields.clear();invalidText.clear();fields=[];collect(snapshot.fieldSchema||snapshot.defaults,snapshot.document);renderForm();byId('settings-file').textContent=snapshot.path;byId('settings-raw').value=snapshot.raw;
    byId('settings-restart').disabled=maintenance||updatePending(global?.update)||!global?.restartAvailable;refreshButton();
    message(snapshot.restartRequired?'Saved. Restart Codexify to apply changes.':'All changes saved.');
    if(global?.connectorRefresh?.source==='manual'||['queued','running'].includes(global?.connectorRefresh?.phase)){shownRefreshState=null;renderRefresh(global.connectorRefresh);}
  }
  async function load(){snapshot=await(await api('/api/settings')).json();extras.preferences({...snapshot.defaults?.agentChat?.extras,...snapshot.document?.agentChat?.extras});dirty={};saveError=null;renderEditor();void loadCutoffEstimates();}
  function changeDocument(changes) {
    const document=JSON.parse(JSON.stringify(snapshot.document));
    for(const [path,value] of Object.entries(changes)){const parts=path.split('.');let target=document;for(const part of parts.slice(0,-1)){if(!target[part]||typeof target[part]!=='object')target[part]={};target=target[part];}if(value===RESET)delete target[parts.at(-1)];else target[parts.at(-1)]=value;}
    return document;
  }
  async function saveChanges() {
    clearTimeout(saveTimer);
    if(saving){const success=await savePromise;if(!success)return false;return Object.keys(dirty).length?saveChanges():!invalidFields.size;}
    if(!snapshot||!Object.keys(dirty).length)return !invalidFields.size&&!saveError;
    const changes=dirty;dirty={};inFlightChanges=changes;saving=true;
    const body=Object.values(changes).includes(RESET)?{revision:snapshot.revision,raw:JSON.stringify(changeDocument(changes))}:{revision:snapshot.revision,changes};
    savePromise=(async()=>{
      try{snapshot=await(await api('/api/settings',{method:'POST',body:JSON.stringify(body)})).json();saveError=null;extras.preferences({...snapshot.defaults?.agentChat?.extras,...snapshot.document?.agentChat?.extras});if(!rawEdited)byId('settings-raw').value=snapshot.raw;window.dispatchEvent(new CustomEvent('codexify-message-mode-change',{detail:snapshot.document?.agentChat?.defaultMessageMode??snapshot.defaults?.agentChat?.defaultMessageMode??'queue'}));if(invalidFields.size)message(invalidFields.values().next().value,true);else if(snapshot.schemaRefreshError)message('Settings saved. Automatic tool refresh could not be queued. Use Refresh connector.',true);else message(snapshot.restartRequired?'Saved. Restart Codexify to apply changes.':'All changes saved.');return true;}
      catch(error){dirty={...changes,...dirty};saveError=error.message||'Settings were not saved.';message(saveError,true);return false;}
      finally{saving=false;inFlightChanges={};}
    })();
    const success=await savePromise;if(!success)return false;return Object.keys(dirty).length?saveChanges():!invalidFields.size;
  }
  function refreshButton() {
    const phase=global?.connectorRefresh?.phase,pending=['queued','running','starting_browser','waiting_for_restart'].includes(phase);
    const button=byId('settings-refresh-connector');button.disabled=refreshSubmitting||pending;
    button.textContent=phase==='waiting_for_restart'?(refreshNeedsRestart()?'Restart required':'Starting…'):phase==='starting_browser'?'Starting browser…':phase==='queued'?'Queued…':phase==='running'?'Refreshing…':'Refresh connector';
    button.title=refreshNeedsRestart()?'Restart Codexify to apply the changed tools. The connector will refresh automatically.':pending?(global?.readiness?.loading?global.readiness.detail:'Waiting for the update, browser startup, or connector refresh to finish.'):'Refresh the ChatGPT connector tools';
    if(pending&&!refreshTimer)refreshTimer=setInterval(()=>{if(!document.hidden&&!maintenance)void pollStatus();},3000);
    else if(!pending&&refreshTimer){clearInterval(refreshTimer);refreshTimer=null;}
  }
  function renderRefresh(refresh) {
    refreshButton();
    const refreshState=refresh?`${refresh.refreshId||refresh.updateId||''}:${refresh.phase}`:null;
    if(refreshState===shownRefreshState)return;shownRefreshState=refreshState;
    if(invalidFields.size||saveError)return;
    if(refresh?.phase==='waiting_for_restart')message(refreshNeedsRestart()?'Saved. Restart Codexify to apply the schema change; tools will refresh automatically.':global?.readiness?.detail||'Codexify is starting. Connector tools will refresh when it is ready.');
    else if(refresh?.phase==='starting_browser')message('Starting the browser companion before refreshing connector tools…');
    else if(refresh?.phase==='queued')message('Connector refresh queued. Waiting for browser access.');
    else if(refresh?.phase==='running')message('Refreshing connector tools…');
    else if(refresh?.phase==='failed')message(refresh.source==='manual'||refresh.source==='settings'
      ?refresh.result?.error||'Connector refresh failed. Use Refresh connector to retry.'
      :'Codexify updated, but connector refresh failed. Use Refresh connector to retry.',true);
    else if(refresh?.phase==='completed')message('Connector refresh completed; Allow all tools is selected.');
  }
  function renderPlan(plan) {
    const label=plan?.label?.trim()||'',node=byId('account-plan');
    node.textContent=label;node.hidden=!label;node.title=label?(plan.sourceLabel||label):'';onPlan(plan);
  }
  async function pollStatus() {
    if(statusReading)return false;statusReading=true;lastStatusAt=Date.now();
    try{
      const epoch=refreshEpoch,next=await(await api('/api/global')).json();if(epoch!==refreshEpoch)return;global=next;byId('global-version').textContent=global.version?`v${global.version}`:'';
      connectionLost=false;
      renderPlan(global.plan);
      byId('settings-update').hidden=!global.latest?.updateAvailable&&!updatePending(global.update);
      byId('settings-restart').disabled=maintenance||updatePending(global.update)||!global.restartAvailable;byId('global-settings').disabled=global.settingsAvailable===false;
      if(notice?.recovering&&global.readiness?.ready!==false&&!global.readiness?.loading&&!updatePending(global.update)&&!maintenance)runtimeNotice('Codexify is ready.');
      renderRefresh(global.connectorRefresh);renderRuntime();return true;
    }catch{if(!global)renderPlan(null);else connectionLost=true;renderRuntime();return false;}
    finally{statusReading=false;}
  }
  async function waitForRestart(targetVersion=null) {
    const deadline=Date.now()+180000;let sawUnavailable=false;
    while(Date.now()<deadline){
      await new Promise(resolve=>setTimeout(resolve,1500));
      try{
        const health=await(await api('/api/health')).json();await pollStatus();
        if(targetVersion&&updateFailure(global?.update))throw new Error(global.update.failureDetail||(global.update.state==='rolled_back'?'The update was rolled back.':'The update failed.'));
        const changed=targetVersion?global?.version===targetVersion:previousInstance!=null&&global?.instanceId!=null?global.instanceId!==previousInstance:sawUnavailable;
        const readiness=global?.readiness||health.readiness;
        if(health.status==='ok'&&changed&&!updatePending(global?.update)){
          if(!readiness||readiness.ready||readiness.loading===false){
            const text=readiness?.ready===false?(targetVersion?'Update installed; startup needs attention.':'Codexify restarted; startup needs attention.'):(targetVersion?'Codexify updated.':'Codexify restarted.');
            message(text);return text;
          }
          renderRuntime();
        }else {if(health.status!=='ok')sawUnavailable=true;if(!global?.readiness?.loading&&!updateMessage(global?.update))runtimeNotice('Starting Codexify…','Waiting for the service and MCP tools to finish loading.');}
      }catch(error){
        if(targetVersion&&updateFailure(global?.update))throw error;
        sawUnavailable=true;connectionLost=true;renderRuntime();
      }
    }
    throw new Error(global?.readiness?.loading?`${global.readiness.detail} This is taking longer than expected. Startup is still being monitored; check Browser settings or the service logs.`:'Codexify is still starting or reconnecting. Startup is being monitored; check the service logs if it does not recover.');
  }
  async function maintain(kind) {
    if(maintenance||updatePending(global?.update))return;if(rawEdited){message('Save raw configuration changes before continuing.',true);runtimeNotice('Save settings before continuing','Save the unsaved raw configuration in settings before updating or restarting.',true);notice.guardSettings=true;notice.action='settings';notice.label='Open settings';renderRuntime();return;}
    maintenance=true;maintenanceKind=kind;dismissedUpdate=null;runtimeNotice(kind==='update'?'Downloading and verifying the update…':'Restarting Codexify…',kind==='update'?'Preparing the verified release.':'Waiting for the service and MCP tools to finish loading.');
    for(const id of ['settings-restart','settings-update','global-update'])byId(id).disabled=true;
    let completed=null;
    try{
      if(!await saveChanges())throw new Error(saveError||'Fix the highlighted settings before restarting.');
      message(kind==='update'?'Downloading and verifying the update…':'Restarting Codexify…');
      previousInstance=global?.instanceId;
      const result=await(await api(`/api/settings/${kind}`,{method:'POST',body:'{}'})).json();
      if(kind==='update'&&result.status!=='scheduled'){completed='Codexify is already up to date.';message(completed);return;}
      completed=await waitForRestart(kind==='update'?result.targetVersion:null);
    }catch(error){message(error.message||'Maintenance failed.',true);notice={title:kind==='update'?'Update needs attention':'Restart needs attention',detail:error.message||'Maintenance failed.',error:true,dismiss:true,recovering:!updateFailure(global?.update)&&(connectionLost||global?.readiness?.loading)};}
    finally{maintenance=false;maintenanceKind=null;for(const id of ['settings-restart','settings-update','global-update'])byId(id).disabled=false;if(completed)runtimeNotice(completed);await pollStatus();renderRuntime();}
  }
  byId('global-settings').addEventListener('click',async()=>{dialog.showModal();if(rawEdited){message('Raw configuration changes are not saved yet.');return;}if(!await saveChanges())return;message('Loading settings…');try{await load();}catch(error){message(error.message,true);}});
  byId('settings-close').addEventListener('click',()=>dialog.close());
  bindBackdrop(dialog,()=>dialog.close());
  byId('settings-reload').addEventListener('click',async()=>{if(saving)await savePromise;clearTimeout(saveTimer);rawEdited=false;try{await load();}catch(error){message(error.message,true);}});
  byId('settings-raw-toggle').addEventListener('click',async()=>{if(!snapshot||!await saveChanges())return;rawMode=!rawMode;if(rawMode&&!rawEdited)byId('settings-raw').value=snapshot.raw;renderForm();byId('settings-main').scrollTop=0;});
  byId('settings-raw').addEventListener('input',()=>{rawEdited=true;message('Raw configuration changes are not saved yet.');});
  byId('settings-save-raw').addEventListener('click',async()=>{const raw=byId('settings-raw').value;byId('settings-save-raw').disabled=true;try{if(!await saveChanges())return;snapshot=await(await api('/api/settings',{method:'POST',body:JSON.stringify({revision:snapshot.revision,raw})})).json();rawEdited=false;if(notice?.guardSettings){notice=null;renderRuntime();}extras.preferences({...snapshot.defaults?.agentChat?.extras,...snapshot.document?.agentChat?.extras});window.dispatchEvent(new CustomEvent('codexify-message-mode-change',{detail:snapshot.document?.agentChat?.defaultMessageMode??snapshot.defaults?.agentChat?.defaultMessageMode??'queue'}));renderEditor();}catch(error){message(error.message,true);}finally{byId('settings-save-raw').disabled=false;}});
  byId('settings-restart').addEventListener('click',()=>void maintain('restart'));
  for(const id of ['global-update','settings-update'])byId(id).addEventListener('click',()=>void maintain('update'));
  byId('settings-refresh-connector').addEventListener('click',async()=>{
    if(refreshSubmitting||['queued','running'].includes(global?.connectorRefresh?.phase))return;
    refreshSubmitting=true;refreshEpoch++;refreshButton();
    try {
      if(!await saveChanges())return;message('Queuing connector refresh…');
      const receipt=await(await api('/api/settings/refresh-connector',{method:'POST',body:'{}'})).json();
      refreshEpoch++;global={...global,connectorRefresh:receipt};renderRefresh(receipt);renderRuntime();
    }catch(error){message(error.message,true);}
    finally{refreshSubmitting=false;refreshButton();}
  });
  void pollStatus();statusTimer=setInterval(()=>{if(!document.hidden&&(maintenance||connectionLost||global?.readiness?.loading||updatePending(global?.update)||Date.now()-lastStatusAt>=15000))void pollStatus();},2000);
  return {close(){clearInterval(statusTimer);clearInterval(refreshTimer);clearInterval(browserTimer);clearTimeout(saveTimer);clearTimeout(noticeTimer);progressSize?.disconnect();}};
}
