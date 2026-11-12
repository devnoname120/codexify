function analyticsDuration(ms){const minutes=Math.floor(Math.max(0,Number.isFinite(ms)?ms:0)/60000),hours=Math.floor(minutes/60);return hours>=100?`${hours}h`:`${hours}h ${minutes%60}m`;}

function createUsageAnalytics({api,getChats,onOpenChat,isVisible,cutoffCard,getHighlights,rootId='usage-analytics',bindBackdrop=()=>{}}) {
 const svgNS='http://www.w3.org/2000/svg',palette=['#8b5cf6','#06b6d4','#f59e0b','#ec4899','#3b82f6','#14b8a6','#f97316','#a855f7','#84cc16','#eab308','#6366f1','#f43f5e'];
 const metricNames={turns:'Turns',activeMs:'Active time',toolCalls:'Tool calls'};
 const rangeCache=new Map();
 let shareCard=null;
 let root=null,data=null,range='7',leaderMetric='turns',request=0,loading=false,hiddenModels=new Set();
 const el=(tag,className,text)=>{const n=document.createElement(tag);if(className)n.className=className;if(text!=null)n.textContent=text;return n;};
 const svg=(tag,attrs={})=>{const n=document.createElementNS(svgNS,tag);for(const [key,value]of Object.entries(attrs))n.setAttribute(key,String(value));return n;};
 const count=value=>new Intl.NumberFormat(undefined,{notation:value>=10000?'compact':'standard',maximumFractionDigits:1}).format(value||0);
 const duration=analyticsDuration;
 const valueText=(key,v)=>key==='activeMs'?duration(v):count(v);
 const fullModel=row=>row.model==='unattributed'?'Unattributed':`${row.model.replace(/^gpt-/,'GPT-').replace(/-sol$/,' Sol').replace(/-astra$/,' Astra')} · ${row.effort==='max'?'Max':row.effort.replace(/^./,c=>c.toUpperCase())}`;
 const emptyTotals=()=>({turns:0,toolCalls:0,goals:0,activeMs:0});
 const totals=value=>Object.fromEntries(Object.keys(emptyTotals()).map(k=>[k,Number.isFinite(value?.[k])&&value[k]>=0?value[k]:0]));
 function sanitize(value){
  const models=Array.isArray(value?.models)?value.models.filter(m=>m&&typeof m.id==='string'&&typeof m.model==='string'&&typeof m.effort==='string'&&m.id.length<300).map(m=>({...totals(m),id:m.id,model:m.model,effort:m.effort})):[];
  const daily=Array.isArray(value?.daily)?value.daily.filter(d=>d&&/^\d{4}-\d{2}-\d{2}$/.test(d.date)&&d.models&&typeof d.models==='object').map(d=>({date:d.date,models:new Map(Object.entries(d.models).map(([id,t])=>[id,totals(t)]))})):[];
  const conversations=Array.isArray(value?.conversations)?value.conversations.filter(c=>c&&typeof c.id==='string'&&/^[a-f0-9]{64}$/.test(c.id)).map(c=>({...totals(c),id:c.id})):[];
  return {models,daily,conversations,totals:totals(value?.totals),lifetime:totals(value?.lifetime),since:Number.isFinite(value?.since)?value.since:null,today:/^\d{4}-\d{2}-\d{2}$/.test(value?.today)?value.today:new Date().toISOString().slice(0,10),timezone:typeof value?.timezone==='string'?value.timezone:'server time',activeDays:Number.isFinite(value?.activeDays)?value.activeDays:0,streak:Number.isFinite(value?.streak)?value.streak:0};
 }
 const style=el('style');style.textContent=`
:is(#settings-dialog,#statistics-dialog) .analytics{--chart-grid:color-mix(in srgb,var(--line) 75%,transparent);--violet:#8b5cf6;display:grid;gap:18px;min-width:0}
:is(#settings-dialog,#statistics-dialog) .analytics-hero{display:flex;align-items:center;justify-content:space-between;gap:20px;padding:24px 28px;border-radius:18px;background:linear-gradient(112deg,#673acb,#7852e4 62%,#9466ec);color:#fff;box-shadow:0 8px 26px #6e43d420}
:is(#settings-dialog,#statistics-dialog) .analytics-eyebrow{font-size:10px;font-weight:750;text-transform:uppercase;letter-spacing:1.6px;color:#e0d4ff;margin:0 0 8px}
:is(#settings-dialog,#statistics-dialog) .analytics-hero h4{font-size:30px;letter-spacing:-1px;font-weight:750;margin:0;line-height:1.18}
:is(#settings-dialog,#statistics-dialog) .analytics-hero p:last-child{font-size:11px;color:#e0d4ff;margin:8px 0 0}
:is(#settings-dialog,#statistics-dialog) .analytics-controls{display:flex;align-items:center;gap:8px;flex-wrap:wrap;justify-content:flex-end}
:is(#settings-dialog,#statistics-dialog) .analytics-select{font:inherit;font-size:12px;color:#fff;background:#fff1;border:1px solid #fff5;border-radius:9px;padding:10px;max-width:100%;height:36px;color-scheme:dark}
:is(#settings-dialog,#statistics-dialog) .analytics-select option{background:#34234f;color:#fff}
:is(#settings-dialog,#statistics-dialog) .analytics-controls button{padding:10px 12px;height:36px;font-size:12px;font-weight:600;white-space:nowrap;background:#fff1;border-color:#fff5;color:#fff}
:is(#settings-dialog,#statistics-dialog) .analytics-controls button:hover{background:#fff2}
:is(#settings-dialog,#statistics-dialog) .analytics-controls .analytics-share-button{background:#dfff75;color:#32224f;border-color:#dfff75;box-shadow:0 3px 0 #41217b}
:is(#settings-dialog,#statistics-dialog) .analytics-controls .analytics-share-button:hover{background:#eaffab}
:is(#settings-dialog,#statistics-dialog) .analytics-controls :is(button,select):focus-visible{outline-color:#fff;outline-offset:3px}
:is(#settings-dialog,#statistics-dialog) .analytics-kpis{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:12px}
:is(#settings-dialog,#statistics-dialog) .analytics-kpi{--metric:#7146d6;--tile:#eee7ff;position:relative;border:1px solid color-mix(in srgb,var(--metric) 15%,transparent);border-radius:15px;background:var(--tile);padding:18px;min-width:0;overflow:hidden;color:#252036}
:is(#settings-dialog,#statistics-dialog) .analytics-kpi[data-metric=activeMs]{--metric:#10705c;--tile:#def5eb}
:is(#settings-dialog,#statistics-dialog) .analytics-kpi[data-metric=toolCalls]{--metric:#925600;--tile:#fff0d5}
:is(#settings-dialog,#statistics-dialog) .analytics-kpi[data-metric=goals]{--metric:#a93268;--tile:#ffe6f0}
:is(#settings-dialog,#statistics-dialog) .analytics-kpi-label{font-size:11px;font-weight:600;color:var(--metric);margin:0;position:relative;z-index:1}
:is(#settings-dialog,#statistics-dialog) .analytics-kpi-value{display:block;font-size:38px;font-weight:750;letter-spacing:-1.6px;line-height:1.1;margin:12px 0 9px;font-variant-numeric:tabular-nums;overflow-wrap:anywhere;position:relative;z-index:1}
:is(#settings-dialog,#statistics-dialog) .analytics-kpi-foot{font-size:10px;color:color-mix(in srgb,var(--metric) 75%,#333)}
:is(#settings-dialog,#statistics-dialog) .analytics-kpi .analytics-spark{position:absolute;right:10px;bottom:13px;width:76px;height:27px;opacity:.22;color:var(--metric);overflow:visible;pointer-events:none}
:is(#settings-dialog,#statistics-dialog) .analytics-grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:16px;align-items:start}
:is(#settings-dialog,#statistics-dialog) .analytics-card{border:1px solid var(--line);border-radius:16px;padding:20px;min-width:0;background:var(--panel);overflow:hidden;box-shadow:0 3px 12px #18223b03}
:is(#settings-dialog,#statistics-dialog) .analytics-card h4{font-size:15px;font-weight:700;letter-spacing:-.25px;margin:0}
:is(#settings-dialog,#statistics-dialog) .analytics-card-copy{font-size:11px;color:var(--muted);margin:6px 0 18px;line-height:1.5}
:is(#settings-dialog,#statistics-dialog) .analytics-wide{grid-column:1/-1}
:is(#settings-dialog,#statistics-dialog) .analytics-chart svg{display:block;width:100%;height:auto;fill:none;stroke:none;overflow:visible}
:is(#settings-dialog,#statistics-dialog) .analytics-chart text{font-family:inherit;font-size:10px;fill:var(--muted)}
:is(#settings-dialog,#statistics-dialog) .analytics-chart .chart-grid{stroke:var(--chart-grid);stroke-dasharray:3 5}
:is(#settings-dialog,#statistics-dialog) .analytics-chart g[tabindex]{transition:filter .15s}
:is(#settings-dialog,#statistics-dialog) .analytics-chart g[tabindex]:is(:focus,:hover){outline:0;filter:brightness(1.12)}
:is(#settings-dialog,#statistics-dialog) .analytics-chart g[tabindex]:focus .chart-hit{stroke:var(--violet);stroke-width:1}
:is(#settings-dialog,#statistics-dialog) .analytics-chart-detail{font-size:10px;color:var(--muted);min-height:15px;margin:8px 0 0;font-variant-numeric:tabular-nums}
:is(#settings-dialog,#statistics-dialog) .analytics-legend{display:flex;flex-wrap:wrap;gap:7px 12px;margin-top:12px}
:is(#settings-dialog,#statistics-dialog) .analytics-legend button{border:0;background:transparent;padding:3px 0;font-size:10px;color:var(--muted);gap:6px;text-align:left}
:is(#settings-dialog,#statistics-dialog) .analytics-dot{display:inline-block;flex:none;width:8px;height:8px;border-radius:50%;background:var(--series)}
:is(#settings-dialog,#statistics-dialog) .analytics-legend button[aria-pressed=false]{opacity:.4;text-decoration:line-through}
:is(#settings-dialog,#statistics-dialog) .analytics-league-tabs{display:flex;gap:4px;flex-wrap:wrap;margin:12px 0}
:is(#settings-dialog,#statistics-dialog) .analytics-league-tabs button{border:0;padding:6px 9px;font-size:10px;color:var(--muted);background:var(--bg)}
:is(#settings-dialog,#statistics-dialog) .analytics-league-tabs button[aria-pressed=true]{color:#7546d0;background:#8b5cf61c;font-weight:650}
:is(#settings-dialog,#statistics-dialog) .analytics-rank{position:relative;display:flex;align-items:center;gap:10px;padding:14px 10px;margin-top:5px;border-radius:10px;overflow:hidden;background:var(--bg)}
:is(#settings-dialog,#statistics-dialog) .analytics-rank:after{content:'';position:absolute;left:0;bottom:0;height:3px;width:var(--rank);background:var(--rank-color);opacity:.65}
:is(#settings-dialog,#statistics-dialog) .analytics-rank-number{display:grid;place-items:center;flex:none;width:28px;height:28px;border-radius:9px;background:var(--panel);color:var(--muted);font-size:12px;font-weight:700}
:is(#settings-dialog,#statistics-dialog) .analytics-rank:first-child .analytics-rank-number{background:#ffe9a6;color:#896016}
:is(#settings-dialog,#statistics-dialog) .analytics-rank-copy{min-width:0;flex:1}
:is(#settings-dialog,#statistics-dialog) button.analytics-chat-link{display:block;text-align:left;max-width:100%;padding:0;border:0;background:transparent;font-size:12px;font-weight:600;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
:is(#settings-dialog,#statistics-dialog) .analytics-rank-project{font-size:10px;color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;margin:3px 0 0}
:is(#settings-dialog,#statistics-dialog) .analytics-rank-value{font-size:14px;font-weight:700;font-variant-numeric:tabular-nums;white-space:nowrap}
:is(#settings-dialog,#statistics-dialog) .analytics-mix-content{display:grid;grid-template-columns:132px minmax(0,1fr);gap:24px;align-items:center}
:is(#settings-dialog,#statistics-dialog) .analytics-mix-ring{width:132px;height:132px;border-radius:50%;background:var(--mix);display:grid;place-items:center;margin:10px auto;box-shadow:0 0 0 7px color-mix(in srgb,var(--line) 25%,transparent)}
:is(#settings-dialog,#statistics-dialog) .analytics-mix-center{display:flex;flex-direction:column;align-items:center;justify-content:center;width:102px;height:102px;border-radius:50%;background:var(--panel);gap:4px}
:is(#settings-dialog,#statistics-dialog) .analytics-mix-center strong{font-size:min(23px,calc(132px / var(--duration-length,1)));font-weight:750;letter-spacing:-.6px;line-height:1.4;white-space:nowrap;text-align:center}
:is(#settings-dialog,#statistics-dialog) .analytics-mix-center span{font-size:10px;color:var(--muted)}
:is(#settings-dialog,#statistics-dialog) .analytics-model-row{margin:16px 0 0}:is(#settings-dialog,#statistics-dialog) .analytics-model-row:first-child{margin-top:0}
:is(#settings-dialog,#statistics-dialog) .analytics-model-copy{display:flex;justify-content:space-between;gap:12px;font-size:11px;align-items:center}
:is(#settings-dialog,#statistics-dialog) .analytics-model-name{display:flex;gap:6px;align-items:center;min-width:0;overflow-wrap:anywhere}
:is(#settings-dialog,#statistics-dialog) .analytics-model-copy strong{font-size:11px;white-space:nowrap;font-weight:600;font-variant-numeric:tabular-nums}
:is(#settings-dialog,#statistics-dialog) .analytics-model-track{height:7px;background:var(--bg);border-radius:9px;margin-top:8px;overflow:hidden}
:is(#settings-dialog,#statistics-dialog) .analytics-model-track span{display:block;height:100%;border-radius:9px;background:var(--series)}
:is(#settings-dialog,#statistics-dialog) .analytics-model-caption{font-size:10px;color:var(--muted);margin:5px 0 0}
:is(#settings-dialog,#statistics-dialog) .analytics-momentum{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:15px}
:is(#settings-dialog,#statistics-dialog) .analytics-momentum>div{border-radius:12px;background:var(--bg);padding:14px}
:is(#settings-dialog,#statistics-dialog) .analytics-momentum-value{font-size:25px;font-weight:750;letter-spacing:-.8px;margin:0 0 4px;color:var(--violet)}
:is(#settings-dialog,#statistics-dialog) .analytics-momentum-label{font-size:10px;color:var(--muted)}
:is(#settings-dialog,#statistics-dialog) .analytics-milestones{display:flex;flex-wrap:wrap;gap:8px;margin-top:16px}
:is(#settings-dialog,#statistics-dialog) .analytics-milestone{font-size:11px;padding:7px 11px;border:1px solid var(--line);border-radius:9px;color:var(--muted)}
:is(#settings-dialog,#statistics-dialog) .analytics-milestone[data-earned=true]{background:#8b5cf61a;color:var(--violet);border-color:#8b5cf633;font-weight:600}
:is(#settings-dialog,#statistics-dialog) .analytics-activity{display:grid;grid-template-columns:repeat(14,minmax(0,1fr));gap:5px;margin-top:20px}
:is(#settings-dialog,#statistics-dialog) .analytics-activity span{height:16px;border-radius:4px;background:color-mix(in srgb,var(--violet) var(--level),var(--bg))}
:is(#settings-dialog,#statistics-dialog) .analytics-empty{padding:22px;border:1px dashed #8b5cf655;border-radius:14px;text-align:center;color:var(--muted);font-size:12px;line-height:1.6;background:#8b5cf609}
:is(#settings-dialog,#statistics-dialog) .analytics-footnote{font-size:10px;color:var(--muted);margin:0;line-height:1.6}
:is(#settings-dialog,#statistics-dialog) .analytics-loading{opacity:.65}
@media(prefers-color-scheme:dark){:is(#settings-dialog,#statistics-dialog) .analytics-kpi{--tile:#332845;color:#f5efff}:is(#settings-dialog,#statistics-dialog) .analytics-kpi[data-metric=activeMs]{--tile:#1e3933;--metric:#66d4b2}:is(#settings-dialog,#statistics-dialog) .analytics-kpi[data-metric=turns]{--metric:#c4a6ff}:is(#settings-dialog,#statistics-dialog) .analytics-kpi[data-metric=toolCalls]{--tile:#3e3321;--metric:#efc26a}:is(#settings-dialog,#statistics-dialog) .analytics-kpi[data-metric=goals]{--tile:#3c2634;--metric:#f49aba}:is(#settings-dialog,#statistics-dialog) .analytics-kpi-foot{color:color-mix(in srgb,var(--metric) 80%,#ddd)}:is(#settings-dialog,#statistics-dialog) .analytics-league-tabs button[aria-pressed=true]{color:#c4a6ff}:is(#settings-dialog,#statistics-dialog) .analytics{--violet:#b89aff}}
@media(max-width:900px){:is(#settings-dialog,#statistics-dialog) .analytics-kpis{grid-template-columns:repeat(2,minmax(0,1fr))}:is(#settings-dialog,#statistics-dialog) .analytics-hero{align-items:flex-start;flex-direction:column}:is(#settings-dialog,#statistics-dialog) .analytics-controls{justify-content:flex-start}}
@media(max-width:580px){:is(#settings-dialog,#statistics-dialog).analytics-view{width:calc(100vw - 20px)}:is(#settings-dialog,#statistics-dialog) .analytics-hero{padding:20px;border-radius:15px;gap:18px}:is(#settings-dialog,#statistics-dialog) .analytics-hero h4{font-size:28px}:is(#settings-dialog,#statistics-dialog) .analytics-controls{gap:7px;width:100%}:is(#settings-dialog,#statistics-dialog) .analytics-controls button{padding:9px 10px;font-size:11px}:is(#settings-dialog,#statistics-dialog) .analytics-select{padding:9px 7px;font-size:11px;min-width:0;width:100%}:is(#settings-dialog,#statistics-dialog) .analytics-controls{display:grid;grid-template-columns:minmax(0,1fr) auto 32px;gap:6px}:is(#settings-dialog,#statistics-dialog) .analytics-controls button[aria-label="Refresh analytics"]{padding:0;height:36px}:is(#settings-dialog,#statistics-dialog) .analytics-mix-content{grid-template-columns:1fr;gap:12px}:is(#settings-dialog,#statistics-dialog) .analytics-kpis{gap:9px}:is(#settings-dialog,#statistics-dialog) .analytics-kpi{padding:15px}:is(#settings-dialog,#statistics-dialog) .analytics-kpi-value{font-size:32px;margin-top:10px}:is(#settings-dialog,#statistics-dialog) .analytics-kpi .analytics-spark{width:58px;height:21px;opacity:.18}:is(#settings-dialog,#statistics-dialog) .analytics-grid{grid-template-columns:1fr}:is(#settings-dialog,#statistics-dialog) .analytics-card{padding:16px}:is(#settings-dialog,#statistics-dialog) .analytics{gap:12px}:is(#settings-dialog,#statistics-dialog) .analytics-momentum{gap:7px}:is(#settings-dialog,#statistics-dialog) .analytics-momentum>div{padding:10px}:is(#settings-dialog,#statistics-dialog) .analytics-momentum-value{font-size:22px}}
@media(prefers-reduced-motion:reduce){:is(#settings-dialog,#statistics-dialog) .analytics-chart g[tabindex]{transition:none}}
`;
 style.textContent+=`
:is(#settings-dialog,#statistics-dialog) .analytics-select{appearance:none;-webkit-appearance:none;line-height:1.4;min-height:40px;height:auto;padding:8px 28px 8px 10px;background-image:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Cpath d='m4 6 4 4 4-4' fill='none' stroke='white' stroke-width='1.5' stroke-linecap='round' stroke-linejoin='round'/%3E%3C/svg%3E");background-size:16px;background-position:right 7px center;background-repeat:no-repeat}
@media(max-width:580px){:is(#settings-dialog,#statistics-dialog) .analytics-select{font-size:16px;line-height:1.4;min-height:42px;padding:8px 24px 8px 9px}:is(#settings-dialog,#statistics-dialog) .analytics-controls button{height:42px;min-height:42px}:is(#settings-dialog,#statistics-dialog) .analytics-controls button[aria-label="Refresh analytics"]{height:42px}}
`;
 document.head.append(style);
 function dayBins(source=data,period=range){
  const today=source?.today||new Date().toISOString().slice(0,10),map=new Map((source?.daily||[]).map(d=>[d.date,d.models]));
  const first=period==='all'?(source?.daily[0]?.date||today):new Date(new Date(today+'T12:00:00Z').getTime()-(Number(period)-1)*86400000).toISOString().slice(0,10);
  const length=Math.min(36600,Math.max(1,Math.round((Date.parse(today)-Date.parse(first))/86400000)+1)),size=Math.max(1,Math.ceil(length/90)),bins=[];
  for(let i=0;i<length;i+=size){const start=new Date(Date.parse(first)+i*86400000).toISOString().slice(0,10),end=new Date(Date.parse(first)+Math.min(length-1,i+size-1)*86400000).toISOString().slice(0,10),models=new Map();for(let j=i;j<Math.min(length,i+size);j++){const date=new Date(Date.parse(first)+j*86400000).toISOString().slice(0,10);for(const [id,t]of map.get(date)||[]){const sum=models.get(id)||emptyTotals();for(const k of Object.keys(sum))sum[k]+=t[k];models.set(id,sum);}}bins.push({start,end,models});}return bins;
 }
 function colorMap(models=data?.models||[]){const map=new Map(),used=new Set();for(const [i,m]of models.entries()){let hash=0;for(const c of m.id)hash=(hash*31+c.charCodeAt(0))>>>0;let n=hash%palette.length;while(used.has(n)&&used.size<palette.length)n=(n+1)%palette.length;used.add(n);map.set(m.id,palette[n]||palette[i%palette.length]);}return map;}
 function chart(key,title,copy,wide=false){
  const source=range==='1'?rangeCache.get('7'):data,period=range==='1'?'7':range;
  const card=el('section',`analytics-card${wide?' analytics-wide':''}`);card.append(el('h4','',title),el('p','analytics-card-copy',range==='1'?`Last 7 days. ${copy}`:copy));
  if(range==='1'&&!source){card.append(el('p','analytics-empty',loading?'Loading seven-day activity…':'Seven-day activity is temporarily unavailable.'));return card;}
  const chartWidth=wide?1040:520,wrap=el('div','analytics-chart'),image=svg('svg',{viewBox:`0 0 ${chartWidth} 200`,role:'img','aria-label':`${title} by model and effort`});wrap.append(image);
  const bins=dayBins(source,period),visible=(source?.models||[]).filter(m=>!hiddenModels.has(m.id)),colors=colorMap(source?.models||[]);const sums=bins.map(b=>visible.reduce((s,m)=>s+(b.models.get(m.id)?.[key]||0),0));const rawMax=Math.max(...sums,1),max=key==='activeMs'?Math.max(rawMax,180000):Math.max(3,Math.ceil(rawMax/3)*3),width=chartWidth-64,height=142,left=43,top=8;
  for(let i=0;i<=3;i++){const y=top+height-i*height/3;image.append(svg('line',{x1:left,x2:chartWidth-9,y1:y,y2:y,class:'chart-grid'}));const text=svg('text',{x:left-8,y:y+3,'text-anchor':'end'});text.textContent=valueText(key,max*i/3);image.append(text);}
  const detail=el('p','analytics-chart-detail','Focus or hover a day to explore its totals.');detail.setAttribute('aria-live','polite');
  const groupWidth=width/bins.length,barWidth=Math.max(2,Math.min(36,groupWidth*.63));
  bins.forEach((bin,index)=>{const x=left+index*groupWidth+groupWidth/2-barWidth/2,g=svg('g',{tabindex:'0',role:'button','aria-label':`${bin.start}${bin.end!==bin.start?' to '+bin.end:''}: ${valueText(key,sums[index])} ${metricNames[key].toLowerCase()}`});let base=top+height;
   g.append(svg('rect',{x:x-3,y:top,width:barWidth+6,height:height+3,fill:'transparent',class:'chart-hit',rx:3}));
   for(const m of visible){const value=bin.models.get(m.id)?.[key]||0;if(!value)continue;const h=value/max*height;const rect=svg('rect',{x,y:base-h,width:barWidth,height:h,fill:colors.get(m.id),rx:Math.min(2,h/2)});const tip=svg('title');tip.textContent=`${bin.start} · ${fullModel(m)}: ${valueText(key,value)}`;rect.append(tip);g.append(rect);base-=h;}
   if(!sums[index])g.append(svg('rect',{x,y:top+height-2,width:barWidth,height:2,fill:'var(--line)',rx:1}));
   const show=()=>{detail.textContent=`${bin.start}${bin.end!==bin.start?' – '+bin.end:''} · ${valueText(key,sums[index])} ${metricNames[key].toLowerCase()}`;};g.addEventListener('pointerenter',show);g.addEventListener('focus',show);g.addEventListener('click',show);g.addEventListener('keydown',e=>{if(e.key==='Enter'||e.key===' '){e.preventDefault();show();}});image.append(g);
   const every=Math.max(1,Math.ceil(bins.length/7));if(index%every===0||index===bins.length-1&&bins.length>1){const label=svg('text',{x:left+index*groupWidth+groupWidth/2,y:top+height+24,'text-anchor':'middle'});label.textContent=new Date(bin.start+'T12:00:00Z').toLocaleDateString(undefined,{month:'short',day:'numeric',timeZone:'UTC'});image.append(label);}
  });
  card.append(wrap,detail);const legend=el('div','analytics-legend');for(const m of source?.models||[]){const button=el('button','');button.type='button';button.setAttribute('aria-pressed',String(!hiddenModels.has(m.id)));button.dataset.analyticsFocus=`legend-${key}-${m.id}`;const dot=el('i','analytics-dot');dot.style.setProperty('--series',colors.get(m.id));button.append(dot,document.createTextNode(fullModel(m)));button.addEventListener('click',()=>{hiddenModels.has(m.id)?hiddenModels.delete(m.id):hiddenModels.add(m.id);render();});legend.append(button);}card.append(legend);return card;
 }
 function league(){
  const card=el('section','analytics-card');card.append(el('h4','','Conversation league'),el('p','analytics-card-copy','The conversations moving things forward in this period.'));
  const tabs=el('div','analytics-league-tabs');for(const [key,title]of Object.entries(metricNames)){const b=el('button','',title);b.type='button';b.setAttribute('aria-pressed',String(key===leaderMetric));b.dataset.analyticsFocus=`leader-${key}`;b.addEventListener('click',()=>{leaderMetric=key;render();});tabs.append(b);}card.append(tabs);
  const list=el('div','analytics-leaderboard');const rows=[...(data?.conversations||[])].filter(c=>c[leaderMetric]>0).sort((a,b)=>b[leaderMetric]-a[leaderMetric]||a.id.localeCompare(b.id)).slice(0,5),chats=new Map(getChats().map(c=>[c.id,c]));
  for(const [i,c]of rows.entries()){const meta=chats.get(c.id),row=el('div','analytics-rank'),copy=el('div','analytics-rank-copy');const b=el('button','analytics-chat-link',meta?.title||`Conversation ${c.id.slice(0,7)}`);b.type='button';b.title=meta?.title||c.id;b.addEventListener('click',()=>onOpenChat(c.id));copy.append(b,el('p','analytics-rank-project',meta?.workspace||'Conversation'));row.style.setProperty('--rank',`${c[leaderMetric]/Math.max(1,rows[0][leaderMetric])*100}%`);row.style.setProperty('--rank-color',palette[i%palette.length]);row.append(el('span','analytics-rank-number',i+1),copy,el('span','analytics-rank-value',valueText(leaderMetric,c[leaderMetric])));list.append(row);}
  if(!rows.length)list.append(el('div','analytics-empty','Your next conversation could take the lead.'));card.append(list);return card;
 }
 function mix(){const card=el('section','analytics-card');card.append(el('h4','','Model mix'),el('p','analytics-card-copy','Estimated active time, with turns and tools for context.'));const colors=colorMap(),rows=[...(data?.models||[])].sort((a,b)=>b.activeMs-a.activeMs||b.toolCalls-a.toolCalls),max=Math.max(1,...rows.map(m=>m.activeMs));
  const body=el('div','analytics-mix-content'),modelRows=el('div','analytics-models'),total=rows.reduce((sum,m)=>sum+m.activeMs,0),ring=el('div','analytics-mix-ring');let offset=0;const segments=rows.filter(m=>m.activeMs>0).map(m=>{const start=offset;offset+=m.activeMs/total*100;return `${colors.get(m.id)} ${start}% ${offset}%`;});ring.style.setProperty('--mix',segments.length?`conic-gradient(${segments.join(',')})`:'var(--line)');ring.setAttribute('aria-hidden','true');const center=el('div','analytics-mix-center'),time=duration(total);center.style.setProperty('--duration-length',String(time.length));center.append(el('strong','',time),el('span','','agent time'));ring.append(center);body.append(ring,modelRows);card.append(body);
  for(const m of rows){const row=el('div','analytics-model-row'),copy=el('div','analytics-model-copy'),name=el('span','analytics-model-name'),dot=el('i','analytics-dot');row.style.setProperty('--series',colors.get(m.id));name.append(dot,document.createTextNode(fullModel(m)));copy.append(name,el('strong','',duration(m.activeMs)));const track=el('div','analytics-model-track'),bar=el('span','');bar.style.width=`${m.activeMs/max*100}%`;track.append(bar);row.append(copy,track,el('p','analytics-model-caption',`${count(m.turns)} turns · ${count(m.toolCalls)} tools`));modelRows.append(row);}if(!rows.length)card.append(el('div','analytics-empty','Your model mix will appear with new activity.'));return card;}
 function render(){
  if(!root?.isConnected)return;const focused=document.activeElement?.dataset?.analyticsFocus;root.replaceChildren();root.classList.toggle('analytics-loading',loading&&!data);const d=data||sanitize(null);
  const hero=el('section','analytics-hero'),copy=el('div','');copy.append(el('p','analytics-eyebrow','Codexify analytics'),el('h4','',range==='all'?'The bigger picture':range==='1'?'Today':`The last ${range} days`),el('p','',d.since?`Collecting since ${new Date(d.since).toLocaleDateString()} · ${d.timezone}`:'A fresh start. New activity only.'));
  const controls=el('div','analytics-controls'),select=el('select','analytics-select');select.setAttribute('aria-label','Analytics date range');select.dataset.analyticsFocus='range';for(const [value,label]of [['1','1 day'],['7','7 days'],['30','30 days'],['90','90 days'],['all','Lifetime']]){const option=el('option','',label);option.value=value;select.append(option);}select.value=range;select.addEventListener('change',()=>{range=select.value;data=rangeCache.get(range)||null;void load();});
  const refresh=el('button','','↻');refresh.type='button';refresh.setAttribute('aria-label','Refresh analytics');refresh.disabled=loading;refresh.addEventListener('click',()=>void load());const share=el('button','analytics-share-button','Share your stats');share.type='button';share.setAttribute('aria-label','Share analytics');share.disabled=!data||loading;share.addEventListener('click',()=>{
    if(!data||loading)return;
    const bins=dayBins(),values=bins.map(bin=>[...bin.models.values()].reduce((sum,t)=>sum+t.turns,0));
    shareCard??=createAnalyticsShareCard({bindBackdrop});
    shareCard.open({period:range,today:data.today,start:bins[0]?.start||data.today,totals:{...data.totals},topModel:analyticsShareTopModel(data.models),activity:values});
  });controls.append(select,share,refresh);hero.append(copy,controls);root.append(hero);
  const kpis=el('div','analytics-kpis');for(const [key,title]of [['turns','Confirmed turns'],['activeMs','Active time · estimated'],['toolCalls','Tool calls'],['goals','Goals completed']]){const card=el('div','analytics-kpi');card.dataset.metric=key;const value=el('strong','analytics-kpi-value',valueText(key,d.totals[key]));value.title=key==='activeMs'?`${Math.round(d.totals[key]/1000)} seconds`:`${d.totals[key]}`;card.append(el('p','analytics-kpi-label',title),value,el('span','analytics-kpi-foot',`${valueText(key,d.lifetime[key])} lifetime`));const values=dayBins().map(bin=>[...bin.models.values()].reduce((sum,t)=>sum+t[key],0));
   const points=values.slice(-30),max=Math.max(1,...points),spark=svg('svg',{viewBox:'0 0 96 34','aria-hidden':'true',class:'analytics-spark'}),path=points.map((v,i)=>`${i?'L':'M'}${points.length===1?48:i/(points.length-1)*96} ${30-v/max*26}`).join(' ');spark.append(svg('path',{d:path,fill:'none',stroke:'currentColor','stroke-width':2.5,'stroke-linecap':'round','stroke-linejoin':'round'}));card.append(spark);kpis.append(card);}root.append(kpis);const highlights=getHighlights?.('summary');if(highlights)root.append(highlights);
  if(!d.totals.turns&&!d.totals.toolCalls&&!d.totals.goals)root.append(el('div','analytics-empty',loading?'Loading your activity…':'Your next turn starts the story. New turns, tools and completed goals will appear here. No history has been backfilled.'));
  const charts=el('div','analytics-grid');charts.append(chart('turns',range==='all'?'Turns over time':'Daily turns','Confirmed ChatGPT sends, stacked by model and effort.'),chart('activeMs','Time in motion','Estimated active time; waiting loops and long idle gaps excluded.'),chart('toolCalls',range==='all'?'Tool calls over time':'Daily tool calls','Agent work requests. Idle waits and interface polling are excluded.',true),league(),mix());root.append(charts);
  const momentum=el('section','analytics-card analytics-wide');momentum.append(el('h4','','Momentum'));const numbers=el('div','analytics-momentum');const daily=d.daily.map(day=>[...day.models.values()].reduce((s,t)=>s+t.turns,0));for(const [value,label]of [[`${d.streak} ${d.streak===1?'day':'days'}`,'Current streak'],[count(d.activeDays),'Active days in this period'],[count(Math.max(0,...daily)),'Best day · turns']]){const item=el('div','');item.append(el('p','analytics-momentum-value',value),el('span','analytics-momentum-label',label));if(!highlights||label!=='Current streak')numbers.append(item);}if(highlights)numbers.style.gridTemplateColumns='repeat(2,minmax(0,1fr))';numbers.style.marginTop='16px';momentum.append(numbers);const milestones=el('div','analytics-milestones');for(const [key,target,label]of [['goals',1,'First goal'],['turns',10,'10 turns'],['toolCalls',100,'100 tools'],['toolCalls',1000,'1,000 tools']]){const earned=d.lifetime[key]>=target,badge=el('span','analytics-milestone',`${earned?'✓ ':''}${label}`);badge.dataset.earned=String(earned);badge.title=`${count(Math.min(d.lifetime[key],target))} / ${count(target)} lifetime`;milestones.append(badge);}if(!highlights)momentum.append(milestones);const activity=el('div','analytics-activity');activity.setAttribute('aria-label','Recent daily tool activity');const recent=dayBins().slice(-28),peak=Math.max(1,...recent.map(b=>[...b.models.values()].reduce((sum,t)=>sum+t.toolCalls,0)));for(const bin of recent){const value=[...bin.models.values()].reduce((sum,t)=>sum+t.toolCalls,0),cell=el('span');cell.title=`${bin.start}: ${count(value)} tool calls`;cell.style.setProperty('--level',`${value?20+value/peak*80:0}%`);activity.append(cell);}momentum.append(activity);root.append(momentum);const badges=getHighlights?.('badges');if(badges)root.append(badges);
  root.append(el('p','analytics-footnote','Active time is a native-activity estimate, not internal reasoning time or billed usage. Parallel conversations add agent-hours. Days follow the server timezone. Local, best-effort collection never controls chat delivery or goals.'));
  if(cutoffCard)root.append(cutoffCard());
  if(focused)for(const node of root.querySelectorAll('[data-analytics-focus]'))if(node.dataset.analyticsFocus===focused){node.focus({preventScroll:true});break;}
 }
 async function load(){const epoch=++request,period=range;loading=true;render();try{const response=await api(`/api/analytics/usage?days=${period}`);const value=await response.json();if(epoch!==request)return;data=sanitize(value);rangeCache.set(period,data);
  // The three daily charts need a week of history even when totals cover today.
  // Read sequentially: the server permits only one optional usage read at a time.
  if(period==='1'){const response=await api('/api/analytics/usage?days=7'),value=await response.json();if(epoch!==request)return;rangeCache.set('7',sanitize(value));}
 }catch{/* Optional analytics remains silent and preserves the last successful view. */}finally{if(epoch===request){loading=false;render();}}}
 window.addEventListener('statistics-extras-change',()=>{if(isVisible())render();});
 const timer=setInterval(()=>{if(isVisible()&&!document.hidden&&!loading&&!shareCard?.isOpen())void load();},30000);
 window.addEventListener('pagehide',()=>clearInterval(timer),{once:true});
 // Cached figures are a placeholder, never a reason to skip fetching on open.
 return {mount(parent){root=el('div','analytics');root.id=rootId;parent.append(root);void load();}};
}


