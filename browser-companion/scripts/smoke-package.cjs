const { createHash } = require("node:crypto");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const root = path.resolve(__dirname, "..");
const repositoryRoot = path.resolve(root, "..");
const PACKAGE_EXTRACTION_TIMEOUT_MS = 10 * 60_000;

function cargoVersion() {
  const contents = fs.readFileSync(path.join(repositoryRoot, "Cargo.toml"), "utf8");
  const packageSection = contents.match(/^\[package\]\s*$([\s\S]*?)(?=^\[|\Z)/m)?.[1];
  const version = packageSection?.match(/^version\s*=\s*"([^"]+)"\s*$/m)?.[1];
  if (!version) throw new Error("Could not determine the Codexify package version");
  return version;
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd:options.cwd ?? root,
    env:options.env ?? process.env,
    encoding:"utf8",
    maxBuffer:64 * 1024 * 1024,
    timeout:options.timeout ?? 60_000,
    windowsHide:true,
    shell:false,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${command} failed with status ${result.status ?? "unknown"}: ${result.stderr?.trim() || result.stdout?.trim() || "no output"}`);
  }
}

const version = cargoVersion();
const platform = process.platform === "darwin" ? `darwin-${process.arch}`
  : process.platform === "linux" ? `linux-${process.arch}`
    : process.platform === "win32" && process.arch === "x64" ? "windows-x64"
      : null;
if (!platform) throw new Error(`Unsupported package smoke platform: ${process.platform}/${process.arch}`);
const extension = process.platform === "win32" ? "zip" : "tar.gz";
const packageRootName = `codexify-browser-v${version}-${platform}`;
const artifact = path.resolve(
  process.env.CODEXIFY_BROWSER_ARTIFACT
    ?? path.join(root, "artifacts", `${packageRootName}.${extension}`),
);
if (!fs.statSync(artifact).isFile()) throw new Error(`Browser companion artifact is missing: ${artifact}`);

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "codexify-browser-smoke-"));
const marker = path.join(scratch, "ready.json");
const descriptor = path.join(scratch, "browser", "descriptor.json");
try {
  const extract = path.join(scratch, "extract");
  fs.mkdirSync(extract);
  run("tar", process.platform === "win32"
    ? ["-xf", artifact, "-C", extract]
    : ["-xzf", artifact, "-C", extract], { timeout:PACKAGE_EXTRACTION_TIMEOUT_MS });
  const packageRoot = path.join(extract, packageRootName);
  const manifest = JSON.parse(fs.readFileSync(path.join(packageRoot, "manifest.json"), "utf8"));
  const protocolSchemaHash = createHash("sha256")
    .update(fs.readFileSync(path.join(root, "protocol-schema-v1.json")))
    .digest("hex");
  const expectedBuild = `${version}+protocol.${protocolSchemaHash}`;
  if (manifest.version !== 1
    || manifest.codexifyVersion !== version
    || manifest.protocolVersion !== 1
    || manifest.protocolSchemaHash !== protocolSchemaHash
    || manifest.build !== expectedBuild
    || manifest.platform !== platform
    || typeof manifest.entrypoint !== "string") {
    throw new Error(`Browser companion artifact manifest is invalid: ${JSON.stringify(manifest)}`);
  }
  for (const notice of [
    "LICENSE",
    "THIRD_PARTY_NOTICES.md",
    path.join("LICENSES", "codex-chatgpt-web-MIT.txt"),
  ]) {
    if (!fs.statSync(path.join(packageRoot, notice)).isFile()) {
      throw new Error(`Browser companion artifact notice is missing: ${notice}`);
    }
  }
  const entrypoint = path.join(packageRoot, ...manifest.entrypoint.split("/"));
  if (!fs.statSync(entrypoint).isFile()) throw new Error("Browser companion packaged entrypoint is missing");

  const env = {
    ...process.env,
    CODEXIFY_BROWSER_HOME:path.join(scratch, "browser"),
    CODEXIFY_BROWSER_DESCRIPTOR:descriptor,
    CODEXIFY_BROWSER_BUILD:expectedBuild,
    CODEXIFY_BROWSER_SMOKE_FILE:marker,
  };
  if (process.platform === "linux") {
    // This smoke path only writes metadata; extracted CI packages lack a configured SUID sandbox.
    run("xvfb-run", ["-a", entrypoint, "--no-sandbox"], { env });
  } else {
    run(entrypoint, [], { env });
  }
  const result = JSON.parse(fs.readFileSync(marker, "utf8"));
  if (result.ok !== true
    || result.packaged !== true
    || result.protocolVersion !== 1
    || result.build !== expectedBuild
    || result.platform !== process.platform
    || result.arch !== process.arch) {
    throw new Error(`Unexpected packaged browser companion marker: ${JSON.stringify(result)}`);
  }
  if (fs.existsSync(descriptor)) throw new Error("Package smoke unexpectedly started the companion control server");
  process.stdout.write(`PACKAGED_BROWSER_COMPANION_SMOKE_OK ${process.platform}/${process.arch}\n`);
} finally {
  const target=path.resolve(scratch),temporaryRoot=path.resolve(os.tmpdir())+path.sep;
  if(!target.startsWith(temporaryRoot)||!path.basename(target).startsWith("codexify-browser-smoke-"))
    throw new Error("Refusing to remove a package smoke path outside its temporary directory");
  // Electron helpers and Windows scanning can release extracted DLLs after
  // the parent exits. Let that bounded shutdown finish before removing them.
  try {fs.rmSync(target, { recursive:true, force:true, maxRetries:12, retryDelay:250 });}
  catch(error) {
    if(process.platform!=="win32" || !["EPERM","EBUSY","ENOTEMPTY"].includes(error.code))throw error;
    // Validation already finished. Retain locked test output for later cleanup;
    // never turn a late helper/antivirus file lease into a false package failure.
    console.warn(`Package smoke temporary files retained until Windows releases them: ${target}`);
  }
}
