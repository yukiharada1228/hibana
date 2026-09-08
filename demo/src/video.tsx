import React from 'react';
import {AbsoluteFill, interpolate, Sequence, useCurrentFrame, useVideoConfig} from 'remotion';
import recording from '../public/recording.json';

const C = {bg:'#080d14',panel:'#101923',panel2:'#152230',ink:'#f7f7f2',muted:'#9caab8',line:'#2b3b4b',orange:'#ff703e',green:'#91ecb2',cyan:'#80e0f0',purple:'#b8a5ff'};
const sans='"Hiragino Sans", "Yu Gothic", sans-serif';
const mono='"Menlo", "SFMono-Regular", monospace';
const rec:any = recording;
const proof = rec.proof;
const step = (id:string) => rec.steps.find((s:any)=>s.id===id);
const stages = [
  {from:0,to:6,label:'HIBANA / WASM FAAS'},
  {from:6,to:19,label:'01 / PLATFORM'},
  {from:19,to:31,label:'02 / HONO'},
  {from:31,to:45,label:'03 / WEBASSEMBLY'},
  {from:45,to:61,label:'04 / HTTP REQUESTS'},
  {from:61,to:72,label:'05 / WASMTIME'},
  {from:72,to:80,label:'06 / DELETE APP'},
  {from:80,to:87,label:'07 / UNINSTALL'},
  {from:87,to:90,label:'HIBANA / COMPLETE'},
];
const clamp={extrapolateLeft:'clamp',extrapolateRight:'clamp'} as const;
function enter(f:number,delay=0){return {opacity:interpolate(f,[delay,delay+13],[0,1],clamp),transform:`translateY(${interpolate(f,[delay,delay+18],[18,0],clamp)}px)`};}
function Flame({size=46}:{size?:number}){return <svg width={size} height={size*1.2} viewBox="0 0 48 60"><path fill={C.orange} d="M27 0C31 15 13 19 16 32C8 28 9 20 9 20C-9 43 10 60 25 59C48 60 55 30 34 16C39 33 21 36 27 0Z"/><path fill="#ffd49e" d="M26 32C16 40 16 47 23 53C38 49 31 39 26 32Z"/></svg>}
function Badge({children,color=C.green}:{children:React.ReactNode,color?:string}){return <div style={{display:'inline-flex',alignItems:'center',gap:12,padding:'12px 20px',background:`${color}12`,border:`1px solid ${color}40`,borderRadius:10,color,fontSize:23,fontWeight:600,letterSpacing:1}}><span style={{width:8,height:8,borderRadius:8,background:color}}/>{children}</div>}
function Arrow({width=72}:{width?:number}){const f=useCurrentFrame();return <svg width={width} height="52" viewBox={`0 0 ${width} 52`}><path d={`M0 26 H${width-10} M${width-22} 15 L${width-9} 26 L${width-22} 37`} stroke={C.line} strokeWidth="3" fill="none"/><circle cx={(f*2.5)%width} cy="26" r="5" fill={C.orange}/></svg>}
function ChromeBar({title}:{title:string}){return <div style={{height:58,borderBottom:`1px solid ${C.line}`,display:'flex',alignItems:'center',padding:'0 28px',gap:10}}>{['#fe725c','#dcb35c','#71bb8d'].map(c=><span key={c} style={{width:12,height:12,borderRadius:10,background:c}}/>)}<span style={{marginLeft:18,color:C.muted,fontSize:23,fontFamily:mono}}>{title}</span></div>}
function Terminal({children,style={},title='terminal'}:{children:React.ReactNode,style?:React.CSSProperties,title?:string}){return <div style={{background:C.panel,border:`1px solid ${C.line}`,borderRadius:18,boxShadow:'0 24px 80px #00000030',overflow:'hidden',...style}}><ChromeBar title={title}/><div style={{padding:'28px 34px',fontFamily:mono,fontSize:31,lineHeight:1.6}}>{children}</div></div>}
function Cmd({text,delay=15,size=36}:{text:string,delay?:number,size?:number}){const f=useCurrentFrame();const n=Math.min(text.length,Math.floor(Math.max(0,f-delay)*1.65));return <div style={{fontSize:size,color:C.ink,whiteSpace:'pre-wrap',overflowWrap:'anywhere',minHeight:size*1.6,marginBottom:16}}><span style={{color:C.orange}}>$ </span>{text.slice(0,n)}{f<delay+text.length/1.65+18&&<span style={{background:C.orange,display:'inline-block',width:18,height:size,verticalAlign:'middle',opacity:Math.floor(f/12)%2?0:1}}/>}</div>}
function Out({children,at=65,color=C.muted,size=30}:{children:React.ReactNode,at?:number,color?:string,size?:number}){const f=useCurrentFrame();return <div style={{...enter(f,at),color,fontSize:size,whiteSpace:'pre-wrap',overflowWrap:'anywhere'}}>{children}</div>}
function Heading({title,kicker}:{title:React.ReactNode,kicker:string}){const f=useCurrentFrame();return <div style={{position:'absolute',top:140,left:88,right:88,...enter(f)}}><div style={{fontSize:23,color:C.orange,letterSpacing:3,marginBottom:16}}>{kicker}</div><div style={{fontSize:64,fontWeight:700,letterSpacing:-2,lineHeight:1.22}}>{title}</div></div>}
function Caption({children}:{children:React.ReactNode}){return <div style={{position:'absolute',bottom:94,left:88,right:88,fontSize:48,fontWeight:500,textAlign:'center',lineHeight:1.5,color:C.ink}}>{children}</div>}
function Panel({children,style={}}:{children:React.ReactNode,style?:React.CSSProperties}){return <div style={{background:C.panel,border:`1px solid ${C.line}`,borderRadius:18,padding:32,...style}}>{children}</div>}
function Intro(){const f=useCurrentFrame();return <AbsoluteFill>
  <div style={{position:'absolute',top:188,left:100,...enter(f,4)}}><Badge color={C.orange}>REAL CLI DEMO</Badge><div style={{fontSize:106,fontWeight:750,letterSpacing:-5,lineHeight:1.28,marginTop:30}}>その Hono、<br/><span style={{color:C.orange}}>Wasm</span> で動きます。</div><div style={{color:C.muted,fontSize:34,marginTop:38}}>構築 → 配備 → 実行 → 削除</div></div>
  <div style={{position:'absolute',left:1270,top:268,width:450,height:450,transform:`translateY(${Math.sin(f/28)*7}px) rotate(-6deg)`,...{opacity:interpolate(f,[15,40],[0,1],clamp)}}}>
    <div style={{position:'absolute',inset:-60,background:'radial-gradient(circle,#ff703e20,transparent 70%)'}}/>
    <div style={{position:'absolute',inset:0,border:`2px solid ${C.orange}`,background:'linear-gradient(145deg,#3a251f,#14202c)',borderRadius:36,boxShadow:'22px 24px 0 #18232f',display:'flex',flexDirection:'column',alignItems:'center',justifyContent:'center'}}><span style={{fontFamily:mono,fontSize:92,fontWeight:700,letterSpacing:-6}}>.wasm</span><div style={{fontSize:28,color:C.orange,marginTop:22}}>WebAssembly Component</div><div style={{fontFamily:mono,fontSize:24,color:C.muted,marginTop:32}}>00 61 73 6d</div></div>
  </div><Caption>Hono を WebAssembly にして、Kubernetes 上で実行。</Caption>
</AbsoluteFill>}
function Install(){const f=useCurrentFrame();const lines=step('install').output.split(/\r?\n/);const ready=lines.find((l:string)=>l.startsWith('Ready. API:'))||'';return <AbsoluteFill>
  <Heading kicker="ONE COMMAND TO START" title={<>まず、<span style={{color:C.orange}}>基盤をつくる。</span></>}/>
  <Terminal style={{position:'absolute',left:88,top:290,width:1744,height:422}} title="hibana / platform"><Cmd text="hibana platform install --cluster hibana-demo"/>
    <Out at={70}>Creating cluster "hibana-demo" ...</Out><Out at={112}>deployment "hibana-control-plane" successfully rolled out</Out><Out at={154}>deployment "hibana-worker" successfully rolled out</Out><Out at={196} color={C.green} size={28}>{ready.split(' | ')[0]}</Out>
  </Terminal>
  <div style={{position:'absolute',left:88,right:88,top:740,display:'flex',gap:24}}>{[['Kubernetes','3 nodes'],['Control Plane','2 Pods'],['Wasmtime Worker',`${proof.workersBefore.length} Pods`]].map(([a,b],i)=><Panel key={a} style={{flex:1,...enter(f,210+i*14),padding:'22px 30px',display:'flex',justifyContent:'space-between',alignItems:'center'}}><span style={{fontSize:27,color:C.muted}}>{a}</span><b style={{fontSize:37}}>{b}</b></Panel>)}</div>
  <Caption>クラスタ作成から、DB準備・起動確認まで。</Caption>
</AbsoluteFill>}
function SyntaxLine({line}:{line:string}){const chunks=line.split(/('.*?'|\b(?:import|from|const|let|export|default)\b|\+\+count|\bHono\b)/g);return <>{chunks.map((t,i)=><span key={i} style={{color:t.startsWith("'")?C.green:t==='++count'?C.orange:/^(import|from|const|let|export|default)$/.test(t)?C.purple:t==='Hono'?C.orange:C.ink}}>{t}</span>)}</>}
function Code(){const f=useCurrentFrame();const lines=rec.source.trim().split('\n');return <AbsoluteFill><Heading kicker="WRITE AN ORDINARY HONO APP" title={<>コードは、いつもの <span style={{color:C.orange}}>Hono。</span></>}/>
  <div style={{position:'absolute',left:88,top:286,width:1744,fontFamily:mono}}><Cmd text="hibana init hello-hono --template hono" size={34}/></div>
  <div style={{position:'absolute',left:88,top:374,width:1040,height:496,background:C.panel,border:`1px solid ${C.line}`,borderRadius:18,overflow:'hidden',...enter(f,40)}}><ChromeBar title="hello-hono / src/index.ts"/><div style={{padding:'18px 26px',fontFamily:mono,fontSize:35,lineHeight:1.25}}>{lines.map((l:string,i:number)=>l.trim()&&<div key={i} style={{whiteSpace:'pre',padding:'1px 8px',background:l.includes('count:')?'#ff703e14':'transparent',borderLeft:l.includes('count:')?`3px solid ${C.orange}`:'3px solid transparent'}}><span style={{color:'#4e6275',fontSize:23,display:'inline-block',width:48,textAlign:'right',paddingRight:24}}>{i+1}</span><SyntaxLine line={l}/></div>)}</div></div>
  <Panel style={{position:'absolute',left:1160,top:374,width:672,height:496,...enter(f,90)}}><Badge color={C.orange}>ISOLATION CHECK</Badge><div style={{fontSize:39,lineHeight:1.5,marginTop:36,fontWeight:600}}>グローバル変数を<br/>リクエストで増やす。</div><div style={{fontFamily:mono,fontSize:65,color:C.orange,marginTop:27}}>++count</div><div style={{fontSize:28,color:C.muted,marginTop:20}}>続けて呼ぶと、どうなる？</div></Panel>
  <Caption>特別なアプリ用コンテナ定義は不要。</Caption>
</AbsoluteFill>}
function Deploy(){const f=useCurrentFrame();const hash=proof.artifact.sha256;const exportName=proof.artifact.export.replace(/^export /,'').replace(/;$/,'');return <AbsoluteFill><Heading kicker="BUILD → PREPARE → ACTIVATE" title={<>配備されるのは、<span style={{color:C.orange}}>.wasm。</span></>}/>
  <Terminal style={{position:'absolute',left:88,top:294,width:1050,height:568}} title="hello-hono / deploy"><Cmd text="hibana deploy --version 1.0.0"/><Out at={62} color={C.green}>Deployed hello-hono</Out><Out at={84}>version 1.0.0</Out><div style={{height:26}}/><Cmd delay={112} text="xxd -l 8 .hibana/build/app.wasm" size={29}/><Out at={155} size={27} color={C.cyan}>{step('wasm-header').output.trim()}</Out><div style={{height:25}}/><Out at={210} color={C.muted} size={25}>WASI HTTP export</Out><Out at={225} color={C.green} size={25}>{exportName}</Out></Terminal>
  <Panel style={{position:'absolute',left:1168,top:294,width:664,height:568,...enter(f,52)}}><Badge color={C.cyan}>VERIFIED ARTIFACT</Badge><div style={{fontFamily:mono,fontSize:100,fontWeight:700,letterSpacing:-7,marginTop:22}}>.wasm</div><div style={{fontSize:28,color:C.muted}}>WebAssembly Component</div><div style={{height:1,background:C.line,margin:'32px 0'}}/><div style={{fontSize:22,color:C.muted}}>SHA-256 / 配備済みバージョンと一致</div><div style={{fontFamily:mono,fontSize:25,color:C.cyan,marginTop:16}}>{hash.slice(0,24)}…</div><div style={{fontSize:26,color:C.green,marginTop:28}}>事前コンパイル済み · {proof.preparedWorkers.length} Workers</div><div style={{fontSize:22,color:C.muted,marginTop:12}}>ビルド＋配備の実測 {step('deploy').durationSeconds.toFixed(1)} 秒</div></Panel>
  <Caption>デプロイ時にコンパイルし、準備を終えてから公開。</Caption>
</AbsoluteFill>}
function Invoke(){const f=useCurrentFrame();return <AbsoluteFill><Heading kicker="THREE REAL HTTP REQUESTS" title={<>3 回呼んでも、<span style={{color:C.orange}}>毎回 1。</span></>}/>
  <div style={{position:'absolute',left:88,top:284,right:88,fontFamily:mono,fontSize:29,color:C.muted}}><span style={{color:C.orange}}>$ </span>curl -H 'Host: hello-hono.smoke.hibana.local' http://127.0.0.1:18084/</div>
  <div style={{position:'absolute',top:364,left:88,right:88,display:'flex',gap:26}}>{proof.responses.map((r:any,i:number)=><Panel key={i} style={{flex:1,height:500,padding:28,...enter(f,48+i*92)}}>
    <div style={{display:'flex',justifyContent:'space-between',alignItems:'center'}}><span style={{fontFamily:mono,fontSize:24,color:C.muted}}>REQUEST 0{i+1}</span><Badge>200 OK</Badge></div>
    <div style={{fontFamily:mono,fontSize:28,color:C.green,marginTop:24}}>{JSON.stringify(r.message)}</div>
    <div style={{display:'flex',alignItems:'baseline',gap:24,marginTop:12}}><span style={{fontSize:31,fontFamily:mono,color:C.muted}}>count:</span><span style={{fontSize:102,lineHeight:1.1,fontFamily:mono,color:C.orange,fontWeight:700}}>{r.count}</span></div>
    <div style={{marginTop:6,display:'flex',gap:18,alignItems:'baseline'}}><span style={{fontFamily:mono,fontSize:42,color:C.cyan}}>{proof.httpTimeMs[i].toFixed(1)} ms</span><span style={{fontSize:21,color:C.muted}}>HTTP実測</span></div>
    <div style={{fontFamily:mono,fontSize:18,color:C.muted,marginTop:18}}>{proof.executions[i].id.slice(0,22)}…</div>
    <div style={{fontFamily:mono,fontSize:18,color:C.muted,marginTop:8}}>worker / {proof.executions[i].workerPod.split('-').at(-1)}</div>
  </Panel>)}</div>
  <Caption>コードを再利用し、Wasm インスタンスは毎回新しく。</Caption>
</AbsoluteFill>}
function Model(){const f=useCurrentFrame();return <AbsoluteFill><Heading kicker="WHAT ACTUALLY RAN" title={<>同じ Worker、<span style={{color:C.orange}}>別々の実行。</span></>}/>
  <div style={{position:'absolute',left:92,top:302,fontSize:22,color:C.muted}}>実行モデル（模式図）</div>
  <div style={{position:'absolute',top:370,left:88,width:1744,display:'flex',alignItems:'center',gap:22}}>
    <Panel style={{width:250,height:298,display:'flex',flexDirection:'column',justifyContent:'center',alignItems:'center'}}><div style={{fontSize:52,fontFamily:mono,color:C.cyan}}>HTTP</div><div style={{fontSize:25,color:C.muted,marginTop:22}}>3 requests</div></Panel><Arrow width={60}/>
    <Panel style={{width:300,height:298,display:'flex',flexDirection:'column',justifyContent:'center',alignItems:'center'}}><div style={{fontSize:35,textAlign:'center',lineHeight:1.5}}>Control<br/>Plane</div><div style={{fontSize:24,color:C.muted,marginTop:22}}>受付・振り分け</div></Panel><Arrow width={60}/>
    <Panel style={{width:624,height:398,borderColor:'#ff703e70',padding:26}}><div style={{display:'flex',justifyContent:'space-between',alignItems:'center',fontSize:27}}><b>Wasmtime Worker</b><Badge color={C.orange}>2 → 2 Pods</Badge></div><div style={{display:'flex',gap:20,marginTop:26}}>{proof.workersAfter.map((w:any,i:number)=><div key={w.uid} style={{background:C.bg,border:`1px solid ${C.line}`,borderRadius:13,padding:18,flex:1,height:240}}><div style={{fontSize:21,fontFamily:mono,color:C.muted}}>worker 0{i+1}</div><div style={{marginTop:29,padding:'24px 8px',borderRadius:10,border:`1px solid ${C.orange}`,background:`rgba(255,112,62,${0.08+Math.sin((f+i*25)/17)**2*.08})`,textAlign:'center',fontFamily:mono,fontSize:28,color:C.orange}}>Wasm instance</div><div style={{fontSize:17,color:C.muted,fontFamily:mono,marginTop:24}}>UID {w.uid.slice(0,8)}</div></div>)}</div></Panel>
    <div style={{flex:1,paddingLeft:6}}><div style={{fontSize:58,color:C.green,fontFamily:mono}}>3 / 3</div><div style={{fontSize:23,color:C.muted,marginTop:10}}>実行記録：succeeded</div><div style={{height:1,background:C.line,margin:'32px 0'}}/><div style={{fontSize:26,color:C.cyan}}>SHA-256 一致</div><div style={{fontSize:22,color:C.muted,marginTop:12,lineHeight:1.6}}>ローカル成果物と<br/>実行したバージョン</div></div>
  </div>
  <div style={{position:'absolute',left:88,top:815,right:88,textAlign:'center',fontSize:27,color:C.muted}}>Worker Pod の UID は、配備・実行の前後で不変。</div>
  <Caption>アプリ専用Podを作らず、共有の Wasmtime で実行。</Caption>
</AbsoluteFill>}
function Delete(){const f=useCurrentFrame();return <AbsoluteFill><Heading kicker="REMOVE THE APPLICATION" title={<>アプリの削除も、<span style={{color:C.orange}}>CLI で。</span></>}/>
  <Terminal style={{position:'absolute',left:88,top:300,width:1158,height:562}} title="hello-hono / cleanup"><Cmd text="hibana delete hello-hono --yes"/><Out at={55} color={C.green}>{step('delete').output.trim().split('\n').at(-1)}</Out><div style={{height:25}}/><Cmd delay={92} text="hibana list"/><Out at={125} color={C.cyan}>{step('list-empty').output.trim()}</Out><Out at={153} size={25}>公開URLへのHTTPリクエスト</Out><Out at={173} color={C.orange}>{proof.deletedHttpStatus}</Out></Terminal>
  <Panel style={{position:'absolute',left:1276,top:300,width:556,height:562,display:'flex',flexDirection:'column',justifyContent:'center',alignItems:'center',...enter(f,85)}}><div style={{fontFamily:mono,fontSize:142,color:C.orange,fontWeight:700}}>404</div><div style={{fontSize:30,marginTop:18}}>公開URLも停止。</div><div style={{fontSize:25,color:C.muted,marginTop:30}}>デプロイ済みアプリ：0</div></Panel>
  <Caption>一覧は空に。削除したアプリは呼び出せません。</Caption>
</AbsoluteFill>}
function Uninstall(){const f=useCurrentFrame();return <AbsoluteFill><Heading kicker="REMOVE THE DEMO PLATFORM" title={<>最後は、<span style={{color:C.orange}}>基盤ごと撤去。</span></>}/>
  <Terminal style={{position:'absolute',left:88,top:300,width:1744,height:315}} title="hibana / teardown"><Cmd text="hibana platform uninstall --cluster hibana-demo --yes" size={35}/><Out at={62}>Deleting cluster "hibana-demo" ...</Out><Out at={105} color={C.green}>Uninstalled local cluster hibana-demo.</Out></Terminal>
  <div style={{position:'absolute',left:88,right:88,top:655,display:'flex',gap:22}}>{['control-plane','worker','worker2'].map((name,i)=><Panel key={name} style={{flex:1,height:181,opacity:interpolate(f,[65+i*14,100+i*14],[1,.32],clamp),display:'flex',flexDirection:'column',alignItems:'center',justifyContent:'center'}}><span style={{fontFamily:mono,fontSize:24,color:C.muted}}>hibana-demo-{name}</span><span style={{fontSize:32,color:C.green,marginTop:19,opacity:interpolate(f,[82+i*14,100+i*14],[0,1],clamp)}}>REMOVED</span></Panel>)}</div>
  <Caption>デモ専用クラスタのノード・データまで削除完了。</Caption>
</AbsoluteFill>}
function End(){const f=useCurrentFrame();return <AbsoluteFill><div style={{position:'absolute',inset:0,display:'flex',flexDirection:'column',alignItems:'center',justifyContent:'center',paddingBottom:70,...enter(f)}}><div style={{display:'flex',alignItems:'center',gap:30}}><Flame size={90}/><span style={{fontSize:140,letterSpacing:-7,fontWeight:700}}>hibana</span></div><div style={{fontSize:57,fontWeight:650,marginTop:45}}>構築から、後片付けまで。</div><div style={{fontFamily:mono,fontSize:28,color:C.muted,marginTop:37}}>Hono · WebAssembly · Wasmtime · Kubernetes</div></div><Caption>自分の Kubernetes に、Wasm FaaS を。</Caption></AbsoluteFill>}