// Browser-local exports. No server endpoint, remote assets, or public URL is used.
function createAnalyticsShareCard({bindBackdrop}) {
 if(createAnalyticsShareCard.instance)return createAnalyticsShareCard.instance;
 const make=(tag,className,text)=>{const n=document.createElement(tag);if(className)n.className=className;if(text!=null)n.textContent=text;return n;};
 const style=make('style');style.textContent=`
#analytics-share-dialog{width:min(650px,calc(100vw - 32px));max-width:none;max-height:calc(100dvh - 32px);box-sizing:border-box;overflow:auto;margin:auto;padding:24px;border:1px solid var(--line);border-radius:20px;background:var(--panel);color:var(--ink);box-shadow:0 24px 90px #0004;font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}
#analytics-share-dialog::backdrop{background:#1019168c;backdrop-filter:blur(5px)}
#analytics-share-dialog [hidden]{display:none!important}
#analytics-share-dialog .stats-share-heading{display:flex;gap:16px;align-items:flex-start;margin-bottom:18px}
#analytics-share-dialog h2{font-size:21px;line-height:1.3;letter-spacing:-.4px;margin:0;font-weight:650}
#analytics-share-dialog .stats-share-description{font-size:12px;color:var(--muted);margin:6px 0 0}
#analytics-share-dialog button,#analytics-share-dialog a{appearance:none;display:inline-flex;align-items:center;justify-content:center;gap:8px;font:inherit;line-height:1.3;border:1px solid var(--line);border-radius:9px;padding:10px 14px;color:var(--ink);background:var(--panel);text-decoration:none;cursor:pointer}
#analytics-share-dialog button:hover,#analytics-share-dialog a:hover{background:var(--bg)}
#analytics-share-dialog button:disabled,#analytics-share-dialog a[aria-disabled=true]{opacity:.5;cursor:default}
#analytics-share-dialog #stats-share-close{margin-left:auto;flex:none;width:32px;height:32px;border:0;background:transparent;padding:6px;color:var(--muted)}
#analytics-share-dialog #stats-share-close svg{display:block;width:18px;height:18px;fill:none;stroke:currentColor;stroke-width:1.6;stroke-linecap:round}
#analytics-share-dialog :is(button,a):focus-visible{outline:2px solid var(--accent);outline-offset:3px}
#analytics-share-dialog .stats-share-preview{display:grid;place-items:center;aspect-ratio:16/9;border-radius:15px;background:var(--bg);overflow:hidden;border:1px solid var(--line)}
#analytics-share-dialog .stats-share-preview img{display:block;width:100%;height:auto}
#analytics-share-dialog .stats-share-formats{display:flex;gap:6px;align-items:center;margin-bottom:12px;flex-wrap:wrap}
#analytics-share-dialog .stats-share-formats button{padding:7px 12px;font-size:12px}
#analytics-share-dialog .stats-share-formats button[aria-pressed=true]{background:var(--selected);color:var(--accent);border-color:var(--accent)}
#analytics-share-dialog #stats-share-motion{margin-left:auto;color:var(--muted);border:0;background:transparent}
#analytics-share-dialog .stats-share-placeholder{padding:24px;text-align:center;color:var(--muted);font-size:12px}
#analytics-share-dialog .stats-share-privacy{font-size:11px;color:var(--muted);line-height:1.6;margin:14px 0 0}
#analytics-share-dialog .stats-share-footer{display:flex;align-items:center;justify-content:flex-end;gap:10px;flex-wrap:wrap;padding-top:18px;margin-top:18px;border-top:1px solid var(--line)}
#analytics-share-dialog #stats-share-copy{background:var(--accent);border-color:var(--accent);color:var(--panel)}
#analytics-share-dialog #stats-share-copy:hover{filter:brightness(.95)}
#analytics-share-dialog .stats-share-status{margin:0 auto 0 0;color:var(--muted);font-size:12px;flex:1;min-width:130px}
@media(max-width:580px){#analytics-share-dialog{width:calc(100vw - 20px);max-height:calc(100dvh - 20px);padding:18px;border-radius:16px}#analytics-share-dialog h2{font-size:20px}#analytics-share-dialog .stats-share-footer{display:grid;grid-template-columns:1fr 1fr}#analytics-share-dialog .stats-share-status{grid-column:1/-1;margin:0 0 2px;min-width:0}#analytics-share-dialog button,#analytics-share-dialog a{padding:10px 8px;font-size:13px}}
`;
 document.head.append(style);
 const dialog=make('dialog');dialog.id='analytics-share-dialog';dialog.setAttribute('aria-labelledby','stats-share-title');
 const header=make('div','stats-share-heading'),copy=make('div'),heading=make('h2','','Share your stats');heading.id='stats-share-title';
 const description=make('p','stats-share-description');description.id='stats-share-description';copy.append(heading,description);dialog.setAttribute('aria-describedby',description.id);
 const close=make('button');close.id='stats-share-close';close.type='button';close.setAttribute('aria-label','Close share preview');close.innerHTML='<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m6 6 12 12M6 18 18 6"/></svg>';close.addEventListener('click',()=>dialog.close());header.append(copy,close);
 const preview=make('div','stats-share-preview'),image=make('img'),placeholder=make('span','stats-share-placeholder','Creating your card…');image.id='stats-share-image';image.hidden=true;preview.append(image,placeholder);
 const formats=make('div','stats-share-formats');formats.setAttribute('role','group');formats.setAttribute('aria-label','Share image format');
 const formatButtons=new Map();for(const [key,label]of [['png','Static PNG'],['svg','Animated SVG']]){const b=make('button','',label);b.type='button';b.setAttribute('aria-pressed',String(key==='png'));b.addEventListener('click',()=>{format=key;paused=false;show();});formatButtons.set(key,b);formats.append(b);}
 const motion=make('button','','Pause preview');motion.id='stats-share-motion';motion.type='button';motion.hidden=true;motion.addEventListener('click',()=>{paused=!paused;show();});formats.append(motion);
 const hint=make('p','stats-share-privacy');hint.id='stats-share-hint';
 const privacy=make('p','stats-share-privacy','Only aggregate usage and the top model appear on the card. Generated on this device, with no upload or public link.');
 const footer=make('div','stats-share-footer'),message=make('p','stats-share-status');message.id='stats-share-status';message.setAttribute('role','status');message.setAttribute('aria-live','polite');
 const download=make('a','','Download PNG');download.id='stats-share-download';download.setAttribute('aria-disabled','true');
 const copyButton=make('button','','Copy image');copyButton.id='stats-share-copy';copyButton.type='button';copyButton.disabled=true;copyButton.setAttribute('aria-describedby',message.id);footer.append(message,download,copyButton);dialog.append(header,formats,preview,hint,privacy,footer);document.body.append(dialog);bindBackdrop(dialog,()=>dialog.close());
 let assets={},snapshot=null,format='png',paused=false,epoch=0,copying=false;
 const reducedMotion=matchMedia('(prefers-reduced-motion: reduce)');
 const canCopy=()=>window.isSecureContext&&typeof window.ClipboardItem==='function'&&typeof navigator.clipboard?.write==='function';
 function release(){image.removeAttribute('src');download.removeAttribute('href');for(const asset of Object.values(assets)){if(asset.url){if(asset.downloaded)setTimeout(()=>URL.revokeObjectURL(asset.url),60000);else URL.revokeObjectURL(asset.url);}}assets={};}
 function show(){
  if(!snapshot)return;const asset=assets[format],animated=format==='svg',still=paused||reducedMotion.matches;
  for(const [key,b]of formatButtons)b.setAttribute('aria-pressed',String(key===format));
  description.textContent=`${snapshot.periodLabel||analyticsPeriodLabel(snapshot.period)} · ${animated?'Animated SVG':'PNG image'}`;
  hint.textContent=animated?(reducedMotion.matches?'Reduced motion: showing the final totals. The SVG respects the viewer’s motion preference.':'A 6-second loop: count up, celebrate, hold. Add the downloaded SVG to your GitHub README as an image.'):'A still image for chats, posts and anywhere you share images.';
  motion.hidden=!animated;motion.disabled=reducedMotion.matches||!assets.still?.url;motion.textContent=reducedMotion.matches?'Motion reduced':paused?'Play preview':'Pause preview';motion.setAttribute('aria-pressed',String(!still));
  // Some image engines do not propagate a page's emulated motion preference
  // into SVG images. Choose an explicit still for the local preview as well.
  const shown=animated&&still?(assets.png?.url?assets.png:assets.still):asset;image.hidden=!shown?.url;placeholder.hidden=!!shown?.url;placeholder.textContent=asset?.error?'The image could not be created.':'Creating your card…';
  if(shown?.url&&image.getAttribute('src')!==shown.url)image.src=shown.url;
  download.textContent=animated?'Download SVG':'Download PNG';download.setAttribute('aria-disabled',String(!asset?.url));
  if(asset?.url){download.href=asset.url;download.download=`codexify-stats-${snapshot.periodLabel?'week-in-review':snapshot.period==='all'?'lifetime':snapshot.period+(snapshot.period==='1'?'-day':'-days')}-${snapshot.today}.${format}`;}else download.removeAttribute('href');
  copyButton.textContent=animated?'Copy PNG':'Copy image';copyButton.disabled=!assets.png?.blob||copying||!canCopy();
  message.textContent=asset?.error?'Close this preview and try again.':!asset?.url?'':canCopy()?'Ready to share.':'Copy isn’t available in this browser. Download the PNG instead.';
 }
 reducedMotion.addEventListener('change',()=>{if(dialog.open)show();});
 dialog.addEventListener('close',()=>{epoch++;release();copying=false;});
 document.getElementById('settings-dialog')?.addEventListener('close',()=>{if(dialog.open)dialog.close();});
 download.addEventListener('click',event=>{const asset=assets[format];if(!asset?.url){event.preventDefault();return;}asset.downloaded=true;message.textContent=`${format.toUpperCase()} download started.`;});
 copyButton.addEventListener('click',()=>{
  const blob=assets.png?.blob;
  if(!blob||copying||!canCopy())return;
  const current=epoch;copying=true;copyButton.disabled=true;message.textContent='Copying image…';
  // Call write directly in the click gesture. Safari loses clipboard access if
  // rasterization or other async work is awaited before this call.
  let result;try{result=navigator.clipboard.write([new ClipboardItem({'image/png':blob})]);}
  catch{result=Promise.reject(new Error('clipboard unavailable'));}
  Promise.resolve(result).then(()=>{if(current===epoch)message.textContent='Image copied.';},()=>{if(current===epoch)message.textContent='Couldn’t copy the image. Use Download PNG, or try again.';}).finally(()=>{if(current===epoch){copying=false;copyButton.disabled=!canCopy();}});
 });
 return createAnalyticsShareCard.instance={
  isOpen:()=>dialog.open,
  open(value){
   if(dialog.open)return;
   release();snapshot=value;format='png';paused=false;const current=++epoch;copying=false;
   image.alt=analyticsShareDescription(snapshot);show();dialog.showModal();
   for(const [key,render]of [['png',()=>renderAnalyticsSharePNG(value)],['svg',()=>new Blob([renderAnalyticsShareSVG(value)],{type:'image/svg+xml'})],['still',()=>new Blob([renderAnalyticsShareSVG(value,true)],{type:'image/svg+xml'})]]){
    Promise.resolve().then(render).then(blob=>{if(current!==epoch||!dialog.open)return;assets[key]={blob,url:URL.createObjectURL(blob)};show();}).catch(()=>{if(current===epoch&&dialog.open){assets[key]={error:true};show();}});
   }
  }
 };
}

