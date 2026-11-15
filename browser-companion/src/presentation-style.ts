import {lstat,readFile,writeFile,rename} from "node:fs/promises";
import {randomUUID} from "node:crypto";

export function validatePresentationCss(css:string):string {
  if(Buffer.byteLength(css)>32768)throw new Error("Fast presentation CSS exceeds 32 KiB");
  // Local presentation only: no imports, resources, escapes that can obscure
  // a resource function, or legacy executable CSS. This never changes CSP.
  if(/@import|url\s*\(|expression\s*\(|-moz-binding|\\/i.test(css))throw new Error("Fast presentation CSS must not request external resources");
  return css;
}
export async function readPresentationCss(path?:string):Promise<string> {
  if(!path)return "";
  try {
    const metadata=await lstat(path);
    if(!metadata.isFile()||metadata.isSymbolicLink()||metadata.size>32768)throw new Error("Fast presentation CSS must be a bounded regular file");
    return validatePresentationCss(await readFile(path,"utf8"));
  }catch(error){if((error as NodeJS.ErrnoException).code==="ENOENT")return "";throw error;}
}
export async function savePresentationCss(path:string,css:string):Promise<void> {
  validatePresentationCss(css);
  await readPresentationCss(path);
  const temporary=`${path}.${randomUUID()}.tmp`;
  await writeFile(temporary,css,{mode:0o600,flag:"wx"});
  await rename(temporary,path);
}