export function HibanaDemo(){const f=useCurrentFrame();const {fps,durationInFrames}=useVideoConfig();if(!rec.complete || !proof.preparedBeforeRequests || proof.httpTimeMs?.length!==3)throw new Error('Record and verify the latest prepared lifecycle before rendering');const s=stages.find(s=>f>=s.from*fps&&f<s.to*fps)||stages[8];return <AbsoluteFill style={{background:C.bg,color:C.ink,fontFamily:sans,overflow:'hidden'}}>
  <style>{`*{box-sizing:border-box;} body{margin:0;}`}</style><div style={{position:'absolute',inset:0,backgroundImage:'radial-gradient(ellipse at 90% 8%,#ff703e0d,transparent 45%),linear-gradient(#ffffff02 1px,transparent 1px),linear-gradient(90deg,#ffffff02 1px,transparent 1px)',backgroundSize:'100% 100%,64px 64px,64px 64px'}}/>
  <div style={{position:'absolute',left:88,right:88,top:42,height:64,borderBottom:`1px solid ${C.line}`,display:'flex',justifyContent:'space-between',alignItems:'flex-start'}}><div style={{display:'flex',gap:13,alignItems:'center'}}><Flame size={27}/><b style={{fontSize:30,letterSpacing:-1}}>hibana</b><span style={{fontSize:20,color:C.muted,marginLeft:18,letterSpacing:2}}>WASM FAAS</span></div><div style={{fontFamily:mono,fontSize:21,color:C.muted,paddingTop:8}}>{s.label}</div></div>
  <Sequence from={0} durationInFrames={180}><Intro/></Sequence>
  <Sequence from={180} durationInFrames={390}><Install/></Sequence>
  <Sequence from={570} durationInFrames={360}><Code/></Sequence>
  <Sequence from={930} durationInFrames={420}><Deploy/></Sequence>
  <Sequence from={1350} durationInFrames={480}><Invoke/></Sequence>
  <Sequence from={1830} durationInFrames={330}><Model/></Sequence>
  <Sequence from={2160} durationInFrames={240}><Delete/></Sequence>
  <Sequence from={2400} durationInFrames={210}><Uninstall/></Sequence>
  <Sequence from={2610} durationInFrames={90}><End/></Sequence>
  <div style={{position:'absolute',left:88,bottom:33,fontSize:19,color:'#7f8d9b',letterSpacing:.5}}>実行ログを編集・待ち時間を短縮 ｜ kind 上の実測デモ</div><div style={{position:'absolute',right:88,bottom:33,fontFamily:mono,fontSize:19,color:'#7f8d9b'}}>{Math.floor(f/fps).toString().padStart(2,'0')} / 90 SEC</div>
  <div style={{position:'absolute',bottom:0,left:0,height:5,background:C.orange,width:`${100*f/(durationInFrames-1)}%`}}/>
</AbsoluteFill>}