function analyticsPeriodLabel(period) {
 return period==='all'?'Lifetime':period==='1'?'Today':`Last ${period} days`;
}
// Export only the winning public model/effort and its aggregate count, never IDs
// or conversation metadata. Turns define usage; ties use active time, then name.
function analyticsShareTopModel(models=[]) {
 const row=[...models].filter(m=>m&&typeof m.model==='string'&&typeof m.effort==='string'&&Number.isFinite(m.turns)&&m.turns>0).sort((a,b)=>b.turns-a.turns||(b.activeMs||0)-(a.activeMs||0)||a.model.localeCompare(b.model)||a.effort.localeCompare(b.effort))[0];
 return row?{model:row.model,effort:row.effort,turns:row.turns}:null;
}
function analyticsShareNumber(key,value) {
 if(key==='activeMs')return analyticsDuration(value);
 const n=Math.max(0,Number.isFinite(value)?value:0);
 return new Intl.NumberFormat('en-US',{notation:n>=10000?'compact':'standard',maximumFractionDigits:n>=10000?1:0}).format(n);
}
function analyticsShareTiles(snapshot) {
 const top=snapshot.topModel,known=top&&top.model!=='unattributed';
 const model=known?top.model.replace(/^gpt-/,'GPT-').replace(/-sol$/,' Sol').replace(/-astra$/,' Astra'):top?'Not recorded':'No turns yet';
 const effort=known&&top.effort!=='unknown'?({max:'Pro',xhigh:'Extra High',low:'Instant'}[top.effort]||top.effort.replace(/^./,c=>c.toUpperCase())):'';
 return [
  {key:'turns',label:'Turns',x:66,y:180,width:330,angle:-.025,color:'#dfff75'},
  {key:'toolCalls',label:'Tool calls',x:435,y:187,width:330,angle:.02,color:'#ff91c8'},
  {key:'goals',label:'Goals completed',x:804,y:179,width:330,angle:-.018,color:'#80eadb'},
  {key:'activeMs',label:'Thinking hours',x:66,y:387,width:330,angle:.018,color:'#ffd28b'},
  {key:'topModel',label:`Top model${known?' · '+analyticsShareNumber('turns',top.turns)+' turns':''}`,value:model+(effort?(effort==='Pro'?' Pro':` (${effort})`):''),x:435,y:387,width:699,angle:-.008,color:'#fff9ea'}
 ].map(card=>({...card,height:174,value:card.value??analyticsShareNumber(card.key,snapshot.totals[card.key])}));
}
function analyticsShareDescription(snapshot) {
 const model=analyticsShareTiles(snapshot).find(card=>card.key==='topModel');
 return `Codexify stats: ${snapshot.totals.turns} turns, ${snapshot.totals.toolCalls} tool calls, ${snapshot.totals.goals} goals completed, ${analyticsShareNumber('activeMs',snapshot.totals.activeMs)} thinking time. ${model.label}: ${model.value}.`;
}

