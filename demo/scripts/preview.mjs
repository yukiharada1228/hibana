import {bundle} from '@remotion/bundler';
import {openBrowser, selectComposition, renderStill} from '@remotion/renderer';
import {mkdir, rm} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {resolve} from 'node:path';

const chrome='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const serveUrl=await bundle({entryPoint:resolve('src/index.tsx')});
const browser=await openBrowser('chrome', {browserExecutable:existsSync(chrome)?chrome:undefined});
try {
  const composition=await selectComposition({serveUrl,id:'HibanaDemo',puppeteerInstance:browser});
  await mkdir('output/preview',{recursive:true});
  for(const second of [4,16,27,41,58,69,78,86,89]){
    await renderStill({serveUrl,composition,puppeteerInstance:browser,frame:second*30,output:resolve(`output/preview/${second}.png`),imageFormat:'png',scale:0.65});
    console.log(`Rendered ${second}s`);
  }
} finally {await browser.close({silent:true});await rm(serveUrl,{recursive:true,force:true});}
