"use strict";
const fs=require("node:fs");
const path=require("node:path");

// On Windows, PowerShell/nvm may omit System32 from PATH. Never fall back
// to an unrelated tar.exe when producing or verifying release artifacts.
function archiveTool({platform=process.platform,systemRoot=process.env.SystemRoot,exists=fs.existsSync}={}) {
  if(platform!=="win32")return "tar";
  const root=systemRoot||"C:\\Windows";
  if(!path.win32.isAbsolute(root))throw new Error("Windows system directory is not absolute");
  const executable=path.win32.join(root,"System32","tar.exe");
  if(!exists(executable))throw new Error("Windows System32 tar.exe is unavailable");
  return executable;
}
module.exports={archiveTool};