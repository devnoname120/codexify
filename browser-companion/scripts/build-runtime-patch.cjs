// Rebuild only the companion app archive; Chrome and the profile stay separate.
const fs=require('node:fs');
const path=require('node:path');
const {createHash,randomUUID}=require('node:crypto');
const asar=require('@electron/asar');
const root=path.resolve(__dirname,'..');
const version=fs.readFileSync(path.join(root,'../Cargo.toml'),'utf8').match(/^version\s*=\s*"([^"]+)"/m)[1];
const resources=path.join(process.env.USERPROFILE,'.codexify/browser/runtime',version,'app/resources');
const live=path.join(resources,'app.asar');
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
(async()=>{
  if(fs.existsSync(live+'.unpacked'))throw new Error('This patch requires an entirely packed JavaScript runtime');
  const staging=path.join(root,'artifacts','runtime-patch-'+randomUUID());
  fs.mkdirSync(staging,{recursive:true});
  const unpacked=path.join(staging,'application');
  asar.extractAll(live,unpacked);
  const metadata=JSON.parse(fs.readFileSync(path.join(unpacked,'package.json'),'utf8'));
  if(metadata.main!=='dist/main.js')throw new Error('Unexpected companion application entrypoint');
  const bundle=fs.readFileSync(path.join(root,'dist/main.js'));
  fs.writeFileSync(path.join(unpacked,'dist/main.js'),bundle);
  const candidate=path.join(staging,'app.asar');
  await asar.createPackage(unpacked,candidate);
  if(hash(asar.extractFile(candidate,'dist/main.js'))!==hash(bundle))throw new Error('Patched runtime bundle verification failed');
  const receipt={version,candidate,previousSha256:hash(fs.readFileSync(live)),sha256:hash(fs.readFileSync(candidate)),bundleSha256:hash(bundle)};
  fs.writeFileSync(path.join(staging,'receipt.json'),JSON.stringify(receipt,null,2));
  fs.writeFileSync(path.join(root,'artifacts','runtime-patch-ready.json'),JSON.stringify(receipt,null,2));
  console.log(JSON.stringify({ok:true,...receipt}));
})().catch(error=>{console.error(error.message);process.exitCode=1;});
