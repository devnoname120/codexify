// Derived from miuuyy/codex-chatgpt-web launcher/scripts/package.cjs at f9ad4ae83a579287105ad822dd0c3e0029b04ef6.
const { createHash } = require("node:crypto");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { installChrome } = require("./install-chrome.cjs");
const { stageWindowsApplication } = require("./package-stage.cjs");

const root = path.resolve(__dirname, "..");
const repositoryRoot = path.resolve(root, "..");
const packageManifest = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
const electronBuilderCli = require.resolve("electron-builder/out/cli/cli.js", { paths:[root] });

function argument(name) {
  const prefix = `${name}=`;
  const inline = process.argv.slice(2).find(value => value.startsWith(prefix));
  if (inline) return inline.slice(prefix.length);
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

function cargoVersion() {
  const contents = fs.readFileSync(path.join(repositoryRoot, "Cargo.toml"), "utf8");
  const packageSection = contents.match(/^\[package\]\s*$([\s\S]*?)(?=^\[|\Z)/m)?.[1];
  const version = packageSection?.match(/^version\s*=\s*"([^"]+)"\s*$/m)?.[1];
  if (!version || !/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error("Could not determine the Codexify package version");
  }
  return version;
}

function runChecked(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd:options.cwd ?? root,
    env:options.env ?? process.env,
    stdio:options.stdio ?? "inherit",
    encoding:options.encoding,
    maxBuffer:64 * 1024 * 1024,
    shell:false,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${command} failed with status ${result.status ?? "unknown"}`);
  }
  return result;
}

function singleDirectory(parent, predicate, label) {
  const matches = fs.readdirSync(parent, { withFileTypes:true })
    .filter(entry => entry.isDirectory() && predicate(entry.name))
    .map(entry => path.join(parent, entry.name));
  if (matches.length !== 1) {
    throw new Error(`Expected exactly one ${label}; found ${matches.map(value => path.basename(value)).join(", ") || "none"}`);
  }
  return matches[0];
}

function executableRelativePath(platform, appRoot) {
  if (platform === "darwin") {
    const appBundle = singleDirectory(appRoot, name => name.endsWith(".app"), "macOS application bundle");
    const executable = path.join(appBundle, "Contents", "MacOS", packageManifest.build.productName);
    if (!fs.statSync(executable).isFile()) throw new Error("Packaged macOS entrypoint is missing");
    return path.relative(appRoot, executable);
  }
  const name = platform === "win32" ? "codexify-browser-companion.exe" : "codexify-browser-companion";
  const executable = path.join(appRoot, name);
  if (!fs.statSync(executable).isFile()) throw new Error(`Packaged entrypoint is missing: ${name}`);
  return path.relative(appRoot, executable);
}

const targetPlatform = argument("--platform") ?? process.platform;
const targetArch = argument("--arch") ?? process.arch;
if (!["darwin", "linux", "win32"].includes(targetPlatform)) {
  throw new Error(`Unsupported browser companion platform: ${targetPlatform}`);
}
if (!["x64", "arm64"].includes(targetArch)) {
  throw new Error(`Unsupported browser companion architecture: ${targetArch}`);
}
if (targetPlatform !== process.platform) {
  throw new Error(`Cross-OS browser companion packaging is disabled (${process.platform} -> ${targetPlatform})`);
}
if (targetPlatform === "win32" && targetArch !== "x64") {
  throw new Error("Windows browser companion releases support x64 only");
}

const version = cargoVersion();
const schema = fs.readFileSync(path.join(root, "protocol-schema-v1.json"));
const protocolSchemaHash = createHash("sha256").update(schema).digest("hex");
const build = `${version}+protocol.${protocolSchemaHash}`;
const publicPlatform = targetPlatform === "darwin" ? `darwin-${targetArch}`
  : targetPlatform === "linux" ? `linux-${targetArch}`
    : "windows-x64";
const extension = targetPlatform === "win32" ? "zip" : "tar.gz";
const packageRootName = `codexify-browser-v${version}-${publicPlatform}`;
const artifactName = `${packageRootName}.${extension}`;
const artifactsDirectory = path.resolve(process.env.CODEXIFY_BROWSER_ARTIFACTS ?? path.join(root, "artifacts"));
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "codexify-browser-package-"));
const builderOutput = path.join(scratch, "builder");
const packageStage = path.join(scratch, "package");
const packageRoot = path.join(packageStage, packageRootName);
const appRoot = path.join(packageRoot, "app");

try {
  const browserRoot = installChrome(targetPlatform, targetArch);
  runChecked("bun", ["run", "build"]);
  const builderConfig = path.join(scratch, "electron-builder.json");
  fs.writeFileSync(builderConfig, JSON.stringify({
    ...packageManifest.build,
    extraResources:[{
      from:browserRoot,
      to:"chrome",
      filter:["chromium-*/**", "chrome-executable.json"],
    }],
  }));
  const targetFlag = targetPlatform === "darwin" ? "--mac"
    : targetPlatform === "linux" ? "--linux"
      : "--win";
  const env = { ...process.env, CSC_IDENTITY_AUTO_DISCOVERY:"false" };
  const builderArguments = [
    electronBuilderCli,
    targetFlag,
    "--dir",
    `--${targetArch}`,
    "--publish",
    "never",
    `--config=${builderConfig}`,
    `--config.directories.output=${builderOutput}`,
    `--config.extraMetadata.version=${version}`,
  ];
  if (targetPlatform === "darwin" && !env.CSC_LINK && !env.CSC_NAME) {
    builderArguments.push("--config.mac.identity=-");
  }
  runChecked("node", builderArguments, { env });

  let unpacked;
  if (targetPlatform === "darwin") {
    const macDirectory = singleDirectory(builderOutput, name => name.startsWith("mac"), "macOS builder directory");
    unpacked = macDirectory;
  } else {
    unpacked = singleDirectory(builderOutput, name => name.endsWith("-unpacked"), "unpacked Electron application");
  }
  if (targetPlatform === "darwin") {
    fs.mkdirSync(appRoot, { recursive:true, mode:0o700 });
    const appBundle = singleDirectory(unpacked, name => name.endsWith(".app"), "macOS application bundle");
    fs.cpSync(appBundle, path.join(appRoot, path.basename(appBundle)), {
      recursive:true,
      dereference:false,
      verbatimSymlinks:true,
    });
  } else if (targetPlatform === "win32") {
    stageWindowsApplication(scratch, unpacked, appRoot);
  } else {
    fs.mkdirSync(appRoot, { recursive:true, mode:0o700 });
    fs.cpSync(unpacked, appRoot, { recursive:true, dereference:false, verbatimSymlinks:true });
  }
  const executable = executableRelativePath(targetPlatform, appRoot);
  const entrypoint = path.posix.join("app", ...executable.split(path.sep));
  fs.writeFileSync(path.join(packageRoot, "manifest.json"), `${JSON.stringify({
    version:1,
    codexifyVersion:version,
    protocolVersion:1,
    protocolSchemaHash,
    build,
    platform:publicPlatform,
    entrypoint,
  }, null, 2)}\n`, { mode:0o600 });
  fs.copyFileSync(path.join(repositoryRoot, "LICENSE"), path.join(packageRoot, "LICENSE"));
  fs.copyFileSync(
    path.join(repositoryRoot, "THIRD_PARTY_NOTICES.md"),
    path.join(packageRoot, "THIRD_PARTY_NOTICES.md"),
  );
  const licenseDirectory = path.join(packageRoot, "LICENSES");
  fs.mkdirSync(licenseDirectory, { recursive:true });
  fs.copyFileSync(
    path.join(repositoryRoot, "LICENSES", "codex-chatgpt-web-MIT.txt"),
    path.join(licenseDirectory, "codex-chatgpt-web-MIT.txt"),
  );

  fs.mkdirSync(artifactsDirectory, { recursive:true });
  const artifact = path.join(artifactsDirectory, artifactName);
  fs.rmSync(artifact, { force:true });
  if (targetPlatform === "win32") {
    const zipOptions=process.env.CODEXIFY_BROWSER_FAST_LOCAL_PACKAGE==="1"?["--options","zip:compression-level=1"]:[];
    runChecked("tar", ["-a", "-cf", artifact,...zipOptions, "-C", packageStage, packageRootName]);
  } else {
    runChecked("tar", ["-czf", artifact, "-C", packageStage, packageRootName], {
      env:{ ...process.env, COPYFILE_DISABLE:"1" },
    });
  }
  process.stdout.write(`${artifact}\n`);
} finally {
  fs.rmSync(scratch, { recursive:true, force:true, maxRetries:8, retryDelay:200 });
}
