const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

async function main() {
  const artifact = path.resolve(process.argv[2]);
  const ready = path.resolve(process.argv[3]);
  const name = path.basename(artifact);
  const version = /^codexify-browser-v(\d+\.\d+\.\d+)-windows-x64\.zip$/.exec(name)?.[1];
  if (!version || !fs.statSync(artifact).isFile()) throw Error('Expected a Windows browser release archive');
  const hash = crypto.createHash('sha256');
  for await (const chunk of fs.createReadStream(artifact)) hash.update(chunk);
  const checksum = `${hash.digest('hex')}  ${name}\n`;
  const server = http.createServer((request, response) => {
    if (!['GET', 'HEAD'].includes(request.method)) { response.writeHead(405).end(); return; }
    if (request.url === `/v${version}/checksums.txt`) {
      response.writeHead(200, {'Content-Type':'text/plain', 'Content-Length':Buffer.byteLength(checksum)});
      response.end(request.method === 'HEAD' ? undefined : checksum);
    } else if (request.url === `/v${version}/${name}`) {
      response.writeHead(200, {'Content-Type':'application/zip', 'Content-Length':fs.statSync(artifact).size});
      if (request.method === 'HEAD') response.end();
      else {
        const stream = fs.createReadStream(artifact);
        stream.on('error', () => response.destroy());
        response.on('close', () => stream.destroy());
        stream.pipe(response);
      }
    } else response.writeHead(404).end();
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  fs.writeFileSync(ready, JSON.stringify({root:`http://127.0.0.1:${server.address().port}`, version}));
}
main().catch(error => { console.error(error.message); process.exitCode = 1; });