function renderAnalyticsSharePNG(snapshot) {
 const canvas=document.createElement('canvas');canvas.width=1200;canvas.height=675;
 const context=canvas.getContext('2d');if(!context)return Promise.reject(new Error('canvas unavailable'));
 const W=1200,H=675,ink='#241739';
 const rounded=(x,y,w,h,r)=>{r=Math.min(r,w/2,h/2);context.beginPath();context.moveTo(x+r,y);context.lineTo(x+w-r,y);context.quadraticCurveTo(x+w,y,x+w,y+r);context.lineTo(x+w,y+h-r);context.quadraticCurveTo(x+w,y+h,x+w-r,y+h);context.lineTo(x+r,y+h);context.quadraticCurveTo(x,y+h,x,y+h-r);context.lineTo(x,y+r);context.quadraticCurveTo(x,y,x+r,y);context.closePath();};
 const star=(x,y,r,color)=>{context.beginPath();for(let i=0;i<16;i++){const angle=i*Math.PI/8-Math.PI/2,rad=i%2?r*.52:r;const px=x+Math.cos(angle)*rad,py=y+Math.sin(angle)*rad;if(i)context.lineTo(px,py);else context.moveTo(px,py);}context.closePath();context.fillStyle=color;context.fill();};
 rounded(0,0,W,H,30);context.clip();context.fillStyle='#7651ed';context.fillRect(0,0,W,H);
 // Flat, saturated sticker colors keep the PNG small and the numbers legible.
 star(1158,558,26,'#dfff75');star(407,143,18,'#ff91c8');
 context.fillStyle='#ff91c8';context.beginPath();context.arc(31,375,10,0,Math.PI*2);context.fill();
 context.fillStyle='#dfff75';context.save();context.translate(1099,143);context.rotate(-.3);context.fillRect(-4,-14,8,28);context.fillRect(-14,-4,28,8);context.restore();
 context.textBaseline='alphabetic';context.fillStyle='#fff9ea';context.font='800 48px system-ui, -apple-system, sans-serif';context.fillText(snapshot.title||'Codexify stats',68,113);
 const period=(snapshot.periodLabel||analyticsPeriodLabel(snapshot.period)).toUpperCase();context.font='750 23px system-ui, -apple-system, sans-serif';const pillWidth=context.measureText(period).width+42;
 context.save();context.translate(W-72-pillWidth/2,85);context.rotate(.045);rounded(-pillWidth/2+5,-27+5,pillWidth,54,14);context.fillStyle=ink;context.fill();rounded(-pillWidth/2,-27,pillWidth,54,14);context.fillStyle='#fff9ea';context.fill();context.strokeStyle=ink;context.lineWidth=3;context.stroke();context.fillStyle=ink;context.textAlign='center';context.fillText(period,0,8);context.restore();
 for(const card of analyticsShareTiles(snapshot)){
  const {width,height}=card;context.save();context.translate(card.x+width/2,card.y+height/2);context.rotate(card.angle);
  rounded(-width/2+8,-height/2+9,width,height,22);context.fillStyle=ink;context.fill();rounded(-width/2,-height/2,width,height,22);context.fillStyle=card.color;context.fill();context.strokeStyle=ink;context.lineWidth=4;context.stroke();
  let value=card.value,size=card.key==='topModel'?50:80;context.font=`850 ${size}px system-ui, -apple-system, sans-serif`;while(context.measureText(value).width>width-52&&size>36){size-=2;context.font=`850 ${size}px system-ui, -apple-system, sans-serif`;}
  while(context.measureText(value).width>width-52&&value.length>1)value=value.slice(0,-2)+'…';
  context.fillStyle=ink;context.fillText(value,-width/2+25,7);context.font=`650 ${card.key==='topModel'?26:29}px system-ui, -apple-system, sans-serif`;context.fillText(card.label,-width/2+27,54);context.restore();
 }
 // One aggregate activity strip, without conversation details.
 const raw=snapshot.activity.length?snapshot.activity:[0,0,0,0,0,0,0],stride=Math.max(1,Math.ceil(raw.length/30)),bars=[];
 for(let i=0;i<raw.length;i+=stride)bars.push(raw.slice(i,i+stride).reduce((a,b)=>a+b,0));
 const max=Math.max(1,...bars),plotWidth=600,gap=bars.length<=7?12:5,barW=(plotWidth-gap*(bars.length-1))/bars.length;
 for(const [index,value]of bars.entries()){const height=value?Math.max(4,value/max*23):3;context.fillStyle=value?(index===bars.length-1?'#dfff75':'#fff9eaaa'):'#fff9ea50';rounded(72+index*(barW+gap),607-height,barW,height,Math.min(4,height/2));context.fill();}
 const date=iso=>{const value=new Date(iso+'T12:00:00Z');return Number.isFinite(value.getTime())?value:null;};
 const first=date(snapshot.start),last=date(snapshot.today),pretty=(d,year)=>new Intl.DateTimeFormat('en-US',{month:'short',day:'numeric',...(year?{year:'numeric'}:{}),timeZone:'UTC'}).format(d);
 const dates=first&&last?(snapshot.start===snapshot.today?pretty(last,true):`${pretty(first,first.getUTCFullYear()!==last.getUTCFullYear())} – ${pretty(last,true)}`):'Your agent activity';
 context.font='550 23px system-ui, -apple-system, sans-serif';context.fillStyle='#fff9ea';context.fillText(dates,72,645);
 context.save();context.translate(1002,617);context.rotate(-.035);rounded(-142,-30,284,60,17);context.fillStyle=ink;context.fill();context.textAlign='center';context.font='750 35px system-ui, -apple-system, sans-serif';context.fillStyle='#fff9ea';context.fillText('codexify.dev',0,12);context.restore();
 return new Promise((resolve,reject)=>{canvas.toBlob(blob=>blob?resolve(blob):reject(new Error('PNG unavailable')),'image/png');});
}

