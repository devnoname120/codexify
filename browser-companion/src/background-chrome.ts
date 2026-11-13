import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

export async function backgroundChromeArgs(profilePath: string): Promise<string[]> {
  // Patchright 1.63 exposes its server argument generator through this package
  // export. Keep those defaults; replace only the foreground startup page.
  const core = require("patchright-core/lib/coreBundle") as {
    server: { createPlaywright(options: { sdkLanguage:string }): {
      chromium: { defaultArgs(options: { channel:string; headless:boolean }, persistent:boolean, profile:string): Promise<string[]> };
    } };
  };
  const server = core.server.createPlaywright({ sdkLanguage:"javascript" });
  const args = await server.chromium.defaultArgs({ channel:"chrome", headless:false }, true, profilePath);
  if (!args.includes(`--user-data-dir=${profilePath}`) || !args.includes("--remote-debugging-pipe") || !args.includes("about:blank")) {
    throw new Error("Patchright's persistent Chrome launch arguments changed");
  }
  // Agent switches navigate directly to their bound URL. Keeping whole old
  // documents for Back/Forward only retains extra chat heaps between workers.
  return [...args.filter(arg => arg !== "about:blank" && arg !== "--disable-back-forward-cache"),
    "--disable-back-forward-cache", "--no-startup-window"];
}
