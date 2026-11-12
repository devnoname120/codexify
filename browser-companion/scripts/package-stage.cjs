const fs = require("node:fs");
const path = require("node:path");

// Windows packages can reuse Electron's completed directory. Both locations
// belong to this invocation's private scratch directory; never move a checkout,
// browser profile, symbolic link or existing destination.
function stageWindowsApplication(scratch, unpacked, appRoot) {
  const root = fs.realpathSync(scratch);
  const source = fs.realpathSync(unpacked);
  const target = path.resolve(appRoot);
  const within = candidate => {
    const relative = path.relative(root, candidate);
    return relative.length > 0 && relative !== ".."
      && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative);
  };
  if (!within(source) || !within(target)
    || fs.lstatSync(unpacked).isSymbolicLink() || !fs.statSync(source).isDirectory()
    || fs.lstatSync(target,{throwIfNoEntry:false})) throw new Error("Unsafe Windows package staging paths");
  let ancestor = path.dirname(target);
  while (!fs.existsSync(ancestor)) ancestor = path.dirname(ancestor);
  const realAncestor = fs.realpathSync(ancestor);
  if (path.relative(root,realAncestor) !== "" && !within(realAncestor)) {
    throw new Error("Windows package staging parent escaped its scratch directory");
  }
  fs.mkdirSync(path.dirname(target), { recursive:true, mode:0o700 });
  if (!within(fs.realpathSync(path.dirname(target)))) {
    throw new Error("Windows package staging parent escaped its scratch directory");
  }
  fs.renameSync(source, target);
}

module.exports = {stageWindowsApplication};