// Declarative SVG animation also runs in an <img>; scripts and remote resources
// do not. Final totals are the base drawing, so a still renderer stays useful.
function renderAnalyticsShareSVG(snapshot,still=false) {
 const escape=value=>String(value).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&apos;'}[c]));
 const text=(x,y,value,size=23,weight=650,extra='')=>`<text x="${x}" y="${y}" font-size="${size}" font-weight="${weight}" ${extra}>${escape(value)}</text>`;
 const rect=(x,y,w,h,r,fill,extra='')=>`<rect x="${x}" y="${y}" width="${w}" height="${h}" rx="${r}" fill="${fill}" ${extra}/>`;
 const anim=(attr,values,times)=>`<animate attributeName="${attr}" values="${values}" keyTimes="${times}" dur="6s" repeatCount="indefinite"/>`;
 const star=(x,y,r,color,rotate=false)=>{const points=Array.from({length:16},(_,i)=>{const a=i*Math.PI/8-Math.PI/2,s=i%2?r*.52:r;return `${(Math.cos(a)*s).toFixed(2)},${(Math.sin(a)*s).toFixed(2)}`;}).join(' ');return `<g transform="translate(${x} ${y})"><polygon points="${points}" fill="${color}"/>${rotate?`<g class="motion" visibility="hidden"><set attributeName="visibility" to="visible" begin="0s"/><circle r="${r+2}" fill="#7651ed"/><polygon points="${points}" fill="${color}"><animateTransform attributeName="transform" type="rotate" values="0;0;90;90" keyTimes="0;.24;.4;1" dur="6s" repeatCount="indefinite"/></polygon></g>`:''}</g>`;};
 const period=(snapshot.periodLabel||analyticsPeriodLabel(snapshot.period)).toUpperCase(),pillWidth=period.length*15+42;
 let defs='<clipPath id="edge"><rect width="1200" height="675" rx="30"/></clipPath>',cards='';
 for(const [i,card]of analyticsShareTiles(snapshot).entries()){
  const {width,height}=card,isModel=card.key==='topModel';
  const value=snapshot.totals[card.key]||0;
  // Bounded text frames keep the SVG compact even for lifetime totals. Ease out
  // over 1.7s, then hold the exact final value for the rest of the six-second loop.
  const frames=isModel?[]:Array.from({length:33},(_,n)=>analyticsShareNumber(card.key,card.key==='activeMs'?value*(1-Math.pow(1-n/32,2)):Math.round(value*(1-Math.pow(1-n/32,2)))));
  const size=Math.max(36,Math.min(isModel?50:80,Math.floor((width-52)/(Math.max(card.value.length,...frames.map(v=>v.length))*.66))));
  const fit=value=>{const limit=Math.floor((width-52)/(size*.66));return value.length>limit?value.slice(0,limit-1)+'…':value;};
  const times=frames.map((_,n)=>(n/32*.28).toFixed(5)).concat('1').join(';');
  const values=frames.map((_,n)=>`0 ${-n*100}`).concat(`0 ${-32*100}`).join(';');
  if(!isModel)defs+=`<clipPath id="digits-${i}"><rect x="20" y="15" width="290" height="95"/></clipPath>`;
  cards+=`<g transform="translate(${card.x+width/2} ${card.y+height/2}) rotate(${card.angle*180/Math.PI}) translate(${-width/2} ${-height/2})">
   ${rect(8,9,width,height,22,'#241739')}${rect(0,0,width,height,22,card.color,'stroke="#241739" stroke-width="4"')}
   ${text(25,94,fit(card.value),size,850,'data-final="'+card.key+'"')}
   ${isModel?'':`<g class="motion" visibility="hidden" clip-path="url(#digits-${i})"><set attributeName="visibility" to="visible" begin="0s"/>
    ${rect(20,15,290,95,0,card.color)}<g><animateTransform attributeName="transform" type="translate" calcMode="discrete" values="${values}" keyTimes="${times}" dur="6s" repeatCount="indefinite"/>
    ${frames.map((v,n)=>text(25,94+n*100,v,size,850)).join('')}</g>
   </g>`}${text(27,141,card.label,isModel?26:29)}
  </g>`;
 }
 const raw=snapshot.activity.length?snapshot.activity:[0,0,0,0,0,0,0],stride=Math.max(1,Math.ceil(raw.length/30)),bars=[];
 for(let i=0;i<raw.length;i+=stride)bars.push(raw.slice(i,i+stride).reduce((a,b)=>a+b,0));
 const max=Math.max(1,...bars),gap=bars.length<=7?12:5,w=(600-gap*(bars.length-1))/bars.length;
 const strip=bars.map((v,i)=>{const h=v?Math.max(4,v/max*23):3,start=(.02+i/bars.length*.12).toFixed(4),end=(.12+i/bars.length*.16).toFixed(4);return `<rect x="${72+i*(w+gap)}" y="${607-h}" width="${w}" height="${h}" rx="${Math.min(4,h/2)}" fill="${v?(i===bars.length-1?'#dfff75':'#fff9eaaa'):'#fff9ea50'}">${anim('height',`${h};0;${h};${h}`,`0;${start};${end};1`)}${anim('y',`${607-h};607;${607-h};${607-h}`,`0;${start};${end};1`)}</rect>`;}).join('');
 const pretty=(iso,year)=>new Intl.DateTimeFormat('en-US',{month:'short',day:'numeric',...(year?{year:'numeric'}:{}),timeZone:'UTC'}).format(new Date(iso+'T12:00:00Z'));
 const dates=snapshot.start===snapshot.today?pretty(snapshot.today,true):`${pretty(snapshot.start,snapshot.start.slice(0,4)!==snapshot.today.slice(0,4))} – ${pretty(snapshot.today,true)}`;
 const sparks=Array.from({length:12},(_,i)=>{const angle=i/12*Math.PI*2,x=Math.cos(angle)*36,y=Math.sin(angle)*22;return `<g transform="translate(${754+x} ${615+y})"><path d="M-3 0h6M0-3v6" fill="none" stroke="${i%2?'#ff91c8':'#dfff75'}" stroke-width="4" stroke-linecap="round"/>${anim('opacity','0;0;1;0;0','0;.25;.32;.52;1')}<animateTransform attributeName="transform" type="translate" additive="sum" values="0 0;0 0;${x*.5} ${y*.5};${x*.5} ${y*.5}" keyTimes="0;.25;.52;1" dur="6s" repeatCount="indefinite"/></g>`;}).join('');
 return `<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="675" viewBox="0 0 1200 675" role="img" aria-labelledby="title desc">
 <title id="title">Codexify stats · ${escape(period)}</title><desc id="desc">${escape(`${analyticsShareDescription(snapshot)} ${dates}.`)}</desc>
 <style>text{font-family:system-ui,-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;fill:#241739}.cream{fill:#fff9ea}@media(prefers-reduced-motion:reduce){.motion{display:none}animate,animateTransform,set{display:none}}</style>
 <defs>${defs}</defs><g clip-path="url(#edge)">${rect(0,0,1200,675,0,'#7651ed')}
 ${star(1158,558,26,'#dfff75',true)}${star(407,143,18,'#ff91c8',true)}<circle cx="31" cy="375" r="10" fill="#ff91c8"/>
 <g transform="translate(1099 143) rotate(-17)"><path d="M-14 0h28M0-14v28" stroke="#dfff75" stroke-width="8"/></g>
 ${text(68,113,snapshot.title||'Codexify stats',48,800,'class="cream"')}
 <g transform="translate(${1128-pillWidth/2} 85) rotate(2.58)">${rect(-pillWidth/2+5,-22,pillWidth,54,14,'#241739')}${rect(-pillWidth/2,-27,pillWidth,54,14,'#fff9ea','stroke="#241739" stroke-width="3"')}${text(0,8,period,23,750,'text-anchor="middle"')}</g>
 ${cards}<g class="motion">${sparks}<g transform="translate(754 645) scale(.7)" opacity="0">${anim('opacity','0;0;1;1;0;0','0;.24;.28;.5;.6;1')}<g>
 <animateTransform attributeName="transform" type="scale" values="1 1;.9 1.08;1.04 .96;1 1" dur=".6s" repeatCount="indefinite"/>
 <path d="M0 5C-39 5-38-29-19-47C-22-33-10-32-12-50C-13-64 4-70 3-84C30-64 8-48 27-39C29-48 26-52 26-52C53-24 35 5 0 5Z" fill="#ff763e" stroke="#241739" stroke-width="3" stroke-linejoin="round"/>
 <path d="M0 0C-20 0-20-19-8-29C-8-15 1-24 1-41C21-25 21 0 0 0Z" fill="#ffe875"/></g></g></g>
 <g class="motion">${strip}</g><g class="still-strip">${bars.map((v,i)=>{const h=v?Math.max(4,v/max*23):3;return rect(72+i*(w+gap),607-h,w,h,Math.min(4,h/2),v?(i===bars.length-1?'#dfff75':'#fff9eaaa'):'#fff9ea50');}).join('')}</g>
 ${text(72,645,dates,23,550,'class="cream"')}<g transform="translate(1002 617) rotate(-2)">${rect(-142,-30,284,60,17,'#241739')}${text(0,12,'codexify.dev',35,750,'class="cream" text-anchor="middle"')}</g>
 </g><style>.still-strip{display:none}@media(prefers-reduced-motion:reduce){.still-strip{display:inline}}${still?'.motion{display:none}.still-strip{display:inline}':''}</style></svg>`;
}
